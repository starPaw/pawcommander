use super::{
    entry_widget, fs_ops, list_notice, Action, ColumnNavigation, DirectoryEntries, DropTargets,
    EntryOptions, FileEntry, FolderView, Location, PaneOptions, Tab, COMPACT_FILE_ROW_HEIGHT,
};
use crate::{i18n::text, icons, theme::paint_focus};
use eframe::egui::{self, RichText, Vec2};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

const MAX_ANCESTORS: usize = 2;
const COLUMN_GAP: f32 = 6.0;
const HEADING_HEIGHT: f32 = 22.0;
const REMOTE_RETRY: Duration = Duration::from_secs(1);

type Listing = Result<Option<Vec<FileEntry>>, String>;

#[derive(Default)]
pub(super) struct Columns {
    tab: Option<u64>,
    archive_index: Option<Arc<crate::archive::Index>>,
    ancestors: Vec<Location>,
    listings: BTreeMap<Location, Result<DirectoryEntries, String>>,
    worker: Option<Read>,
    retry_at: Option<Instant>,
    scroll_geometry: Option<(Location, f32)>,
}

struct Read {
    location: Location,
    cancelled: Arc<AtomicBool>,
    result: mpsc::Receiver<Listing>,
}

impl Drop for Read {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Columns {
    pub(super) fn refresh(&mut self) {
        if let Some(read) = &self.worker {
            read.cancelled.store(true, Ordering::Relaxed);
        }
        self.listings.clear();
        self.retry_at = None;
    }

    fn sync(&mut self, current: Option<&Tab>) {
        let tab = current.map(|tab| tab.id);
        if self.tab != tab {
            self.refresh();
            self.scroll_geometry = None;
            self.tab = tab;
        }
        let archive_index = current.and_then(|tab| tab.archive_index.as_ref());
        let changed_index = match (&self.archive_index, archive_index) {
            (None, None) => false,
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            _ => true,
        };
        if changed_index {
            self.archive_index = archive_index.cloned();
            self.listings.retain(|location, _| !location.is_archive());
            if let Some(read) = self
                .worker
                .as_ref()
                .filter(|read| read.location.is_archive())
            {
                read.cancelled.store(true, Ordering::Relaxed);
            }
        }
        let mut ancestors = Vec::new();
        if let Some(tab) = current {
            ancestors.extend(tab.path.ancestors().skip(1).take(MAX_ANCESTORS));
            ancestors.reverse();
        }
        if self.ancestors != ancestors {
            self.listings
                .retain(|location, _| ancestors.contains(location));
            if let Some(read) = &self.worker {
                if !ancestors.contains(&read.location) {
                    read.cancelled.store(true, Ordering::Relaxed);
                }
            }
            self.retry_at = None;
            self.ancestors = ancestors;
        }
    }

