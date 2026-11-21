use super::actions::Action;
use super::address_bar::address_bar;
use super::column_view;
use super::entry_widget::{entry_widget, EntryOptions, COMPACT_FILE_ROW_HEIGHT};
use super::state::{
    Density, FileOrder, FolderMode, FolderView, FolderViewChange, FolderViews, PaneOrientation,
    Settings, Sort,
};
use super::tab_strip::{paint_pane_outline, tab_strip};
use super::tabs::{Pane, Tab};
use crate::drag_drop::DropTargets;
use crate::icons;
use crate::list_columns::{Column, ColumnLayout};
use crate::tab_drag::TabDropTargets;
use crate::theme::{paint_focus, Palette};
use crate::thumbnails::Thumbnails;
use eframe::egui;
use eframe::egui::{Color32, FontId, RichText, Sense, Stroke, Vec2};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) struct PaneOptions<'a> {
    pub(super) index: usize,
    pub(super) active: bool,
    pub(super) settings: &'a Settings,
    pub(super) folder_views: &'a FolderViews,
    pub(super) idle: bool,
    pub(super) zed_available: bool,
}

pub(super) fn workspace_border(colors: Palette) -> Color32 {
    colors.line.lerp_to_gamma(colors.muted, 0.35)
}

pub(super) fn workspace_card(colors: Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(colors.bg)
        .corner_radius(12)
        .stroke(Stroke::new(1.0, workspace_border(colors)))
}

pub(super) fn sort_controls(ui: &mut egui::Ui, order: FileOrder, mut choose: impl FnMut(Sort)) {
    for (sort, column) in [
        (Sort::Name, Column::Name),
        (Sort::Size, Column::Size),
        (Sort::Modified, Column::Modified),
    ] {
        let label = column.label();
        let selected = order.key == sort;
        let title = if selected {
            format!("{label} {}", if order.descending { "↓" } else { "↑" })
        } else {
            label.to_owned()
        };
        let response = ui.selectable_label(selected, title);
        paint_focus(ui, &response, 4);
        if response.clicked() {
            choose(sort);
            ui.close();
        }
    }
}

pub(super) fn list_header(
    ui: &mut egui::Ui,
    layout: &ColumnLayout,
    order: FileOrder,
    padding: f32,
    compact: bool,
    mut choose_sort: impl FnMut(Sort),
) {
    if !compact {
        ui.add_space(4.0);
    }
    let height = if compact { 22.0 } else { 36.0 };
    let (header, _) = ui.allocate_exact_size(Vec2::new(layout.width, height), Sense::hover());
    let band = header.expand2(Vec2::new(padding, 0.0));
    let colors = Palette::from_ctx(ui.ctx());
    ui.painter().rect_filled(band, 0, colors.raised);
    for y in [band.top(), band.bottom()] {
        ui.painter().hline(
            band.x_range(),
            y,
            Stroke::new(1.0, workspace_border(colors)),
        );
    }
    for (column, rect) in layout.cells(header) {
        let label = column.label();
        let sort = match column {
            Column::Name => Some(Sort::Name),
            Column::Size => Some(Sort::Size),
            Column::Modified => Some(Sort::Modified),
            _ => None,
        };
        let response = sort.map(|sort| {
            let response = ui.interact(rect, ui.scope_id().with(column as u8), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::Button,
                    ui.is_enabled(),
                    crate::tr_format!("Sort: {label}", label = label),
                )
            });
            paint_focus(ui, &response, 4);
            if response.clicked() {
                choose_sort(sort);
            }
            response
        });
        let painter = ui.painter().with_clip_rect(rect);
        let text = painter.text(
            rect.left_center() + Vec2::new(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            if response.is_some_and(|response| response.hovered()) {
                colors.ink
            } else {
                colors.muted
            },
        );
        if sort == Some(order.key) {
            let x = text.right() + 6.0;
            let y = rect.center().y;
            let direction = if order.descending { -1.0 } else { 1.0 };
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(x, y + 2.0 * direction),
                    egui::pos2(x + 4.0, y - 2.0 * direction),
                    egui::pos2(x + 8.0, y + 2.0 * direction),
                ],
                Stroke::new(1.2, colors.muted),
            ));
        }
    }
}

