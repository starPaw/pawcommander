mod mode;

pub(crate) use mode::ModeRule;

use super::location::{normalize_remote, Location};
use super::sftp::{remote, should_reopen};
use crate::i18n::text;
use crate::operations::{Cancellation, Progress};
use crate::result_history::Outcome;
use libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED;
use mode::MODE_BITS;
use ssh2::{Error, ErrorCode, FileStat, Sftp};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) struct PermissionsPreview {
    directory: Location,
    canonical_directory: PathBuf,
    pub(crate) rows: Vec<PermissionRow>,
    pub(crate) changes: usize,
}

pub(crate) struct PermissionRow {
    pub(crate) source: Location,
    pub(crate) change: Result<PermissionChange, String>,
}

pub(crate) struct PermissionChange {
    before: Stamp,
    pub(crate) after: u32,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    perm: u32,
    owner: Option<u32>,
    group: Option<u32>,
    size: Option<u64>,
    modified: Option<u64>,
    directory: bool,
}

enum ChangeError {
    Failed(String),
    Interrupted(String),
    Cancelled,
}

enum ReadError {
    Server(Error),
    Unsupported(&'static str),
}

impl ReadError {
    fn disconnected(&self) -> bool {
        matches!(self, Self::Server(error) if should_reopen(error))
    }

    fn into_message(self) -> String {
        match self {
            Self::Server(error) => server_error(error),
            Self::Unsupported(message) => message.into(),
        }
    }
}

impl PermissionsPreview {
    pub(crate) fn load(
        sources: &[Location],
        rule: &ModeRule,
        cancel: &Cancellation,
    ) -> Result<Self, String> {
        let invalid = || text!("Выберите файлы или папки в одной папке SFTP.").to_owned();
        let directory = sources
            .first()
            .filter(|source| source.remote())
            .and_then(Location::parent)
            .ok_or_else(invalid)?;
        for source in sources {
            if !source.same_backend(&directory)
                || source.path().parent() != Some(directory.path())
                || normalize_remote(source.path())?.as_os_str() != source.path().as_os_str()
            {
                return Err(invalid());
            }
        }
        cancel.check().map_err(super::err)?;
        let channel = remote(&directory)?;
        let canonical_directory = channel.realpath(directory.path()).map_err(server_error)?;
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        let mut changes = 0;
        for source in sources.iter().filter(|&source| seen.insert(source)) {
            cancel.check().map_err(super::err)?;
            let path = canonical_directory.join(source.path().file_name().ok_or_else(invalid)?);
            let change = match Stamp::read(&channel, &path) {
                Ok(before) => {
                    let after = rule.evaluate(before.perm, before.directory);
                    changes += usize::from(after != before.perm & MODE_BITS);
                    Ok(PermissionChange { before, after })
                }
                Err(error) if error.disconnected() => return Err(error.into_message()),
                Err(error) => Err(error.into_message()),
            };
            rows.push(PermissionRow {
                source: source.clone(),
                change,
            });
        }
        Ok(Self {
            directory,
            canonical_directory,
            rows,
            changes,
        })
    }

    pub(crate) fn directory(&self) -> &Location {
        &self.directory
    }

