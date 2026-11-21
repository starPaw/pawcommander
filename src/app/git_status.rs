use super::tabs::Tab;
use crate::i18n::text;
use crate::theme::Palette;
use crate::vfs::Location;
use eframe::egui;
use git2::{Repository, Status, StatusOptions};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FileStatus {
    Ignored,
    Untracked,
    Modified,
}

impl FileStatus {
    fn from_git(status: Status) -> Option<Self> {
        if status.is_empty() {
            None
        } else if !(status - Status::WT_NEW - Status::IGNORED).is_empty() {
            Some(Self::Modified)
        } else if status.contains(Status::WT_NEW) {
            Some(Self::Untracked)
        } else {
            Some(Self::Ignored)
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Modified => text!("Изменено в Git"),
            Self::Untracked => text!("Не отслеживается Git"),
            Self::Ignored => text!("Игнорируется Git"),
        }
    }

    pub(super) fn paint(self, ui: &egui::Ui, center: egui::Pos2, colors: Palette) {
        let (symbol, color) = match self {
            Self::Modified => ("M", colors.peach),
            Self::Untracked => ("?", colors.blue),
            Self::Ignored => ("!", colors.muted),
        };
        let rect = egui::Rect::from_center_size(center, egui::Vec2::splat(14.0));
        ui.painter().rect_filled(rect, 3, colors.bg);
        ui.painter().text(
            center,
            egui::Align2::CENTER_CENTER,
            symbol,
            egui::FontId::monospace(11.0),
            color,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Head {
    Branch(String),
    Detached(String),
}

impl Head {
    fn read(repo: &Repository) -> Option<Self> {
        let head = repo.find_reference("HEAD").ok()?;
        if let Some(target) = head.symbolic_target_bytes() {
            let branch = target.strip_prefix(b"refs/heads/").unwrap_or(target);
            return Some(Self::Branch(String::from_utf8_lossy(branch).into_owned()));
        }
        let oid = head.target()?.to_string();
        Some(Self::Detached(oid[..7].to_owned()))
    }
}

#[derive(Debug)]
pub(super) struct Snapshot {
    folder: PathBuf,
    head: Head,
    entries: HashMap<OsString, FileStatus>,
}

impl Snapshot {
    pub(super) fn matches(&self, folder: &Location) -> bool {
        folder.local() == Some(self.folder.as_path())
    }

    pub(super) fn branch_label(&self) -> String {
        match &self.head {
            Head::Branch(branch) => format!("Git · {branch}"),
            Head::Detached(oid) => format!("{} · {oid}", text!("Git · Отсоединённый HEAD")),
        }
    }

    pub(super) fn entry(&self, path: &Location) -> Option<FileStatus> {
        let path = path.local()?;
        if path.parent()? != self.folder {
            return None;
        }
        self.entries.get(path.file_name()?).copied()
    }

    fn read(folder: &Path, cancelled: &AtomicBool) -> Option<Self> {
        let canonical = folder.canonicalize().ok()?;
        let repo = Repository::discover(&canonical).ok()?;
        let root = repo.workdir()?.canonicalize().ok()?;
        let relative = canonical.strip_prefix(root).ok()?;
        if relative.starts_with(".git") || cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let head = Head::read(&repo)?;
        let mut options = StatusOptions::new();
        options
            .include_ignored(true)
            .include_untracked(true)
            .recurse_untracked_dirs(false)
            .recurse_ignored_dirs(false)
            .update_index(false);
        // Keep a collapsed, wholly untracked or ignored ancestor in the result.
        // Restrict the scan to its top-level subtree when browsing below the root.
        if let Some(component) = relative.components().next() {
            let component = component.as_os_str();
            if !component
                .as_bytes()
                .iter()
                .any(|byte| b"*?[!\\".contains(byte))
            {
                options.pathspec(component).disable_pathspec_match(true);
            }
        }
        let statuses = repo.statuses(Some(&mut options)).ok()?;
        let mut entries = HashMap::new();
        let mut collapsed = None;
        for status in statuses.iter() {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let Some(kind) = FileStatus::from_git(status.status()) else {
                continue;
            };
            let path = Path::new(OsStr::from_bytes(status.path_bytes()));
            if let Some(child) = path
                .strip_prefix(relative)
                .ok()
                .and_then(|path| path.components().next())
            {
                entries
                    .entry(child.as_os_str().to_owned())
                    .and_modify(|old: &mut FileStatus| *old = (*old).max(kind))
                    .or_insert(kind);
            } else if kind != FileStatus::Modified && relative.starts_with(path) {
                collapsed = Some(kind);
            }
        }
        // libgit2 skips ignored descendants in a partly tracked ignored folder.
        // Fill in the visible level without walking the entire ignored subtree.
        let folder_ignored = !relative.as_os_str().is_empty()
            && repo.status_should_ignore(relative).unwrap_or(false);
        if collapsed.is_some() || folder_ignored {
            let index = repo.index().ok()?;
            for entry in canonical.read_dir().ok()?.filter_map(Result::ok) {
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                let name = entry.file_name();
                let path = relative.join(&name);
                if name == ".git"
                    || entries.contains_key(&name)
                    || index.get_path(&path, 0).is_some()
                {
                    continue;
                }
                if repo.status_should_ignore(&path).unwrap_or(folder_ignored) {
                    entries.insert(name, FileStatus::Ignored);
                } else if collapsed == Some(FileStatus::Untracked) {
                    let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    if !directory
                        || entry
                            .path()
                            .read_dir()
                            .is_ok_and(|mut iter| iter.next().is_some())
                    {
                        entries.insert(name, FileStatus::Untracked);
                    }
                }
            }
        }
        Some(Self {
            folder: folder.to_owned(),
            head,
            entries,
        })
    }
}

struct Scan {
    cancelled: Arc<AtomicBool>,
    result: mpsc::Receiver<Option<Snapshot>>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(super) struct Background {
    scope: Option<(u64, PathBuf)>,
    next_refresh: Option<Instant>,
    scan: Option<Scan>,
}

impl Background {
    pub(super) fn refresh(&mut self) {
        self.next_refresh = None;
        if let Some(scan) = &self.scan {
            scan.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn update(&mut self, ctx: &egui::Context, mut tab: Option<&mut Tab>, now: Instant) {
        let current = tab
            .as_ref()
            .and_then(|tab| Some((tab.id, tab.path.local()?)));
        if self.scope.as_ref().map(|(id, path)| (*id, path.as_path())) != current {
            self.scope = current.map(|(id, path)| (id, path.to_owned()));
            self.refresh();
            if let Some(tab) = &mut tab {
                tab.git_snapshot = None;
            }
        }

        if let Some(scan) = &self.scan {
            let snapshot = match scan.result.try_recv() {
                Ok(snapshot) => snapshot,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => None,
            };
            if !scan.cancelled.load(Ordering::Relaxed)
                && let Some(tab) = tab
            {
                let interval = if snapshot.is_some() {
                    REFRESH_INTERVAL
                } else {
                    DISCOVERY_INTERVAL
                };
                self.next_refresh = Some(now + interval);
                tab.git_snapshot = snapshot;
            }
            self.scan = None;
        }

        let Some((_, folder)) = &self.scope else {
            return;
        };
        if let Some(delay) = self
            .next_refresh
            .and_then(|due| due.checked_duration_since(now))
            && !delay.is_zero()
        {
            ctx.request_repaint_after(delay);
            return;
        }
        let folder = folder.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let (tx, result) = mpsc::channel();
        let worker_ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("pawcommander-git-status".into())
            .spawn(move || {
                let snapshot = if worker_cancelled.load(Ordering::Relaxed) {
                    None
                } else {
                    Snapshot::read(&folder, &worker_cancelled)
                };
                let _ = tx.send(snapshot);
                worker_ctx.request_repaint_of(egui::ViewportId::ROOT);
            }) {
            Ok(_) => self.scan = Some(Scan { cancelled, result }),
            Err(_) => {
                self.next_refresh = Some(now + DISCOVERY_INTERVAL);
                ctx.request_repaint_after(DISCOVERY_INTERVAL);
            }
        }
    }
}

#[cfg(test)]
mod tests;
