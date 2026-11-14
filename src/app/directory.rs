use super::state::Sort;
use crate::vfs::FileEntry;
use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

// Poll only displayed directories. Wait from completion to bound work on slow
// filesystems, and poll SFTP less frequently to limit network traffic.
pub(super) const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(super) const SFTP_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DirectoryLoadMode {
    Progressive,
    Reload,
    Automatic,
}

#[derive(Clone)]
pub(super) enum FolderVisit {
    Unseen,
    CountOnOpen,
    SharedOpen(Rc<Cell<bool>>),
    Recorded,
}

pub(super) enum DirectoryRead {
    Batch(Vec<FileEntry>),
    Finished(Result<Option<Vec<FileEntry>>, String>),
    Archive(Result<(Arc<crate::archive::Index>, Vec<FileEntry>), String>),
}

pub(super) struct DirectoryLoad {
    pub(super) receiver: Receiver<DirectoryRead>,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) mode: DirectoryLoadMode,
}

impl DirectoryLoad {
    pub(super) fn channel(mode: DirectoryLoadMode) -> (SyncSender<DirectoryRead>, Self) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let load = Self {
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            mode,
        };
        (sender, load)
    }
}

impl Drop for DirectoryLoad {
    fn drop(&mut self) {
        // The receiver also closes, unblocking a producer waiting to send.
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

pub(super) fn send_progressive_listing(
    read: impl FnOnce(&mut dyn FnMut(FileEntry)) -> Result<(), String>,
    send: impl Fn(DirectoryRead) -> bool,
) {
    const BATCH_SIZE: usize = 1024;
    const BATCH_INTERVAL: Duration = Duration::from_millis(40);
    let mut batch = Vec::new();
    let mut last_batch = None;
    let result = read(&mut |entry| {
        batch.push(entry);
        // Publish the first entry immediately even if the next read stalls.
        if batch.len() == BATCH_SIZE
            || last_batch.is_none_or(|at: Instant| at.elapsed() >= BATCH_INTERVAL)
        {
            send(DirectoryRead::Batch(std::mem::take(&mut batch)));
            last_batch = Some(Instant::now());
        }
    });
    if batch.is_empty() || send(DirectoryRead::Batch(batch)) {
        send(DirectoryRead::Finished(result.map(|()| None)));
    }
}

#[derive(Default)]
pub(super) struct DirectoryEntries {
    pub(super) files: Vec<FileEntry>,
    pub(super) view: RefCell<DirectoryView>,
}

impl Deref for DirectoryEntries {
    type Target = [FileEntry];

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

#[derive(Default)]
pub(super) struct DirectoryView {
    pub(super) folded_names: Vec<Option<Box<str>>>,
    pub(super) query: Option<ViewQuery>,
    pub(super) scanned: usize,
    pub(super) rows: Rc<Vec<usize>>,
}

pub(super) struct ViewQuery {
    pub(super) hidden: bool,
    pub(super) filter: String,
    pub(super) sort: Sort,
    pub(super) descending: bool,
}

impl DirectoryEntries {
    pub(super) fn replace(&mut self, files: Vec<FileEntry>) {
        self.files = files;
        *self.view.get_mut() = DirectoryView::default();
    }

    pub(super) fn extend(&mut self, files: Vec<FileEntry>) {
        self.files.extend(files);
    }

    pub(super) fn visible(
        &self,
        hidden: bool,
        filter: &str,
        sort: Sort,
        descending: bool,
    ) -> Rc<Vec<usize>> {
        let mut view = self.view.borrow_mut();
        let DirectoryView {
            folded_names,
            query,
            scanned,
            rows,
        } = &mut *view;
        if query.as_ref().is_none_or(|previous| {
            previous.hidden != hidden
                || previous.filter != filter
                || previous.sort != sort
                || previous.descending != descending
        }) {
            *query = Some(ViewQuery {
                hidden,
                filter: filter.into(),
                sort,
                descending,
            });
            *scanned = 0;
            if let Some(rows) = Rc::get_mut(rows) {
                rows.clear();
            } else {
                *rows = Rc::default();
            }
        }
        if *scanned == self.len() {
            return rows.clone();
        }

        let filter = filter.to_lowercase();
        if sort == Sort::Name || !filter.is_empty() {
            folded_names.extend(self[folded_names.len()..].iter().map(|entry| {
                let name = &entry.name;
                if name.is_ascii() && !name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                    return None;
                }
                let folded = name.to_lowercase();
                (folded != *name).then(|| folded.into_boxed_str())
            }));
        }
        let folded_name =
            |index: usize| folded_names[index].as_deref().unwrap_or(&self[index].name);
        let indices = Rc::make_mut(rows);
        let previous_len = indices.len();
        indices.extend((*scanned..self.len()).filter(|&index| {
            (hidden || !self[index].name.starts_with('.'))
                && (filter.is_empty() || folded_name(index).contains(&filter))
        }));
        if indices.len() != previous_len {
            // Prior rows are already sorted after an append. Stable sorting also
            // keeps enumeration order for entries with identical sort keys.
            indices.sort_by(|&a, &b| {
                self[b].is_dir.cmp(&self[a].is_dir).then_with(|| {
                    let order = match sort {
                        Sort::Name => folded_name(a).cmp(folded_name(b)),
                        Sort::Size => self[a].size.cmp(&self[b].size),
                        Sort::Modified => self[a].modified.cmp(&self[b].modified),
                    }
                    .then_with(|| self[a].name.cmp(&self[b].name));
                    if descending {
                        order.reverse()
                    } else {
                        order
                    }
                })
            });
        }
        *scanned = self.len();
        rows.clone()
    }
}
