use crate::vfs::{self, Location};
use eframe::egui;
use icu_normalizer::ComposingNormalizerBorrowed;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DIRECTORY_DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_PENDING_READS: usize = 2;
const MAX_ACTIVE_READS: usize = 4;
static ACTIVE_READS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub struct Suggestion {
    pub name: String,
    pub path: Location,
    folded_name: String,
}

#[derive(Default)]
pub struct Completion {
    pub edited: bool,
    pub selected: Option<usize>,
    completed_path: Option<Location>,
    requested: Option<(Location, Instant)>,
    cached: Option<(Location, Vec<Suggestion>)>,
    pending: Vec<DirectoryRead>,
}

struct DirectoryRead {
    parent: Location,
    receiver: Option<Receiver<Vec<Suggestion>>>,
    cancelled: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl DirectoryRead {
    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.receiver = None;
    }
}

impl Drop for DirectoryRead {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ReadPermit {
    finished: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Drop for ReadPermit {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Relaxed);
        ACTIVE_READS.fetch_sub(1, Ordering::Relaxed);
        // All editors share this context. Repaint once when a slot is free.
        self.ctx.request_repaint();
    }
}

impl Completion {
    pub fn reset(&mut self) {
        self.edited = false;
        self.completed_path = None;
        self.suspend();
    }

    pub fn suspend(&mut self) {
        self.selected = None;
        self.requested = None;
        self.cached = None;
        self.cancel_obsolete(None);
    }

    fn cancel_obsolete(&mut self, parent: Option<&Location>) {
        for read in &mut self.pending {
            if parent != Some(&read.parent) {
                read.cancel();
            }
        }
    }

    pub fn remember_completion(&mut self, path: &Location) {
        self.completed_path = path.remote().then(|| path.clone());
    }

    pub fn address_changed(&mut self, address: &str, current: &Location) {
        self.edited = true;
        self.selected = None;
        if let Some(completed) = &self.completed_path {
            // Ignore the path when checking the binding. An incomplete path
            // can still be an edit to the same authenticated authority.
            let authority = address.strip_prefix("sftp://").and_then(|rest| {
                let end = rest
                    .find('/')
                    .map_or(address.len(), |slash| "sftp://".len() + slash);
                vfs::resolve(&address[..end], current).ok()
            });
            if !authority.is_some_and(|path| completed.same_sftp_authority(&path)) {
                self.completed_path = None;
            }
        }
    }

    pub fn resolve(
        &self,
        address: &str,
        current: &Location,
    ) -> Result<Location, crate::operations::TransferError> {
        let path = vfs::resolve(address, current)?;
        if address.starts_with("sftp:")
            && let Some(completed) = self
                .completed_path
                .as_ref()
                .filter(|p| p.same_sftp_authority(&path))
        {
            return Ok(completed.with_path(path.path().into()));
        }
        Ok(path)
    }

