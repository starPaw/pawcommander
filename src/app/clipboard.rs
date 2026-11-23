use super::actions::Action;
use super::PawCommander;
use crate::file_clipboard::FileList;
#[cfg(target_os = "macos")]
use crate::file_clipboard::{ClipboardReadError, ClipboardWriteError};
use crate::fs_ops::TransferKind;
use crate::vfs::Location;
use eframe::egui;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    AppWithUnreadablePasteboard(isize),
    System(FileList),
}

#[cfg(target_os = "macos")]
impl ClipboardOrigin {
    fn accepts_unreadable(&self, error: &ClipboardReadError) -> bool {
        matches!(
            (self, error),
            (Self::AppWithUnreadablePasteboard(previous), ClipboardReadError::Unreadable { revision, .. })
                if previous == revision
        )
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
            ClipboardOrigin::AppWithUnreadablePasteboard(_) => observed.is_none(),
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
        let origin = match self.system_clipboard.read() {
            #[cfg(target_os = "macos")]
            Err(ClipboardReadError::Unreadable { revision, .. }) => {
                ClipboardOrigin::AppWithUnreadablePasteboard(revision)
            }
            observed => ClipboardOrigin::App(observed.ok().flatten()),
        };
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
        let observed = match self.system_clipboard.read() {
            Ok(files) => files,
            #[cfg(target_os = "macos")]
            Err(error)
                if self
                    .clipboard
                    .as_ref()
                    .is_some_and(|clipboard| clipboard.origin.accepts_unreadable(&error)) =>
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
                let error = crate::i18n::clipboard_read_error(&error);
                self.notice = Some((
                    crate::tr_format!(
                        "Could not read system file clipboard: {error}",
                        error = error
                    ),
                    true,
                ));
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
        generation: u64,
    ) {
        if let Some(clipboard) = self
            .clipboard
            .as_mut()
            .filter(|clipboard| clipboard.generation == generation)
        {
            if let Some(source) = source.local() {
                let moved: HashSet<_> = std::iter::once(source)
                    .chain(covered.iter().map(PathBuf::as_path))
                    .collect();
                #[cfg(target_os = "macos")]
                if let ClipboardOrigin::System(files) = &clipboard.origin {
                    self.system_clipboard.consume_moved(files, &moved);
                }
                clipboard
                    .paths
                    .retain(|path| path.local().is_none_or(|path| !moved.contains(path)));
            } else {
                clipboard.paths.retain(|path| {
                    !path.same_backend(source) || !path.path().starts_with(source.path())
                });
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::file_clipboard::ClipboardReadFailure;

    #[test]
    fn app_cut_fallback_requires_the_same_unreadable_pasteboard_revision() {
        let origin = ClipboardOrigin::AppWithUnreadablePasteboard(12);
        let unreadable = |revision| ClipboardReadError::Unreadable {
            revision,
            cause: ClipboardReadFailure::ItemsUnavailable,
        };

        assert!(origin.accepts_unreadable(&unreadable(12)));
        assert!(!origin.accepts_unreadable(&unreadable(13)));
        assert!(!origin.accepts_unreadable(&ClipboardReadError::Changed));
        assert!(!ClipboardOrigin::App(None).accepts_unreadable(&unreadable(12)));
    }
}