    pub(super) fn update(
        &mut self,
        ctx: &egui::Context,
        current: Option<&Tab>,
        can_read: bool,
        now: Instant,
    ) {
        self.sync(current);
        if !can_read {
            return;
        }
        if let Some(read) = &self.worker {
            let result = match read.result.try_recv() {
                Ok(listing) => listing,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(text("Не удалось прочитать папку: фоновая задача завершилась").into())
                }
            };
            if !read.cancelled.load(Ordering::Relaxed) {
                let listing = match result {
                    Ok(None) => {
                        self.retry_at = Some(now + REMOTE_RETRY);
                        None
                    }
                    Ok(Some(files)) => {
                        let mut entries = DirectoryEntries::default();
                        entries.replace(files);
                        Some(Ok(entries))
                    }
                    Err(error) => Some(Err(error)),
                };
                if let Some(listing) = listing {
                    self.listings.insert(read.location.clone(), listing);
                }
            }
            self.worker = None;
        }
        if let Some(at) = self.retry_at.take() {
            if at > now {
                self.retry_at = Some(at);
                ctx.request_repaint_after(at - now);
                return;
            }
        }
        let Some(location) = self
            .ancestors
            .iter()
            .rev()
            .find(|location| {
                (!location.is_archive() || self.archive_index.is_some())
                    && !self.listings.contains_key(*location)
            })
            .cloned()
        else {
            return;
        };
        let (sender, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let source = location.clone();
        let archive_index = self
            .archive_index
            .as_ref()
            .filter(|_| source.is_archive())
            .cloned();
        let worker_ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("pawcommander-column-read".into())
            .spawn(move || {
                let cancelled = || worker_cancelled.load(Ordering::Relaxed);
                let listing = if let Some(index) = archive_index {
                    index.list(&source, &cancelled).map(Some)
                } else {
                    fs_ops::read_automatic_directory(&source, &cancelled)
                };
                let _ = sender.send(listing);
                worker_ctx.request_repaint();
            }) {
            Ok(_) => {
                self.worker = Some(Read {
                    location,
                    cancelled,
                    result,
                })
            }
            Err(error) => {
                self.listings.insert(location, Err(error.to_string()));
                ctx.request_repaint();
            }
        }
    }
}

fn column_width(available: f32, count: usize) -> f32 {
    let preferred = if count == 1 {
        available
    } else {
        (available - COLUMN_GAP) / 2.0
    };
    preferred.clamp(144.0, 240.0).min(available.max(0.0))
}

pub(super) fn show(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    state: &mut Columns,
    options: PaneOptions<'_>,
    height: f32,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
) -> usize {
    state.sync(Some(tab));
    let view = tab.folder_view(options.settings, options.index, options.folder_views);
    let available = ui.available_width();
    let count = state.ancestors.len() + 1;
    let width = column_width(available, count);
    let full_width = width * count as f32 + COLUMN_GAP * (count - 1) as f32;
    let mut scroll = egui::ScrollArea::horizontal()
        .id_salt(("folder_columns", tab.id))
        .min_scrolled_width(0.0)
        .max_height(height)
        .auto_shrink([false, false])
        .drag_to_scroll(false);
    let geometry = (tab.path.clone(), available);
    if state.scroll_geometry.as_ref() != Some(&geometry) || tab.scroll_to.is_some() {
        scroll = scroll.horizontal_scroll_offset((full_width - available).max(0.0));
        if !ui.is_sizing_pass() {
            state.scroll_geometry = Some(geometry);
        }
    }

    scroll
        .show(ui, |ui| {
            ui.set_width(full_width);
            ui.style_mut().always_scroll_the_only_direction = false;
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            ui.horizontal_top(|ui| {
                for (index, location) in state.ancestors.iter().enumerate() {
                    let child = state.ancestors.get(index + 1).unwrap_or(&tab.path);
                    column(ui, location, width, height, |ui| {
                        if !ui.clip_rect().is_positive() {
                            return;
                        }
                        if let Some(navigation) = show_ancestor(
                            ui,
                            tab,
                            location,
                            child,
                            state.listings.get(location),
                            options,
                            drop_targets,
                        ) {
                            actions.push(tab.column_action(options.index, navigation));
                        }
                    });
                }
                let location = tab.path.clone();
                column(ui, &location, width, height, |ui| {
                    if ui.clip_rect().is_positive() {
                        show_current(ui, tab, view, options, actions, drop_targets)
                    } else {
                        tab.visible(view).len()
                    }
                })
            })
            .inner
        })
        .inner
}

fn column<R>(
    ui: &mut egui::Ui,
    location: &Location,
    width: f32,
    height: f32,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.push_id(location, |ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                ui.set_height(height);
                ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
                content(ui)
            },
        )
        .inner
    })
    .inner
}

