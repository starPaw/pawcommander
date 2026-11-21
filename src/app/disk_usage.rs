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
        usage::{self, Analysis, Finish, Kind, Node, NodeId},
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
                    show_progress_with_stop(ui, worker, "Проверяем занятое место…", "Остановка…");
                }
                Phase::Failed(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                Phase::Stopped => {
                    ui.label(text("Анализ места остановлен"));
                }
                Phase::Finished(analysis) => {
                    show_summary(ui, analysis);
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut self.view, View::Tree, text("Дерево размеров"));
                        ui.selectable_value(&mut self.view, View::Files, text("Крупнейшие файлы"));
                        ui.selectable_value(&mut self.view, View::Map, text("Карта места"));
                    });
                    match self.view {
                        View::Tree => {
                            if let Some(id) =
                                show_tree(ui, analysis, &self.tree, &self.expanded, self.run)
                            {
                                if !self.expanded.remove(&id) {
                                    self.expanded.insert(id);
                                }
                                self.tree = tree_rows(analysis, &self.expanded);
                            }
                        }
                        View::Files => show_files(ui, analysis, self.run),
                        View::Map => {
                            self.map_folder = map::show(ui, analysis, self.map_folder, self.run)
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
                            egui::Button::new(text("Остановить")),
                        )
                        .clicked();
                } else {
                    let response = ui.button(text("Начать анализ"));
                    if self.focus_start && !ui.is_sizing_pass() {
                        response.request_focus();
                        self.focus_start = false;
                    }
                    start = response.clicked();
                    close = ui.button(text("Закрыть")).clicked();
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

fn show_summary(ui: &mut egui::Ui, analysis: &Analysis) {
    let size = human_size(analysis.nodes[0].allocated);
    ui.strong(crate::tr_format!(
        "Учтено на диске: {size}",
        "Allocated space counted: {size}"
    ));
    let status = match analysis.finish {
        Finish::Stopped => Some(text("Анализ остановлен. Показаны частичные размеры.")),
        Finish::Limit => Some(text(
            "Достигнут предел анализа. Показаны частичные размеры.",
        )),
        Finish::Complete if analysis.issues != 0 => Some(text(
            "Некоторые объекты недоступны. Показаны частичные размеры.",
        )),
        Finish::Complete => None,
    };
    if let Some(status) = status {
        ui.colored_label(ui.visuals().warn_fg_color, status);
    }
    ui.label(crate::tr_format!(
        "Файлов: {} · символьных ссылок пропущено: {} · дополнительных жёстких ссылок пропущено: {} · ошибок: {}",
        "Files: {} · symbolic links skipped: {} · extra hard links excluded: {} · read errors: {}",
        analysis.files.len(), analysis.symlinks, analysis.hard_links, analysis.issues,
    ));
    ui.small(text(
        "Размеры по выделенным блокам. Клоны APFS могут использовать общие блоки.",
    ));
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
) -> Option<NodeId> {
    let mut toggle = None;
    let row_height = row_height(ui);
    let height = body_height(ui);
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
                                text("Свернуть папку")
                            } else {
                                text("Развернуть папку")
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
                        ui.add_sized([124.0, row_height], egui::Label::new(size).truncate())
                            .on_hover_text(&detail);
                        ui.add(egui::Label::new(label).extend().selectable(true))
                            .on_hover_text(&detail);
                        if let Some(issue) = &node.issue {
                            ui.colored_label(ui.visuals().warn_fg_color, issue)
                                .on_hover_text(&detail);
                        }
                    })
                });
            }
        });
    toggle
}

fn show_files(ui: &mut egui::Ui, analysis: &Analysis, run: u64) {
    if analysis.files.is_empty() {
        ui.label(text("Обычные файлы не найдены"));
        return;
    }
    let row_height = row_height(ui);
    let height = body_height(ui);
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

fn body_height(ui: &egui::Ui) -> f32 {
    (ui.ctx().content_rect().height() * 0.52).clamp(80.0, 420.0)
}

fn node_name(analysis: &Analysis, node: &Node) -> String {
    node.path.file_name().map_or_else(
        || analysis.folder.name().escape_debug().to_string(),
        |name| name.to_string_lossy().escape_debug().to_string(),
    )
}

fn row_size(node: &Node) -> String {
    match node.kind {
        Kind::Symlink => text("Символическая ссылка").into(),
        Kind::Other if node.issue.is_some() => text("Недоступно").into(),
        Kind::Other => text("Специальный объект").into(),
        Kind::File if node.hard_link => text("Уже учтён").into(),
        Kind::Folder if node.issue.is_some() && node.allocated == 0 => text("Нет данных").into(),
        _ => human_size(node.allocated),
    }
}

fn node_detail(analysis: &Analysis, node: &Node) -> String {
    let path = analysis
        .folder
        .join(&node.path)
        .display()
        .escape_debug()
        .to_string();
    let bytes = node.allocated;
    let mut detail = if node.hard_link {
        format!(
            "{path}\n{}",
            text("Выделенные блоки учтены по другой жёсткой ссылке")
        )
    } else {
        crate::tr_format!(
            "{path}\nВыделено: {bytes} байт",
            "{path}\nAllocated: {bytes} bytes"
        )
    };
    if node.kind == Kind::File {
        let bytes = node.logical;
        detail.push_str(&crate::tr_format!(
            "\nРазмер файла: {bytes} байт",
            "\nFile length: {bytes} bytes"
        ));
    }
    if let Some(issue) = &node.issue {
        detail.push('\n');
        detail.push_str(issue);
    }
    detail
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
