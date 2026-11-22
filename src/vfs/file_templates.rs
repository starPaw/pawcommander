//! Snapshot templates and publish files through guarded, exclusive staging.
use super::{
    guarded_directory::{open_staging, Directory},
    location::validate_name,
    read_scope::ReadScope,
    staged_file::StagedFile,
    transfer::with_staging_typed,
    Location,
};
use crate::{
    file_error::{
        AccessError, Failure, FileTemplateError, MutationError, PathError, RecoveryNotice,
        ScanError,
    },
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
const MAX_TITLE_CHARS: usize = 80;

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

    fn bytes(self, value: &str) -> Result<Vec<u8>, TransferError> {
        if value.len() > MAX_BYTES * 4 {
            return Err(FileTemplateError::TooLarge(None).into());
        }
        let (codec, bom): (Codec, &[u8]) = match self {
            Self::Utf8 => (Codec::Utf8, b""),
            Self::Utf8Bom => (Codec::Utf8, b"\xef\xbb\xbf"),
            Self::Utf16Le => (Codec::Utf16Le, b"\xff\xfe"),
            Self::Utf16Be => (Codec::Utf16Be, b"\xfe\xff"),
            Self::Windows1251 => (Codec::Windows1251, b""),
            Self::Windows1252 => (Codec::Windows1252, b""),
        };
        let bytes = codec
            .encode(value)
            .ok_or(FileTemplateError::UnrepresentableCharacters(self))?;
        let total = bytes.len() + bom.len();
        if total > MAX_BYTES {
            return Err(FileTemplateError::TooLarge(None).into());
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
    Stored {
        title: String,
        payload: Arc<str>,
    },
}

impl Source {
    fn bytes(self, progress: &mut Progress) -> Result<Vec<u8>, TransferError> {
        progress.check_cancelled()?;
        match self {
            Self::Text { value, encoding } => encoding.bytes(&value),
            Self::File(source) => read_source(&source, progress),
            Self::Stored { title, payload } => {
                if payload.len() > MAX_ENCODED_BYTES {
                    return Err(FileTemplateError::SavedPayloadTooLarge(title).into());
                }
                let bytes = match STANDARD.decode(payload.as_bytes()) {
                    Ok(bytes) => bytes,
                    Err(cause) => {
                        return Err(FileTemplateError::InvalidSavedPayload { title, cause }.into())
                    }
                };
                if bytes.len() > MAX_BYTES {
                    return Err(FileTemplateError::SavedPayloadTooLarge(title).into());
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
    pub(crate) fn check_new(&self, title: &str) -> Result<(), TransferError> {
        let title = checked_title(title)?;
        let key = title.to_lowercase();
        if let Some(existing) = self
            .templates
            .iter()
            .find(|template| template.title.to_lowercase() == key)
        {
            return Err(FileTemplateError::DuplicateTitle(existing.title.clone()).into());
        }
        if self.templates.len() >= MAX_TEMPLATES {
            return Err(FileTemplateError::LibraryCountLimit(MAX_TEMPLATES).into());
        }
        Ok(())
    }

    pub(crate) fn insert(&mut self, template: SavedTemplate) -> Result<usize, TransferError> {
        self.check_new(&template.title)?;
        let total = self
            .templates
            .iter()
            .fold(template.payload.len(), |size, item| {
                size.saturating_add(item.payload.len())
            });
        if total > MAX_LIBRARY_BYTES {
            return Err(FileTemplateError::LibrarySizeLimit(MAX_LIBRARY_BYTES).into());
        }
        let index = self.templates.len();
        self.templates.push(template);
        Ok(index)
    }
}

impl SavedTemplate {
    pub(crate) fn source(&self) -> Source {
        Source::Stored {
            title: self.title.clone(),
            payload: self.payload.clone(),
        }
    }
}

fn checked_title(title: &str) -> Result<&str, TransferError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(FileTemplateError::EmptyTitle.into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(FileTemplateError::TitleTooLong(MAX_TITLE_CHARS).into());
    }
    if title.chars().any(char::is_control) {
        return Err(FileTemplateError::TitleHasControl.into());
    }
    Ok(title)
}

pub(crate) fn save_template(
    title: String,
    filename: String,
    source: Source,
    progress: &mut Progress,
) -> Result<SavedTemplate, TransferError> {
    progress.check_cancelled()?;
    let title = checked_title(&title)?.to_owned();
    let capture = (|| {
        validate_filename(&filename)?;
        let bytes = source.bytes(progress)?;
        let payload = STANDARD.encode(bytes).into();
        progress.check_cancelled()?;
        Ok(payload)
    })();
    Ok(SavedTemplate {
        payload: capture.map_err(|cause| FileTemplateError::saving(&title, cause))?,
        title,
        filename,
    })
}

pub(crate) struct NewFile {
    pub(crate) directory: Location,
    pub(crate) name: String,
    pub(crate) source: Source,
}

#[derive(Debug)]
pub(crate) struct CreatedFile {
    pub(crate) target: Location,
    pub(crate) cleanup_warning: Option<TransferError>,
}

impl NewFile {
    pub(crate) fn target(&self) -> Location {
        self.directory.join(&self.name)
    }

    pub(crate) fn create(self, progress: &mut Progress) -> Result<CreatedFile, TransferError> {
        validate_filename(&self.name)?;
        if self.directory.is_archive() {
            return Err(MutationError::ReadOnlyArchive(self.directory).into());
        }
        let target = self.target();
        progress.results.plan_selected_items([target.clone()]);
        progress.results.begin(&target);
        let mut published = false;
        let result = self.publish(progress, &mut published);
        let outcome = match &result {
            Ok(()) => Outcome::Completed,
            Err(_) if published => Outcome::CompletedWithWarning,
            Err(TransferError::Cancelled) => {
                progress.results.cancelled = true;
                Outcome::Cancelled
            }
            Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
            Err(TransferError::Failed(_)) => Outcome::Failed,
        };
        progress
            .results
            .record_with_error(&target, outcome, result.as_ref().err());
        let cleanup_warning = match result {
            Ok(()) => None,
            Err(error) if published => Some(error),
            Err(error) => return Err(error),
        };
        Ok(CreatedFile {
            target,
            cleanup_warning,
        })
    }

    fn publish(self, progress: &mut Progress, published: &mut bool) -> Result<(), TransferError> {
        progress.check_cancelled()?;
        let scope = ReadScope::open(&self.directory)?;
        let destination = Directory::from_scope(&scope)?;
        let name = OsStr::new(&self.name);
        let target = destination.location.join(name);
        destination.require_absent(name)?;
        let cancel = progress.cancellation();
        let bytes = self.source.bytes(progress)?;
        progress.item(target.path());
        with_staging_typed(&target, Some(&cancel), |stage, preserve| {
            let stage = Directory::from_scope(&open_staging(stage)?)?;
            let payload = stage.location.join("payload");
            let mut output = StagedFile::open_for(&payload, &target, cancel.clone())?;
            for chunk in bytes.chunks(64 * 1024) {
                output
                    .write_all(chunk)
                    .map_err(|error| AccessError::write(&target, error))?;
            }
            output.close()?;
            destination.require_absent(name)?;
            cancel.check()?;
            match stage.rename(OsStr::new("payload"), &destination, name) {
                Ok(()) => {
                    *published = true;
                    Ok(())
                }
                Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                    *preserve = true;
                    Err(error
                        .with_retained_staging(&stage.location, RecoveryNotice::InspectStaging))
                }
                Err(error) => Err(error),
            }
        })
    }
}

pub(crate) fn validate_filename(name: &str) -> Result<(), PathError> {
    validate_name(name)
}

fn read_source(source: &Location, progress: &mut Progress) -> Result<Vec<u8>, TransferError> {
    let invalid_source = || ScanError::ExpectedRegularFile(source.clone());
    let parent = source.parent().ok_or_else(invalid_source)?;
    let name = source.path().file_name().ok_or_else(invalid_source)?;
    let too_large = || FileTemplateError::TooLarge(Some(source.clone()));
    progress.item(source.path());
    let cancel = progress.cancellation();
    let scope = ReadScope::open(&parent)?;
    let mut input = scope
        .require_file(Path::new(name), &cancel)
        .map_err(|error| match error {
            TransferError::Failed(Failure::Access(cause)) if cause.cause.is_absent() => {
                FileTemplateError::SourceMissing {
                    source: source.clone(),
                    cause,
                }
                .into()
            }
            error => error,
        })?;
    let mut bytes = Vec::new();
    {
        let (_, length, reader) = input.stream();
        if length > MAX_BYTES as u64 {
            return Err(too_large().into());
        }
        let mut reader = reader.take(MAX_BYTES as u64 + 1);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            cancel.check()?;
            let count = reader
                .read(&mut buffer)
                .map_err(|error| AccessError::read(source, error))?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.len() > MAX_BYTES {
                return Err(too_large().into());
            }
        }
    }
    input.verify(&cancel)?;
    scope.verify_selected_root(&parent)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::check_sftp;
