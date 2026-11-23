mod map;

use super::{
    comparison::{show_progress_with_stop, ComparisonBody, ComparisonDialog, Phase},
    footer::human_size,
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    vfs::{
        usage::{self, Analysis, Coverage, Finish, Issue, Kind, Node, NodeId},
        Location,
    },
};
use eframe::egui;
use std::collections::HashSet;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum View {
    #[default]
    Tree,
    Files,
    Map,
}

pub(super) struct DiskUsage {
    folder: Location,
    phase: Phase<Analysis>,
    view: View,
    expanded: HashSet<NodeId>,
    tree: Vec<(NodeId, usize)>,
    map_folder: NodeId,
    run: u64,
    focus_start: bool,
}

impl DiskUsage {
    fn new(folder: Location) -> Self {
        Self {
            folder,
            phase: Phase::Ready,
            view: View::Tree,
            expanded: HashSet::from([0]),
            tree: Vec::new(),
            map_folder: 0,
            run: 0,
            focus_start: true,
        }
    }

    pub(super) fn poll(&mut self) {
        let running = matches!(self.phase, Phase::Running(_));
        self.phase.poll();
        if running && let Phase::Finished(analysis) = &self.phase {
            self.tree = tree_rows(analysis, &self.expanded);
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> bool {
        let mut start = false;
        let mut stop = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("disk_usage")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(230.0, 760.0));
            ui.heading(Command::DiskUsage.label());
            let folder = self.folder.display().escape_debug().to_string();
            ui.add(egui::Label::new(&folder).truncate().selectable(true))
                .on_hover_text(&folder);
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => {
                    show_progress_with_stop(
                        ui,
                        worker,
                        crate::i18n::key!("Проверяем занятое место…"),
                        crate::i18n::key!("Остановка…"),
                    );
                }
                Phase::Failed(error) => {
                    let message = egui::RichText::new(error.message(Command::DiskUsage.label()))
                        .color(ui.visuals().error_fg_color);
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                }
                Phase::Stopped => {
                    ui.label(text!("Анализ места остановлен"));
                }
                Phase::Finished(analysis) => {
                    let summary_top = ui.next_widget_position().y;
                    show_summary(ui, analysis, self.run);
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut self.view, View::Tree, text!("Дерево размеров"));
                        ui.selectable_value(&mut self.view, View::Files, text!("Крупнейшие файлы"));
                        ui.selectable_value(&mut self.view, View::Map, text!("Карта места"));
                    });
                    let controls_height = ui.next_widget_position().y - summary_top;
                    let height =
                        (ctx.content_rect().height() * 0.64 - controls_height).clamp(80.0, 420.0);
                    match self.view {
                        View::Tree => {
                            if let Some(id) = show_tree(
                                ui,
                                analysis,
                                &self.tree,
                                &self.expanded,
                                self.run,
                                height,
                            ) {
                                if !self.expanded.remove(&id) {
                                    self.expanded.insert(id);
                                }
                                self.tree = tree_rows(analysis, &self.expanded);
                            }
                        }
                        View::Files => show_files(ui, analysis, self.run, height),
                        View::Map => {
                            self.map_folder =
                                map::show(ui, analysis, self.map_folder, self.run, height)
                        }
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Phase::Running(worker) = &self.phase {
                    stop = ui
                        .add_enabled(
                            !worker.progress.cancel.is_requested(),
                            egui::Button::new(text!("Остановить")),
                        )
                        .clicked();
                } else {
                    let response = ui.button(text!("Начать анализ"));
                    if self.focus_start && !ui.is_sizing_pass() {
                        response.request_focus();
                        self.focus_start = false;
                    }
                    start = response.clicked();
                    close = ui.button(text!("Закрыть")).clicked();
                }
            });
        });
        if let Phase::Running(worker) = &self.phase {
            if stop || modal.should_close() {
                worker.progress.cancel.request();
            }
        } else if close || modal.should_close() {
            return false;
        } else if start {
            self.run = self.run.wrapping_add(1);
            self.expanded = HashSet::from([0]);
            self.map_folder = 0;
            let folder = self.folder.clone();
            self.phase = Phase::start(ctx, move |progress| usage::analyze(&folder, progress));
        }
        true
    }
}

