use crate::i18n::text;
use crate::theme::paint_focus;
use crate::vfs::{self, Location};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

type Children = Result<Vec<Location>, crate::operations::TransferError>;

const ROW_HEIGHT: f32 = 23.0;
const MAX_HEIGHT: f32 = 208.0;

#[derive(Default)]
pub struct DirectoryTree {
    tab: Option<u64>,
    ancestry: Vec<Location>,
    expanded: HashSet<Location>,
    children: HashMap<Location, Children>,
    show_hidden: bool,
    rows: Option<TreeRows>,
    follow_current: bool,
    job: Option<BranchRead>,
}

struct BranchRead {
    location: Location,
    cancelled: Arc<AtomicBool>,
    result: mpsc::Receiver<Children>,
}

impl Drop for BranchRead {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct TreeRows {
    items: Vec<(usize, TreeRow)>,
    pending: Vec<Location>,
    current: Option<usize>,
}

enum TreeRow {
    Folder(Location),
    Loading,
    Error(Location),
}

enum TreeClick {
    Toggle(Location),
    Open(Location),
    Retry(Location),
}

impl DirectoryTree {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        tab: u64,
        current: &Location,
        show_hidden: bool,
        waiting_for_sftp: bool,
    ) -> Option<Location> {
        let mut title_clicked = false;
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            ui.make_persistent_id("directory_tree"),
            false,
        );
        let was_open = state.is_open();
        let mut header = state.show_header(ui, |ui| {
            let response = ui.selectable_label(was_open, text!("Дерево папок"));
            paint_focus(ui, &response, 4);
            title_clicked = response.clicked();
        });
        if title_clicked {
            header.toggle();
        }
        let open = header.is_open();
        let (toggle, _, body) = header.body_unindented(|ui| {
            if !open {
                return None;
            }
            self.sync(tab, current, show_hidden);
            let (pointer_down, using_rows) = ui.input(|input| {
                let using_rows = input.pointer.any_released()
                    || input.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::AccessKitActionRequest(_)
                                | egui::Event::Key {
                                    key: egui::Key::Tab | egui::Key::Enter | egui::Key::Space,
                                    pressed: true,
                                    ..
                                }
                        )
                    });
                (input.pointer.any_down(), using_rows)
            });
            // Handle input against the displayed rows before a completed read
            // can recenter the tree and scroll an input target out of view.
            if !pointer_down && self.job.is_some() {
                if using_rows {
                    ui.ctx().request_repaint();
                } else {
                    self.poll();
                }
            }
            let reveal = self.ensure_rows() && self.follow_current;
            let click = self.show_rows(ui, current, reveal);
            let mut navigate = None;
            match click {
                Some(TreeClick::Toggle(location)) => {
                    self.follow_current = false;
                    if self.expanded.contains(&location) {
                        let outside_branch =
                            |candidate: &Location| !candidate.path().starts_with(location.path());
                        self.expanded.retain(outside_branch);
                        self.children
                            .retain(|candidate, _| outside_branch(candidate));
                    } else {
                        self.expanded.insert(location);
                    }
                    self.rows = None;
                    ui.ctx().request_repaint();
                }
                Some(TreeClick::Open(location)) => navigate = Some(location),
                Some(TreeClick::Retry(location)) => {
                    self.children.remove(&location);
                    self.rows = None;
                    ui.ctx().request_repaint();
                }
                None => {}
            }
            self.ensure_rows();
            self.schedule(ui.ctx(), waiting_for_sftp);
            navigate
        });
        paint_focus(ui, &toggle, 4);
        if !open {
            self.inactive();
        }
        body.and_then(|body| body.inner)
    }

    pub fn inactive(&mut self) {
        self.tab = None;
        self.cancel();
        self.poll();
    }

    pub fn refresh(&mut self) {
        self.cancel();
        self.children.clear();
        self.rows = None;
    }

    fn sync(&mut self, tab: u64, current: &Location, show_hidden: bool) {
        if self.show_hidden != show_hidden {
            self.show_hidden = show_hidden;
            self.rows = None;
        }
        if self.tab == Some(tab) && self.ancestry.last() == Some(current) {
            return;
        }
        let mut ancestry: Vec<_> = current.ancestors().collect();
        ancestry.reverse();
        if ancestry.first() != self.ancestry.first() {
            self.refresh();
            self.expanded.clear();
        }
        self.expanded.extend(ancestry.iter().rev().skip(1).cloned());
        self.ancestry = ancestry;
        self.tab = Some(tab);
        self.follow_current = true;
        self.rows = None;
    }

    fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let result = match job.result.try_recv() {
            Ok(children) => children,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(text!("Не удалось прочитать папку: фоновая задача завершилась").into())
            }
        };
        if !job.cancelled.load(Ordering::Relaxed) {
            self.children.insert(job.location.clone(), result);
            self.rows = None;
        }
        self.job = None;
    }

    fn schedule(&mut self, ctx: &egui::Context, waiting_for_sftp: bool) {
        let pending = &self.rows.as_ref().expect("tree rows").pending;
        if let Some(job) = &self.job {
            if !pending.contains(&job.location) {
                self.cancel();
            }
            // A cancelled worker may still be inside a filesystem or SFTP call.
            // Keep its slot until the result or disconnect arrives.
            return;
        }
        let Some(location) = pending.first().filter(|_| !waiting_for_sftp).cloned() else {
            return;
        };
        let (sender, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let branch = location.clone();
        let worker_ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("pawcommander-folder-tree".into())
            .spawn(move || {
                let children = read_children(&branch, &|| worker_cancelled.load(Ordering::Relaxed));
                let _ = sender.send(children);
                worker_ctx.request_repaint();
            }) {
            Ok(_) => {
                self.job = Some(BranchRead {
                    location,
                    cancelled,
                    result,
                });
            }
            Err(error) => {
                self.children.insert(location, Err(error.into()));
                self.rows = None;
                ctx.request_repaint();
            }
        }
    }

    fn ensure_rows(&mut self) -> bool {
        if self.rows.is_some() {
            return false;
        }
        self.rows = Some(self.build_rows());
        true
    }

    fn build_rows(&self) -> TreeRows {
        let mut rows = TreeRows::default();
        let Some(root) = self.ancestry.first() else {
            return rows;
        };
        let mut stack = vec![(root, 0, true)];
        while let Some((location, depth, on_path)) = stack.pop() {
            if self.ancestry.last() == Some(location) {
                rows.current = Some(rows.items.len());
            }
            rows.items.push((depth, TreeRow::Folder(location.clone())));
            if !self.expanded.contains(location) {
                continue;
            }

            let next_depth = depth + 1;
            let active_child = self.ancestry.get(next_depth).filter(|_| on_path);
            let mut included_active_child = false;
            match self.children.get(location) {
                Some(Ok(children)) => {
                    for child in children.iter().rev() {
                        let on_path = active_child == Some(child);
                        if on_path || self.show_hidden || !child.name().starts_with('.') {
                            stack.push((child, next_depth, on_path));
                            included_active_child |= on_path;
                        }
                    }
                }
                Some(Err(_)) => rows
                    .items
                    .push((next_depth, TreeRow::Error(location.clone()))),
                None => {
                    rows.items.push((next_depth, TreeRow::Loading));
                    rows.pending.push(location.clone());
                }
            }
            // Retain the active pane's path while listings are pending or inaccessible,
            // and when the path crosses a directory alias omitted by a listing.
            if let Some(child) = active_child.filter(|_| !included_active_child) {
                stack.push((child, next_depth, true));
            }
        }
        rows
    }

    fn show_rows(
        &mut self,
        ui: &mut egui::Ui,
        current: &Location,
        reveal: bool,
    ) -> Option<TreeClick> {
        let rows = self.rows.as_ref().expect("tree rows");
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("folder_tree_scroll")
            .max_height(MAX_HEIGHT)
            .auto_shrink([false, true]);
        let original_style = if let Some(index) = rows.current.filter(|_| reveal) {
            let pitch = ROW_HEIGHT + ui.spacing().item_spacing.y;
            scroll =
                scroll.vertical_scroll_offset((index as f32 * pitch - MAX_HEIGHT / 2.0).max(0.0));
            // Rubber-banding uses the previous frame's scroll limits. A branch
            // may have made the new offset valid only in this frame.
            let original_style = ui.style().clone();
            ui.spacing_mut().scroll.kinetic.rubber_band = false;
            Some(original_style)
        } else {
            None
        };
        let output = scroll.show_rows(ui, ROW_HEIGHT, rows.items.len(), |ui, range| {
            ui.spacing_mut().interact_size.y = ROW_HEIGHT;
            let mut click = None;
            for (depth, row) in &rows.items[range] {
                ui.horizontal(|ui| {
                    let indent = (ui.available_width() - 74.0).max(0.0);
                    ui.add_space((*depth as f32 * 10.0).min(indent));
                    match row {
                        TreeRow::Folder(location) => {
                            let expanded = self.expanded.contains(location);
                            let hint = if expanded {
                                crate::i18n::text!("Свернуть папку")
                            } else {
                                crate::i18n::text!("Раскрыть папку")
                            };
                            let label = location.name();
                            let disclosure = tree_button(
                                ui,
                                21.0,
                                ("disclosure", location),
                                if expanded { "▾" } else { "▸" },
                                &format!("{hint}: {label}"),
                                None,
                            );
                            if disclosure.on_hover_text(hint).clicked() {
                                click = Some(TreeClick::Toggle(location.clone()));
                            }

                            let selected = location == current;
                            let response = tree_button(
                                ui,
                                ui.available_width(),
                                ("folder", location),
                                &label,
                                &label,
                                Some(selected),
                            );
                            if response.on_hover_text(location.display()).clicked() && !selected {
                                click = Some(TreeClick::Open(location.clone()));
                            }
                        }
                        TreeRow::Loading => {
                            ui.add_sized(
                                [ui.available_width().max(0.0), ROW_HEIGHT],
                                egui::Label::new(text!("Чтение вложенных папок…")).truncate(),
                            );
                        }
                        TreeRow::Error(location) => {
                            let error = self.children[location].as_ref().unwrap_err();
                            let retry = text!("Повторить чтение папки");
                            let response = tree_button(
                                ui,
                                ui.available_width(),
                                ("retry", location),
                                retry,
                                retry,
                                None,
                            );
                            if response
                                .on_hover_text(crate::i18n::file_error(error))
                                .clicked()
                            {
                                click = Some(TreeClick::Retry(location.clone()));
                            }
                        }
                    }
                });
            }
            click
        });
        if let Some(style) = original_style {
            ui.set_style(style);
        }
        if ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|pointer| output.inner_rect.contains(pointer))
                && input.events.iter().any(|event| matches!(event, egui::Event::MouseWheel { delta, .. } if *delta != egui::Vec2::ZERO))
        }) {
            self.follow_current = false;
        }
        output.inner
    }
}

