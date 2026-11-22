//! App-owned collections and a root-relative exchange format for macOS.
mod reference;
#[cfg(test)]
mod tests;

pub(crate) use crate::file_error::CollectionUnavailable as Unavailable;

use super::{
    guarded_directory::{open_staging, Directory},
    location::validate_name,
    read_scope::ReadScope,
    smb_output,
    staged_file::StagedFile,
    transfer::with_staging_typed,
    Location,
};
use crate::{
    app::SavedPath,
    file_error::{AccessError, CollectionAction, CollectionError, SmbMutationAction},
    operations::{Progress, TransferError},
};
use reference::Reference;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

const MAX_GROUPS: usize = 128;
const MAX_MEMBERS: usize = 2048;
const MAX_NAME: usize = 80;
const MAX_TAGS: usize = 16;
const MAX_TAG: usize = 48;
const MAX_PORTABLE_BYTES: usize = 4 * 1024 * 1024;
const PORTABLE_VERSION: u32 = 1;
pub(crate) const PORTABLE_SUFFIX: &str = ".pawcollection.json";

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub(crate) collections: Vec<Collection>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Collection {
    pub(crate) name: String,
    pub(crate) members: Vec<Member>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Member {
    reference: Reference,
    pub(crate) tags: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Portable {
    version: u32,
    name: String,
    members: Vec<PortableMember>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableMember {
    path: SavedPath,
    folder: bool,
    tags: Vec<String>,
}

impl Catalog {
    pub(crate) fn create(&mut self, name: &str) -> Result<usize, CollectionError> {
        let name = checked_name(name)?;
        self.require_distinct(name, None)?;
        self.insert(Collection {
            name: name.into(),
            members: Vec::new(),
        })
    }

    pub(crate) fn rename(&mut self, index: usize, name: &str) -> Result<(), CollectionError> {
        let name = checked_name(name)?;
        self.require_distinct(name, Some(index))?;
        self.collections[index].name = name.into();
        Ok(())
    }

    pub(crate) fn insert_imported(
        &mut self,
        mut collection: Collection,
    ) -> Result<usize, CollectionError> {
        let name = collection.name.clone();
        let mut suffix = 2;
        while self.require_distinct(&collection.name, None).is_err() {
            let ending = format!(" ({suffix})");
            let prefix: String = name
                .chars()
                .take(MAX_NAME.saturating_sub(ending.len()))
                .collect();
            collection.name = prefix + &ending;
            suffix += 1;
        }
        self.insert(collection)
    }

    pub(crate) fn capacity_for(&self, index: usize) -> usize {
        let other = self
            .collections
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, group)| group.members.len())
            .sum();
        MAX_MEMBERS.saturating_sub(other)
    }

    fn require_distinct(&self, name: &str, except: Option<usize>) -> Result<(), CollectionError> {
        let name = name.to_lowercase();
        if self
            .collections
            .iter()
            .enumerate()
            .any(|(i, group)| Some(i) != except && group.name.to_lowercase() == name)
        {
            return Err(CollectionError::NameInUse);
        }
        Ok(())
    }

    fn insert(&mut self, collection: Collection) -> Result<usize, CollectionError> {
        let index = self.collections.len();
        if index >= MAX_GROUPS {
            return Err(CollectionError::CollectionLimit(MAX_GROUPS));
        }
        if collection.members.len() > self.capacity_for(index) {
            return Err(CollectionError::CatalogItemLimit(MAX_MEMBERS));
        }
        self.collections.push(collection);
        Ok(index)
    }
}

impl Collection {
    pub(crate) fn add(
        &mut self,
        paths: &[Location],
        tags: &[String],
        capacity: usize,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        if paths.len() > MAX_MEMBERS {
            return Err(CollectionError::CatalogItemLimit(MAX_MEMBERS).into());
        }
        let mut members = self.members.clone();
        for path in paths {
            progress.check_cancelled()?;
            progress.item(path.path());
            let incoming = Member::capture(local(path)?, tags.to_vec())?;
            if let Some(existing) = members
                .iter_mut()
                .find(|item| item.reference.same_item(&incoming.reference))
            {
                existing.tags =
                    normalize_tags(existing.tags.iter().chain(tags).map(String::as_str))?;
                existing.reference = incoming.reference;
            } else if members.len() < capacity {
                members.push(incoming);
            } else {
                return Err(CollectionError::CatalogItemLimit(MAX_MEMBERS).into());
            }
        }
        progress.check_cancelled()?;
        self.members = members;
        Ok(())
    }

    pub(crate) fn relink(&mut self, index: usize, path: &Location) -> Result<(), TransferError> {
        let reference = Reference::capture(local(path)?)?;
        if reference.is_folder() != self.members[index].is_folder() {
            return Err(CollectionError::RelinkKind.into());
        }
        if self
            .members
            .iter()
            .enumerate()
            .any(|(i, member)| i != index && member.reference.same_item(&reference))
        {
            return Err(CollectionError::AlreadyMember.into());
        }
        self.members[index].reference = reference;
        Ok(())
    }

    pub(crate) fn refresh(
        &mut self,
        progress: &mut Progress,
    ) -> Result<Vec<Option<Unavailable>>, TransferError> {
        let mut states = Vec::with_capacity(self.members.len());
        for member in &mut self.members {
            progress.check_cancelled()?;
            if let Some(path) = member.last_path() {
                progress.item(&path);
            }
            states.push(member.resolve().err());
        }
        Ok(states)
    }
}

impl Member {
    fn capture(path: &Path, tags: Vec<String>) -> Result<Self, TransferError> {
        Ok(Self {
            reference: Reference::capture(path)?,
            tags,
        })
    }

    pub(crate) fn last_path(&self) -> Option<PathBuf> {
        self.reference.path.clone().into_path()
    }

    pub(crate) fn is_folder(&self) -> bool {
        self.reference.is_folder()
    }

    pub(crate) fn resolve(&mut self) -> Result<PathBuf, Unavailable> {
        self.reference.resolve()
    }
}

pub(crate) fn parse_tags(input: &str) -> Result<Vec<String>, CollectionError> {
    normalize_tags(input.split(','))
}

pub(crate) fn is_portable(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(PORTABLE_SUFFIX))
}

