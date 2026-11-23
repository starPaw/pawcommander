//! Allocated blocks reported by macOS for entries under a local folder.
use super::{
    compare::scan_limit,
    guarded_directory::Directory,
    read_scope::{local_subdirectory, visit_local_entries, ReadScope},
    smb_output, Location,
};
use crate::{
    file_error::{
        AccessAction, AccessError, Failure, MutationError, ScanError, ScanLimit, SmbMutationAction,
    },
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

pub(crate) enum Issue {
    Failure(Box<TransferError>),
    AlreadyCounted,
    Partial,
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coverage {
    Unavailable,
    Partial,
    Complete,
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
    pub(crate) issue: Option<Issue>,
    pub(crate) coverage: Coverage,
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
            coverage: Coverage::Unavailable,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Finish {
    Complete,
    Stopped,
    Limit(ScanLimit),
    VolumeUnverified(TransferError),
}

pub(crate) struct Analysis {
    pub(crate) folder: Location,
    pub(crate) nodes: Vec<Node>,
    pub(crate) files: Vec<NodeId>,
    pub(crate) finish: Finish,
    pub(crate) issues: usize,
    pub(crate) issue_examples: Vec<NodeId>,
    pub(crate) root_verification: Option<TransferError>,
    pub(crate) symlinks: usize,
    pub(crate) hard_links: usize,
}

impl Analysis {
    pub(crate) fn is_partial(&self) -> bool {
        self.nodes[0].coverage != Coverage::Complete
    }

    fn record_root_error(&mut self, error: TransferError) {
        if matches!(self.finish, Finish::Complete) && is_smb_mount_error(&error) {
            self.finish = Finish::VolumeUnverified(error);
        } else {
            self.root_verification = Some(error);
        }
        if self.nodes[0].coverage == Coverage::Complete {
            self.nodes[0].coverage = Coverage::Partial;
        }
    }
}

struct Scan<'a> {
    result: Analysis,
    scope: &'a ReadScope,
    root: &'a OwnedFd,
    folders: Vec<(NodeId, Identity)>,
    visited_folders: HashSet<Identity>,
    files: Vec<(NodeId, Identity)>,
    path_bytes: usize,
}

pub(crate) fn analyze(
    folder: &Location,
    progress: &mut Progress,
) -> Result<Analysis, TransferError> {
    if folder.local().is_none() {
        return Err(ScanError::DiskUsageRequiresLocalFolder(folder.clone()).into());
    }
    let cancel = progress.cancellation();
    cancel.check()?;
    let mut mount = smb_output::guard(folder, SmbMutationAction::AnalyzeDiskUsage)?;
    let scope = ReadScope::open_with_smb(folder, &mut mount)
        .map_err(|error| folder_error(folder, mount.recheck().err().unwrap_or(error)))?;
    let location = &scope.parent.location;
    let root = scope
        .descriptor
        .as_ref()
        .ok_or_else(|| ScanError::FolderUnavailable(location.clone()))?;
    let stat = (|| {
        Directory::from_scope(&scope)?;
        fstat(root).map_err(|error| {
            AccessError::new(
                location,
                AccessAction::ReadOpenMetadata,
                io::Error::from(error),
            )
            .into()
        })
    })()
    .map_err(|error| folder_error(location, scope.verify_mount().err().unwrap_or(error)))?;
    let mut scan = Scan {
        result: Analysis {
            folder: location.clone(),
            nodes: vec![Node::new(PathBuf::new(), Kind::Folder, None)],
            files: Vec::new(),
            finish: Finish::Complete,
            issues: 0,
            issue_examples: Vec::new(),
            root_verification: None,
            symlinks: 0,
            hard_links: 0,
        },
        scope: &scope,
        root,
        folders: vec![(0, identity(&stat))],
        visited_folders: HashSet::new(),
        files: Vec::new(),
        path_bytes: 0,
    };
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
            Err(TransferError::Failed(Failure::Scan(error)))
                if matches!(error.as_ref(), ScanError::DiskUsageLimit(_)) =>
            {
                let ScanError::DiskUsageLimit(limit) = *error else {
                    unreachable!("matched the disk usage limit");
                };
                scan.incomplete(node, Finish::Limit(limit));
                break;
            }
            Err(error) => {
                if cancel.is_requested() {
                    scan.incomplete(node, Finish::Stopped);
                    break;
                }
                // A listing error can be the first symptom of losing the mount.
                let error = scope.verify_mount().err().unwrap_or(error);
                if is_smb_mount_error(&error) {
                    scan.incomplete(node, Finish::VolumeUnverified(error));
                    break;
                }
                scan.result.nodes[node].issue = Some(Issue::Failure(Box::new(error)));
            }
        }
    }
    let mut result = scan.finish();
    if !matches!(result.finish, Finish::VolumeUnverified(_)) {
        let verified = scope.verify_selected_root(folder);
        if let Err(error) = scope.verify_mount().and(verified) {
            result.record_root_error(folder_error(location, error));
        }
    }
    Ok(result)
}