fn tree_button(
    ui: &mut egui::Ui,
    width: f32,
    key: impl Hash + std::fmt::Debug,
    label: &str,
    accessible_label: &str,
    selected: Option<bool>,
) -> egui::Response {
    let (_, rect) = ui.allocate_space(egui::vec2(width.max(0.0), ROW_HEIGHT));
    // The same path keeps its control ID when an adjacent branch inserts rows.
    let response = ui.interact(rect, ui.make_persistent_id(key), egui::Sense::click());
    response.widget_info(|| match selected {
        Some(selected) => egui::WidgetInfo::selected(
            egui::Role::Button,
            ui.is_enabled(),
            selected,
            accessible_label,
        ),
        None => egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), accessible_label),
    });
    if response.gained_focus() {
        response.scroll_to_me(None);
    }
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let selected = selected == Some(true);
        let color = if selected {
            let selection = ui.visuals().selection;
            ui.painter().rect(
                rect,
                4,
                selection.bg_fill,
                selection.stroke,
                egui::StrokeKind::Inside,
            );
            selection.stroke.color
        } else {
            if response.hovered() {
                ui.painter().rect_filled(rect, 4, visuals.weak_bg_fill);
            }
            visuals.text_color()
        };
        let galley = egui::WidgetText::from(label).into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            (rect.width() - 10.0).max(0.0),
            egui::TextStyle::Button,
        );
        let pos = rect.left_center() + egui::vec2(5.0, -galley.size().y / 2.0);
        ui.painter().galley(pos, galley, color);
        paint_focus(ui, &response, 4);
    }
    response
}

