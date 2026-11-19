use super::actions::Action;
use super::file_name;
use super::pane_view::workspace_border;
use super::tabs::Tab;
use crate::drag_drop::DropTargets;
use crate::icons;
use crate::icons::Icon;
use crate::theme::{paint_focus, Palette};
use eframe::egui;
use eframe::egui::{Color32, FontId, Sense, Stroke, Vec2};

pub(super) fn address_bar(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    index: usize,
    actions: &mut Vec<Action>,
    targets: &mut DropTargets,
) {
    let colors = Palette::from_ctx(ui.ctx());
    let editing = tab.edit_address;
    let fill = colors.bg.lerp_to_gamma(colors.raised, 0.25);
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .stroke(Stroke::new(
            1.0,
            if tab.address_error.is_some() {
                colors.error
            } else if editing {
                colors.blue
            } else {
                workspace_border(colors)
            },
        ));
    let row_size = Vec2::new(ui.available_width(), icons::COMPACT_SIZE + 4.0);
    ui.allocate_ui_with_layout(
        row_size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (icon, label, delta, enabled) in [
                (
                    Icon::Back,
                    crate::i18n::text("Назад"),
                    -1,
                    tab.history_index > 0,
                ),
                (
                    Icon::Forward,
                    crate::i18n::text("Вперёд"),
                    1,
                    tab.history_index + 1 < tab.history.len(),
                ),
            ] {
                let button = ui
                    .add_enabled_ui(enabled, |ui| icons::compact_button(ui, icon, label, false))
                    .inner;
                if button.clicked() {
                    actions.push(Action::History(index, delta));
                }
            }
            ui.add_space(6.0);
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let spacing = ui.spacing_mut();
                    spacing.item_spacing.x = 3.0;
                    spacing.interact_size.y = icons::COMPACT_SIZE;
                    let trailing = icons::COMPACT_SIZE + spacing.item_spacing.x;
                    if editing {
                        edit_address(ui, tab, index, trailing, actions);
                    } else {
                        let width = (ui.available_width() - trailing).max(40.0);
                        let scroll = egui::ScrollArea::horizontal()
                            .id_salt("breadcrumbs")
                            .max_width(width)
                            .auto_shrink([false, true])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .stick_to_right(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let home = dirs::home_dir();
                                    let ancestors: Vec<_> = tab.path.ancestors().collect();
                                    for path in ancestors.iter().rev() {
                                        let current = *path == tab.path;
                                        let home = home
                                            .as_deref()
                                            .is_some_and(|home| path.local() == Some(home));
                                        let (label, icon) = if home {
                                            (crate::i18n::text("Домой").into(), Some(Icon::Home))
                                        } else if path.parent().is_none() && !path.remote() {
                                            (
                                                if cfg!(windows) {
                                                    file_name(path)
                                                } else {
                                                    crate::i18n::text("Диск").into()
                                                },
                                                Some(Icon::Drive),
                                            )
                                        } else {
                                            (file_name(path), None)
                                        };
                                        let response =
                                            breadcrumb(ui, &label, icon, current, colors);
                                        targets.add(ui, response.rect, index, path, true);
                                        if response.clicked() {
                                            actions.push(Action::Navigate(index, (*path).clone()));
                                        }
                                        if current && crate::terminal::available(path) {
                                            response.context_menu(|ui| {
                                                if ui
                                                    .button(
                                                        crate::shortcuts::Command::OpenTerminal
                                                            .label(),
                                                    )
                                                    .clicked()
                                                {
                                                    actions.push(Action::OpenTerminal(
                                                        (*path).clone(),
                                                    ));
                                                    ui.close();
                                                }
                                            });
                                        }
                                        response.on_hover_text(crate::tr_format!(
                                            "Перейти в {}",
                                            "Go to {}",
                                            path.display()
                                        ));
                                        if !current {
                                            let (r, _) = ui.allocate_exact_size(
                                                Vec2::new(8.0, 20.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().add(egui::Shape::line(
                                                vec![
                                                    r.center() + Vec2::new(-2.0, -4.0),
                                                    r.center() + Vec2::new(2.0, 0.0),
                                                    r.center() + Vec2::new(-2.0, 4.0),
                                                ],
                                                Stroke::new(1.2, colors.muted),
                                            ));
                                        }
                                    }
                                });
                            });
                        if scroll.state.clamped_offset().x > 0.5 {
                            let rect = scroll.inner_rect;
                            let width = rect.width().min(16.0);
                            let mut fade = egui::Mesh::default();
                            fade.colored_vertex(rect.left_top(), fill);
                            fade.colored_vertex(rect.left_bottom(), fill);
                            fade.colored_vertex(
                                rect.left_bottom() + Vec2::new(width, 0.0),
                                Color32::TRANSPARENT,
                            );
                            fade.colored_vertex(
                                rect.left_top() + Vec2::new(width, 0.0),
                                Color32::TRANSPARENT,
                            );
                            fade.add_triangle(0, 1, 2);
                            fade.add_triangle(0, 2, 3);
                            ui.painter().add(egui::Shape::mesh(fade));
                        }
                        if icons::compact_button(
                            ui,
                            Icon::ChevronDown,
                            crate::i18n::text("Ввести путь вручную"),
                            false,
                        )
                        .clicked()
                        {
                            tab.begin_address();
                            actions.push(Action::Focus(index));
                        }
                    }
                });
            });
        },
    );
    if let Some(error) = &tab.address_error {
        ui.colored_label(colors.error, error);
    }
}