pub(crate) fn export(
    directory: &Location,
    file_name: &str,
    group: &mut Collection,
    progress: &mut Progress,
) -> Result<Location, TransferError> {
    let name = file_name.trim();
    let requested = directory.join(name);
    export_to(directory, name, group, progress)
        .map_err(|cause| CollectionError::document(CollectionAction::Export, &requested, cause))
}

fn export_to(
    directory: &Location,
    name: &str,
    group: &mut Collection,
    progress: &mut Progress,
) -> Result<Location, TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    local(directory)?;
    validate_name(name)?;
    if !name.ends_with(PORTABLE_SUFFIX) || name == PORTABLE_SUFFIX {
        return Err(CollectionError::ExportSuffix.into());
    }
    let mut mount = smb_output::guard(directory, SmbMutationAction::ExportCollection)?;
    let scope = ReadScope::open_with_smb(directory, &mut mount)?;
    let destination = Directory::from_scope(&scope)?;
    let target = destination.location.join(name);
    let smb_destination = smb_output::is_destination(&mut mount, &target)?;
    let name = OsStr::new(name);
    destination.require_absent(name)?;
    let mut members = Vec::with_capacity(group.members.len());
    for (index, item) in group.members.iter_mut().enumerate() {
        cancel.check()?;
        let row = index + 1;
        scope
            .verify_mount()
            .map_err(|cause| entry_error(row, item.last_path().as_deref(), cause))?;
        let path = item.resolve().map_err(|state| {
            entry_error(
                row,
                item.last_path().as_deref(),
                CollectionError::Unavailable(state),
            )
        })?;
        progress.item(&path);
        let folder = destination.location.path();
        let relative =
            relative_to(&path, folder).map_err(|cause| entry_error(row, Some(&path), cause))?;
        item.reference
            .verify_portable_target(&scope, relative, &cancel)
            .map_err(|cause| entry_error(row, Some(&path), cause))?;
        members.push(PortableMember {
            path: relative.into(),
            folder: item.is_folder(),
            tags: item.tags.clone(),
        });
    }
    let file = Portable {
        version: PORTABLE_VERSION,
        name: group.name.clone(),
        members,
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(CollectionError::Json)?;
    if bytes.len() > MAX_PORTABLE_BYTES {
        return Err(CollectionError::ByteLimit(MAX_PORTABLE_BYTES).into());
    }
    progress.item(target.path());
    if smb_destination {
        scope.verify_selected_root(directory)?;
        smb_output::publish(
            &mut bytes.as_slice(),
            bytes.len() as u64,
            &target,
            &target,
            &scope.parent,
            &mount,
            progress,
        )?;
        return Ok(target);
    }
    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let staging = Directory::from_scope(&open_staging(staging)?)?;
        let payload = staging.location.join("payload");
        let mut output = StagedFile::open_for(&payload, &target, cancel.clone())?;
        output
            .write_all(&bytes)
            .map_err(|cause| AccessError::write(&target, cause))?;
        output.close()?;
        scope.verify_selected_root(directory)?;
        destination.require_absent(name)?;
        cancel.check()?;
        staging.rename(OsStr::new("payload"), &destination, name)
    })?;
    Ok(target)
}

pub(crate) fn import(
    directory: &Location,
    source: &Location,
    progress: &mut Progress,
) -> Result<Collection, TransferError> {
    import_from(directory, source, progress)
        .map_err(|cause| CollectionError::document(CollectionAction::Import, source, cause))
}

