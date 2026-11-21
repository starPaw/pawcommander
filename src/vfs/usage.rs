//! Allocated blocks reported by macOS for entries under a local folder.
use super::{
    compare::{MAX_ENTRIES, MAX_PATH_BYTES},
    guarded_directory::Directory,
    read_scope::{local_subdirectory, visit_local_entries, ReadScope},
    Location, MAX_TREE_DEPTH,
};
use crate::{
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use rustix::fs::{fstat, FileType, Stat};
use std::{collections::HashSet, io, os::fd::OwnedFd, path::PathBuf};

pub(crate) type NodeId = usize;
type Identity = (i32, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Folder,
    File,
    Symlink,
    Other,
}

pub(crate) struct Node {
    pub(crate) path: PathBuf,
    pub(crate) kind: Kind,
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) allocated: u64,
    pub(crate) own_blocks: u64,
    pub(crate) logical: u64,
    pub(crate) hard_link: bool,
    pub(crate) issue: Option<String>,
}

impl Node {
    fn new(path: PathBuf, kind: Kind, parent: Option<NodeId>) -> Self {
        Self {
            path,
            kind,
            parent,
            children: Vec::new(),
            allocated: 0,
            own_blocks: 0,
            logical: 0,
            hard_link: false,
            issue: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    Complete,
    Stopped,
    Limit,
}

pub(crate) struct Analysis {
    pub(crate) folder: Location,
    pub(crate) nodes: Vec<Node>,
    pub(crate) files: Vec<NodeId>,
    pub(crate) finish: Finish,
    pub(crate) issues: usize,
    pub(crate) symlinks: usize,
    pub(crate) hard_links: usize,
}

struct Scan {
    result: Analysis,
    root: OwnedFd,
    folders: Vec<(NodeId, Identity)>,
    visited_folders: HashSet<Identity>,
    files: Vec<(NodeId, Identity)>,
    path_bytes: usize,
    limit: bool,
}

pub(crate) fn analyze(
    folder: &Location,
    progress: &mut Progress,
) -> Result<Analysis, TransferError> {
    if folder.local().is_none() {
        return Err(text!("Анализ места доступен в локальных папках").into());
    }
    let scope = ReadScope::open(folder)?;
    Directory::from_scope(&scope)?;
    let root = scope
        .descriptor
        .ok_or_else(|| text!("Не удалось открыть папку анализа"))?;
    let stat = fstat(&root).map_err(io::Error::from)?;
    let mut scan = Scan {
        result: Analysis {
            folder: scope.parent.location.clone(),
            nodes: vec![Node::new(PathBuf::new(), Kind::Folder, None)],
            files: Vec::new(),
            finish: Finish::Complete,
            issues: 0,
            symlinks: 0,
            hard_links: 0,
        },
        root,
        folders: vec![(0, identity(&stat))],
        visited_folders: HashSet::new(),
        files: Vec::new(),
        path_bytes: 0,
        limit: false,
    };
    let cancel = progress.cancellation();
    while let Some((node, expected)) = scan.folders.pop() {
        progress.item(
            &scan
                .result
                .folder
                .path()
                .join(&scan.result.nodes[node].path),
        );
        match scan.visit_folder(node, expected, &cancel) {
            Ok(()) => {}
            Err(TransferError::Cancelled) => {
                scan.incomplete(node, Finish::Stopped);
                break;
            }
            Err(_) if scan.limit => {
                scan.incomplete(node, Finish::Limit);
                break;
            }
            Err(error) => {
                scan.result.issues += 1;
                scan.result.nodes[node].issue = Some(crate::i18n::file_error(&error));
            }
        }
    }
    if let Err(error) = scope.parent.verify() {
        scan.result.issues += 1;
        scan.result.nodes[0].issue = Some(crate::i18n::file_error(&error));
    }
    Ok(scan.finish())
}

impl Scan {
    fn visit_folder(
        &mut self,
        node: NodeId,
        expected: Identity,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        cancel.check()?;
        let path = self.result.nodes[node].path.clone();
        let nested = local_subdirectory(&self.root, &path, cancel)?;
        // Own this descriptor so the visitor can accumulate rows without borrowing self.root.
        let descriptor =
            nested.map_or_else(|| rustix::io::dup(&self.root).map_err(io::Error::from), Ok)?;
        let stat = fstat(&descriptor).map_err(io::Error::from)?;
        if identity(&stat) != expected {
            return Err(text!("Папка изменилась или недоступна").into());
        }
        if !self.visited_folders.insert(expected) {
            self.result.nodes[node].issue = Some(text!("Эта папка уже учтена").into());
            return Ok(());
        }
        let allocated = allocated_bytes(&stat);
        self.result.nodes[node].allocated = allocated;
        self.result.nodes[node].own_blocks = allocated;

        visit_local_entries(&descriptor, cancel, |name, stat| {
            let relative = path.join(name);
            self.path_bytes += relative.as_os_str().len();
            if self.result.nodes.len() >= MAX_ENTRIES
                || self.path_bytes > MAX_PATH_BYTES
                || relative.components().count() > MAX_TREE_DEPTH
            {
                self.limit = true;
                return Err(text!("Достигнут предел анализа места").into());
            }
            let mut child = Node::new(relative, Kind::Other, Some(node));
            let id = self.result.nodes.len();
            match stat {
                Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                    FileType::Directory => {
                        child.kind = Kind::Folder;
                        self.folders.push((id, identity(&stat)));
                    }
                    FileType::RegularFile => {
                        child.kind = Kind::File;
                        child.allocated = allocated_bytes(&stat);
                        child.logical = u64::try_from(stat.st_size).unwrap_or_default();
                        self.files.push((id, identity(&stat)));
                    }
                    FileType::Symlink => {
                        child.kind = Kind::Symlink;
                        self.result.symlinks += 1;
                    }
                    _ => {}
                },
                Err(error) => {
                    self.result.issues += 1;
                    child.issue = Some(io::Error::from(error).to_string());
                }
            }
            self.result.nodes[node].children.push(id);
            self.result.nodes.push(child);
            Ok(())
        })
    }

    fn incomplete(&mut self, node: NodeId, finish: Finish) {
        self.result.finish = finish;
        self.result.nodes[node].issue = Some(text!("Папка проверена частично").into());
        for (pending, _) in self.folders.drain(..) {
            self.result.nodes[pending].issue = Some(text!("Папка ещё не проверена").into());
        }
    }

    fn finish(mut self) -> Analysis {
        let nodes = &mut self.result.nodes;
        self.files
            .sort_unstable_by(|(left, _), (right, _)| nodes[*left].path.cmp(&nodes[*right].path));
        let mut counted = HashSet::new();
        for (id, identity) in self.files {
            if identity.1 != 0 && !counted.insert(identity) {
                nodes[id].allocated = 0;
                nodes[id].hard_link = true;
                self.result.hard_links += 1;
            } else {
                self.result.files.push(id);
            }
        }
        // Parents are inserted before their descendants.
        for id in (1..nodes.len()).rev() {
            let parent = nodes[id].parent.unwrap();
            nodes[parent].allocated = nodes[parent].allocated.saturating_add(nodes[id].allocated);
        }
        for id in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[id].children);
            children.sort_unstable_by(|&left, &right| by_allocation(nodes, left, right));
            nodes[id].children = children;
        }
        self.result
            .files
            .sort_unstable_by(|&left, &right| by_allocation(nodes, left, right));
        self.result
    }
}

