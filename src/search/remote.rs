use super::directory::{EntryKind, EntryMetadata};
use super::{FolderSearch, Results, SearchMatch, Warnings, MAX_RESULTS};
use crate::operations::TransferError;
use crate::vfs::{search, Location};
use std::time::Instant;

pub(super) const DEFAULT_DEPTH: usize = 8;
pub(super) const DEFAULT_SECONDS: u64 = 15;
pub(super) const MAX_SECONDS: u64 = 60;

pub(super) fn run(
    folder: &Location,
    query: &FolderSearch<'_>,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<Results, TransferError> {
    let mut matches = Vec::new();
    let mut missing_metadata = 0;
    let coverage = search::walk(
        folder,
        query.filters.depth.unwrap_or(DEFAULT_DEPTH),
        deadline,
        cancelled,
        |path, stat| {
            let name = path.path().file_name().expect("directory entry name");
            if query.excluded(name) {
                return search::Visit::Prune;
            }
            let kind = stat.perm.map(|_| {
                if stat.is_dir() {
                    EntryKind::Directory
                } else if stat.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                }
            });
            if !query.matches_name(name, kind) {
                return search::Visit::Continue;
            }
            if query.filters.needs_metadata() {
                let Some(kind) = kind else {
                    return search::Visit::Continue;
                };
                let metadata = EntryMetadata {
                    kind,
                    size: stat.size,
                    modified: stat.mtime.and_then(|seconds| {
                        chrono::DateTime::from_timestamp(i64::try_from(seconds).ok()?, 0)
                    }),
                };
                match query.filters.matches_metadata(metadata) {
                    Ok(true) => {}
                    Ok(false) => return search::Visit::Continue,
                    Err(_) => {
                        missing_metadata += 1;
                        return search::Visit::Continue;
                    }
                }
            }
            matches.push(SearchMatch::from(path.clone()));
            if matches.len() > MAX_RESULTS {
                search::Visit::Stop
            } else {
                search::Visit::Continue
            }
        },
    )?;
    let limited = matches.len() > MAX_RESULTS;
    matches.truncate(MAX_RESULTS);
    matches.sort_unstable_by(|a, b| a.path.cmp(&b.path));

    Ok(Results {
        matches,
        limited,
        warning: Warnings::Sftp {
            coverage,
            missing_metadata,
        },
    })
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
pub(crate) use tests::check_sftp_search;
