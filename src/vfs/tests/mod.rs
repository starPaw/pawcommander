use super::listing::{
    read_automatic_directory, read_directory, visit_address_directory, visit_connected_directory,
};
#[cfg(windows)]
use super::location::validate_name;
use super::location::{resolve, Endpoint, Location};
use super::mutations::{
    create_directory, rename_entry, trash_entries, trash_entries_with_progress,
};
use super::preview::preview_data;
use super::sftp::{connect_with_key, remote, sessions};
#[cfg(unix)]
use super::transfer::validate_source_child_name;
use super::transfer::{
    canonical, check_copy_space, copy_transfer_cancellable, copy_transfer_checked, entry_metadata,
    publish_item, publish_replacement_with_backup_rename, record_item_result,
    remote_source_matches, rename_without_replacing, staged_copy, staging_result,
    staging_result_typed, transfer, transfer_with_conflicts, with_staging, CopyPhase, TransferItem,
    TransferSourceManifest,
};
use super::{connect, TransferSummary};
use crate::conflict::{Answer, Conflict, Decision};
use crate::fs_ops;
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, Progress, TransferError};
use ssh2::FileStat;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

mod bandwidth;
mod cancellation;
mod conflicts;
mod history;
mod interactive_ssh;
mod local;
mod location;
mod pause;
mod retry;
mod sftp;
mod source;
mod staging;
mod undo;

#[cfg(unix)]
fn unsupported_sftp_names() -> Vec<OsString> {
    // macOS filesystems reject invalid UTF-8 at creation time; test those
    // raw bytes separately below without requiring a compatible volume.
    vec![OsString::from("bad\\name"), OsString::from(" ")]
}
