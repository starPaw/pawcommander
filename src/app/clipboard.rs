use super::actions::Action;
use super::PawCommander;
use crate::file_clipboard::FileList;
#[cfg(target_os = "macos")]
use crate::file_clipboard::{ClipboardReadError, ClipboardWriteError, PasteboardMove};
use crate::fs_ops::TransferKind;
use crate::vfs::Location;
use eframe::egui;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
type FileClipboardReadError = ClipboardReadError;
#[cfg(not(target_os = "macos"))]
type FileClipboardReadError = String;

pub(super) struct Clipboard {
    pub(super) paths: Vec<Location>,
    pub(super) cut: bool,
    pub(super) generation: u64,
    pub(super) origin: ClipboardOrigin,
}

pub(super) enum ClipboardOrigin {
    // Remember which system selection preceded the app-only copy. A newly copied system file
    // takes precedence, while an unrelated or empty text clipboard leaves SFTP copies usable.
    App(Option<FileList>),
    #[cfg(target_os = "macos")]
    AppWithPasteboardRevision(isize),
    System(FileList),
}

#[cfg(target_os = "macos")]
impl ClipboardOrigin {
    fn accepts_read_error(&self, error: &ClipboardReadError) -> bool {
        matches!(self, Self::AppWithPasteboardRevision(previous) if Some(*previous) == error.revision())
    }
}

#[derive(Clone)]
pub(super) struct ClipboardCut {
    pub(super) generation: u64,
    #[cfg(target_os = "macos")]
    pub(super) pasteboard: Option<Arc<PasteboardMove>>,
}

#[cfg(target_os = "macos")]
impl ClipboardCut {
    pub(super) fn pasteboard_revision(&self) -> Option<isize> {
        self.pasteboard.as_ref().map(|selection| selection.revision)
    }
}

impl Clipboard {
    pub(super) fn new(paths: Vec<Location>, cut: bool, origin: ClipboardOrigin) -> Self {
        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        Self {
            paths,
            cut,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            origin,
        }
    }

    pub(super) fn matches(&self, observed: Option<&FileList>) -> bool {
        match &self.origin {
            ClipboardOrigin::App(previous) => observed.is_none() || previous.as_ref() == observed,
            #[cfg(target_os = "macos")]
            ClipboardOrigin::AppWithPasteboardRevision(revision) => {
                observed.is_none_or(|files| files.pasteboard_revision() == Some(*revision))
            }
            ClipboardOrigin::System(previous) => Some(previous) == observed,
        }
    }
}