fn show_ancestor(
    ui: &mut egui::Ui,
    tab: &Tab,
    location: &Location,
    child: &Location,
    listing: Option<&Result<DirectoryEntries, String>>,
    options: PaneOptions<'_>,
    drop_targets: &mut DropTargets,
) -> Option<ColumnNavigation> {
    let pane = options.index;
    let ready = listing.is_some_and(Result::is_ok);
    drop_targets.add(ui, ui.max_rect(), pane, location, ready);
    let archive_child = child
        .archive_source()
        .filter(|_| location.local().is_some())
        .map(|file| Location::Local(file.to_owned()));
    let child = archive_child.as_ref().unwrap_or(child);
    let header = ui.add_sized(
        [ui.available_width(), HEADING_HEIGHT],
        egui::Button::new(RichText::new(location.name()).strong()).truncate(),
    );
    paint_focus(ui, &header, 4);
    let mut navigation = header
        .on_hover_text(location.display())
        .clicked()
        .then(|| ColumnNavigation::Reveal(child.clone()));

    let Some(listing) = listing else {
        column_notice(ui, text("Чтение папки…"));
        return navigation;
    };
    let entries = match listing {
        Ok(entries) => entries,
        Err(error) => {
            column_notice(ui, error);
            return navigation;
        }
    };
    let order = tab.legacy_order();
    let defaults = FolderView {
        hidden: options.settings.hidden,
        order,
        ..FolderView::default()
    };
    let view = options
        .folder_views
        .get(location)
        .map_or(defaults, |view| view.resolve(defaults));
    let visible = entries.visible(view.hidden, "", view.order.key, view.order.descending);
    if visible.is_empty() {
        column_notice(ui, text("Папка пуста"));
        return navigation;
    }
    let height = ui.available_height().max(0.0);
    let pitch = COMPACT_FILE_ROW_HEIGHT + ui.spacing().item_spacing.y;
    let mut scroll = egui::ScrollArea::vertical()
        .max_height(height)
        .min_scrolled_height(height)
        .auto_shrink([false, false])
        .id_salt("ancestor_files");
    // Recenter the selected branch only when it changes, leaving manual scrolling intact.
    let branch_id = ui.id().with("shown_branch");
    if ui
        .data(|data| data.get_temp::<Location>(branch_id))
        .as_ref()
        != Some(child)
    {
        if let Some(row) = visible
            .iter()
            .position(|&index| &entries[index].path == child)
        {
            let offset = row as f32 * pitch - (height - COMPACT_FILE_ROW_HEIGHT) / 2.0;
            let maximum = (visible.len() as f32 * pitch - height).max(0.0);
            scroll = scroll.vertical_scroll_offset(offset.clamp(0.0, maximum));
        }
        if !ui.is_sizing_pass() {
            ui.data_mut(|data| data.insert_temp(branch_id, child.clone()));
        }
    }
    let palette = crate::theme::Palette::from_ctx(ui.ctx());
    scroll.show_rows(ui, COMPACT_FILE_ROW_HEIGHT, visible.len(), |ui, range| {
        for &index in &visible[range] {
            let entry = &entries[index];
            let (_, rect) =
                ui.allocate_space(Vec2::new(ui.available_width(), COMPACT_FILE_ROW_HEIGHT));
            let response = ui.interact(rect, ui.id().with(&entry.path), egui::Sense::click());
            let selected = &entry.path == child;
            if selected || response.hovered() {
                ui.painter().rect_filled(
                    rect,
                    4,
                    if selected {
                        palette.selection
                    } else {
                        palette.raised
                    },
                );
            }
            let icon = icons::file_icon(&entry.name, entry.is_dir);
            let icon_rect = egui::Rect::from_center_size(
                rect.left_center() + Vec2::new(12.0, 0.0),
                Vec2::splat(16.0),
            );
            icons::paint(ui, icon_rect, icon, icons::tint(icon, palette));
            let clip = rect
                .with_min_x(rect.left() + 24.0)
                .with_max_x(rect.right() - 4.0);
            if clip.is_positive() {
                ui.painter().with_clip_rect(clip).text(
                    clip.left_center(),
                    egui::Align2::LEFT_CENTER,
                    &entry.name,
                    egui::FontId::proportional(13.0),
                    palette.ink,
                );
            }
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    selected,
                    &entry.name,
                )
            });
            paint_focus(ui, &response, 4);
            if entry.is_dir {
                drop_targets.add(ui, rect, pane, &entry.path, ready);
            }
            if response.on_hover_text(entry.path.display()).clicked() {
                navigation = Some(if entry.is_dir {
                    ColumnNavigation::Browse(entry.path.clone())
                } else {
                    ColumnNavigation::Reveal(entry.path.clone())
                });
            }
        }
    });
    navigation
}

fn column_notice(ui: &mut egui::Ui, message: &str) {
    ui.add(egui::Label::new(message).truncate())
        .on_hover_text(message);
}

