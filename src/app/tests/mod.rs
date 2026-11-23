use super::actions::{Action, ColumnNavigation, SelectionChange};
use super::address_bar::address_bar;
use super::bookmarks::{
    apply_bookmark_edit, assign_bookmark_shortcut, bookmark_peer, move_bookmark,
    validate_bookmark_shortcuts, Bookmark, SavedBookmark,
};
use super::clipboard::{Clipboard, ClipboardOrigin};
use super::dialogs::{Dialog, SelectionScan, SftpForm};
use super::directory::{
    send_progressive_listing, DirectoryLoad, DirectoryLoadMode, DirectoryRead,
    AUTO_REFRESH_INTERVAL, SFTP_REFRESH_INTERVAL,
};
use super::entry_widget::{entry_widget, EntryOptions, COMPACT_FILE_ROW_HEIGHT};
use super::footer::operation_details;
use super::jobs::{
    wait_for_conflict_answer, Job, JobCompletion, TransferEvent, TransferInteraction,
    MAX_PENDING_JOBS,
};
use super::pane_view::{render_pane, PaneOptions};
use super::selection::RuleKind;
use super::state::{
    Density, FileOrder, FolderMode, FolderView, FolderViewChange, FolderViewOverrides, FolderViews,
    NamedWorkspace, PaneLayout, PaneOrientation, SavedLocation, Settings, Sort,
};
use super::tab_strip::tab_strip;
use super::tabs::{move_tab, Pane, Tab};
use super::PawCommander;
use crate::conflict::{Answer, Decision};
use crate::drag_drop::DropTargets;
use crate::file_clipboard::FileList;
use crate::fs_ops::TransferKind;
use crate::operations::Progress;
use crate::result_history::History;
use crate::search::panel::{ReadyAction, TabTarget};
use crate::shortcuts::{Command, ShortcutEditor, Shortcuts};
use crate::tab_drag::{TabDrop, TabDropTargets};
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::theme::Theme;
use crate::vfs::{Endpoint, FileEntry, Location};
use crate::{shortcuts, vfs as fs_ops};
use eframe::egui;
use eframe::egui::Vec2;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

mod accessibility;
mod address;
mod bookmarks;
mod clipboard;
#[cfg(target_os = "macos")]
mod custom_actions;
mod directory;
mod drag_drop;
mod jobs;
mod keyboard;
mod layout;
mod lifecycle;
mod navigation;
mod recovery;
mod search;
mod selection;
mod sessions;
mod sftp;
mod tabs;
mod undo;

pub(super) fn wait_for_directory(tab: &mut Tab) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while tab.loading.is_some() && Instant::now() < deadline {
        tab.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(tab.loading.is_none(), "directory read did not finish");
}

#[derive(Default)]
struct MemoryStorage(std::collections::HashMap<String, String>);
impl eframe::Storage for MemoryStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.into(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn operation_footer_frame(
    app: &mut PawCommander,
    ctx: &egui::Context,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    ctx.run_test_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 600.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            let ctx = &ui.ctx().clone();
            app.transfer_stop_shortcut(ctx);
            app.footer(ui);
        },
    )
}

fn operation_footer_text(output: &egui::FullOutput) -> String {
    output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hold_queue(app: &mut PawCommander) -> mpsc::Sender<JobCompletion> {
    let (send, receive) = mpsc::channel();
    let (_, progress) = Progress::tracked();
    app.jobs.current = Some(Job {
        transfer: None,
        cut: None,
        label: "held operation".into(),
        started: Instant::now(),
        result: receive,
        undo: None,
        progress,
        #[cfg(target_os = "macos")]
        remote_save: false,
        #[cfg(target_os = "macos")]
        export_target: None,
    });
    send
}

fn poll_queue_until(
    app: &mut PawCommander,
    ctx: &egui::Context,
    condition: impl Fn(&PawCommander) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition(app) && Instant::now() < deadline {
        eframe::App::logic(app, ctx, &mut eframe::Frame::_new_kittest());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(condition(app), "queue did not reach expected state");
}

fn answer_queue_conflict(app: &mut PawCommander, ctx: &egui::Context, label: &str) {
    let mut draw = |events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let ctx = &ui.ctx().clone();
                let transfer = app.jobs.transfer_mut().unwrap();
                if transfer.conflict.as_mut().unwrap().show(ctx) {
                    transfer.conflict = None;
                }
            },
        )
    };
    draw(vec![]);
    let output = draw(vec![]);
    let point = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("conflict decision button");
    for pressed in [true, false] {
        draw(vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
}

fn visible_rows(tab: &Tab, hidden: bool) -> Rc<Vec<usize>> {
    tab.visible(FolderView {
        hidden,
        order: tab.legacy_order(),
        ..Default::default()
    })
}
