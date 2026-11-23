use crate::archive::{ArchiveVerification, Format};
use crate::i18n::{file_error, text, MessageKey};
use crate::operations::TransferError;
use crate::vfs::{
    ChecksumVerification, DeletionFailure, ExtractionFailure, ExtractionSummary, Location,
    PermissionsReport, TransferFailure, TransferSummary,
};

/// The worker owns the outcome until it can send it to the UI. History can
/// take a text snapshot without consuming the error or the operation report.
pub(super) enum FileResult {
    Action {
        result: Result<String, TransferError>,
        target: Option<Location>,
        stopped: Option<MessageKey>,
    },
    Trash(Result<usize, DeletionFailure>),
    Extract {
        directory: Location,
        result: Result<ExtractionSummary, ExtractionFailure>,
    },
    VerifyArchive {
        archive: Location,
        result: Result<ArchiveVerification, TransferError>,
    },
    VerifyChecksum {
        manifest: Location,
        result: Result<ChecksumVerification, TransferError>,
    },
    Permissions {
        directory: Location,
        result: Result<PermissionsReport, TransferError>,
    },
    Transfer {
        destination: Location,
        result: Result<TransferSummary, TransferFailure>,
    },
    #[cfg(target_os = "macos")]
    CompatibleNames {
        destination: Location,
        result: Result<TransferSummary, TransferFailure>,
    },
    #[cfg(target_os = "macos")]
    Resume(Result<crate::vfs::resume_copy::ResumeCompletion, TransferError>),
}