pub(super) fn list_notice(ui: &mut egui::Ui, tab: &Tab, empty: bool, compact: bool) {
    let colors = Palette::from_ctx(ui.ctx());
    if let Some(error) = &tab.error {
        ui.colored_label(colors.error, error);
    }
    if tab.blocking_load() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if tab.path.is_archive() {
                crate::i18n::text!("Чтение архива…")
            } else {
                crate::i18n::text!("Чтение папки…")
            });
        });
    }
    if empty && !tab.blocking_load() && tab.error.is_none() {
        if !compact {
            ui.add_space(50.0);
        }
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(if tab.filter.is_empty() {
                    crate::i18n::text!("Папка пуста")
                } else {
                    crate::i18n::text!("Ничего не найдено")
                })
                .size(if compact { 13.0 } else { 20.0 })
                .color(colors.muted),
            );
            if !compact && (!tab.path.is_archive() || !tab.filter.is_empty()) {
                ui.label(
                    RichText::new(if tab.filter.is_empty() {
                        crate::i18n::text!("Создайте папку или вставьте файлы")
                    } else {
                        crate::i18n::text!("Попробуйте другое имя")
                    })
                    .size(12.0)
                    .color(colors.muted),
                );
            }
        });
    }
}

pub(super) fn render_pane(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    options: PaneOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
    tab_targets: &mut TabDropTargets,
    mut thumbnails: Option<&mut Thumbnails>,
) {
    let PaneOptions {
        index: pane_index,
        active,
        settings,
        folder_views,
        idle,
        zed_available,
    } = options;
    let colors = Palette::from_ctx(ui.ctx());
    // Below this height the regular pane chrome can obscure a grid tile's name.
    let compact = ui.available_height() < 280.0;
    let sort_label = crate::i18n::text!("Сортировка");
    ui.push_id(pane_index, |ui| {
        if compact {
            ui.spacing_mut().item_spacing.y = 1.0;
        }
        let border = workspace_border(colors);
        const PANE_PADDING: i8 = 10;
        let outline = if active {
            border.lerp_to_gamma(colors.mauve, 0.3)
        } else {
            border
        };
        let background = ui.painter().add(egui::Shape::Noop);
        let frame = egui::Frame::new().inner_margin(egui::Margin {
            left: 1,
            right: 1,
            top: 1,
            bottom: if compact { 1 } else { PANE_PADDING + 1 },
        });
        let frame = frame.show(ui, |pane_ui| {
            pane_ui.set_min_height((pane_ui.available_height() - 1.0).max(0.0));
            let (strip, active_tab) = tab_strip(
                pane_ui,
                pane,
                pane_index,
                actions,
                drop_targets,
                tab_targets,
                idle,
            );
            let mut content = egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(PANE_PADDING, 0))
                .begin(pane_ui);
            let ui = &mut content.content_ui;
            let tab = &mut pane.tabs[pane.current];
            let view = tab.folder_view(settings, pane_index, folder_views);
            let FolderView {
                mode,
                order,
                density,
                ..
            } = view;
            let grid = mode == FolderMode::Grid;
            let columns_mode = mode == FolderMode::Columns;
            let compact_rows = compact || density == Density::Compact;
            tab.rendered_rows.clear();
            address_bar(ui, tab, pane_index, actions, drop_targets);
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let show_sort = compact || columns_mode;
                    let controls = (3.0 + if show_sort { 1.0 } else { 0.0 })
                        * (icons::COMPACT_SIZE + ui.spacing().item_spacing.x);
                    let filter_width = (ui.available_width() - controls).max(12.0);
                    let response = ui.add_sized(
                        [filter_width, icons::COMPACT_SIZE],
                        egui::TextEdit::singleline(&mut tab.filter)
                            .align(egui::Align2::LEFT_CENTER)
                            .margin(egui::Margin::symmetric(5, 0))
                            .hint_text(crate::i18n::text!("Фильтр по имени…")),
                    );
                    for choice in [FolderMode::Grid, FolderMode::List, FolderMode::Columns] {
                        if icons::compact_button(ui, choice.icon(), choice.label(), mode == choice)
                            .clicked()
                        {
                            actions.push(
                                tab.folder_view_action(pane_index, FolderViewChange::Mode(choice)),
                            );
                        }
                    }
                    if show_sort {
                        let menu =
                            ui.menu_button("↕", |ui| {
                                sort_controls(ui, order, |sort| {
                                    actions.push(tab.folder_view_action(
                                        pane_index,
                                        FolderViewChange::Sort(sort),
                                    ));
                                });
                            });
                        ui.ctx().accesskit_node_builder(menu.response.id, |node| {
                            node.set_label(sort_label);
                        });
                        paint_focus(ui, &menu.response, 4);
                        menu.response.on_hover_text(sort_label);
                    }
                    response
                })
                .inner;
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label(crate::i18n::text!("Фильтр по имени"));
            });
            if tab.focus_filter {
                response.request_focus();
                tab.focus_filter = false;
            }
            if response.has_focus() {
                actions.push(Action::Focus(pane_index));
            }
            if response.changed() {
                tab.column_reveal = None;
            }
            if response.changed() && !tab.selected.is_empty() {
                let visible_paths: BTreeSet<_> = tab
                    .visible(view)
                    .iter()
                    .map(|i| &tab.entries[*i].path)
                    .filter(|path| tab.selected.contains(*path))
                    .collect();
                tab.selected.retain(|p| visible_paths.contains(p));
            }
            let viewport_x = ui.available_rect_before_wrap().x_range();
            let available_width = viewport_x.span();
            let layout = if grid {
                ColumnLayout::responsive(available_width)
            } else {
                settings.list_columns.layout(available_width)
            };
            const STATUS_HEIGHT: f32 = 22.0;
            const STATUS_RULE_HEIGHT: f32 = 1.0;
            let spacing = ui.spacing().item_spacing.y;
            let status_height = if !compact {
                STATUS_HEIGHT + STATUS_RULE_HEIGHT + 2.0 * spacing
            } else {
                0.0
            };
            let scrolling_columns = layout.width > available_width;
            let tab_id = tab.id;
            let mut render_files = |ui: &mut egui::Ui| {
                if !compact || (!grid && settings.list_columns.is_custom()) {
                    list_header(
                        ui,
                        &layout,
                        order,
                        f32::from(PANE_PADDING),
                        compact,
                        |sort| {
                            actions.push(
                                tab.folder_view_action(pane_index, FolderViewChange::Sort(sort)),
                            );
                        },
                    );
                }
                drop_targets.add(
                    ui,
                    ui.available_rect_before_wrap(),
                    pane_index,
                    &tab.path,
                    !tab.blocking_load() && tab.error.is_none(),
                );
                let visible = tab.visible(view);
                if tab.error.is_some() || tab.blocking_load() || visible.is_empty() {
                    let notice_rect = egui::Rect::from_x_y_ranges(
                        viewport_x,
                        ui.available_rect_before_wrap().y_range(),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(notice_rect), |ui| {
                        list_notice(ui, tab, visible.is_empty(), compact);
                    });
                }
                let row_height = if compact_rows {
                    COMPACT_FILE_ROW_HEIGHT
                } else if grid {
                    104.0
                } else {
                    36.0
                };
                let columns = if grid {
                    (ui.available_width() / 108.0).floor().max(1.0) as usize
                } else {
                    1
                };
                let rows = visible.len().div_ceil(columns);
                let reserved_status_height = if scrolling_columns {
                    0.0
                } else {
                    status_height
                };
                let files_height = (ui.available_height() - reserved_status_height).max(0.0);
                let mut scroll = egui::ScrollArea::vertical();
                let scope = ui.scope_id();
                let previous_scope = tab.file_scroll_scope;
                let row_pitch = row_height + spacing;
                if !ui.is_sizing_pass() {
                    let previous_geometry = tab.file_scroll_geometry.replace((columns, row_pitch));
                    if previous_scope.is_some_and(|previous| previous != scope) {
                        tab.scroll_restore.get_or_insert(tab.scroll_offset);
                    }
                    tab.file_scroll_scope = Some(scope);
                    if let Some(position) = tab
                        .scroll_to
                        .take()
                        .and_then(|path| visible.iter().position(|i| tab.entries[*i].path == path))
                    {
                        tab.scroll_restore = None;
                        scroll =
                            scroll.vertical_scroll_offset((position / columns) as f32 * row_pitch);
                    } else if tab.has_complete_listing {
                        // Progressive batches can initially be too short to hold
                        // the offset or may still change which rows are at that offset.
                        let offset = tab.scroll_restore.take().or_else(|| {
                            let (old_columns, old_pitch) = previous_geometry?;
                            if (old_columns, old_pitch) == (columns, row_pitch) {
                                return None;
                            }
                            // Keep the first visible item in view when the grid reflows.
                            let old_row = tab.scroll_offset / old_pitch;
                            let item = old_row as usize * old_columns;
                            let row = item / columns;
                            Some((row as f32 + old_row.fract()) * row_pitch)
                        });
                        if let Some(offset) = offset {
                            let max_offset =
                                (rows as f32 * row_pitch - spacing - files_height).max(0.0);
                            scroll = scroll.vertical_scroll_offset(offset.min(max_offset));
                        }
                    }
                }
                let files_id = tab.file_list_id();
                let files_label = {
                    let pane = match (
                        settings.split.then_some(settings.pane_layout.orientation),
                        pane_index,
                    ) {
                        (Some(PaneOrientation::Stacked), 0) => crate::i18n::text!("Верхняя панель"),
                        (Some(PaneOrientation::Stacked), _) => crate::i18n::text!("Нижняя панель"),
                        (_, 0) => crate::i18n::text!("Левая панель"),
                        _ => crate::i18n::text!("Правая панель"),
                    };
                    let view = if grid {
                        crate::i18n::text!("Сетка")
                    } else {
                        crate::i18n::text!("Список")
                    };
                    format!("{pane}, {view}, {}", tab.path.display())
                };
                let list_ui = egui::UiBuilder::new()
                    .scope_id(files_id)
                    .accessibility_role(egui::Role::List)
                    .accessibility_label(files_label);
                let file_scroll = ui
                    .scope_builder(list_ui, |ui| {
                        scroll
                            .max_height(files_height)
                            .min_scrolled_height(files_height)
                            .id_salt((tab.id, &tab.path, "files"))
                            .auto_shrink([false, false])
                            .show_rows(ui, row_height, rows, |ui, range| {
                                if grid && compact_rows {
                                    ui.spacing_mut().interact_size.y = row_height;
                                }
                                for row in range {
                                    if grid {
                                        ui.horizontal(|ui| {
                                            for col in 0..columns {
                                                let position = row * columns + col;
                                                if position < visible.len() {
                                                    entry_widget(
                                                        ui,
                                                        tab,
                                                        position,
                                                        &visible,
                                                        EntryOptions {
                                                            columns: None,
                                                            thumbnails: thumbnails.as_deref_mut(),
                                                            pane: pane_index,
                                                            split: settings.split,
                                                            grid: true,
                                                            folder_columns: false,
                                                            compact: compact_rows,
                                                            idle,
                                                            zed_available,
                                                        },
                                                        actions,
                                                        drop_targets,
                                                    );
                                                }
                                            }
                                        });
                                    } else if row < visible.len() {
                                        entry_widget(
                                            ui,
                                            tab,
                                            row,
                                            &visible,
                                            EntryOptions {
                                                columns: Some(&layout),
                                                thumbnails: None,
                                                pane: pane_index,
                                                split: settings.split,
                                                grid: false,
                                                folder_columns: false,
                                                compact: compact_rows,
                                                idle,
                                                zed_available,
                                            },
                                            actions,
                                            drop_targets,
                                        );
                                    }
                                }
                            })
                    })
                    .inner;
                if !ui.is_sizing_pass() {
                    let offset = file_scroll.state.clamped_offset().y;
                    if previous_scope == Some(scope) && offset > 0.0 && offset != tab.scroll_offset
                    {
                        // User scrolling in a partial listing takes precedence.
                        tab.scroll_restore = None;
                    }
                    tab.scroll_offset = offset;
                }
                visible.len()
            };
            let visible_count = if columns_mode {
                column_view::show(
                    ui,
                    tab,
                    &mut pane.columns,
                    options,
                    (ui.available_height() - status_height).max(0.0),
                    actions,
                    drop_targets,
                )
            } else if scrolling_columns {
                let height = (ui.available_height() - status_height).max(0.0);
                egui::ScrollArea::horizontal()
                    .id_salt(("file_columns", tab_id))
                    .min_scrolled_width(0.0)
                    .max_height(height)
                    .auto_shrink([false, false])
                    .scroll_source(
                        egui::scroll_area::ScrollSource::SCROLL_BAR
                            | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
                    )
                    .show(ui, |ui| {
                        ui.set_width(layout.width);
                        ui.style_mut().always_scroll_the_only_direction = false;
                        render_files(ui)
                    })
                    .inner
            } else {
                render_files(ui)
            };
            if !compact {
                ui.add(egui::Separator::default().spacing(STATUS_RULE_HEIGHT));
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), STATUS_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new(crate::tr_format!(
                                "Selected: {}",
                                crate::i18n::display::integer(tab.selected.len())
                            ))
                            .size(11.0)
                            .color(colors.muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(crate::tr_format!(
                                    "{} items",
                                    crate::i18n::display::integer(visible_count)
                                ))
                                .size(11.0)
                                .color(colors.muted),
                            );
                        });
                    },
                );
            }
            content.end(pane_ui);
            (strip, active_tab)
        });
        // Observe the whole pane without allocating a click target over its
        // children: empty rows, margins and status space activate it too.
        if ui.is_enabled()
            && ui.memory(|memory| memory.allows_interaction(ui.layer_id()))
            && ui.input(|input| input.pointer.any_pressed())
            && ui.rect_contains_pointer(frame.response.rect)
        {
            actions.push(Action::Focus(pane_index));
        }
        let (strip, active_tab) = frame.inner;
        let mut body = frame.response.rect.shrink(0.5);
        body.min.y = strip.bottom() - 0.5;
        paint_pane_outline(
            ui.painter(),
            background,
            body,
            active_tab,
            colors.bg,
            Stroke::new(1.0, outline),
        );
        tab_targets.set_pane_rect(ui, pane_index, frame.response.rect);
    });
}