pub(super) fn file_clipboard_action(event: &egui::Event) -> Option<Action> {
    match event {
        egui::Event::Copy => Some(Action::Copy(false)),
        egui::Event::Cut => Some(Action::Copy(true)),
        // macOS receives presses directly; the other backends still intercept them.
        egui::Event::Key {
            key: egui::Key::V,
            modifiers,
            pressed,
            repeat: false,
            ..
        } if *pressed == cfg!(target_os = "macos")
            && modifiers.command
            && !modifiers.shift
            && (!modifiers.alt || cfg!(target_os = "macos")) =>
        {
            Some(Action::Paste(modifiers.alt))
        }
        #[cfg(target_os = "macos")]
        egui::Event::Key {
            key: egui::Key::Paste,
            pressed: true,
            repeat: false,
            ..
        } => Some(Action::Paste(false)),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
pub(super) fn macos_clipboard_events(events: &mut Vec<egui::Event>) {
    translate_macos_clipboard_events(events, crate::file_clipboard::SystemClipboard::read_text);
}

#[cfg(target_os = "macos")]
pub(super) fn translate_macos_clipboard_events(
    events: &mut Vec<egui::Event>,
    mut read_text: impl FnMut() -> Option<String>,
) {
    let mut translated = Vec::with_capacity(events.len());
    for event in std::mem::take(events) {
        if let egui::Event::Key {
            key,
            modifiers,
            pressed: true,
            ..
        } = event
        {
            if key == egui::Key::Copy || (key == egui::Key::C && modifiers.command_only()) {
                translated.push(egui::Event::Copy);
                continue;
            }
            if key == egui::Key::Cut || (key == egui::Key::X && modifiers.command_only()) {
                translated.push(egui::Event::Cut);
                continue;
            }
            if (key == egui::Key::Paste || (key == egui::Key::V && modifiers.command_only()))
                && let Some(text) = read_text().filter(|text| !text.is_empty())
            {
                translated.push(egui::Event::Paste(text.replace("\r\n", "\n")));
            }
        }
        // Keep V/Paste for file dispatch after egui has determined repeat and focus.
        // TextEdit consumes Paste; pane dispatch ignores it and uses this press.
        translated.push(event);
    }
    *events = translated;
}

impl PawCommander {
    fn read_file_clipboard(&mut self) -> Result<Option<FileList>, FileClipboardReadError> {
        #[cfg(target_os = "macos")]
        {
            let jobs = &self.jobs;
            self.system_clipboard
                .read(|revision| jobs.reserves_pasteboard_move(revision))
        }
        #[cfg(not(target_os = "macos"))]
        self.system_clipboard.read()
    }

    pub(super) fn capture_clipboard_cut(&self) -> Option<ClipboardCut> {
        let clipboard = self.clipboard.as_ref()?;
        #[cfg(target_os = "macos")]
        let pasteboard = match &clipboard.origin {
            ClipboardOrigin::System(files) => self.system_clipboard.move_snapshot(files),
            _ => None,
        };
        Some(ClipboardCut {
            generation: clipboard.generation,
            #[cfg(target_os = "macos")]
            pasteboard,
        })
    }

    pub(super) fn copy_files(&mut self, cut: bool) {
        let paths = self.selected();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let local_paths: Option<Vec<_>> = paths
            .iter()
            .map(|path| path.local().map(Path::to_path_buf))
            .collect();
        let write_error = if let Some(local_paths) = local_paths {
            match self.system_clipboard.write(&local_paths, cut) {
                Ok(()) => {
                    self.clipboard = None;
                    self.notice = Some((
                        crate::tr_format!("{} items in the system file clipboard", count),
                        false,
                    ));
                    return;
                }
                #[cfg(target_os = "macos")]
                Err(error @ ClipboardWriteError::Changed) => {
                    self.notice = Some((crate::i18n::clipboard_write_error(&error), true));
                    return;
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };
        let observed = self.read_file_clipboard();
        #[cfg(target_os = "macos")]
        let revision = match &observed {
            Ok(files) => files.as_ref().and_then(FileList::pasteboard_revision),
            Err(error) => error.revision(),
        };
        let origin = ClipboardOrigin::App(observed.ok().flatten());
        #[cfg(target_os = "macos")]
        let origin = revision.map_or(origin, ClipboardOrigin::AppWithPasteboardRevision);
        self.clipboard = Some(Clipboard::new(paths, cut, origin));
        #[cfg(target_os = "macos")]
        let write_error = write_error.as_ref().map(crate::i18n::clipboard_write_error);
        self.notice = Some(match write_error {
            Some(error) => (
                crate::tr_format!(
                    "System clipboard unavailable: {error}. Using the PawCommander clipboard.",
                    error = error
                ),
                true,
            ),
            None => (
                crate::tr_format!("{} items in the PawCommander clipboard (SFTP)", count),
                false,
            ),
        });
    }

    pub(super) fn paste_files(&mut self, move_here: bool, ctx: &egui::Context) {
        let observed = match self.read_file_clipboard() {
            Ok(files) => files,
            #[cfg(target_os = "macos")]
            Err(error)
                if self
                    .clipboard
                    .as_ref()
                    .is_some_and(|clipboard| clipboard.origin.accepts_read_error(&error)) =>
            {
                None
            }
            #[cfg(not(target_os = "macos"))]
            Err(_)
                if self.clipboard.as_ref().is_some_and(|clipboard| {
                    matches!(clipboard.origin, ClipboardOrigin::App(_))
                }) =>
            {
                None
            }
            Err(error) => {
                #[cfg(target_os = "macos")]
                let notice = crate::i18n::clipboard_read_error(&error);
                #[cfg(not(target_os = "macos"))]
                let notice = crate::tr_format!(
                    "Could not read system file clipboard: {error}",
                    error = error
                );
                self.notice = Some((notice, true));
                return;
            }
        };
        if !self
            .clipboard
            .as_ref()
            .is_some_and(|clipboard| clipboard.matches(observed.as_ref()))
        {
            self.clipboard = observed.map(|files| {
                let paths = files.paths.iter().cloned().map(Location::from).collect();
                Clipboard::new(paths, files.cut, ClipboardOrigin::System(files))
            });
        }
        let Some(clipboard) = self
            .clipboard
            .as_ref()
            .filter(|clipboard| !clipboard.paths.is_empty())
        else {
            self.notice = Some((
                crate::i18n::text!("В файловом буфере нет объектов для вставки").into(),
                false,
            ));
            return;
        };
        let cut = clipboard.cut || move_here;
        let kind = if cut {
            TransferKind::Move
        } else {
            TransferKind::Copy
        };
        let label = if cut {
            crate::i18n::text!("Перемещение файлов")
        } else {
            crate::i18n::text!("Копирование файлов")
        };
        self.start_transfer(
            ctx,
            label,
            clipboard.paths.clone(),
            self.tab().path.clone(),
            kind,
            cut,
        );
    }

    pub(super) fn consume_cut_source(
        &mut self,
        source: &Location,
        covered: &[PathBuf],
        cut: &ClipboardCut,
    ) {
        let clipboard = self
            .clipboard
            .as_mut()
            .filter(|clipboard| clipboard.generation == cut.generation);
        if let Some(source) = source.local() {
            let moved: HashSet<_> = std::iter::once(source)
                .chain(covered.iter().map(PathBuf::as_path))
                .collect();
            #[cfg(target_os = "macos")]
            if let Some(pasteboard) = &cut.pasteboard {
                self.system_clipboard.consume_moved(pasteboard, &moved);
            }
            if let Some(clipboard) = clipboard {
                clipboard
                    .paths
                    .retain(|path| path.local().is_none_or(|path| !moved.contains(path)));
            }
        } else if let Some(clipboard) = clipboard {
            clipboard.paths.retain(|path| {
                !path.same_backend(source) || !path.path().starts_with(source.path())
            });
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::file_clipboard::ClipboardReadFailure;

    #[test]
    fn app_cut_fallback_requires_the_same_pasteboard_revision() {
        let origin = ClipboardOrigin::AppWithPasteboardRevision(12);
        let unreadable = |revision| ClipboardReadError::Unreadable {
            revision,
            cause: ClipboardReadFailure::ItemsUnavailable,
        };

        assert!(origin.accepts_read_error(&unreadable(12)));
        assert!(!origin.accepts_read_error(&unreadable(13)));
        assert!(origin.accepts_read_error(&ClipboardReadError::MovePending { revision: 12 }));
        assert!(!origin.accepts_read_error(&ClipboardReadError::MovePending { revision: 13 }));
        assert!(!origin.accepts_read_error(&ClipboardReadError::Changed));
        assert!(!ClipboardOrigin::App(None).accepts_read_error(&unreadable(12)));
    }
}
