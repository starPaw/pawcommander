//! Write-ahead state for a two-way run, separate from the last verified baseline.
use super::{
    super::{is_sync_staging, retained_stage},
    baseline::{decode_path, invalid, read_record, BaselineStore, PairLock},
    Snapshot,
};
use crate::{
    operations::{Cancellation, TransferError},
    recovery::sync_directory,
    vfs::{transfer::entry_metadata, Location},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::ffi::OsStrExt};

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
    pub(super) fn read_run(&self) -> Result<Option<Run>, TransferError> {
        if !self.directory_exists()? {
            return Ok(None);
        }
        let run: Option<Run> = read_record(&self.path.with_extension("run"), MAX_BYTES)?;
        if run
            .as_ref()
            .is_some_and(|run| run.version != VERSION || run.id.is_empty())
        {
            return Err(invalid());
        }
        Ok(run)
    }

    pub(super) fn check_stage(
        &self,
        run: &Run,
        observed: &[Snapshot; 2],
    ) -> Result<(), TransferError> {
        if let Some(staging) = &run.staging {
            let pane = *self.panes.get(staging.side).ok_or_else(invalid)?;
            let path = decode_path(&staging.path)?;
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
        self.write(
            &self.store.path.with_extension("run"),
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
            .ok_or_else(invalid)?;
        run.staging = Some(Staging {
            side,
            path: URL_SAFE_NO_PAD.encode(relative.as_os_str().as_bytes()),
        });
        self.save_run(run, cancel)
    }

    pub(super) fn finish_run(&self) -> Result<(), TransferError> {
        fs::remove_file(self.store.path.with_extension("run"))?;
        sync_directory(&self.store.directory)?;
        Ok(())
    }
}
