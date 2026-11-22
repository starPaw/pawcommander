//! Write-ahead state for a two-way run, separate from the last verified baseline.
use super::{
    super::{is_sync_staging, retained_stage},
    baseline::{decode_path, read_record, BaselineStore, PairLock, StatePath},
    Snapshot,
};
use crate::{
    file_error::{SyncStateAction as Action, SyncStateKind, SyncStateProblem as Problem},
    operations::{Cancellation, TransferError},
    recovery::sync_directory,
    vfs::{transfer::entry_metadata, Location},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::ffi::OsStrExt, path::PathBuf};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 128 * 1024;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Run {
    version: u32,
    id: String,
    staging: Option<Staging>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Staging {
    // Store the canonical pair order so pane swaps cannot redirect recovery.
    side: usize,
    path: String,
}

impl Run {
    pub(super) fn new() -> Self {
        Self {
            version: VERSION,
            id: crate::result_history::operation_id(),
            staging: None,
        }
    }
}

impl BaselineStore {
    pub(super) fn run_path(&self) -> PathBuf {
        self.path.with_extension("run")
    }

    pub(super) fn read_run(&self) -> Result<Option<Run>, TransferError> {
        if !self.directory_exists()? {
            return Ok(None);
        }
        let path = self.run_path();
        let record = StatePath::new(SyncStateKind::RunRecord, &path);
        let run: Option<Run> = read_record(record, MAX_BYTES)?;
        if let Some(run) = &run {
            if run.version != VERSION {
                return Err(record.error(Problem::UnsupportedVersion(run.version)));
            }
            if run.id.is_empty() {
                return Err(record.error(Problem::InvalidContents));
            }
        }
        Ok(run)
    }

    pub(super) fn check_stage(
        &self,
        run: &Run,
        observed: &[Snapshot; 2],
    ) -> Result<(), TransferError> {
        if let Some(staging) = &run.staging {
            let path = self.run_path();
            let record = StatePath::new(SyncStateKind::RunRecord, &path);
            let invalid = || record.error(Problem::InvalidContents);
            let pane = *self.panes.get(staging.side).ok_or_else(invalid)?;
            let path = decode_path(&staging.path, record)?;
            if !path.file_name().is_some_and(is_sync_staging) {
                return Err(invalid());
            }
            let location = observed[pane].root.join(path);
            if entry_metadata(&location)?.is_some() {
                return Err(retained_stage(&location));
            }
        }
        Ok(())
    }
}

impl PairLock<'_> {
    pub(super) fn save_run(&self, run: &Run, cancel: &Cancellation) -> Result<(), TransferError> {
        let path = self.store.run_path();
        self.write(
            StatePath::new(SyncStateKind::RunRecord, &path),
            run,
            MAX_BYTES,
            cancel,
        )
    }

    pub(super) fn track_stage(
        &self,
        run: &mut Run,
        location: &Location,
        observed: &[Snapshot; 2],
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        self.store.check_roots(observed)?;
        self.store.check_stage(run, observed)?;
        let (side, relative) = self
            .store
            .panes
            .iter()
            .enumerate()
            .find_map(|(side, pane)| {
                let root = &observed[*pane].root;
                if !location.same_backend(root) {
                    return None;
                }
                Some((side, location.path().strip_prefix(root.path()).ok()?))
            })
            .ok_or_else(|| {
                let path = self.store.run_path();
                StatePath::new(SyncStateKind::RunRecord, &path).error(Problem::InvalidContents)
            })?;
        run.staging = Some(Staging {
            side,
            path: URL_SAFE_NO_PAD.encode(relative.as_os_str().as_bytes()),
        });
        self.save_run(run, cancel)
    }

    pub(super) fn finish_run(&self) -> Result<(), TransferError> {
        let path = self.store.run_path();
        let record = StatePath::new(SyncStateKind::RunRecord, &path);
        fs::remove_file(&path).map_err(record.io(Action::Remove))?;
        sync_directory(&self.store.directory).map_err(record.io(Action::SyncRemoval))
    }
}