    pub fn suggestions(
        &mut self,
        address: &str,
        current: &Location,
        ctx: &egui::Context,
    ) -> Vec<Suggestion> {
        let Some((parent, prefix)) = self.edited.then(|| self.query(address, current)).flatten()
        else {
            self.suspend();
            return vec![];
        };
        self.cancel_obsolete(Some(&parent));
        self.pending.retain(|read| {
            let Some(receiver) = &read.receiver else {
                return !read.finished.load(Ordering::Relaxed);
            };
            let entries = match receiver.try_recv() {
                Ok(entries) => entries,
                Err(TryRecvError::Disconnected) => Vec::new(),
                Err(TryRecvError::Empty) => return true,
            };
            self.cached = Some((read.parent.clone(), entries));
            false
        });
        if let Some((_, entries)) = self.cached.as_ref().filter(|(path, _)| *path == parent) {
            let start = entries.partition_point(|item| item.folded_name.as_str() < prefix.as_str());
            return entries[start..]
                .iter()
                .take_while(|item| item.folded_name.starts_with(&prefix))
                .filter(|item| prefix.starts_with('.') || !item.name.starts_with('.'))
                .take(8)
                .cloned()
                .collect();
        }
        let now = Instant::now();
        let deadline = match &self.requested {
            Some((path, deadline)) if *path == parent => *deadline,
            _ => {
                let deadline = now + DIRECTORY_DEBOUNCE;
                self.requested = Some((parent.clone(), deadline));
                deadline
            }
        };
        if deadline > now {
            ctx.request_repaint_after(deadline - now);
            return vec![];
        }
        // A cancelled filesystem or SSH call can remain blocked until it returns.
        // Allow the current parent to start alongside it, while bounding queued I/O.
        if self.pending.len() < MAX_PENDING_READS
            && !self.pending.iter().any(|read| read.receiver.is_some())
        {
            let allow_unique_session = parent.remote()
                && !parent.same_sftp_authority(current)
                && self
                    .completed_path
                    .as_ref()
                    .is_none_or(|path| !path.same_sftp_authority(&parent));
            self.start_read(parent, allow_unique_session, ctx);
        }
        vec![]
    }

    fn start_read(&mut self, parent: Location, allow_unique_session: bool, ctx: &egui::Context) {
        self.start_read_with(parent, allow_unique_session, ctx, |path, unique, token| {
            read_folders(path, unique, &|| token.load(Ordering::Relaxed))
        });
    }

    fn start_read_with(
        &mut self,
        parent: Location,
        allow_unique_session: bool,
        ctx: &egui::Context,
        reader: impl FnOnce(&Location, bool, &AtomicBool) -> Vec<Suggestion> + Send + 'static,
    ) {
        if ACTIVE_READS
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_ACTIVE_READS).then_some(count + 1)
            })
            .is_err()
        {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let permit = ReadPermit {
            finished: Arc::clone(&finished),
            ctx: ctx.clone(),
        };
        let token = Arc::clone(&cancelled);
        let path = parent.clone();
        let result = std::thread::Builder::new()
            .name("pawcommander-path-completion".into())
            .spawn(move || {
                let _permit = permit;
                // Disconnect the result channel before releasing the worker slot.
                let sender = sender;
                let cancelled = || token.load(Ordering::Relaxed);
                let entries = reader(&path, allow_unique_session, &token);
                if !cancelled() {
                    let _ = sender.send(entries);
                }
            });
        if result.is_ok() {
            self.pending.push(DirectoryRead {
                parent,
                receiver: Some(receiver),
                cancelled,
                finished,
            });
        } else {
            self.cached = Some((parent, vec![]));
        }
    }

    fn query(&self, address: &str, current: &Location) -> Option<(Location, String)> {
        if address.starts_with("archive:") {
            return None;
        }
        let url = address.starts_with("sftp:");
        if url && !address.strip_prefix("sftp://")?.contains('/') {
            return None;
        }
        let windows_path = cfg!(windows) && !url && !current.remote();
        let split = address
            .rfind(|ch| ch == '/' || (windows_path && ch == '\\'))
            .map_or(0, |index| index + 1);
        let (parent, prefix) = address.split_at(split);
        let parent = self.resolve(parent, current).ok()?;
        let prefix = if url {
            folded(
                &percent_encoding::percent_decode_str(prefix)
                    .decode_utf8()
                    .ok()?,
            )
        } else {
            folded(prefix)
        };
        Some((parent, prefix))
    }
}

fn folded(text: &str) -> String {
    // A prefix has no following context. Use character lowercase mappings so
    // a capital sigma folds the same way in the prefix and the whole name.
    let lowercase: String = text.chars().flat_map(char::to_lowercase).collect();
    ComposingNormalizerBorrowed::new_nfc()
        .normalize(&lowercase)
        .into_owned()
}

