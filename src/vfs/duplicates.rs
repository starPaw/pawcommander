//! Bounded local duplicate discovery. A digest is only a candidate filter.
mod trash;

use super::{
    compare::{scan, MAX_ENTRIES, MAX_PATH_BYTES},
    file_compare::stream_difference,
    guarded_directory::Directory,
    read_scope::{CheckedFile, ReadScope},
    transfer::{EntryMetadata, TransferSourceEntry},
    undo::Parent,
    Location, MAX_TREE_DEPTH,
};
use crate::{
    file_error::ScanError,
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsStr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use trash::SelectedGroup;
pub(crate) use trash::TrashPlan;

#[derive(Clone)]
pub(crate) struct Member {
    pub(crate) path: PathBuf,
    version: TransferSourceEntry,
}

pub(crate) struct Group {
    pub(crate) size: u64,
    pub(crate) members: Vec<Member>,
}

pub(crate) struct SearchResult {
    root: Parent,
    pub(crate) groups: Vec<Group>,
    pub(crate) skipped: usize,
    pub(crate) examples: Vec<(PathBuf, TransferError)>,
}

impl SearchResult {
    fn skip(&mut self, path: &Path, error: TransferError) {
        self.skipped += 1;
        if self.examples.len() < 4 {
            self.examples.push((path.to_owned(), error));
        }
    }

    pub(crate) fn trash_plan(
        &self,
        keepers: &[usize],
        selected: &BTreeSet<(usize, usize)>,
    ) -> Option<TrashPlan> {
        let mut removals = BTreeMap::<_, SelectedGroup>::new();
        for &(group, copy) in selected {
            let keeper = *keepers.get(group)?;
            let members = &self.groups.get(group)?.members;
            if copy == keeper {
                return None;
            }
            let original = members.get(keeper)?;
            let item = members.get(copy)?;
            removals
                .entry(group)
                .or_insert_with(|| SelectedGroup {
                    keeper: original.clone(),
                    copies: Vec::new(),
                })
                .copies
                .push(item.clone());
        }
        (!removals.is_empty()).then(|| TrashPlan {
            root: self.root.clone(),
            groups: removals.into_values().collect(),
        })
    }
}

pub(crate) fn find(
    folder: &Location,
    progress: &mut Progress,
) -> Result<SearchResult, TransferError> {
    if folder.local().is_none() {
        return Err(text!("Поиск дубликатов доступен в локальных папках").into());
    }
    let scope = ReadScope::open(folder)?;
    if scope
        .parent
        .location
        .path()
        .iter()
        .any(is_staging_directory)
    {
        return Err(text!("Выберите папку вне временных папок PawCommander").into());
    }
    Directory::from_scope(&scope)?;
    let mut result = SearchResult {
        root: scope.parent.clone(),
        groups: Vec::new(),
        skipped: 0,
        examples: Vec::new(),
    };
    let cancel = progress.cancellation();
    let sizes = collect_sizes(&scope, &mut result, progress, &cancel)?;
    let mut seen = HashSet::new();
    for (size, mut paths) in sizes.into_iter().filter(|(_, paths)| paths.len() > 1) {
        cancel.check()?;
        paths.sort_unstable();
        let mut hashes = BTreeMap::<[u8; 32], Vec<Member>>::new();
        for path in paths {
            let found = hash_candidate(&scope, &mut seen, &path, size, progress, &cancel);
            match found {
                Ok(Some((digest, version))) => hashes
                    .entry(digest)
                    .or_default()
                    .push(Member { path, version }),
                Ok(None) => {}
                Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                Err(error) => result.skip(&path, error),
            }
        }
        for members in hashes.into_values().filter(|group| group.len() > 1) {
            group_identical(&scope, members, size, &mut result, &cancel)?;
        }
    }
    cancel.check()?;
    scope.parent.verify()?;
    result.groups.sort_unstable_by(|left, right| {
        right
            .size
            .cmp(&left.size)
            .then_with(|| left.members[0].path.cmp(&right.members[0].path))
    });
    Ok(result)
}

fn collect_sizes(
    scope: &ReadScope,
    result: &mut SearchResult,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<BTreeMap<u64, Vec<PathBuf>>, TransferError> {
    let mut sizes = BTreeMap::<_, Vec<_>>::new();
    let mut folders = vec![PathBuf::new()];
    let mut count = 0;
    let mut path_bytes = 0;
    while let Some(folder) = folders.pop() {
        progress.item(&scope.parent.location.path().join(&folder));
        let mut limited = false;
        let visited = scan::visit(scope, &folder, cancel, &mut |name, entry| {
            let path = folder.join(name);
            count += 1;
            path_bytes += path.as_os_str().len();
            if count > MAX_ENTRIES
                || path_bytes > MAX_PATH_BYTES
                || path.components().count() > MAX_TREE_DEPTH
            {
                limited = true;
                return Err(text!("Достигнут предел поиска дубликатов: 100 000 объектов, 32 МиБ путей или 128 уровней папок").into());
            }
            match entry {
                scan::Entry::Directory if !path.file_name().is_some_and(is_staging_directory) => {
                    folders.push(path)
                }
                scan::Entry::Directory => {}
                scan::Entry::File(info) => match info.size {
                    Some(size) => sizes.entry(size).or_default().push(path),
                    None => result.skip(&path, ScanError::MissingLocalFileSize.into()),
                },
                scan::Entry::Link(_) => {}
                scan::Entry::Unreadable(error) => result.skip(&path, error),
            }
            Ok(())
        });
        match visited {
            Ok(()) => {}
            Err(error)
                if limited
                    || folder.as_os_str().is_empty()
                    || matches!(error, TransferError::Cancelled) =>
            {
                return Err(error)
            }
            Err(error) => result.skip(&folder, error),
        }
    }
    Ok(sizes)
}

fn is_staging_directory(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    [
        ".pawcommander-copy-",
        ".pawcommander-transfer-",
        super::transfer::SYNC_STAGING_PREFIX,
    ]
    .iter()
    .filter_map(|prefix| name.strip_prefix(prefix))
    .any(|suffix| {
        let mut parts = suffix.split('-');
        (0..3).all(|_| {
            parts.next().is_some_and(|part| {
                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
            })
        }) && parts.next().is_none()
    })
}

fn hash_candidate(
    scope: &ReadScope,
    seen: &mut HashSet<(u64, u64)>,
    path: &Path,
    size: u64,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<Option<([u8; 32], TransferSourceEntry)>, TransferError> {
    let mut file = scope
        .open_file(path, cancel)?
        .ok_or_else(|| changed_since_search(&scope.parent.location.join(path)))?;
    let version = file.stamp().clone();
    let EntryMetadata::Local(metadata) = &version.metadata else {
        return Err(changed_since_search(&scope.parent.location.join(path)));
    };
    if size != metadata.len() {
        return Err(changed_since_search(&scope.parent.location.join(path)));
    }
    let identity = (metadata.dev(), metadata.ino());
    if seen.contains(&identity) {
        return Ok(None);
    }
    let (location, size, input) = file.stream();
    let digest = progress
        .hash_file(location.path(), size, input)?
        .ok_or_else(|| changed_since_search(location))?;
    file.verify(cancel)?;
    seen.insert(identity);
    Ok(Some((digest, version)))
}

fn group_identical(
    scope: &ReadScope,
    members: Vec<Member>,
    size: u64,
    result: &mut SearchResult,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let mut equal_groups: Vec<Vec<Member>> = Vec::new();
    'candidate: for candidate in members {
        cancel.check()?;
        for group in &mut equal_groups {
            let equal = (|| {
                let mut source = checked(scope, &group[0], cancel)?;
                let mut copy = checked(scope, &candidate, cancel)?;
                same_contents(&mut source, &mut copy, cancel)
            })();
            match equal {
                Ok(true) => {
                    group.push(candidate);
                    continue 'candidate;
                }
                Ok(false) => {}
                Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                Err(error) => {
                    result.skip(&candidate.path, error);
                    continue 'candidate;
                }
            }
        }
        equal_groups.push(vec![candidate]);
    }
    result.groups.extend(
        equal_groups
            .into_iter()
            .filter(|members| members.len() > 1)
            .map(|members| Group { size, members }),
    );
    Ok(())
}

fn checked<'a>(
    scope: &'a ReadScope,
    member: &'a Member,
    cancel: &Cancellation,
) -> Result<CheckedFile<'a>, TransferError> {
    let location = scope.parent.location.join(&member.path);
    let file = scope
        .open_file(&member.path, cancel)?
        .ok_or_else(|| changed_since_search(&location))?;
    member
        .version
        .verify_metadata(&location, &file.stamp().metadata)
        .map_err(|_| changed_since_search(&location))?;
    Ok(file)
}

