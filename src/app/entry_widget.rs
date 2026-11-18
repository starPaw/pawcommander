use super::actions::{Action, ColumnNavigation};
use super::footer::human_size;
use super::tabs::Tab;
use crate::drag_drop::{DragFiles, DropTargets};
use crate::fs_ops::TransferKind;
use crate::icons;
use crate::list_columns::{Column, ColumnLayout};
use crate::theme::{paint_focus, Palette};
use crate::thumbnails::Thumbnails;
use eframe::egui;
use eframe::egui::{Color32, FontId, Sense, Stroke, Vec2};

pub(super) const COMPACT_FILE_ROW_HEIGHT: f32 = 24.0;

pub(super) struct EntryOptions<'a> {
    pub(super) columns: Option<&'a ColumnLayout>,
    pub(super) thumbnails: Option<&'a mut Thumbnails>,
    pub(super) pane: usize,
    pub(super) split: bool,
    pub(super) grid: bool,
    pub(super) folder_columns: bool,
    pub(super) compact: bool,
    pub(super) idle: bool,
    pub(super) zed_available: bool,
}

pub(super) fn entry_widget(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    position: usize,
    visible: &[usize],
    options: EntryOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
) {
    let index = visible[position];
    let colors = Palette::from_ctx(ui.ctx());
    let EntryOptions {
        columns,
        thumbnails,
        pane,
        split,
        grid,
        folder_columns,
        compact,
        idle,
        zed_available,
    } = options;
    let entry = &tab.entries[index];
    let is_dir = entry.is_dir;
    let path = entry.path.clone();
    let archive_member = path.is_archive();
    let selected = tab.selected.contains(&path);
    let list_width = columns.map_or_else(|| ui.available_width(), |columns| columns.width);
    let size = if compact {
        let width = if grid { 100.0 } else { list_width };
        Vec2::new(width, COMPACT_FILE_ROW_HEIGHT)
    } else if grid {
        Vec2::new(100.0, 98.0)
    } else {
        Vec2::new(list_width, 36.0)
    };
    let (_, rect) = ui.allocate_space(size);
    let sense = if archive_member {
        Sense::click()
    } else {
        Sense::click_and_drag()
    };
    let response = ui.interact(rect, tab.entry_id(&path), sense);
    tab.rendered_rows.insert(response.id, path.clone());
    if entry.is_dir {
        drop_targets.add(
            ui,
            rect,
            pane,
            &path,
            !tab.blocking_load() && tab.error.is_none(),
        );
    }
    if response.has_focus() && (response.gained_focus() || tab.cursor.as_ref() != Some(&path)) {
        tab.cursor = Some(path.clone());
        actions.push(Action::Focus(pane));
    }
    if !grid && position.is_multiple_of(2) {
        ui.painter()
            .rect_filled(rect, 6, colors.bg.lerp_to_gamma(colors.raised, 0.22));
    }
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            5,
            if selected {
                colors.selection
            } else {
                colors.raised
            },
        );
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            6,
            Stroke::new(1.0, colors.mauve.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    }
    let check_rect =
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(15.0, 0.0), Vec2::splat(14.0));
    if !grid {
        ui.painter().rect(
            check_rect,
            3,
            if selected {
                colors.mauve
            } else {
                Color32::TRANSPARENT
            },
            Stroke::new(1.0, if selected { colors.mauve } else { colors.muted }),
            egui::StrokeKind::Inside,
        );
        if selected {
            ui.painter().add(egui::Shape::line(
                vec![
                    check_rect.min + Vec2::new(3.0, 7.0),
                    check_rect.min + Vec2::new(6.0, 10.0),
                    check_rect.min + Vec2::new(11.0, 4.0),
                ],
                Stroke::new(1.6, colors.surface),
            ));
        }
    }
    let icon_rect = if compact {
        let left = if grid { 11.0 } else { 44.0 };
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(left, 0.0), Vec2::splat(16.0))
    } else if grid {
        egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 30.0), Vec2::splat(37.0))
    } else {
        egui::Rect::from_min_size(rect.min + Vec2::new(32.0, 6.0), Vec2::splat(24.0))
    };
    let has_thumbnail = thumbnails.is_some_and(|thumbnails| {
        let thumbnail_rect = if compact {
            icon_rect
        } else {
            egui::Rect::from_center_size(icon_rect.center(), Vec2::new(74.0, 46.0))
        };
        !ui.is_sizing_pass()
            && ui.is_rect_visible(thumbnail_rect)
            && thumbnails.paint(ui, entry, thumbnail_rect)
    });
    if !has_thumbnail {
        let icon = icons::file_icon(&entry.name, entry.is_dir);
        icons::paint(ui, icon_rect, icon, icons::tint(icon, colors));
    }
    if entry.is_symlink {
        ui.painter().text(
            icon_rect.right_bottom(),
            egui::Align2::RIGHT_BOTTOM,
            "↗",
            FontId::proportional(12.0),
            colors.ink,
        );
    }
    let name_color = colors.ink;
    if grid && compact {
        let clip = egui::Rect::from_min_max(
            rect.left_top() + Vec2::new(23.0, 0.0),
            rect.right_bottom() - Vec2::new(3.0, 0.0),
        );
        ui.painter().with_clip_rect(clip).text(
            clip.left_center(),
            egui::Align2::LEFT_CENTER,
            &entry.name,
            FontId::proportional(12.0),
            name_color,
        );
    } else if grid {
        let galley = ui.painter().layout(
            entry.name.clone(),
            FontId::proportional(12.0),
            name_color,
            93.0,
        );
        let clip =
            egui::Rect::from_min_size(rect.min + Vec2::new(3.0, 55.0), Vec2::new(94.0, 36.0));
        ui.painter().with_clip_rect(clip).galley(
            egui::pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 55.0),
            galley,
            name_color,
        );
    } else if folder_columns {
        let end_padding = if is_dir { 19.0 } else { 4.0 };
        let clip = egui::Rect::from_min_max(
            rect.left_top() + Vec2::new(57.0, 0.0),
            rect.right_bottom() - Vec2::new(end_padding, 0.0),
        );
        if clip.is_positive() {
            ui.painter().with_clip_rect(clip).text(
                clip.left_center(),
                egui::Align2::LEFT_CENTER,
                &entry.name,
                FontId::proportional(13.0),
                name_color,
            );
        }
        if is_dir && rect.width() >= 78.0 {
            ui.painter().text(
                rect.right_center() - Vec2::new(5.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                "›",
                FontId::proportional(15.0),
                colors.muted,
            );
        }
    } else {
        let fallback;
        let columns = if let Some(columns) = columns {
            columns
        } else {
            fallback = ColumnLayout::responsive(rect.width());
            &fallback
        };
        for (column, cell) in columns.cells(rect) {
            if !ui.is_rect_visible(cell) {
                continue;
            }
            let (padding, align, size, color, value) = match column {
                Column::Name => (
                    64.0,
                    egui::Align2::LEFT_CENTER,
                    14.0,
                    name_color,
                    std::borrow::Cow::Borrowed(entry.name.as_str()),
                ),
                Column::Size if !entry.is_dir => (
                    5.0,
                    egui::Align2::RIGHT_CENTER,
                    11.0,
                    colors.muted,
                    human_size(entry.size).into(),
                ),
                _ => (
                    5.0,
                    egui::Align2::RIGHT_CENTER,
                    11.0,
                    colors.muted,
                    column.metadata(entry).unwrap_or("—".into()),
                ),
            };
            let origin = if align == egui::Align2::LEFT_CENTER {
                cell.left_center() + Vec2::new(padding, 0.0)
            } else {
                cell.right_center() - Vec2::new(padding, 0.0)
            };
            ui.painter().with_clip_rect(cell).text(
                origin,
                align,
                value,
                FontId::proportional(size),
                color,
            );
        }
    }
    paint_focus(ui, &response, 5);
    if idle && !archive_member && response.drag_started_by(egui::PointerButton::Primary) {
        let icon = icons::file_icon(&entry.name, entry.is_dir);
        let paths = tab.drag_paths(index, visible);
        egui::DragAndDrop::set_payload(ui.ctx(), DragFiles { paths, icon });
        actions.push(Action::Focus(pane));
    }
    let on_checkbox = !grid
        && response
            .interact_pointer_pos()
            .is_some_and(|p| check_rect.expand(4.0).contains(p));
    if response.clicked() && ui.memory(|memory| memory.allows_interaction(ui.layer_id())) {
        let pointer = response.clicked_by(egui::PointerButton::Primary);
        let mut modifiers = if pointer {
            ui.input(|i| i.modifiers)
        } else {
            egui::Modifiers::NONE
        };
        if !pointer || on_checkbox {
            modifiers.command = true;
        }
        tab.choose(index, visible, modifiers);
        if !pointer {
            response.request_focus();
        }
        actions.push(Action::Focus(pane));
    }
    if response.secondary_clicked() {
        tab.column_reveal = None;
        if !selected {
            tab.selected.clear();
            tab.selected.insert(path.clone());
            tab.anchor = Some(path.clone());
        }
        actions.push(Action::Focus(pane));
    }
    let open = || {
        if folder_columns {
            tab.column_action(pane, ColumnNavigation::Open(path.clone()))
        } else {
            Action::Open(pane, path.clone())
        }
    };
    if response.double_clicked() && !on_checkbox {
        actions.push(open());
    }
    response.widget_info(|| {
        let entry = &tab.entries[index];
        let kind = crate::i18n::text(if entry.is_symlink {
            "Символическая ссылка"
        } else if entry.is_dir {
            "Папка"
        } else {
            "Файл"
        });
        egui::WidgetInfo::selected(
            egui::Role::CheckBox,
            ui.is_enabled(),
            tab.selected.contains(&path),
            format!("{}, {kind}", entry.name),
        )
    });
    response
        .on_hover_text(path.display().to_string())
        .context_menu(|ui| {
            if (!archive_member || is_dir) && ui.button(crate::i18n::text("Открыть")).clicked() {
                actions.push(open());
                ui.close();
            }
            let local_archive = !is_dir
                && tab.selected.len() == 1
                && tab.selected.contains(&path)
                && path.local().and_then(crate::archive::Format::from_path).is_some();
            if (archive_member || local_archive)
                && ui.add_enabled(idle, egui::Button::new(crate::i18n::text("Проверить архив…"))).clicked()
            {
                actions.push(Action::Focus(pane));
                actions.push(Action::VerifyArchive);
                ui.close();
            }
            if archive_member {
                if ui.add_enabled(idle && !tab.selected.is_empty(), egui::Button::new(crate::i18n::text("Распаковать…"))).clicked() {
                    actions.push(Action::Focus(pane));
                    actions.push(Action::ExtractArchive);
                    ui.close();
                }
                if ui.button(crate::i18n::text("Копировать путь")).clicked() {
                    actions.push(Action::Focus(pane));
                    actions.push(Action::CopyPath);
                    ui.close();
                }
                return;
            }
            if !is_dir && ui.add_enabled(zed_available, egui::Button::new(crate::i18n::text("Открыть в Zed")))
                .on_disabled_hover_text(crate::i18n::text("Zed не найден. Установите редактор и выберите Настройки → Инструменты → Проверить снова."))
                .clicked() {
                actions.push(Action::OpenInZed(pane, path.clone()));
                ui.close();
            }
            ui.separator();
            if split {
            ui.menu_button(crate::i18n::text("В соседнюю панель"), |ui| {
                for (label, kind) in [(crate::i18n::text("Копировать"), TransferKind::Copy), (crate::i18n::text("Переместить"), TransferKind::Move)] {
                    if ui.add_enabled(idle, egui::Button::new(label)).clicked() {
                        actions.push(Action::Focus(pane));
                        actions.push(Action::Transfer(kind));
                        ui.close();
                    }
                }
            });
            }
            for (label, action, enabled) in [
                (crate::i18n::text("Копировать"), Action::Copy(false), true),
                (crate::i18n::text("Вырезать"), Action::Copy(true), true),
                (crate::i18n::text("Вставить"), Action::Paste(false), idle),
                (
                    crate::i18n::text("Переименовать…"),
                    Action::Rename,
                    idle && !tab.selected.is_empty(),
                ),
                (
                    crate::i18n::text("Создать архив…"),
                    Action::CreateArchive,
                    !tab.selected.is_empty(),
                ),
                (
                    if path.remote() {
                        crate::i18n::text("Удалить навсегда…")
                    } else {
                        crate::i18n::text("В корзину…")
                    },
                    Action::Trash,
                    idle,
                ),
                (crate::i18n::text("Свойства…"), Action::Properties,
                    !tab.selected.is_empty() && tab.selected.iter().all(|path| !path.remote())),
                (crate::i18n::text("Копировать путь"), Action::CopyPath, true),
                (crate::i18n::text("Показать в системе"), Action::Reveal, !path.remote()),
            ] {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    actions.push(Action::Focus(pane));
                    actions.push(action);
                    ui.close();
                }
            }
        });
}
