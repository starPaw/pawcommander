use super::{member_path, Format, MAX_ENTRIES, MAX_INDEX_BYTES, MAX_METADATA_BYTES};
use flate2::{write::DeflateEncoder, Compression};
use rawzip::time::{DosDateTime, UtcDateTime};
use std::{
    io::{self, Read, Write},
    path::Path,
};

pub(crate) struct Attributes {
    pub size: u64,
    pub mode: u32,
    pub modified: Option<u64>,
    pub directory: bool,
}

#[derive(Default)]
pub(crate) struct CreationLimits {
    entries: usize,
    path_bytes: usize,
    metadata_bytes: usize,
}

impl CreationLimits {
    pub(crate) fn add(
        &mut self,
        format: Format,
        name: &str,
        directory: bool,
        protected: bool,
    ) -> io::Result<()> {
        let path = member_path(name.as_bytes())?;
        let length = name.len();
        if path.as_os_str().is_empty()
            || (format == Format::Zip && directory && length == super::MAX_PATH_BYTES)
        {
            return Err(super::invalid_path());
        }
        self.entries += 1;
        self.path_bytes += length;
        // Include the ZIP local headers, directory records and end-of-directory
        // search, or the TAR headers, GNU long names and trailer.
        self.metadata_bytes += match format {
            Format::Zip => {
                160 + 2 * (length + usize::from(directory))
                    + 22 * usize::from(protected && !directory)
            }
            Format::Tar if length >= 100 => 1024 + (length + 1).next_multiple_of(512),
            Format::Tar => 512,
        };
        let trailer = match format {
            Format::Zip => 128 * 1024,
            Format::Tar => 1024,
        };
        if self.entries > MAX_ENTRIES
            || self.path_bytes > MAX_INDEX_BYTES
            || self.metadata_bytes + trailer > MAX_METADATA_BYTES
        {
            return Err(io::Error::other(crate::i18n::text!(
                "Превышен лимит записей или путей в архиве",
            )));
        }
        Ok(())
    }
}

pub(crate) enum ArchiveWriter<'a, W: Write> {
    Zip(rawzip::ZipArchiveWriter<W>),
    StreamZip {
        writer: Box<zip::ZipWriter<zip::write::StreamWriter<W>>>,
        password: Option<&'a str>,
    },
    Tar(tar::Builder<W>),
}

impl<'a, W: Write> ArchiveWriter<'a, W> {
    pub(crate) fn new(format: Format, password: Option<&'a str>, output: W) -> io::Result<Self> {
        Ok(match (format, password) {
            (Format::Zip, Some(password)) => Self::StreamZip {
                writer: Box::new(zip::ZipWriter::new_stream(output)),
                password: Some(password),
            },
            (Format::Zip, None) => Self::Zip(rawzip::ZipArchiveWriter::new(output)),
            (Format::Tar, None) => Self::Tar(tar::Builder::new(output)),
            (Format::Tar, Some(_)) => {
                return Err(super::invalid(crate::i18n::key!(
                    "Пароль можно задать только для ZIP"
                )))
            }
        })
    }

    pub(crate) fn rebuild(
        format: Format,
        password: Option<&'a str>,
        output: W,
    ) -> io::Result<Self> {
        match format {
            Format::Zip => Ok(Self::StreamZip {
                writer: Box::new(zip::ZipWriter::new_stream(output)),
                password,
            }),
            Format::Tar => Self::new(format, password, output),
        }
    }

    pub(crate) fn append(
        &mut self,
        name: &str,
        attributes: &Attributes,
        mut input: impl Read,
    ) -> io::Result<()> {
        match self {
            Self::Zip(zip) => {
                let modified = attributes
                    .modified
                    .and_then(|time| i64::try_from(time).ok())
                    .map(UtcDateTime::from_unix);
                if attributes.directory {
                    let name = format!("{name}/");
                    let mut entry = zip.new_dir(name.as_str()).unix_permissions(attributes.mode);
                    if let Some(modified) = modified {
                        entry = entry.last_modified(modified);
                    }
                    entry.create().map_err(zip_io)?;
                } else {
                    let mut entry = zip
                        .new_file(name)
                        .compression_method(rawzip::CompressionMethod::DEFLATE)
                        .unix_permissions(attributes.mode);
                    if let Some(modified) = modified {
                        entry = entry.last_modified(modified);
                    }
                    let (mut entry, config) = entry.start().map_err(zip_io)?;
                    let encoder = DeflateEncoder::new(&mut entry, Compression::default());
                    let mut writer = config.wrap(encoder);
                    io::copy(&mut input, &mut writer)?;
                    let (encoder, descriptor) = writer.finish().map_err(zip_io)?;
                    encoder.finish()?;
                    entry.finish(descriptor).map_err(zip_io)?;
                }
                Ok(())
            }
            Self::StreamZip { writer, password } => {
                let mut options =
                    zip::write::SimpleFileOptions::default().unix_permissions(attributes.mode);
                if let Some(time) = attributes
                    .modified
                    .and_then(|time| i64::try_from(time).ok())
                {
                    let time = DosDateTime::from(&UtcDateTime::from_unix(time));
                    let time = zip::DateTime::from_date_and_time(
                        time.year(),
                        time.month(),
                        time.day(),
                        time.hour(),
                        time.minute(),
                        time.second(),
                    );
                    if let Ok(time) = time {
                        options = options.last_modified_time(time);
                    }
                }
                if attributes.directory {
                    writer
                        .add_directory(format!("{name}/"), options)
                        .map_err(super::protected_zip::zip_error)?;
                } else {
                    // Reserve ZIP64 before streaming for sizes near 4 GiB, where
                    // Deflate overhead could cross the 32-bit boundary.
                    let large = attributes.size >= zip::ZIP64_BYTES_THR - 16 * 1024 * 1024;
                    let mut options = options
                        .large_file(large)
                        .compression_method(zip::CompressionMethod::Deflated);
                    if let Some(password) = password {
                        options = options.with_aes_encryption(zip::AesMode::Aes256, password);
                    }
                    writer
                        .start_file(name, options)
                        .map_err(super::protected_zip::zip_error)?;
                    io::copy(&mut input, writer)?;
                }
                Ok(())
            }
            Self::Tar(tar) => {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(if attributes.directory {
                    tar::EntryType::Directory
                } else {
                    tar::EntryType::Regular
                });
                header.set_size(attributes.size);
                header.set_mode(attributes.mode);
                header.set_mtime(attributes.modified.unwrap_or(0));
                tar.append_data(&mut header, Path::new(name), input)
            }
        }
    }

    pub(crate) fn finish(self) -> io::Result<W> {
        match self {
            Self::Zip(zip) => zip.finish().map_err(zip_io),
            Self::StreamZip { writer, .. } => writer
                .finish()
                .map(|stream| stream.into_inner())
                .map_err(super::protected_zip::zip_error),
            Self::Tar(tar) => tar.into_inner(),
        }
    }
}

fn zip_io(error: rawzip::Error) -> io::Error {
    match error.into_kind() {
        rawzip::ErrorKind::IO(error) => error,
        kind => io::Error::other(kind.to_string()),
    }
}
