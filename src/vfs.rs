//! Local, SFTP and read-only archive locations. Remote credentials live only in authenticated sessions;
//! saved tabs/bookmarks contain connection coordinates and key paths, never secrets.

mod archive_create;
mod archive_edit;
mod archive_extract;
mod batch_rename;
mod checksum;
mod cross_move;
mod listing;
mod location;
mod mutations;
mod preview;
mod retry;
mod sftp;
mod staged_file;
mod transfer;
mod undo;

use std::fmt;

pub(crate) use archive_create::{archive_file_name, create_archive};
pub(crate) use archive_edit::{edit_archive, ArchiveEdit};
pub(crate) use archive_extract::extract_archive;
pub(crate) use batch_rename::{BatchRenamePreview, BatchRenameRules};
pub(crate) use checksum::{
    checksum_manifest_name, create_checksum_manifest, looks_like_checksum_manifest,
    verify_checksum_manifest,
};
pub use listing::{
    read_automatic_directory, read_directory, visit_address_directory, visit_connected_directory,
    visit_directory, FileEntry,
};
pub use location::{resolve, Endpoint, Location};
pub use mutations::{create_directory, trash_entries_with_progress};
#[cfg(test)]
pub use mutations::{rename_entry, trash_entries};
#[cfg(target_os = "macos")]
pub(crate) use preview::cache_media_preview;
pub(crate) use preview::open_preview_stream;
pub use preview::preview_data;
pub(crate) use retry::RetryTransfer;
#[cfg(test)]
pub use sftp::connect;
pub use sftp::connect_location;
#[cfg(test)]
pub use transfer::transfer;
pub use transfer::transfer_with_conflicts;
pub(crate) use undo::{rename_with_undo, UndoBatch};

#[derive(Debug, Default)]
pub struct TransferSummary {
    pub completed: usize,
    pub skipped: usize,
    pub cancelled: bool,
    pub remaining: Vec<Location>,
    pub preparing: bool,
}

const MAX_TREE_DEPTH: usize = 128;

fn err(error: impl fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests;
