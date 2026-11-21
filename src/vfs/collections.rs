//! App-owned collections and a root-relative exchange format for macOS.
mod reference;
#[cfg(test)]
mod tests;

pub(crate) use reference::Unavailable;

use super::{
    guarded_directory::{open_staging, Directory},
    location::validate_name,
    read_scope::ReadScope,
    staged_file::StagedFile,
    transfer::with_staging_typed,
    Location,
};
use crate::{
    app::SavedPath,
    i18n::text,
    operations::{Progress, TransferError},
};
use reference::Reference;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::{Read, Write},
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
    pub(crate) fn create(&mut self, name: &str) -> Result<usize, String> {
        let name = checked_name(name)?;
        self.require_distinct(name, None)?;
        self.insert(Collection {
            name: name.into(),
            members: Vec::new(),
        })
    }

    pub(crate) fn rename(&mut self, index: usize, name: &str) -> Result<(), String> {
        let name = checked_name(name)?;
        self.require_distinct(name, Some(index))?;
        self.collections[index].name = name.into();
        Ok(())
    }

    pub(crate) fn insert_imported(&mut self, mut collection: Collection) -> Result<usize, String> {
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

    fn require_distinct(&self, name: &str, except: Option<usize>) -> Result<(), String> {
        let name = name.to_lowercase();
        if self
            .collections
            .iter()
            .enumerate()
            .any(|(i, group)| Some(i) != except && group.name.to_lowercase() == name)
        {
            return Err(text!("Коллекция с таким названием уже есть").into());
        }
        Ok(())
    }

    fn insert(&mut self, collection: Collection) -> Result<usize, String> {
        let index = self.collections.len();
        if index >= MAX_GROUPS || collection.members.len() > self.capacity_for(index) {
            return Err(text!("Достигнут предел коллекций или объектов").into());
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
            return Err(text!("Достигнут предел коллекций или объектов").into());
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
                return Err(text!("Достигнут предел коллекций или объектов").into());
            }
        }
        progress.check_cancelled()?;
        self.members = members;
        Ok(())
    }

    pub(crate) fn relink(&mut self, index: usize, path: &Location) -> Result<(), TransferError> {
        let reference = capture(local(path)?)?;
        if reference.is_folder() != self.members[index].is_folder() {
            return Err(text!("Выберите объект того же типа").into());
        }
        if self
            .members
            .iter()
            .enumerate()
            .any(|(i, member)| i != index && member.reference.same_item(&reference))
        {
            return Err(text!("Этот объект уже есть в коллекции").into());
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
            reference: capture(path)?,
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

pub(crate) fn parse_tags(input: &str) -> Result<Vec<String>, String> {
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
    local(directory)?;
    let cancel = progress.cancellation();
    let name = file_name.trim();
    validate_name(name)?;
    if !name.ends_with(PORTABLE_SUFFIX) || name == PORTABLE_SUFFIX {
        return Err(text!("Имя файла должно оканчиваться на .pawcollection.json").into());
    }
    let scope = ReadScope::open(directory)?;
    let destination = Directory::from_scope(&scope)?;
    let target = destination.location.join(name);
    let name = OsStr::new(name);
    destination.verify_absent(name)?;
    let mut members = Vec::with_capacity(group.members.len());
    for item in &mut group.members {
        cancel.check()?;
        let path = item
            .resolve()
            .map_err(|state| member_error(item, state.message()))?;
        progress.item(&path);
        let relative = path
            .strip_prefix(destination.location.path())
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| member_error(item, text!("Объект вне папки экспорта")))?;
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
    let bytes = serde_json::to_vec_pretty(&file).map_err(io_error)?;
    if bytes.len() > MAX_PORTABLE_BYTES {
        return Err(portable_limit());
    }
    progress.item(target.path());
    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let staging = Directory::from_scope(&open_staging(staging)?)?;
        let mut output = StagedFile::open(&staging.location.join("payload"), cancel.clone())?;
        output.write_all(&bytes)?;
        output.close()?;
        destination.verify_absent(name)?;
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
    local(directory)?;
    if source.parent().as_ref() != Some(directory) || !is_portable(local(source)?) {
        return Err(text!("Выберите файл .pawcollection.json в активной папке").into());
    }
    let cancel = progress.cancellation();
    let scope = ReadScope::open(directory)?;
    let name = source
        .path()
        .file_name()
        .ok_or_else(|| text!("Выберите файл .pawcollection.json в активной папке"))?;
    let mut file = scope
        .open_file(Path::new(name), &cancel)?
        .ok_or_else(|| text!("Файл коллекции отсутствует"))?;
    let mut bytes = Vec::new();
    {
        let (_, length, input) = file.stream();
        if length > MAX_PORTABLE_BYTES as u64 {
            return Err(portable_limit());
        }
        input
            .take(MAX_PORTABLE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() > MAX_PORTABLE_BYTES {
        return Err(portable_limit());
    }
    file.verify(&cancel)?;
    let document: Portable = serde_json::from_slice(&bytes).map_err(io_error)?;
    if document.version != PORTABLE_VERSION {
        return Err(text!("Версия переносимой коллекции не поддерживается").into());
    }
    if document.members.len() > MAX_MEMBERS {
        return Err(portable_limit());
    }
    let root = scope.parent.location.path();
    let mut members: Vec<Member> = Vec::new();
    for item in document.members {
        cancel.check()?;
        let relative = item
            .path
            .into_path()
            .filter(|path| safe_relative(path))
            .ok_or_else(|| text!("Недопустимый путь в файле коллекции"))?;
        let path = root.join(&relative);
        progress.item(&path);
        let tags = normalize_tags(item.tags.iter().map(String::as_str))?;
        let member = Member::capture(&path, tags)?;
        if !member
            .last_path()
            .is_some_and(|path| path != root && path.starts_with(root))
            || member.is_folder() != item.folder
        {
            return Err(member_error(
                &member,
                text!("Объект вне папки импорта или другого типа"),
            ));
        }
        if members
            .iter()
            .any(|existing| existing.reference.same_item(&member.reference))
        {
            return Err(member_error(
                &member,
                text!("В файле коллекции повторяется объект"),
            ));
        }
        members.push(member);
    }
    scope.parent.verify()?;
    cancel.check()?;
    Ok(Collection {
        name: checked_name(&document.name)?.into(),
        members,
    })
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn checked_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(text!("Введите название коллекции до 80 символов").into());
    }
    Ok(name)
}

fn normalize_tags<'a>(tags: impl Iterator<Item = &'a str>) -> Result<Vec<String>, String> {
    let mut unique = BTreeSet::new();
    let mut result = Vec::new();
    for name in tags.map(str::trim).filter(|name| !name.is_empty()) {
        if name.chars().count() > MAX_TAG || name.chars().any(|c| c == ',' || c.is_control()) {
            return Err(text!("Тег должен содержать до 48 символов без запятых").into());
        }
        if unique.insert(name.to_lowercase()) {
            if result.len() == MAX_TAGS {
                return Err(text!("Для объекта доступно до 16 тегов").into());
            }
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

fn local(location: &Location) -> Result<&Path, TransferError> {
    location
        .local()
        .ok_or_else(|| text!("Выберите локальные файлы или папку").into())
}

fn capture(path: &Path) -> Result<Reference, TransferError> {
    Reference::capture(path)
        .map_err(|error| format!("{} — {error}", path.display().to_string().escape_debug()).into())
}

fn member_error(member: &Member, error: &str) -> TransferError {
    let path = member.last_path().unwrap_or_default();
    format!("{} — {error}", path.display().to_string().escape_debug()).into()
}

fn portable_limit() -> TransferError {
    text!("Файл коллекции превышает допустимый размер").into()
}

fn io_error(error: serde_json::Error) -> TransferError {
    crate::tr_format!("Collection format error: {error}", error = error).into()
}