fn show_summary(ui: &mut egui::Ui, analysis: &Analysis, run: u64) {
    let size = human_size(analysis.nodes[0].allocated);
    let total = match analysis.nodes[0].coverage {
        Coverage::Complete => crate::tr_format!("Allocated space counted: {size}", size = size),
        Coverage::Partial => crate::tr_format!("Partial allocated space: {size}", size = size),
        Coverage::Unavailable => text!("Не удалось учесть ни одного объекта").into(),
    };
    ui.strong(total);
    let status = match &analysis.finish {
        Finish::Stopped => Some(text!("Анализ места остановлен").into()),
        Finish::Limit(limit) => Some(crate::i18n::disk_usage_limit(limit)),
        Finish::VolumeUnverified(error) => Some(crate::tr_format!(
            "Disk usage analysis stopped. {error}",
            error = crate::i18n::file_error(error)
        )),
        Finish::Complete => None,
    };
    if status.is_some() || analysis.root_verification.is_some() {
        egui::ScrollArea::vertical()
            .id_salt(("disk-usage-status", run))
            .max_height(96.0)
            .show(ui, |ui| {
                if let Some(status) = status {
                    warning(ui, status);
                }
                if let Some(error) = &analysis.root_verification {
                    warning(
                        ui,
                        crate::tr_format!(
                            "Could not verify the scanned folder. {error}",
                            error = crate::i18n::file_error(error)
                        ),
                    );
                }
            });
    }
    ui.label(crate::tr_format!(
        "Files counted: {} · symbolic links skipped: {} · extra hard links excluded: {}",
        analysis.files.len(),
        analysis.symlinks,
        analysis.hard_links,
    ));
    ui.small(text!(
        "Размеры по выделенным блокам. Клоны APFS могут использовать общие блоки.",
    ));
    if analysis.issues > 0 {
        show_issues(ui, analysis, run);
    }
}

fn warning(ui: &mut egui::Ui, message: String) {
    let message = egui::RichText::new(message).color(ui.visuals().warn_fg_color);
    ui.add(egui::Label::new(message).wrap().selectable(true));
}

fn show_issues(ui: &mut egui::Ui, analysis: &Analysis, run: u64) {
    let count = analysis.issues;
    egui::CollapsingHeader::new(crate::tr_format!(
        "Items with scan errors: {count}",
        count = count
    ))
    .id_salt(("disk-usage-errors", run))
    .show(ui, |ui| {
        let shown = analysis.issue_examples.len();
        if shown < count {
            ui.weak(crate::tr_format!(
                "Showing {shown} of {count} scan errors.",
                shown = shown,
                count = count
            ));
        }
        egui::ScrollArea::vertical()
            .id_salt(("disk-usage-error-details", run))
            .max_height(100.0)
            .show(ui, |ui| {
                for &id in &analysis.issue_examples {
                    if let Some(Issue::Failure(error)) = &analysis.nodes[id].issue {
                        let message = crate::i18n::file_error(error);
                        ui.add(egui::Label::new(message).wrap().selectable(true));
                    }
                }
            });
    });
}

fn tree_rows(analysis: &Analysis, expanded: &HashSet<NodeId>) -> Vec<(NodeId, usize)> {
    let mut pending = vec![(0, 0)];
    let mut rows = Vec::new();
    while let Some((id, depth)) = pending.pop() {
        rows.push((id, depth));
        if expanded.contains(&id) {
            pending.extend(
                analysis.nodes[id]
                    .children
                    .iter()
                    .rev()
                    .map(|&child| (child, depth + 1)),
            );
        }
    }
    rows
}

fn show_tree(
    ui: &mut egui::Ui,
    analysis: &Analysis,
    rows: &[(NodeId, usize)],
    expanded: &HashSet<NodeId>,
    run: u64,
    height: f32,
) -> Option<NodeId> {
    let mut toggle = None;
    let row_height = row_height(ui);
    egui::ScrollArea::both()
        .id_salt(("disk-usage-tree", run))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            for &(id, depth) in &rows[range] {
                let node = &analysis.nodes[id];
                ui.push_id(id, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(depth.min(32) as f32 * 12.0);
                        let label = node_name(analysis, node);
                        if node.kind == Kind::Folder && !node.children.is_empty() {
                            let opened = expanded.contains(&id);
                            let symbol = if opened { "-" } else { "+" };
                            let button =
                                ui.add_sized([24.0, row_height], egui::Button::new(symbol));
                            let title = if opened {
                                text!("Свернуть папку")
                            } else {
                                text!("Развернуть папку")
                            };
                            ui.ctx().accesskit_node_builder(button.id, |access| {
                                access.set_label(format!("{title} · {label}"))
                            });
                            if button.on_hover_text(title).clicked() {
                                toggle = Some(id);
                            }
                        } else {
                            ui.add_space(24.0);
                        }
                        let size = row_size(node);
                        let detail = node_detail(analysis, node);
                        ui.add_sized([164.0, row_height], egui::Label::new(size).truncate())
                            .on_hover_text(&detail);
                        ui.add(egui::Label::new(label).extend().selectable(true))
                            .on_hover_text(&detail);
                        if let Some(issue) = &node.issue {
                            ui.colored_label(ui.visuals().warn_fg_color, issue_caption(issue))
                                .on_hover_text(&detail);
                        }
                    })
                });
            }
        });
    toggle
}

