//! Snapshot templates and publish files through guarded, exclusive staging.
use super::{
    guarded_directory::{open_staging, Directory},
    location::validate_name,
    read_scope::ReadScope,
    staged_file::StagedFile,
    transfer::{entry_metadata, with_staging_typed},
    Location,
};
use crate::{
    i18n::text,
    operations::{Progress, TransferError},
    preview::encoding::Codec,
    result_history::Outcome,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_BYTES.div_ceil(3) * 4;
const MAX_LIBRARY_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEMPLATES: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TextEncoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1251,
    Windows1252,
}

impl TextEncoding {
    pub(crate) const ALL: [Self; 6] = [
        Self::Utf8,
        Self::Utf8Bom,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1251,
        Self::Windows1252,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => text!("UTF-8 с BOM"),
            Self::Utf16Le => text!("UTF-16 LE с BOM"),
            Self::Utf16Be => text!("UTF-16 BE с BOM"),
            Self::Windows1251 => "Windows-1251",
            Self::Windows1252 => "Windows-1252",
        }
    }

    fn bytes(self, value: &str) -> Result<Vec<u8>, TransferError> {
        if value.len() > MAX_BYTES * 4 {
            return Err(too_large());
        }
        let (codec, bom): (Codec, &[u8]) = match self {
            Self::Utf8 => (Codec::Utf8, b""),
            Self::Utf8Bom => (Codec::Utf8, b"\xef\xbb\xbf"),
            Self::Utf16Le => (Codec::Utf16Le, b"\xff\xfe"),
            Self::Utf16Be => (Codec::Utf16Be, b"\xfe\xff"),
            Self::Windows1251 => (Codec::Windows1251, b""),
            Self::Windows1252 => (Codec::Windows1252, b""),
        };
        let bytes = codec.encode(value).ok_or_else(|| {
            let encoding = self.label();
            crate::tr_format!(
                "Некоторые символы отсутствуют в {encoding}. Выберите другую кодировку.",
                "Some characters cannot be represented in {encoding}. Choose a different encoding."
            )
        })?;
        let total = bytes.len() + bom.len();
        if total > MAX_BYTES {
            return Err(too_large());
        }
        let mut result = Vec::with_capacity(total);
        result.extend_from_slice(bom);
        result.extend_from_slice(&bytes);
        Ok(result)
    }
}

pub(crate) enum Source {
    Text {
        value: String,
        encoding: TextEncoding,
    },
    File(Location),
    Stored(Arc<str>),
}

impl Source {
    fn bytes(self, progress: &mut Progress) -> Result<Vec<u8>, TransferError> {
        progress.check_cancelled()?;
        match self {
            Self::Text { value, encoding } => encoding.bytes(&value),
            Self::File(source) => read_source(&source, progress),
            Self::Stored(encoded) => {
                if encoded.len() > MAX_ENCODED_BYTES {
                    return Err(too_large());
                }
                let bytes = STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|_| text!("Не удалось прочитать сохранённый шаблон"))?;
                if bytes.len() > MAX_BYTES {
                    return Err(too_large());
                }
                Ok(bytes)
            }
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Library {
    pub(crate) templates: Vec<SavedTemplate>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedTemplate {
    pub(crate) title: String,
    pub(crate) filename: String,
    payload: Arc<str>,
}

impl Library {
    pub(crate) fn check_title(&self, title: &str) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            return Err(text!("Введите название шаблона до 80 символов").into());
        }
        let key = title.to_lowercase();
        if self
            .templates
            .iter()
            .any(|template| template.title.to_lowercase() == key)
        {
            return Err(text!("Шаблон с таким названием уже есть").into());
        }
        Ok(())
    }

    pub(crate) fn insert(&mut self, template: SavedTemplate) -> Result<usize, String> {
        self.check_title(&template.title)?;
        let size = self
            .templates
            .iter()
            .map(|item| item.payload.len())
            .sum::<usize>();
        let index = self.templates.len();
        if index >= MAX_TEMPLATES || size.saturating_add(template.payload.len()) > MAX_LIBRARY_BYTES
        {
            return Err(text!("Достигнут предел сохранённых шаблонов").into());
        }
        self.templates.push(template);
        Ok(index)
    }
}

impl SavedTemplate {
    pub(crate) fn source(&self) -> Source {
        Source::Stored(self.payload.clone())
    }
}

pub(crate) fn save_template(
    title: String,
    filename: String,
    source: Source,
    progress: &mut Progress,
) -> Result<SavedTemplate, TransferError> {
    validate_filename(&filename)?;
    let bytes = source.bytes(progress)?;
    progress.check_cancelled()?;
    Ok(SavedTemplate {
        title,
        filename,
        payload: STANDARD.encode(bytes).into(),
    })
}

pub(crate) struct NewFile {
    pub(crate) directory: Location,
    pub(crate) name: String,
    pub(crate) source: Source,
}

impl NewFile {
    pub(crate) fn target(&self) -> Location {
        self.directory.join(&self.name)
    }