fn read_folders(
    parent: &Location,
    allow_unique_session: bool,
    cancelled: &impl Fn() -> bool,
) -> Vec<Suggestion> {
    let mut folders = Vec::new();
    let result = vfs::visit_address_directory(parent, allow_unique_session, cancelled, |entry| {
        if entry.is_dir && !entry.name.contains(['\r', '\n']) {
            let Some(name) = entry.path.path().file_name().and_then(|name| name.to_str()) else {
                return;
            };
            folders.push(Suggestion {
                folded_name: folded(name),
                name: name.to_owned(),
                path: entry.path,
            });
        }
    });
    if result.is_err() || cancelled() {
        return vec![];
    }
    folders.sort_unstable_by(|a, b| a.folded_name.cmp(&b.folded_name).then(a.name.cmp(&b.name)));
    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    #[test]
    fn remote_completion_uses_the_decoded_path_and_current_session_key() {
        let current = Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 2222,
                user: "alice".into(),
                key: Some(PathBuf::from("test-key")),
                jump: None,
            },
            path: "/work".into(),
        };
        let completion = Completion::default();
        let (parent, prefix) = completion
            .query(
                "sftp://alice@example.invalid:2222/Two%20Words/Ne%20",
                &current,
            )
            .unwrap();
        assert_eq!(parent, current.with_path("/Two Words".into()));
        assert_eq!(prefix, "ne ");
        assert!(completion
            .query("sftp://alice@example.invalid:2222", &current)
            .is_none());
    }

    #[test]
    fn completed_session_key_is_kept_only_while_editing_the_same_authority() {
        let local: Location = PathBuf::from("/work").into();
        let address = "sftp://alice@example.invalid:2222/folder";
        let unbound = vfs::resolve(address, &local).unwrap();
        let Location::Sftp { mut endpoint, path } = unbound.clone() else {
            unreachable!();
        };
        endpoint.key = Some(PathBuf::from("test-key"));
        let bound = Location::Sftp { endpoint, path };

        let mut completion = Completion::default();
        completion.remember_completion(&bound);
        completion.address_changed("sftp://alice@example.invalid:2222/another#fragment", &local);
        assert_eq!(completion.resolve(address, &local).unwrap(), bound);
        completion.address_changed("sftp://bob@example.invalid:2222/folder", &local);
        completion.address_changed(address, &local);
        assert_eq!(completion.resolve(address, &local).unwrap(), unbound);
    }

    #[test]
    fn superseded_directory_read_cannot_block_or_replace_current_suggestions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("new/Café папка 📂")).unwrap();
        let current: Location = root.path().to_path_buf().into();
        let old = current.join("old");
        let new = current.join("new");
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let mut completion = Completion {
            edited: true,
            requested: Some((new, Instant::now())),
            pending: vec![DirectoryRead {
                parent: old.clone(),
                receiver: Some(receiver),
                cancelled: Arc::clone(&cancelled),
                finished: Arc::clone(&finished),
            }],
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let query = "new/CAFE\u{301}";
        assert!(completion.suggestions(query, &current, &ctx).is_empty());
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(completion.pending.len(), 2);
        let deadline = Instant::now() + Duration::from_secs(3);
        let suggested = loop {
            let mut suggestions = completion.suggestions(query, &current, &ctx);
            if let Some(item) = suggestions.pop() {
                break item;
            }
            assert!(
                Instant::now() < deadline,
                "expected local Unicode completion"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(folded(&suggested.name), "café папка 📂");
        assert!(suggested.path.path().is_dir());
        assert!(sender
            .send(vec![Suggestion {
                name: "stale".into(),
                path: old.join("stale"),
                folded_name: "stale".into(),
            }])
            .is_err());
        finished.store(true, Ordering::Relaxed);
        assert!(completion.suggestions("old/st", &current, &ctx).is_empty());
        assert!(completion.pending.is_empty());
    }

    #[test]
    fn spawned_reader_finishing_after_replacement_cannot_publish_stale_folders() {
        assert_blocked_reader_is_discarded(false);
    }

    #[test]
    fn spawned_reader_finishing_after_reset_cannot_restore_the_old_cache() {
        assert_blocked_reader_is_discarded(true);
    }

    fn assert_blocked_reader_is_discarded(reset_same_parent: bool) {
        let root = tempfile::tempdir().unwrap();
        let old_dir = root.path().join("old");
        let new_dir = root.path().join("new");
        std::fs::create_dir_all(old_dir.join("Café stale")).unwrap();
        std::fs::create_dir_all(new_dir.join("Café fresh")).unwrap();
        let marker = root.path().join("current-marker.txt");
        std::fs::write(&marker, b"current directory stays unchanged").unwrap();
        let current: Location = root.path().to_path_buf().into();
        let original_current = current.clone();
        let old = current.join("old");
        let ctx = egui::Context::default();
        let mut completion = Completion {
            edited: true,
            ..Default::default()
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        completion.start_read_with(old.clone(), false, &ctx, move |path, unique, token| {
            let entries = read_folders(path, unique, &|| token.load(Ordering::Relaxed));
            ready_tx
                .send(
                    entries
                        .iter()
                        .map(|item| item.name.clone())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            // Hold a real worker after its filesystem read, emulating I/O that
            // cannot return until after the editor has moved to another request.
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            cancel_tx.send(token.load(Ordering::Relaxed)).unwrap();
            entries
        });
        assert_eq!(completion.pending.len(), 1, "worker slot unavailable");
        let old_finished = Arc::clone(&completion.pending[0].finished);
        assert_eq!(
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ["Café stale"]
        );

        let (parent, query) = if reset_same_parent {
            completion.reset();
            // A reopened editor requests the same parent after its contents
            // changed. A stale cache would have the right path but wrong folders.
            std::fs::rename(old_dir.join("Café stale"), old_dir.join("Café fresh")).unwrap();
            (old, "old/CAFE\u{301}")
        } else {
            (current.join("new"), "new/CAFE\u{301}")
        };
        completion.address_changed(query, &current);
        completion.requested = Some((parent.clone(), Instant::now()));
        assert!(completion.suggestions(query, &current, &ctx).is_empty());
        assert!(completion.pending[0].cancelled.load(Ordering::Relaxed));
        assert!(completion.pending[0].receiver.is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let suggestions = completion.suggestions(query, &current, &ctx);
            if !suggestions.is_empty() {
                assert_eq!(suggestions.len(), 1);
                assert_eq!(suggestions[0].name, "Café fresh");
                assert_eq!(suggestions[0].path, parent.join("Café fresh"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new worker blocked by stale read"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Force the old worker to finish only after the newer result is cached.
        release_tx.send(()).unwrap();
        assert!(cancel_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        while !old_finished.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "old worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        let suggestions = completion.suggestions(query, &current, &ctx);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "Café fresh");
        assert_eq!(suggestions[0].path, parent.join("Café fresh"));
        assert!(completion.pending.is_empty());
        assert_eq!(current, original_current);
        assert_eq!(
            std::fs::read(marker).unwrap(),
            b"current directory stays unchanged"
        );
    }

    #[test]
    fn resetting_completion_cancels_and_discards_an_old_read_for_the_same_parent() {
        let current: Location = PathBuf::from("/work").into();
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let mut completion = Completion {
            edited: true,
            pending: vec![DirectoryRead {
                parent: current.clone(),
                receiver: Some(receiver),
                cancelled: Arc::clone(&cancelled),
                finished: Arc::clone(&finished),
            }],
            ..Default::default()
        };
        sender
            .send(vec![Suggestion {
                name: "stale".into(),
                path: current.join("stale"),
                folded_name: "stale".into(),
            }])
            .unwrap();
        completion.reset();
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(sender.send(vec![]).is_err());
        finished.store(true, Ordering::Relaxed);
        completion.edited = true;
        let ctx = egui::Context::default();
        assert!(completion.suggestions("st", &current, &ctx).is_empty());
        assert!(completion.cached.is_none());
        assert!(completion.pending.is_empty());
    }
}