fn show_files(ui: &mut egui::Ui, analysis: &Analysis, run: u64, height: f32) {
    if analysis.files.is_empty() {
        ui.label(if analysis.is_partial() {
            text!("Обычные файлы пока не учтены")
        } else {
            text!("Обычные файлы не найдены")
        });
        return;
    }
    let row_height = row_height(ui);
    egui::ScrollArea::both()
        .id_salt(("disk-usage-files", run))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, analysis.files.len(), |ui, range| {
            for &id in &analysis.files[range] {
                let node = &analysis.nodes[id];
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [124.0, row_height],
                        egui::Label::new(human_size(node.allocated)).truncate(),
                    );
                    let label = node.path.to_string_lossy().escape_debug().to_string();
                    ui.add(egui::Label::new(label).extend().selectable(true))
                        .on_hover_text(node_detail(analysis, node));
                });
            }
        });
}

fn row_height(ui: &egui::Ui) -> f32 {
    ui.spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&egui::TextStyle::Body))
}

fn node_name(analysis: &Analysis, node: &Node) -> String {
    node.path.file_name().map_or_else(
        || analysis.folder.name().escape_debug().to_string(),
        |name| name.to_string_lossy().escape_debug().to_string(),
    )
}

fn row_size(node: &Node) -> String {
    match (node.kind, node.coverage) {
        (_, Coverage::Unavailable) => text!("Нет данных").into(),
        (_, Coverage::Partial) => {
            crate::tr_format!("{size} (partial)", size = human_size(node.allocated))
        }
        (Kind::Symlink, _) => text!("Символическая ссылка").into(),
        (Kind::Other, _) => text!("Специальный объект").into(),
        (Kind::File, _) if node.hard_link => text!("Уже учтён").into(),
        _ => human_size(node.allocated),
    }
}

fn node_detail(analysis: &Analysis, node: &Node) -> String {
    let mut detail = match &node.issue {
        Some(Issue::Failure(error)) => crate::i18n::file_error(error),
        _ => analysis
            .folder
            .join(&node.path)
            .display()
            .escape_debug()
            .to_string(),
    };
    let bytes = node.allocated;
    if node.hard_link {
        detail.push('\n');
        detail.push_str(text!("Выделенные блоки учтены по другой жёсткой ссылке"));
    } else {
        match node.coverage {
            Coverage::Unavailable => {
                detail.push('\n');
                detail.push_str(text!("Нет данных"));
            }
            Coverage::Partial => detail.push_str(&crate::tr_format!(
                "\nAllocated space counted: {bytes} bytes (partial)",
                bytes = bytes
            )),
            Coverage::Complete => detail.push_str(&crate::tr_format!(
                "\nAllocated: {bytes} bytes",
                bytes = bytes
            )),
        }
    }
    if node.kind == Kind::File {
        let bytes = node.logical;
        detail.push_str(&crate::tr_format!(
            "\nFile length: {bytes} bytes",
            bytes = bytes
        ));
    }
    if let Some(issue) = &node.issue
        && !matches!(issue, Issue::Failure(_))
    {
        detail.push('\n');
        detail.push_str(issue_caption(issue));
    }
    detail
}

fn issue_caption(issue: &Issue) -> &'static str {
    match issue {
        Issue::Failure(_) => text!("Ошибка обхода"),
        Issue::AlreadyCounted => text!("Эта папка уже учтена"),
        Issue::Partial => text!("Папка проверена частично"),
        Issue::Pending => text!("Папка ещё не проверена"),
    }
}

impl PawCommander {
    pub(super) fn can_analyze_disk_usage(&self) -> bool {
        !self.conflict_open() && self.tab().path.local().is_some()
    }

    pub(super) fn open_disk_usage(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_none() && self.can_analyze_disk_usage() {
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Usage(DiskUsage::new(self.tab().path.clone())),
                return_focus,
                ctx,
            ));
        }
    }
}