    pub(crate) fn create(self, progress: &mut Progress) -> Result<String, String> {
        validate_filename(&self.name)?;
        if self.directory.is_archive() {
            return Err(crate::archive::readonly().into());
        }
        let target = self.target();
        progress.results.plan_selected_items([target.clone()]);
        progress.results.begin(&target);
        let mut published = false;
        match self.publish(progress, &mut published) {
            Ok(()) => {
                progress
                    .results
                    .record(&target, None, Outcome::Completed, None);
                let path = target.display().escape_debug().to_string();
                Ok(crate::tr_format!(
                    "Файл создан: {path}",
                    "File created: {path}"
                ))
            }
            Err(TransferError::Cancelled) => {
                progress.results.cancelled = true;
                Err(TransferError::Cancelled.to_string())
            }
            Err(error) => {
                let outcome = if published {
                    Outcome::CompletedWithWarning
                } else if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                    Outcome::PublicationUnconfirmed
                } else {
                    Outcome::Failed
                };
                let message = error.to_string();
                progress
                    .results
                    .record(&target, None, outcome, Some(message.clone()));
                Err(message)
            }
        }
    }

    fn publish(self, progress: &mut Progress, published: &mut bool) -> Result<(), TransferError> {
        let scope = ReadScope::open(&self.directory)?;
        let destination = Directory::from_scope(&scope)?;
        let name = OsStr::new(&self.name);
        let target = destination.location.join(name);
        require_absent(&destination, &target)?;
        let cancel = progress.cancellation();
        let bytes = self.source.bytes(progress)?;
        progress.item(target.path());
        with_staging_typed(&target, Some(&cancel), |stage, _| {
            let stage = Directory::from_scope(&open_staging(stage)?)?;
            let payload = stage.location.join("payload");
            let mut output = StagedFile::open(&payload, cancel.clone())?;
            for chunk in bytes.chunks(64 * 1024) {
                output.write_all(chunk)?;
            }
            output.close()?;
            require_absent(&destination, &target)?;
            cancel.check()?;
            match stage.rename(OsStr::new("payload"), &destination, name) {
                Err(TransferError::Failed(_))
                    if entry_metadata(&target).is_ok_and(|item| item.is_some()) =>
                {
                    return Err(conflict());
                }
                result => result?,
            }
            *published = true;
            Ok(())
        })
    }
}

pub(crate) fn validate_filename(name: &str) -> Result<(), String> {
    validate_name(name).map_err(|_| text!("Введите имя файла без разделителей пути").into())
}

fn require_absent(destination: &Directory, target: &Location) -> Result<(), TransferError> {
    destination.verify()?;
    if entry_metadata(target)?.is_some() {
        return Err(conflict());
    }
    Ok(())
}

fn conflict() -> TransferError {
    text!("В этой папке уже есть объект с таким именем. Введите другое имя.").into()
}

fn read_source(source: &Location, progress: &mut Progress) -> Result<Vec<u8>, TransferError> {
    let parent = source
        .parent()
        .ok_or_else(|| text!("Выберите один обычный файл для шаблона"))?;
    let name = source
        .path()
        .file_name()
        .ok_or_else(|| text!("Выберите один обычный файл для шаблона"))?;
    progress.item(source.path());
    let cancel = progress.cancellation();
    let scope = ReadScope::open(&parent)?;
    let mut input = scope
        .open_file(Path::new(name), &cancel)?
        .ok_or_else(|| text!("Исходный файл шаблона отсутствует"))?;
    let mut bytes = Vec::new();
    {
        let (_, length, reader) = input.stream();
        if length > MAX_BYTES as u64 {
            return Err(too_large());
        }
        let mut reader = reader.take(MAX_BYTES as u64 + 1);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            cancel.check()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.len() > MAX_BYTES {
                return Err(too_large());
            }
        }
    }
    input.verify(&cancel)?;
    scope.parent.verify()?;
    Ok(bytes)
}

fn too_large() -> TransferError {
    text!("Шаблон файла превышает 4 МиБ").into()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::check_sftp;