fn by_allocation(nodes: &[Node], left: NodeId, right: NodeId) -> std::cmp::Ordering {
    nodes[right]
        .allocated
        .cmp(&nodes[left].allocated)
        .then_with(|| nodes[left].path.cmp(&nodes[right].path))
}

fn identity(stat: &Stat) -> Identity {
    (stat.st_dev, stat.st_ino)
}

fn allocated_bytes(stat: &Stat) -> u64 {
    u64::try_from(stat.st_blocks)
        .unwrap_or_default()
        .saturating_mul(512)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        os::{macos::fs::MetadataExt, unix::fs::symlink},
        path::Path,
    };

    #[test]
    fn uses_reported_blocks_and_never_follows_symbolic_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let folder = root.join("media");
        fs::create_dir_all(&folder).unwrap();
        let dense = folder.join("dense");
        fs::write(&dense, vec![0xa7; 64 * 1024]).unwrap();
        let sparse = root.join("sparse");
        let mut file = fs::File::create(&sparse).unwrap();
        file.write_all(&[0xc5; 4096]).unwrap();
        file.set_len(8 * 1024 * 1024).unwrap();
        drop(file);
        fs::hard_link(&dense, root.join("zz-alias")).unwrap();
        symlink("..", folder.join("loop")).unwrap();
        fs::write(temp.path().join("outside"), b"outside the scope").unwrap();
        symlink(temp.path(), root.join("external")).unwrap();
        let location = Location::Local(root.clone());

        let data = analyze(&location, &mut Progress::default()).unwrap();
        let blocks = |path: &Path| path.metadata().unwrap().st_blocks() * 512;
        let node = |path: &str| {
            data.nodes
                .iter()
                .find(|node| node.path == Path::new(path))
                .unwrap()
        };
        assert_eq!((data.finish, data.issues), (Finish::Complete, 0));
        assert_eq!(
            (data.symlinks, data.hard_links, data.nodes.len()),
            (2, 1, 7)
        );
        assert_eq!(node("media/loop").kind, Kind::Symlink);
        assert_eq!(node("external").allocated, 0);
        assert!(node("zz-alias").hard_link);
        assert_eq!(node("zz-alias").allocated, 0);
        assert_eq!(node("media").allocated, blocks(&folder) + blocks(&dense));
        assert_eq!(node("sparse").logical, 8 * 1024 * 1024);
        assert_eq!(node("sparse").allocated, blocks(&sparse));
        assert_eq!(
            data.nodes[0].allocated,
            blocks(&root) + blocks(&folder) + blocks(&dense) + blocks(&sparse)
        );
        assert_eq!(data.files.len(), 2);
        let first = &data.nodes[data.files[0]];
        let second = &data.nodes[data.files[1]];
        assert!(first.allocated >= second.allocated);
        assert_eq!(
            fs::read(temp.path().join("outside")).unwrap(),
            b"outside the scope"
        );
    }

    #[test]
    fn cancellation_preserves_the_files_already_counted() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..40 {
            fs::write(temp.path().join(index.to_string()), [0x23; 4096]).unwrap();
        }
        let mut checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == 12
        });
        let location = Location::Local(temp.path().into());
        let data = analyze(&location, &mut Progress::with_cancellation(cancel)).unwrap();
        assert_eq!(data.finish, Finish::Stopped);
        assert!(!data.files.is_empty() && data.files.len() < 40);
        let root = &data.nodes[0];
        assert!(root.issue.is_some());
        assert_eq!(
            root.allocated,
            root.own_blocks
                + data
                    .files
                    .iter()
                    .map(|&id| data.nodes[id].allocated)
                    .sum::<u64>()
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 40);
    }
}