fn same_contents(
    left: &mut CheckedFile<'_>,
    right: &mut CheckedFile<'_>,
    cancel: &Cancellation,
) -> Result<bool, TransferError> {
    let (_, left_size, left_input) = left.stream();
    let (_, right_size, right_input) = right.stream();
    let equal = left_size == right_size
        && stream_difference([left_input, right_input], [left_size, right_size], cancel)?
            .0
            .is_none();
    left.verify(cancel)?;
    right.verify(cancel)?;
    Ok(equal)
}

fn changed_since_search(location: &Location) -> TransferError {
    crate::tr_format!(
        "An item changed after the search or is unavailable: {location}. Run a fresh search.",
        location = location
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn separates_same_size_files_and_counts_hard_links_once_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("tree");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.bin"), b"match").unwrap();
        fs::write(folder.join("b.bin"), b"match").unwrap();
        fs::write(folder.join("c.bin"), b"other").unwrap();
        fs::write(root.path().join("outside.bin"), b"match").unwrap();
        fs::hard_link(folder.join("a.bin"), folder.join("alias.bin")).unwrap();
        symlink(".", folder.join("cycle")).unwrap();
        symlink("..", folder.join("outside")).unwrap();
        let staging = folder.join(".pawcommander-transfer-7-81-0");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("preserved.bin"), b"match").unwrap();
        assert!(find(&Location::Local(staging), &mut Progress::default()).is_err());

        let location = Location::Local(folder);
        let result = find(&location, &mut Progress::default()).unwrap();
        assert_eq!(result.groups.len(), 1);
        let members = &result.groups[0].members;
        let paths = members
            .iter()
            .map(|member| member.path.as_path())
            .collect::<Vec<_>>();
        assert_eq!(paths, [Path::new("a.bin"), Path::new("b.bin")]);

        // Simulate equal hash buckets directly. The byte check must keep the
        // different content out even when its size and candidate digest agree.
        let scope = ReadScope::open(&location).unwrap();
        let cancel = Cancellation::default();
        let mut contenders = members.clone();
        let different = Path::new("c.bin");
        contenders.insert(
            1,
            Member {
                path: different.into(),
                version: scope
                    .open_file(different, &cancel)
                    .unwrap()
                    .unwrap()
                    .stamp()
                    .clone(),
            },
        );
        let mut synthetic = SearchResult {
            root: scope.parent.clone(),
            groups: Vec::new(),
            skipped: 0,
            examples: Vec::new(),
        };
        group_identical(&scope, contenders, 5, &mut synthetic, &cancel).unwrap();
        assert_eq!(synthetic.groups.len(), 1);
        assert_eq!(synthetic.groups[0].members.len(), 2);
        assert_eq!(synthetic.groups[0].members[1].path, Path::new("b.bin"));
    }
}