pub(super) fn edit_address(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    index: usize,
    trailing: f32,
    actions: &mut Vec<Action>,
) {
    let field_id = ui.make_persistent_id(("path_input", tab.id));
    let popup_id = field_id.with("suggestions");
    let mut suggestions = tab
        .address_completion
        .suggestions(&tab.address, &tab.path, ui.ctx());
    let breadcrumbs = icons::compact_button(
        ui,
        Icon::Breadcrumbs,
        crate::i18n::text("Вернуться к хлебным крошкам · Esc"),
        false,
    );
    let cancel = breadcrumbs.clicked();

    let focused = ui.memory(|memory| memory.has_focus(field_id));
    let (enter, complete, step) = ui.input_mut(|input| {
        if !focused || input.modifiers != egui::Modifiers::NONE {
            return (false, false, None);
        }
        let enter = input.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        if suggestions.is_empty() {
            return (enter, false, None);
        }
        let complete = input.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
        let step = if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            Some(1)
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            Some(-1)
        } else {
            None
        };
        (enter, complete, step)
    });
    let margin = egui::Margin::symmetric(2, 0);
    let width = (ui.available_width() - trailing - margin.sum().x).max(24.0);
    let mut output = egui::TextEdit::singleline(&mut tab.address)
        .id(field_id)
        .font(egui::TextStyle::Body)
        .hint_text(crate::i18n::text("Путь к папке или sftp://сервер/папка"))
        .align(egui::Align2::LEFT_CENTER)
        .frame(egui::Frame::NONE)
        .margin(margin)
        .min_size(Vec2::new(0.0, icons::COMPACT_SIZE))
        .desired_width(width)
        .return_key(None)
        .lock_focus(!suggestions.is_empty())
        .show(ui);
    let response = &output.response;
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_label(crate::i18n::text("Путь к папке"));
    });
    let mut cursor_range = None;
    if tab.focus_address {
        response.request_focus();
        cursor_range = Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(tab.address.chars().count()),
        ));
        tab.focus_address = false;
    }
    if response.changed() {
        tab.address_completion
            .address_changed(&tab.address, &tab.path);
        tab.address_error = None;
        suggestions = tab
            .address_completion
            .suggestions(&tab.address, &tab.path, ui.ctx());
    }
    if let Some(step) = step.filter(|_| !response.changed() && !suggestions.is_empty()) {
        let count = suggestions.len();
        tab.address_completion.selected = Some(match tab.address_completion.selected {
            Some(selected) => (selected as isize + step).rem_euclid(count as isize) as usize,
            None if step > 0 => 0,
            None => count - 1,
        });
    }
    let completed = if let Some(item) = (complete && !response.changed())
        .then(|| suggestions.get(tab.address_completion.selected.unwrap_or(0)))
        .flatten()
    {
        tab.address_completion.remember_completion(&item.path);
        tab.address = item.path.display();
        let separator = if item.path.remote() {
            '/'
        } else {
            std::path::MAIN_SEPARATOR
        };
        if !tab.address.ends_with(separator) {
            tab.address.push(separator);
        }
        tab.address_completion.selected = None;
        tab.address_error = None;
        let end = egui::text::CCursor::new(tab.address.chars().count());
        cursor_range = Some(egui::text::CCursorRange::one(end));
        suggestions = tab
            .address_completion
            .suggestions(&tab.address, &tab.path, ui.ctx());
        ui.ctx().request_repaint();
        true
    } else {
        false
    };
    if let Some(range) = cursor_range {
        output.state.cursor.set_char_range(Some(range));
        output.state.store(ui.ctx(), field_id);
    }
    let go_button = icons::compact_button(
        ui,
        Icon::Forward,
        crate::i18n::text("Перейти по введённому пути · Enter"),
        false,
    );
    let go = go_button.clicked();

    let escape =
        (focused || response.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Escape));
    let mut destination = None;
    if !cancel && !escape && !suggestions.is_empty() {
        if response.has_focus() {
            egui::Popup::open_id(ui.ctx(), popup_id);
        }
        let mut anchor = response.clone();
        anchor.rect = breadcrumbs
            .rect
            .union(go_button.rect)
            .expand2(Vec2::new(7.0, 4.0));
        destination = egui::Popup::from_response(&anchor)
            .id(popup_id)
            .open_memory(None)
            .width(anchor.rect.width())
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(ui.available_width().max(148.0));
                ui.label(
                    egui::RichText::new(crate::i18n::text("Подсказки пути"))
                        .small()
                        .weak(),
                )
                .on_hover_text(crate::i18n::text(
                    "↑↓ выбрать · Tab дополнить · Enter открыть",
                ));
                for (i, item) in suggestions.iter().enumerate() {
                    let selected = tab.address_completion.selected == Some(i);
                    if ui
                        .add(egui::Button::new(&item.name).selected(selected).truncate())
                        .on_hover_text(item.path.display())
                        .clicked()
                    {
                        return Some(item.path.clone());
                    }
                }
                None
            })
            .and_then(|response| response.inner);
    }
    if destination.is_none() && (enter || go) && !cancel && !escape {
        let selected = tab
            .address_completion
            .selected
            .and_then(|i| suggestions.get(i));
        if let (true, Some(item)) = (enter, selected) {
            destination = Some(item.path.clone());
        } else {
            match tab.address_completion.resolve(&tab.address, &tab.path) {
                Ok(path) => destination = Some(path),
                Err(error) => {
                    tab.address_error = Some(error);
                    response.request_focus();
                }
            }
        }
    }
    if cancel || escape || destination.is_some() {
        tab.address = tab.path.display();
        tab.edit_address = false;
        tab.address_error = None;
        tab.address_completion.reset();
        response.surrender_focus();
    }
    if !tab.edit_address || suggestions.is_empty() {
        egui::Popup::close_id(ui.ctx(), popup_id);
    }
    if let Some(path) = destination {
        actions.push(Action::Navigate(index, path));
    }
    if tab.edit_address {
        if completed {
            // egui may already be advancing focus to the next widget on Tab.
            // Restore focus after the buttons and popup have registered.
            response.request_focus();
        }
        if response.has_focus() {
            actions.push(Action::Focus(index));
        }
    }
}