fn import_from(
    directory: &Location,
    source: &Location,
    progress: &mut Progress,
) -> Result<Collection, TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    local(directory)?;
    if source.parent().as_ref() != Some(directory) || !is_portable(local(source)?) {
        return Err(CollectionError::ImportSelection.into());
    }
    let mut mount = smb_output::guard(directory, SmbMutationAction::ImportCollection)?;
    let scope = ReadScope::open_with_smb(directory, &mut mount)?;
    let name = source
        .path()
        .file_name()
        .ok_or(CollectionError::ImportSelection)?;
    let mut file = scope.require_file(Path::new(name), &cancel)?;
    let mut bytes = Vec::new();
    {
        let (location, length, input) = file.stream();
        if length > MAX_PORTABLE_BYTES as u64 {
            return Err(CollectionError::ByteLimit(MAX_PORTABLE_BYTES).into());
        }
        input
            .take(MAX_PORTABLE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|cause| AccessError::read(location, cause))?;
    }
    if bytes.len() > MAX_PORTABLE_BYTES {
        return Err(CollectionError::ByteLimit(MAX_PORTABLE_BYTES).into());
    }
    file.verify(&cancel)?;
    let document: Portable = serde_json::from_slice(&bytes).map_err(CollectionError::Json)?;
    if document.version != PORTABLE_VERSION {
        return Err(CollectionError::Version(document.version).into());
    }
    if document.members.len() > MAX_MEMBERS {
        return Err(CollectionError::EntryLimit(MAX_MEMBERS).into());
    }
    let root = scope.parent.location.path();
    let mut members: Vec<Member> = Vec::with_capacity(document.members.len());
    for (index, item) in document.members.into_iter().enumerate() {
        cancel.check()?;
        let row = index + 1;
        let relative = item
            .path
            .into_path()
            .ok_or_else(|| entry_error(row, None, CollectionError::InvalidPath))?;
        if !safe_relative(&relative) {
            return Err(entry_error(
                row,
                Some(&relative),
                CollectionError::InvalidPath,
            ));
        }
        let path = root.join(&relative);
        progress.item(&path);
        scope
            .verify_mount()
            .map_err(|cause| entry_error(row, Some(&path), cause))?;
        let tags = normalize_tags(item.tags.iter().map(String::as_str))
            .map_err(|cause| entry_error(row, Some(&path), cause))?;
        let member =
            Member::capture(&path, tags).map_err(|cause| entry_error(row, Some(&path), cause))?;
        let resolved = member
            .last_path()
            .ok_or_else(|| entry_error(row, Some(&path), CollectionError::InvalidPath))?;
        let inside =
            relative_to(&resolved, root).map_err(|cause| entry_error(row, Some(&path), cause))?;
        if member.is_folder() != item.folder {
            return Err(entry_error(
                row,
                Some(&path),
                CollectionError::WrongKind {
                    folder: item.folder,
                },
            ));
        }
        member
            .reference
            .verify_portable_target(&scope, inside, &cancel)
            .map_err(|cause| entry_error(row, Some(&path), cause))?;
        if members
            .iter()
            .any(|existing| existing.reference.same_item(&member.reference))
        {
            return Err(entry_error(
                row,
                Some(&path),
                CollectionError::DuplicateEntry,
            ));
        }
        members.push(member);
    }
    scope.verify_selected_root(directory)?;
    cancel.check()?;
    Ok(Collection {
        name: checked_name(&document.name)?.into(),
        members,
    })
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.as_os_str().as_bytes().contains(&0)
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn relative_to<'a>(item: &'a Path, folder: &Path) -> Result<&'a Path, CollectionError> {
    item.strip_prefix(folder)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| CollectionError::OutsideFolder {
            resolved: item.to_owned(),
            folder: folder.to_owned(),
        })
}

fn checked_name(name: &str) -> Result<&str, CollectionError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(CollectionError::InvalidName);
    }
    Ok(name)
}

fn normalize_tags<'a>(tags: impl Iterator<Item = &'a str>) -> Result<Vec<String>, CollectionError> {
    let mut unique = BTreeSet::new();
    let mut result = Vec::new();
    for name in tags.map(str::trim).filter(|name| !name.is_empty()) {
        if name.chars().count() > MAX_TAG || name.chars().any(|c| c == ',' || c.is_control()) {
            return Err(CollectionError::InvalidTag);
        }
        if unique.insert(name.to_lowercase()) {
            if result.len() == MAX_TAGS {
                return Err(CollectionError::TagLimit);
            }
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

fn local(location: &Location) -> Result<&Path, TransferError> {
    location
        .local()
        .ok_or_else(|| CollectionError::LocalOnly(location.clone()).into())
}

fn entry_error(row: usize, path: Option<&Path>, cause: impl Into<TransferError>) -> TransferError {
    let cause = cause.into();
    if matches!(cause, TransferError::Cancelled) {
        return cause;
    }
    CollectionError::Entry {
        row,
        path: path.map(Path::to_owned),
        cause: Box::new(cause),
    }
    .into()
}
