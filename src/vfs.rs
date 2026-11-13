//! Local, SFTP and read-only archive locations. Remote credentials live only in authenticated sessions;
//! saved tabs/bookmarks contain connection coordinates and key paths, never secrets.

mod listing;
mod location;
mod mutations;
mod preview;
mod sftp;
mod transfer;

use std::fmt;

pub use listing::{
    read_automatic_directory, read_directory, visit_address_directory, visit_connected_directory,
    visit_directory, FileEntry,
};
pub use location::{resolve, Endpoint, Location};
#[cfg(test)]
pub use mutations::trash_entries;
pub use mutations::{create_directory, rename_entry, trash_entries_with_progress};
pub use preview::preview_data;
#[cfg(test)]
pub use sftp::connect;
pub use sftp::connect_location;
#[cfg(test)]
pub use transfer::transfer;
pub use transfer::transfer_with_conflicts;

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