fn show_current(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    view: FolderView,
    options: PaneOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
) -> usize {
    ui.add_sized(
        [ui.available_width(), HEADING_HEIGHT],
        egui::Label::new(RichText::new(tab.path.name()).strong()).truncate(),
    )
    .on_hover_text(tab.path.display());
    drop_targets.add(
        ui,
        ui.max_rect(),
        options.index,
        &tab.path,
        !tab.blocking_load() && tab.error.is_none(),
    );
    let visible = tab.visible(view);
    if tab.error.is_some() || tab.blocking_load() || visible.is_empty() {
        list_notice(ui, tab, visible.is_empty(), true);
    }

    let height = ui.available_height().max(0.0);
    let row_height = COMPACT_FILE_ROW_HEIGHT;
    let spacing = ui.spacing().item_spacing.y;
    let pitch = row_height + spacing;
    let mut scroll = egui::ScrollArea::vertical()
        .max_height(height)
        .min_scrolled_height(height)
        .auto_shrink([false, false])
        .id_salt("current_files");
    if !ui.is_sizing_pass() {
        let scope = ui.id();
        if tab
            .file_scroll_scope
            .replace(scope)
            .is_some_and(|previous| previous != scope)
        {
            tab.scroll_restore.get_or_insert(tab.scroll_offset);
        }
        tab.file_scroll_geometry = Some((1, pitch));
        let reveal = tab.scroll_to.take().and_then(|path| {
            visible
                .iter()
                .position(|&index| tab.entries[index].path == path)
        });
        let offset = if let Some(row) = reveal {
            tab.scroll_restore = None;
            Some(row as f32 * pitch)
        } else if tab.has_complete_listing {
            tab.scroll_restore.take()
        } else {
            None
        };
        if let Some(offset) = offset {
            let maximum = (visible.len() as f32 * pitch - spacing - height).max(0.0);
            scroll = scroll.vertical_scroll_offset(offset.min(maximum));
        }
    }
    let files_id = tab.file_list_id();
    let ctx = ui.ctx().clone();
    ctx.accesskit_node_builder(files_id, |node| {
        node.set_role(egui::accesskit::Role::List);
        node.set_label(format!("{}, {}", view.mode.label(), tab.path.display()));
    });
    let output = ctx.with_accessibility_parent(files_id, || {
        scroll.show_rows(ui, row_height, visible.len(), |ui, range| {
            for row in range {
                entry_widget(
                    ui,
                    tab,
                    row,
                    &visible,
                    EntryOptions {
                        columns: None,
                        pane: options.index,
                        split: options.settings.split,
                        grid: false,
                        folder_columns: true,
                        compact: true,
                        idle: options.idle,
                        zed_available: options.zed_available,
                    },
                    actions,
                    drop_targets,
                );
            }
        })
    });
    if !ui.is_sizing_pass() {
        let offset = output.state.offset.y;
        if offset > 0.0 && offset != tab.scroll_offset {
            tab.scroll_restore = None;
        }
        tab.scroll_offset = offset;
    }
    visible.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    #[test]
    fn folder_columns_drop_stale_workers_after_backend_or_tab_changes() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(directory.path().to_path_buf().into(), &ctx);
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 2022,
                user: "reader".into(),
                key: Some(PathBuf::from("key")),
            },
            path: PathBuf::from("/one/two/three"),
        };
        tab.path = remote.clone();
        let mut columns = Columns::default();
        columns.sync(Some(&tab));
        let root = remote.parent().unwrap().parent().unwrap();
        let parent = remote.parent().unwrap();
        assert_eq!(columns.ancestors, vec![root, parent.clone()]);

        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        columns.worker = Some(Read {
            location: parent,
            cancelled: Arc::clone(&cancelled),
            result: receiver,
        });
        tab.id += 1;
        columns.update(&ctx, Some(&tab), true, Instant::now());
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(
            columns.worker.is_some(),
            "cancelled reads keep their worker slot"
        );
        sender.send(Ok(Some(Vec::new()))).unwrap();
        columns.update(&ctx, None, true, Instant::now());
        assert!(columns.worker.is_none());
        assert!(columns.listings.is_empty());
        assert!(columns.ancestors.is_empty());
    }

    #[test]
    fn archive_columns_discard_ancestor_rows_when_the_tab_reloads() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(directory.path().to_path_buf().into(), &ctx);
        let archive = tab.path.join("sample.zip").browse_archive().unwrap();
        let parent = archive.join("outer");
        tab.path = parent.join("inner");
        tab.archive_index = Some(Arc::new(crate::archive::Index::default()));
        let mut columns = Columns::default();
        columns.sync(Some(&tab));
        columns
            .listings
            .insert(parent.clone(), Ok(DirectoryEntries::default()));

        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        columns.worker = Some(Read {
            location: parent,
            cancelled: Arc::clone(&cancelled),
            result: receiver,
        });
        tab.archive_index = None;
        sender.send(Ok(Some(Vec::new()))).unwrap();
        columns.update(&ctx, Some(&tab), true, Instant::now());
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(columns.listings.is_empty());
        assert!(
            columns.worker.is_none(),
            "ancestor reads wait for the tab index"
        );
    }
}