impl FileResult {
    pub(super) fn text(result: Result<String, TransferError>) -> Self {
        Self::Action {
            result,
            target: None,
            stopped: None,
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        use TransferError::Cancelled;

        match self {
            Self::Action { result, .. } => matches!(result, Err(Cancelled)),
            Self::Trash(result) => {
                matches!(result, Err(failure) if matches!(failure.error, Cancelled))
            }
            Self::Extract { result, .. } => {
                matches!(result, Err(failure) if matches!(failure.error, Cancelled))
            }
            Self::VerifyArchive { result, .. } => match result {
                Err(Cancelled) => true,
                Ok(report) => matches!(report.interruption, Some(Cancelled)),
                _ => false,
            },
            Self::VerifyChecksum { result, .. } => match result {
                Err(Cancelled) => true,
                Ok(report) => report.cancelled,
                _ => false,
            },
            Self::Permissions { result, .. } => match result {
                Err(Cancelled) => true,
                Ok(report) => report.cancelled,
                _ => false,
            },
            Self::Transfer { result, .. } => transfer_cancelled(result),
            #[cfg(target_os = "macos")]
            Self::CompatibleNames { result, .. } => transfer_cancelled(result),
            #[cfg(target_os = "macos")]
            Self::Resume(result) => matches!(result, Err(Cancelled)),
        }
    }

    pub(super) fn notice(&self) -> Result<String, String> {
        match self {
            Self::Action {
                result,
                target,
                stopped,
            } => match result {
                Ok(message) => Ok(message.clone()),
                Err(TransferError::Cancelled) => {
                    Ok(stopped.map_or_else(cancelled_message, |key| text(key).into()))
                }
                Err(error) => {
                    let message = file_error(error);
                    #[cfg(target_os = "macos")]
                    if target.is_some() && error.is_completed_cleanup_warning() {
                        return Ok(message);
                    }
                    Err(match target {
                        Some(target) => with_location(target, message),
                        None => message,
                    })
                }
            },
            Self::Trash(Ok(count)) => Ok(crate::tr_format!("Items processed: {n}", n = count)),
            Self::Trash(Err(failure)) => {
                let message = crate::i18n::deletion_failure(failure);
                if matches!(failure.error, TransferError::Cancelled) {
                    Ok(message)
                } else {
                    Err(message)
                }
            }
            Self::Extract { directory, result } => {
                extraction_notice(result).map_err(|message| with_location(directory, message))
            }
            Self::VerifyArchive { archive, result } => match result {
                Ok(report) => archive_verification_notice(report)
                    .map_err(|message| with_location(archive, message)),
                Err(TransferError::Cancelled) => Ok(cancelled_message()),
                Err(error) => Err(with_location(archive, file_error(error))),
            },
            Self::VerifyChecksum { manifest, result } => {
                checksum_notice(result).map_err(|message| with_location(manifest, message))
            }
            Self::Permissions { directory, result } => {
                permissions_notice(result).map_err(|message| with_location(directory, message))
            }
            Self::Transfer {
                destination,
                result,
            } => transfer_notice(destination, result),
            #[cfg(target_os = "macos")]
            Self::CompatibleNames {
                destination,
                result,
            } => compatible_names_notice(destination, result),
            #[cfg(target_os = "macos")]
            Self::Resume(result) => match result {
                Ok(report) => Ok(crate::i18n::resume_completion(report)),
                Err(TransferError::Cancelled) => Ok(cancelled_message()),
                Err(error) if error.is_completed_cleanup_warning() => Ok(file_error(error)),
                Err(error) => Err(file_error(error)),
            },
        }
    }
}

fn cancelled_message() -> String {
    crate::tr_format!("Operation cancelled")
}

fn with_location(location: &Location, message: String) -> String {
    #[cfg(target_os = "macos")]
    {
        let path = crate::i18n::display::launch_location(location);
        format!("{message}\n{path}")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = location;
        message
    }
}

fn transfer_cancelled(result: &Result<TransferSummary, TransferFailure>) -> bool {
    match result {
        Ok(summary) => summary.cancelled,
        Err(failure) => matches!(failure.error, TransferError::Cancelled),
    }
}

fn extraction_notice(
    result: &Result<ExtractionSummary, ExtractionFailure>,
) -> Result<String, String> {
    let summary_text = |summary: &ExtractionSummary| {
        crate::tr_format!(
            "Extracted: {completed}. Skipped: {skipped}. Remaining: {remaining}.",
            completed = summary.completed,
            skipped = summary.skipped,
            remaining = summary.remaining()
        )
    };
    match result {
        Ok(summary) => Ok(summary_text(summary)),
        Err(ExtractionFailure {
            summary,
            error: TransferError::Cancelled,
        }) => Ok(crate::tr_format!(
            "Extraction stopped. {summary}",
            summary = summary_text(summary)
        )),
        Err(ExtractionFailure { summary, error }) => {
            #[cfg(target_os = "macos")]
            if summary.completed != 0
                && summary.remaining() == 0
                && error.is_completed_cleanup_warning()
            {
                return Ok(format!("{}\n{}", file_error(error), summary_text(summary)));
            }
            let error = file_error(error);
            if summary.completed != 0 || summary.skipped != 0 {
                Err(format!("{error}. {}", summary_text(summary)))
            } else {
                Err(error)
            }
        }
    }
}

fn archive_verification_notice(report: &ArchiveVerification) -> Result<String, String> {
    let summary = crate::tr_format!(
        "Verified entries: {}. Entry errors: {}. Unverified: {}.",
        report.passed,
        report.damaged,
        report.unverified
    );
    let details = text!("Подробности в журнале результатов.");
    match &report.interruption {
        Some(TransferError::Cancelled) => Ok(crate::tr_format!(
            "Archive check stopped. {summary}",
            summary = summary
        )),
        Some(error) => Err(crate::tr_format!(
            "Archive check could not finish: {error}. {summary} {details}",
            error = file_error(error),
            summary = summary,
            details = details
        )),
        None if report.damaged != 0 || report.unverified != 0 => {
            let message = format!("{summary} {details}");
            #[cfg(target_os = "macos")]
            let message = if let Some(issue) = &report.first_issue {
                let name = issue
                    .name
                    .as_deref()
                    .unwrap_or_else(|| text!("Заголовок TAR"));
                let entry = crate::tr_format!(
                    "Entry {index} · {name}",
                    index = issue.index,
                    name = name.escape_debug()
                );
                let reason = crate::i18n::archive_io_message(&issue.source);
                format!("{message}\n{entry}\n{reason}")
            } else {
                message
            };
            Err(message)
        }
        None => {
            let scope = match report.format {
                Format::Zip => text!("Структура и данные ZIP проверены."),
                Format::Tar => text!("Заголовки TAR проверены, данные прочитаны."),
            };
            Ok(format!("{scope} {summary}"))
        }
    }
}

fn checksum_notice(result: &Result<ChecksumVerification, TransferError>) -> Result<String, String> {
    let report = match result {
        Ok(report) => report,
        Err(TransferError::Cancelled) => return Ok(cancelled_message()),
        Err(error) => return Err(file_error(error)),
    };
    let summary = crate::tr_format!(
        "Matched: {} · changed: {} · missing: {} · read errors: {}",
        report.matched,
        report.changed,
        report.missing,
        report.errors,
    );
    #[cfg(target_os = "macos")]
    let summary = match &report.first_error {
        Some(error) => format!(
            "{summary}\n{}\n{}",
            file_error(error),
            text!("Подробности в журнале результатов.")
        ),
        None => summary,
    };
    if report.cancelled {
        Ok(format!(
            "{}\n{summary}",
            text!("Проверка манифеста остановлена")
        ))
    } else if report.changed + report.missing + report.errors > 0 {
        Err(summary)
    } else {
        Ok(summary)
    }
}

fn permissions_notice(result: &Result<PermissionsReport, TransferError>) -> Result<String, String> {
    let report = match result {
        Ok(report) => report,
        Err(TransferError::Cancelled) => {
            return Ok(text!("Операция остановлена пользователем").into());
        }
        Err(error) => return Err(file_error(error)),
    };
    let mut summary = crate::tr_format!(
        "Changed {}. With warnings {}. Skipped {}. Failed {}.",
        report.completed,
        report.warned,
        report.skipped,
        report.failed
    );
    if report.cancelled {
        summary = format!("{}. {summary}", text!("Операция остановлена пользователем"));
    }
    #[cfg(target_os = "macos")]
    if let Some(warning) = &report.warning {
        summary.push('\n');
        summary.push_str(&file_error(warning));
    }
    match &report.error {
        Some(error) => Err(format!("{summary} {}", file_error(error))),
        None => Ok(summary),
    }
}

fn transfer_notice(
    destination: &Location,
    result: &Result<TransferSummary, TransferFailure>,
) -> Result<String, String> {
    let summary = match result {
        Ok(summary) => summary,
        #[cfg(target_os = "macos")]
        Err(failure) if failure.completed_with_cleanup_warning() => {
            return Ok(crate::i18n::transfer_failure(failure));
        }
        Err(failure) => {
            let message = crate::i18n::transfer_failure(failure);
            return Err(with_location(destination, message));
        }
    };
    let mut message = crate::i18n::transfer_finished(
        summary.completed,
        summary.skipped,
        &summary
            .remaining
            .iter()
            .map(Location::display)
            .collect::<Vec<_>>(),
        summary.preparing,
        summary.cancelled,
        &destination.display(),
    );
    if let Some(warning) = &summary.move_history_warning {
        message.push('\n');
        message.push_str(&file_error(warning));
    }
    Ok(append_metadata_warning(message, summary))
}

fn append_metadata_warning(mut message: String, summary: &TransferSummary) -> String {
    if summary.metadata_warnings != 0 {
        message.push('\n');
        message.push_str(text!(
            "Даты или права доступа сохранены частично. Подробности указаны в строках журнала."
        ));
    }
    message
}

#[cfg(target_os = "macos")]
fn compatible_names_notice(
    destination: &Location,
    result: &Result<TransferSummary, TransferFailure>,
) -> Result<String, String> {
    let summary = match result {
        Ok(summary) => summary,
        Err(failure) if failure.completed_with_cleanup_warning() => {
            return Ok(crate::i18n::transfer_failure(failure));
        }
        Err(failure) => {
            let error = file_error(&failure.error);
            let message = if failure.summary.preparing {
                error
            } else {
                crate::tr_format!(
                    "Completed items: {completed}. {error}",
                    completed = failure.summary.completed,
                    error = error
                )
            };
            return Err(with_location(destination, message));
        }
    };
    let completed = summary.completed;
    let message = if summary.cancelled {
        crate::tr_format!(
            "Completed items: {completed}. {error}",
            completed = completed,
            error = cancelled_message()
        )
    } else {
        crate::tr_format!(
            "Transfer with compatible names finished. Items: {completed}",
            completed = completed
        )
    };
    Ok(append_metadata_warning(message, summary))
}