    pub(crate) fn apply(self, progress: &mut Progress) -> Result<String, String> {
        progress
            .results
            .plan_selected_items(self.rows.iter().map(|row| row.source.clone()));
        let cancel = progress.cancellation();
        if cancel.check().is_err() {
            progress.results.cancelled = true;
            return Ok(text!("Операция остановлена пользователем").into());
        }
        let channel = remote(&self.directory)?;
        if channel
            .realpath(self.directory.path())
            .map_err(server_error)?
            != self.canonical_directory
        {
            return Err(text!("Папка SFTP изменилась. Обновите предпросмотр прав.").into());
        }

        let (mut completed, mut warned, mut skipped, mut failed) = (0, 0, 0, 0);
        let mut first_error = None;
        for row in self.rows {
            if cancel.check().is_err() {
                progress.results.cancelled = true;
                break;
            }
            progress.results.begin(&row.source);
            progress.item(row.source.path());
            let mut stop = false;
            let (outcome, reason) = match row.change {
                Err(reason) => (Outcome::Skipped, Some(reason)),
                Ok(change) if change.current() == change.after => (Outcome::Skipped, None),
                Ok(change) => {
                    let name = row.source.path().file_name().expect("validated entry path");
                    let path = self.canonical_directory.join(name);
                    match change.apply(&channel, &path, &cancel) {
                        Ok(None) => (Outcome::Completed, None),
                        Ok(Some(warning)) => (Outcome::CompletedWithWarning, Some(warning)),
                        Err(ChangeError::Failed(reason)) => (Outcome::Failed, Some(reason)),
                        Err(ChangeError::Interrupted(reason)) => {
                            stop = true;
                            (Outcome::Failed, Some(reason))
                        }
                        Err(ChangeError::Cancelled) => {
                            progress.results.cancelled = true;
                            stop = true;
                            (Outcome::Cancelled, None)
                        }
                    }
                }
            };
            match outcome {
                Outcome::Completed => completed += 1,
                Outcome::CompletedWithWarning => warned += 1,
                Outcome::Skipped => skipped += 1,
                Outcome::Failed => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error.clone_from(&reason);
                    }
                }
                _ => {}
            }
            progress.results.record(&row.source, None, outcome, reason);
            if stop {
                break;
            }
        }
        let summary = crate::tr_format!(
            "Changed {}. With warnings {}. Skipped {}. Failed {}.",
            completed,
            warned,
            skipped,
            failed
        );
        if let Some(reason) = first_error {
            Err(format!("{summary} {reason}"))
        } else if progress.results.cancelled {
            Ok(format!(
                "{}. {summary}",
                text!("Операция остановлена пользователем")
            ))
        } else {
            Ok(summary)
        }
    }
}

impl PermissionChange {
    pub(crate) fn current(&self) -> u32 {
        self.before.perm & MODE_BITS
    }

    fn apply(
        &self,
        channel: &Sftp,
        path: &Path,
        cancel: &Cancellation,
    ) -> Result<Option<String>, ChangeError> {
        // SFTP SETSTAT uses a path. Check the previewed attributes and the final
        // entry type immediately before requesting the change.
        let current = Stamp::read(channel, path).map_err(|error| {
            if error.disconnected() {
                ChangeError::Interrupted(error.into_message())
            } else {
                ChangeError::Failed(error.into_message())
            }
        })?;
        if current != self.before {
            return Err(ChangeError::Failed(
                text!("Объект SFTP изменился. Обновите предпросмотр прав.").into(),
            ));
        }
        cancel.check().map_err(|_| ChangeError::Cancelled)?;
        let attributes = FileStat {
            perm: Some(self.after),
            size: None,
            uid: None,
            gid: None,
            atime: None,
            mtime: None,
        };
        channel.setstat(path, attributes).map_err(|error| {
            if should_reopen(&error) {
                ChangeError::Interrupted(unconfirmed_change())
            } else {
                ChangeError::Failed(server_error(error))
            }
        })?;
        Ok(match Stamp::read(channel, path) {
            Ok(actual) if actual.perm & MODE_BITS == self.after => None,
            Ok(actual) => Some(crate::tr_format!(
                "The server set {:04o} instead of {:04o}.",
                actual.perm & MODE_BITS,
                self.after
            )),
            Err(error) if error.disconnected() => {
                return Err(ChangeError::Interrupted(unconfirmed_change()));
            }
            Err(error) => Some(crate::tr_format!(
                "Could not verify the new permissions. {}",
                error.into_message()
            )),
        })
    }
}

impl Stamp {
    fn read(channel: &Sftp, path: &Path) -> Result<Self, ReadError> {
        let stat = channel.lstat(path).map_err(ReadError::Server)?;
        let perm = stat.perm.ok_or_else(|| {
            ReadError::Unsupported(text!("Сервер SFTP не сообщил права доступа."))
        })?;
        if !stat.is_file() && !stat.is_dir() {
            return Err(ReadError::Unsupported(text!(
                "Изменение прав доступно для обычных файлов и папок.",
            )));
        }
        Ok(Self {
            perm,
            owner: stat.uid,
            group: stat.gid,
            size: stat.size,
            modified: stat.mtime,
            directory: stat.is_dir(),
        })
    }
}

fn unconfirmed_change() -> String {
    text!("Связь с SFTP прервалась. Изменение прав не подтверждено.").into()
}

fn server_error(error: Error) -> String {
    if error.code() == ErrorCode::SFTP(LIBSSH2_FX_PERMISSION_DENIED) {
        text!("Сервер SFTP отказал в доступе.").into()
    } else {
        error.to_string()
    }
}