fn read_children(location: &Location, cancelled: &impl Fn() -> bool) -> Children {
    let mut children = Vec::new();
    vfs::visit_connected_directory(location, cancelled, |entry| {
        if entry.is_dir && !entry.is_symlink {
            children.push(entry.path);
        }
    })?;
    if !cancelled() {
        children.sort_by_cached_key(|child| {
            let name = child.name();
            (name.to_lowercase(), name)
        });
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    fn remote_root() -> Location {
        Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 22,
                user: "test".into(),
                key: Some(PathBuf::from("key")),
                jump: None,
            },
            path: "/".into(),
        }
    }

    #[test]
    fn tree_reads_only_open_branches_and_reveals_the_active_folder() {
        let root = remote_root();
        let parent = root.join(".hidden");
        let active = parent.join("current");
        let sibling = root.join("sibling");
        let mut tree = DirectoryTree::default();

        tree.sync(1, &root, false);
        assert!(tree.build_rows().pending.is_empty());
        tree.sync(2, &active, false);
        let rows = tree.build_rows();
        assert_eq!(rows.pending, [root.clone(), parent.clone()]);
        assert!(matches!(
            &rows.items[rows.current.unwrap()],
            (2, TreeRow::Folder(location)) if *location == active
        ));

        tree.children
            .insert(root.clone(), Ok(vec![sibling.clone()]));
        tree.children
            .insert(parent.clone(), Err("inaccessible".into()));
        let rows = tree.build_rows();
        assert!(rows.pending.is_empty());
        assert!(matches!(
            &rows.items[rows.current.unwrap()],
            (2, TreeRow::Folder(location)) if *location == active
        ));
        assert!(rows.items.iter().any(
            |(depth, item)| matches!(item, TreeRow::Folder(location) if *depth == 1 && *location == sibling)
        ));

        tree.expanded.remove(&root);
        tree.sync(2, &active, false);
        assert!(tree.build_rows().current.is_none());
        tree.sync(3, &active, false);
        assert!(tree.build_rows().current.is_some());
    }

    #[test]
    fn closing_or_switching_roots_cancels_without_reusing_the_worker_slot_early() {
        let root = remote_root();
        let parent = root.join("parent");
        let active = parent.join("active");
        let mut tree = DirectoryTree::default();
        tree.sync(1, &active, false);
        let (sender, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        tree.job = Some(BranchRead {
            location: parent.clone(),
            cancelled: cancelled.clone(),
            result,
        });
        tree.expanded.remove(&root);
        tree.ensure_rows();
        tree.schedule(&egui::Context::default(), false);
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(tree.job.is_some());

        let Location::Sftp { mut endpoint, path } = root else {
            unreachable!()
        };
        endpoint.key = Some("another key".into());
        let other_root = Location::Sftp { endpoint, path };
        tree.sync(2, &other_root.join("next"), false);
        assert_eq!(tree.build_rows().pending, [other_root]);
        tree.poll();
        assert!(tree.job.is_some());
        sender.send(Ok(vec![active])).unwrap();
        tree.poll();
        assert!(tree.job.is_none());
        assert!(!tree.children.contains_key(&parent));
    }

    #[test]
    fn tree_activates_a_visible_folder_before_applying_a_completed_branch() {
        use crate::test_ui::ContextExt as _;
        use egui::accesskit::{Action, ActionRequest, TreeId};

        let root = remote_root();
        let parent = root.join("Parent");
        let current = parent.join("Current");
        let mut tree = DirectoryTree::default();
        tree.sync(1, &current, false);
        tree.children.insert(root, Ok(vec![parent.clone()]));

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let render = |tree: &mut DirectoryTree, events| {
            let mut navigation = None;
            let output = ctx.run_test_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        ui.make_persistent_id("directory_tree"),
                        true,
                    )
                    .store(ui.ctx());
                    if let Some(location) = tree.show(ui, 1, &current, false, true) {
                        navigation = Some(location);
                    }
                },
            );
            (navigation, output.platform_output.accesskit_update.unwrap())
        };
        let (_, initial) = render(&mut tree, vec![]);
        let target = initial
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Parent"))
            .map(|(id, _)| *id)
            .unwrap();

        let (sender, result) = mpsc::channel();
        tree.job = Some(BranchRead {
            location: parent.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            result,
        });
        // The newly visible siblings push the parent out of view when the tree
        // centers the active folder again.
        let children = (0..40)
            .map(|index| parent.join(format!("A folder {index:02}")))
            .chain(std::iter::once(current.clone()))
            .collect();
        sender.send(Ok(children)).unwrap();

        let (navigation, _) = render(
            &mut tree,
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: Action::Click,
                target_tree: TreeId::ROOT,
                target_node: target,
                data: None,
            })],
        );
        assert_eq!(navigation, Some(parent.clone()));
        assert!(tree.job.is_some());

        render(&mut tree, vec![]);
        assert!(tree.job.is_none());
        assert_eq!(tree.children[&parent].as_ref().unwrap().len(), 41);
    }

    #[test]
    fn growing_a_branch_keeps_the_current_folder_in_view() {
        use crate::test_ui::ContextExt as _;

        let root = remote_root();
        let parent = root.join("Parent");
        let current = parent.join("Current");
        let mut tree = DirectoryTree::default();
        tree.sync(1, &current, false);
        let root_children = std::iter::once(parent.clone())
            .chain((0..12).map(|index| root.join(format!("Sibling {index:02}"))))
            .collect();
        tree.children.insert(root, Ok(root_children));
        tree.ensure_rows();

        let ctx = egui::Context::default();
        let mut time = 0.0;
        let mut current_row_top = |tree: &mut DirectoryTree, reveal| {
            time += 0.1;
            let mut top = 0.0;
            ctx.run_test_ui(
                egui::RawInput {
                    time: Some(time),
                    ..Default::default()
                },
                |ui| {
                    let scroll_id = ui.make_persistent_id(egui::IdSalt::new("folder_tree_scroll"));
                    tree.show_rows(ui, &current, reveal);
                    let offset = egui::scroll_area::State::load(ui.ctx(), scroll_id)
                        .unwrap()
                        .unclamped_offset()
                        .y;
                    let index = tree.rows.as_ref().unwrap().current.unwrap();
                    top = index as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y) - offset;
                },
            );
            top
        };
        let visible_range = 0.0..=MAX_HEIGHT - ROW_HEIGHT;
        assert!(visible_range.contains(&current_row_top(&mut tree, true)));

        let children = (0..40)
            .map(|index| parent.join(format!("A folder {index:02}")))
            .chain(std::iter::once(current.clone()))
            .collect();
        tree.children.insert(parent, Ok(children));
        tree.rows = None;
        tree.ensure_rows();

        let revealed = current_row_top(&mut tree, true);
        let next_frame = current_row_top(&mut tree, false);
        assert!(
            visible_range.contains(&revealed) && visible_range.contains(&next_frame),
            "current row left the viewport after expansion: {revealed}, {next_frame}"
        );
    }

    #[test]
    fn local_branch_reader_lists_immediate_directories_with_exact_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let root = Location::Local(fixture.path().into());
        let folder = root.join("A folder");
        let hidden = root.join(".hidden");
        std::fs::create_dir_all(folder.join("grandchild").path()).unwrap();
        std::fs::create_dir(hidden.path()).unwrap();
        std::fs::write(root.join("file.txt").path(), b"example").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), root.join("loop").path()).unwrap();
        assert_eq!(
            read_children(&root, &|| false).unwrap(),
            vec![hidden, folder]
        );
        assert!(read_children(&root, &|| true).unwrap().is_empty());
    }
}
