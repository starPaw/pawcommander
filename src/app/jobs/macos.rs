//! File-operation replies for native callers, the history worker and the macOS UI.
use super::{append_history_warning, HistoryWarning, WorkResult};
use crate::app::{
    external_editors::{save_notice, SaveResult},
    file_result::FileResult,
    file_templates::{creation_notice, CreationResult},
    PawCommander,
};
use crate::file_error::{ExportTarget, Failure, RecoveryStartError};
use crate::operations::TransferError;
use crate::result_history::{Outcome, Results};
use crate::vfs::{Location, RemoteEdit};
use std::sync::{mpsc, Arc};

#[derive(Clone)]
pub(super) enum NativeCompletion {
    FilePromise(mpsc::Sender<Result<(), TransferError>>),
    CreateFile(mpsc::Sender<Arc<CreationResult>>),
    Export {
        target: ExportTarget,
        reply: Option<mpsc::Sender<Result<(), TransferError>>>,
    },
}

impl NativeCompletion {
    pub(super) fn export_target(&self) -> Option<&ExportTarget> {
        match self {
            Self::Export { target, .. } => Some(target),
            _ => None,
        }
    }

    fn start_error(&self, error: TransferError) -> TransferError {
        match self.export_target() {
            Some(target) => target.operation_error(error),
            None => error,
        }
    }

    pub(super) fn report_start_error(&self, error: TransferError) -> String {
        let error = self.start_error(error);
        let notice = crate::i18n::file_error(&error);
        self.report_error(error);
        notice
    }

    pub(super) fn report_error(&self, error: TransferError) {
        match self {
            Self::FilePromise(reply)
            | Self::Export {
                reply: Some(reply), ..
            } => {
                let _ = reply.send(Err(error));
            }
            Self::CreateFile(reply) => {
                let _ = reply.send(Arc::new(Err(error)));
            }
            Self::Export { reply: None, .. } => {}
        }
    }

    fn report_success(&self) {
        if let Self::FilePromise(reply)
        | Self::Export {
            reply: Some(reply), ..
        } = self
        {
            let _ = reply.send(Ok(()));
        }
    }
}

impl WorkResult {
    pub(super) fn after_panic(
        message: &str,
        creation_target: Option<Location>,
        native: Option<&NativeCompletion>,
        results: &mut Results,
    ) -> Self {
        if let Some(target) = creation_target {
            let error = TransferError::from(message).with_unconfirmed_publication();
            results.record_with_error(&target, Outcome::PublicationUnconfirmed, Some(&error));
            Self::CreateFile(target, Arc::new(Err(error)))
        } else if let Some(target) = native.and_then(NativeCompletion::export_target) {
            Self::Text(Err(target.worker_stopped()))
        } else {
            Self::Text(Err(message.into()))
        }
    }
}

pub(in crate::app) enum JobOutcome {
    Text(Result<String, String>),
    File(Box<FileResult>),
    CreateFile {
        target: Location,
        result: Arc<CreationResult>,
    },
    RemoteSave {
        edit: Box<RemoteEdit>,
        result: SaveResult,
    },
}

impl JobOutcome {
    /// A native receiver needs the file result as soon as the operation finishes,
    /// even if persisting its history or removing its recovery record later fails.
    pub(super) fn acknowledge(
        work: WorkResult,
        native: Option<&NativeCompletion>,
        remote_save: bool,
        results: &mut Results,
    ) -> Self {
        match work {
            // A stopped remote save has no edit to return to the editor.
            WorkResult::Text(result) if native.is_some() || remote_save => {
                let notice = if matches!(result, Err(TransferError::Cancelled)) {
                    results.cancelled = true;
                    Ok(crate::tr_format!("Operation cancelled"))
                } else {
                    result.as_ref().cloned().map_err(crate::i18n::file_error)
                };
                if let Some(native) = native {
                    match result {
                        Ok(_) => native.report_success(),
                        Err(error) => native.report_error(error),
                    }
                }
                Self::Text(notice)
            }
            WorkResult::Text(result) => Self::file(FileResult::text(result), results),
            WorkResult::File(result) => Self::file(result, results),
            WorkResult::CreateFile(target, result) => {
                if let Some(NativeCompletion::CreateFile(reply)) = native {
                    let _ = reply.send(Arc::clone(&result));
                }
                Self::CreateFile { target, result }
            }
            WorkResult::RemoteSave(edit, result) => Self::RemoteSave { edit, result },
        }
    }

    fn file(result: FileResult, results: &mut Results) -> Self {
        results.cancelled |= result.cancelled();
        Self::File(Box::new(result))
    }

    /// History takes a text snapshot. The UI can render typed outcomes again in
    /// its current language when it receives the completion.
    pub(super) fn notice(&self) -> Result<String, String> {
        match self {
            Self::Text(result) => result.clone(),
            Self::File(result) => result.notice(),
            Self::CreateFile { target, result } => creation_notice(target, result),
            Self::RemoteSave { edit, result } => save_notice(edit.target(), edit.copy(), result),
        }
    }
}

pub(in crate::app) enum JobCompletion {
    StartFailed(TransferError),
    Finished {
        outcome: JobOutcome,
        history_warning: Option<HistoryWarning>,
    },
}

impl JobCompletion {
    pub(in crate::app) fn text(result: Result<String, String>) -> Self {
        Self::Finished {
            outcome: JobOutcome::Text(result),
            history_warning: None,
        }
    }

    pub(super) fn start_failed(
        cause: RecoveryStartError,
        native: Option<&NativeCompletion>,
    ) -> Self {
        let cause = Arc::new(cause);
        let error = || {
            let refusal = TransferError::Failed(Failure::RecoveryStart(Arc::clone(&cause)));
            match native {
                Some(native) => native.start_error(refusal),
                None => refusal,
            }
        };
        if let Some(native) = native {
            native.report_error(error());
        }
        Self::StartFailed(error())
    }

    pub(super) fn apply(self, app: &mut PawCommander, remote_save: bool) -> Result<String, String> {
        match self {
            Self::StartFailed(error) => {
                let job = app.jobs.current.as_mut().unwrap();
                let retry = job.retry_origin.take();
                let undo = job.undo_origin.take();
                if let Some(retry) = retry {
                    app.remember_retry(retry);
                }
                if let Some(undo) = undo {
                    app.remember_undo(undo);
                }
                let notice = crate::i18n::file_error(&error);
                if remote_save {
                    app.remote_editor.finish_save(None, Err(error), None);
                }
                Err(notice)
            }
            Self::Finished {
                outcome,
                history_warning,
            } => {
                let notice = append_history_warning(outcome.notice(), history_warning.as_ref());
                match outcome {
                    JobOutcome::Text(Err(error)) if remote_save => {
                        app.remote_editor
                            .finish_save(None, Err(error.into()), history_warning);
                    }
                    JobOutcome::RemoteSave { edit, result } => {
                        app.remote_editor
                            .finish_save(Some(*edit), result, history_warning);
                    }
                    JobOutcome::Text(_) | JobOutcome::File(_) | JobOutcome::CreateFile { .. } => {}
                }
                notice
            }
        }
    }
}
