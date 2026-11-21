use super::file_name;
use super::state::SavedLocation;
use crate::shortcuts;
use crate::shortcuts::Shortcuts;
use crate::vfs::Location;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) static NEXT_BOOKMARK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bookmark {
    pub(super) id: u64,
    pub(super) location: Location,
    pub(super) name: String,
    pub(super) group: String,
    pub(super) shortcut: Option<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SavedBookmark {
    #[serde(default)]
    pub(super) id: u64,
    pub(super) location: SavedLocation,
    pub(super) name: String,
    pub(super) group: String,
    pub(super) shortcut: Option<u8>,
}

impl From<&Bookmark> for SavedBookmark {
    fn from(bookmark: &Bookmark) -> Self {
        Self {
            id: bookmark.id,
            location: SavedLocation::from(&bookmark.location),
            name: bookmark.name.clone(),
            group: bookmark.group.clone(),
            shortcut: bookmark.shortcut,
        }
    }
}

impl SavedBookmark {
    pub(super) fn into_bookmark(self) -> Option<Bookmark> {
        let location = self.location.into_location()?;
        Some(Bookmark {
            id: if self.id == 0 {
                next_bookmark_id()
            } else {
                self.id
            },
            name: if self.name.trim().is_empty() {
                file_name(&location)
            } else {
                self.name
            },
            location,
            group: self.group,
            shortcut: self.shortcut.filter(|slot| (1..=9).contains(slot)),
        })
    }
}

impl Bookmark {
    pub(super) fn new(location: Location) -> Self {
        Self {
            id: next_bookmark_id(),
            name: file_name(&location),
            location,
            group: String::new(),
            shortcut: None,
        }
    }
}

pub(super) fn next_bookmark_id() -> u64 {
    NEXT_BOOKMARK_ID.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn reserve_bookmark_ids(bookmarks: &[Bookmark]) {
    let next = bookmarks
        .iter()
        .map(|bookmark| bookmark.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    NEXT_BOOKMARK_ID.fetch_max(next, Ordering::Relaxed);
}

pub(super) struct BookmarkEditor {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) group: String,
    pub(super) focus_name: bool,
}

pub(super) fn apply_bookmark_edit(
    bookmarks: &mut [Bookmark],
    id: u64,
    name: &str,
    group: &str,
) -> bool {
    let Some(bookmark) = bookmarks.iter_mut().find(|bookmark| bookmark.id == id) else {
        return false;
    };
    bookmark.name = if name.trim().is_empty() {
        file_name(&bookmark.location)
    } else {
        name.trim().to_owned()
    };
    bookmark.group = group.trim().to_owned();
    true
}

impl BookmarkEditor {
    pub(super) fn new(bookmark: &Bookmark) -> Self {
        Self {
            id: bookmark.id,
            name: bookmark.name.clone(),
            group: bookmark.group.clone(),
            focus_name: true,
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, bookmarks: &mut [Bookmark]) -> bool {
        let response = egui::Modal::new(egui::Id::unique("bookmark_editor")).show(ctx, |ui| {
            ui.heading(crate::i18n::text!("Изменить избранное"));
            ui.label(crate::i18n::text!("Название"));
            let name = ui.text_edit_singleline(&mut self.name);
            if self.focus_name && !ui.is_sizing_pass() {
                name.request_focus();
                self.focus_name = false;
            }
            ui.label(crate::i18n::text!("Группа"));
            ui.text_edit_singleline(&mut self.group);
            let mut done = false;
            ui.horizontal(|ui| {
                if ui.button(crate::i18n::text!("Сохранить")).clicked() {
                    apply_bookmark_edit(bookmarks, self.id, &self.name, &self.group);
                    done = true;
                }
                if ui.button(crate::i18n::text!("Отмена")).clicked() {
                    done = true;
                }
            });
            done
        });
        response.inner || response.should_close()
    }
}

pub(super) fn bookmark_slot_key(slot: u8) -> Option<egui::Key> {
    match slot {
        1 => Some(egui::Key::Num1),
        2 => Some(egui::Key::Num2),
        3 => Some(egui::Key::Num3),
        4 => Some(egui::Key::Num4),
        5 => Some(egui::Key::Num5),
        6 => Some(egui::Key::Num6),
        7 => Some(egui::Key::Num7),
        8 => Some(egui::Key::Num8),
        9 => Some(egui::Key::Num9),
        _ => None,
    }
}

pub(super) fn assign_bookmark_shortcut(
    bookmarks: &mut [Bookmark],
    index: usize,
    slot: Option<u8>,
    shortcuts: &Shortcuts,
) -> bool {
    if index >= bookmarks.len() {
        return false;
    }
    if let Some(slot) = slot {
        let Some(key) = bookmark_slot_key(slot) else {
            return false;
        };
        let chord = shortcuts::chord(key, egui::Modifiers::ALT);
        if shortcuts.command(chord).is_some()
            || bookmarks
                .iter()
                .enumerate()
                .any(|(other, item)| other != index && item.shortcut == Some(slot))
        {
            return false;
        }
    }
    bookmarks[index].shortcut = slot;
    true
}

pub(super) fn move_bookmark(bookmarks: &mut [Bookmark], from: usize, to: usize) -> bool {
    if from >= bookmarks.len() || to >= bookmarks.len() || from == to {
        return false;
    }
    bookmarks.swap(from, to);
    true
}

pub(super) fn bookmark_peer(
    bookmarks: &[Bookmark],
    index: usize,
    direction: isize,
) -> Option<usize> {
    let bookmark = bookmarks.get(index)?;
    let mut next = index as isize + direction;
    while (0..bookmarks.len() as isize).contains(&next) {
        let candidate = &bookmarks[next as usize];
        if candidate.group.trim() == bookmark.group.trim() {
            return Some(next as usize);
        }
        next += direction;
    }
    None
}

pub(super) fn validate_bookmark_shortcuts(
    bookmarks: &mut [Bookmark],
    shortcuts: &Shortcuts,
) -> bool {
    let mut changed = false;
    let mut used = HashSet::new();
    for bookmark in bookmarks {
        let Some(slot) = bookmark.shortcut else {
            continue;
        };
        let chord = bookmark_slot_key(slot).map(|key| shortcuts::chord(key, egui::Modifiers::ALT));
        if chord.is_none_or(|chord| shortcuts.command(chord).is_some()) || !used.insert(slot) {
            bookmark.shortcut = None;
            changed = true;
        }
    }
    changed
}