pub(super) fn breadcrumb(
    ui: &mut egui::Ui,
    label: &str,
    icon: Option<Icon>,
    current: bool,
    colors: Palette,
) -> egui::Response {
    let color = if current { colors.ink } else { colors.muted };
    let text = ui
        .painter()
        .layout_no_wrap(label.into(), FontId::proportional(13.0), color);
    let icon_width = if icon.is_some() { 20.0 } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(text.size().x + icon_width + 12.0, 28.0),
        Sense::click(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    if response.gained_focus() {
        response.scroll_to_me(Some(egui::Align::Center));
    }
    let highlight = response.hovered() || response.has_focus();
    if highlight {
        ui.painter().rect_filled(rect, 5, colors.raised);
    }
    if let Some(icon) = icon {
        icons::paint(
            ui,
            egui::Rect::from_center_size(
                rect.left_center() + egui::vec2(14.0, 0.0),
                Vec2::splat(17.0),
            ),
            icon,
            color,
        );
    }
    let color = if highlight { colors.ink } else { color };
    ui.painter().galley_with_override_text_color(
        egui::pos2(
            rect.left() + 6.0 + icon_width,
            rect.center().y - text.size().y / 2.0,
        ),
        text,
        color,
    );
    paint_focus(ui, &response, 5);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