impl Scan<'_> {
    fn visit_folder(
        &mut self,
        node: NodeId,
        expected: Identity,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        cancel.check()?;
        self.scope.verify_mount()?;
        let path = self.result.nodes[node].path.clone();
        let location = self.result.folder.join(&path);
        let unavailable = || ScanError::FolderUnavailable(location.clone());
        let read_children = |error| AccessError::new(&location, AccessAction::ReadChildren, error);
        let nested = local_subdirectory(self.root, &path, cancel).map_err(read_children)?;
        let descriptor = nested.as_ref().unwrap_or(self.root);
        let mount = self.scope.owned_smb_mount();
        if let Some(mount) = mount {
            mount.verify_directory(descriptor, location.path(), location.path())?;
        }
        let stat = fstat(descriptor).map_err(|error| {
            AccessError::new(
                &location,
                AccessAction::ReadOpenMetadata,
                io::Error::from(error),
            )
        })?;
        if identity(&stat) != expected {
            return Err(unavailable().into());
        }
        if !self.visited_folders.insert(expected) {
            self.result.nodes[node].issue = Some(Issue::AlreadyCounted);
            self.result.nodes[node].coverage = Coverage::Complete;
            return Ok(());
        }
        let allocated = allocated_bytes(&stat);
        let device = stat.st_dev;
        self.result.nodes[node].allocated = allocated;
        self.result.nodes[node].own_blocks = allocated;
        // Counting the folder itself gives a partial size until its entries have been read.
        self.result.nodes[node].coverage = Coverage::Partial;

        visit_local_entries(&location, descriptor, cancel, |name, stat| {
            let relative = path.join(name);
            self.path_bytes += relative.as_os_str().len();
            let id = self.result.nodes.len();
            if let Some(limit) = scan_limit(&self.result.folder, &relative, id + 1, self.path_bytes)
            {
                return Err(ScanError::DiskUsageLimit(limit).into());
            }
            let mut child = Node::new(relative, Kind::Other, Some(node));
            let stat = match stat {
                Ok(stat) if mount.is_some() && stat.st_dev != device => {
                    Err(ScanError::OtherVolume(location.join(name)).into())
                }
                Ok(stat) => Ok(stat),
                Err(error) => {
                    self.scope.verify_mount()?;
                    Err(AccessError::new(
                        &location.join(name),
                        AccessAction::ReadMetadata,
                        io::Error::from(error),
                    )
                    .into())
                }
            };
            match stat {
                Ok(stat) => {
                    child.coverage = Coverage::Complete;
                    match FileType::from_raw_mode(stat.st_mode) {
                        FileType::Directory => {
                            child.kind = Kind::Folder;
                            child.coverage = Coverage::Unavailable;
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
                    }
                }
                Err(error) => {
                    child.issue = Some(Issue::Failure(Box::new(error)));
                }
            }
            self.result.nodes[node].children.push(id);
            self.result.nodes.push(child);
            Ok(())
        })?;
        cancel.check()?;
        self.scope.verify_mount()?;
        self.result.nodes[node].coverage = Coverage::Complete;
        Ok(())
    }

    fn incomplete(&mut self, node: NodeId, finish: Finish) {
        self.result.finish = finish;
        let current = &mut self.result.nodes[node];
        current.issue = Some(if current.coverage == Coverage::Unavailable {
            Issue::Pending
        } else {
            Issue::Partial
        });
        for (pending, _) in self.folders.drain(..) {
            self.result.nodes[pending].issue = Some(Issue::Pending);
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
            if nodes[id].coverage != Coverage::Complete {
                nodes[parent].coverage = Coverage::Partial;
            }
        }
        for id in 0..nodes.len() {
            if matches!(nodes[id].issue, Some(Issue::Failure(_))) {
                self.result.issues += 1;
                if self.result.issue_examples.len() < 4 {
                    self.result.issue_examples.push(id);
                }
            }
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

fn is_smb_mount_error(error: &TransferError) -> bool {
    matches!(error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::SmbMount { .. }))
}

fn folder_error(location: &Location, error: TransferError) -> TransferError {
    match error {
        TransferError::Failed(Failure::Io { source, .. }) => {
            AccessError::new(location, AccessAction::ReadChildren, source).into()
        }
        TransferError::Failed(Failure::Mutation(error))
            if matches!(*error, MutationError::UndoUnavailable(_)) =>
        {
            ScanError::FolderUnavailable(location.clone()).into()
        }
        error => error,
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
    use crate::{file_error::ScanLimitKind, vfs::MAX_TREE_DEPTH};
    use std::{
        fs,
        io::Write,
        os::{
            macos::fs::MetadataExt,
            unix::fs::{symlink, PermissionsExt},
        },
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
        assert!(matches!(data.finish, Finish::Complete));
        assert!(!data.is_partial());
        assert_eq!(data.issues, 0);
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
        assert!(matches!(data.finish, Finish::Stopped));
        assert!(data.is_partial());
        assert!(!data.files.is_empty() && data.files.len() < 40);
        let root = &data.nodes[0];
        assert!(matches!(root.issue, Some(Issue::Partial)));
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

    #[test]
    fn keeps_allocated_blocks_and_the_cause_of_a_denied_folder() {
        // A privileged process can read the permission fixture.
        // SAFETY: geteuid has no inputs and reads the process identity.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        let denied = folder.join("denied");
        let visible = folder.join("visible");
        fs::create_dir_all(&denied).unwrap();
        fs::write(denied.join("unread"), [0x76; 4096]).unwrap();
        fs::write(&visible, [0x23; 8192]).unwrap();
        let permissions = fs::metadata(&denied).unwrap().permissions();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let location = Location::Local(temp.path().into());
        let result = analyze(&location, &mut Progress::default());
        fs::set_permissions(&denied, permissions).unwrap();

        let data = result.unwrap();
        assert!(matches!(data.finish, Finish::Complete));
        assert_eq!((data.issues, data.files.len()), (1, 1));
        assert!(data.is_partial());
        let visible_blocks = visible.metadata().unwrap().st_blocks() * 512;
        assert!(visible_blocks > 0);
        let folder = data
            .nodes
            .iter()
            .find(|node| node.path == Path::new("folder"))
            .unwrap();
        assert_eq!(folder.coverage, Coverage::Partial);
        assert_eq!(folder.allocated, folder.own_blocks + visible_blocks);
        assert_eq!(
            data.nodes[0].allocated,
            data.nodes[0].own_blocks + folder.allocated
        );
        let [sample] = data.issue_examples.as_slice() else {
            panic!("expected one scan error example");
        };
        let node = &data.nodes[*sample];
        assert_eq!(node.path, Path::new("folder/denied"));
        assert_eq!(node.allocated, 0);
        assert_eq!(node.coverage, Coverage::Unavailable);
        let Some(Issue::Failure(error)) = &node.issue else {
            panic!("expected a read error for the denied folder");
        };
        assert!(matches!(
            error.as_ref(),
            TransferError::Failed(Failure::Access(error))
                if error.location == data.folder.join("folder/denied") && error.action == AccessAction::ReadChildren
                    && matches!(&error.cause, crate::file_error::AccessCause::Local(cause)
                        if cause.raw_os_error() == Some(libc::EACCES))
        ));
        assert_eq!(fs::read(denied.join("unread")).unwrap(), [0x76; 4096]);
    }

    #[test]
    fn depth_limit_preserves_counted_files_and_reports_the_exact_path() {
        let temp = tempfile::tempdir().unwrap();
        let relative: PathBuf = std::iter::repeat_n("d", MAX_TREE_DEPTH + 1).collect();
        fs::create_dir_all(temp.path().join(&relative)).unwrap();
        let counted = temp.path().join("counted");
        fs::write(&counted, [0x32; 4096]).unwrap();

        let location = Location::Local(temp.path().into());
        let data = analyze(&location, &mut Progress::default()).unwrap();
        assert!(matches!(&data.finish, Finish::Limit(limit)
            if limit.location == data.folder.join(&relative)
                && limit.kind == ScanLimitKind::Depth
                && limit.maximum == MAX_TREE_DEPTH
        ));
        assert!(data.is_partial());
        assert_eq!(data.issues, 0);
        assert_eq!(data.files.len(), 1);
        assert_eq!(
            data.nodes[data.files[0]].allocated,
            counted.metadata().unwrap().st_blocks() * 512
        );
        assert!(data.nodes.iter().all(|node| node.path != relative));
        assert!(data
            .nodes
            .iter()
            .any(|node| node.path == relative.parent().unwrap()
                && matches!(node.issue, Some(Issue::Partial))));
    }

    #[test]
    fn root_verification_does_not_overwrite_the_stop_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let displaced = temp.path().join("displaced");
        fs::create_dir(&root).unwrap();
        for index in 0..20 {
            fs::write(root.join(index.to_string()), [0x42; 4096]).unwrap();
        }
        let location = Location::Local(root.clone());
        let mut checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checks += 1;
            if checks != 12 {
                return false;
            }
            fs::rename(&root, &displaced).unwrap();
            fs::create_dir(&root).unwrap();
            true
        });
        let data = analyze(&location, &mut Progress::with_cancellation(cancel)).unwrap();

        assert!(matches!(data.finish, Finish::Stopped));
        assert!(matches!(data.nodes[0].issue, Some(Issue::Partial)));
        assert!(data.is_partial() && !data.files.is_empty());
        assert_eq!(data.issues, 0);
        assert!(
            matches!(data.root_verification, Some(TransferError::Failed(Failure::Scan(error)))
                if matches!(error.as_ref(), ScanError::FolderUnavailable(path) if path == &data.folder)
            )
        );
        assert_eq!(
            fs::read_dir(temp.path().join("displaced")).unwrap().count(),
            20
        );
    }

    #[test]
    fn losing_the_mount_at_final_verification_retains_counted_blocks_as_partial() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("counted");
        fs::write(&file, [0x73; 8192]).unwrap();
        let location = Location::Local(temp.path().into());
        let mut data = analyze(&location, &mut Progress::default()).unwrap();
        let allocated = data.nodes[0].allocated;
        assert!(!data.is_partial());

        let path = data.folder.path();
        let error = MutationError::smb_mount(
            path,
            SmbMutationAction::AnalyzeDiskUsage,
            crate::smb::SmbError::MissingMount(path.to_owned()),
        );
        data.record_root_error(error.into());

        assert!(data.is_partial());
        assert_eq!(data.nodes[0].allocated, allocated);
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.nodes[data.files[0]].coverage, Coverage::Complete);
        assert!(data.root_verification.is_none());
        assert!(
            matches!(data.finish, Finish::VolumeUnverified(TransferError::Failed(Failure::Mutation(error)))
                if matches!(error.as_ref(), MutationError::SmbMount { path, .. } if path == data.folder.path()))
        );
    }
}
