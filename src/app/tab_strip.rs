use super::actions::Action;
use super::file_name;
use super::pane_view::workspace_border;
use super::tabs::Pane;
use crate::drag_drop::DropTargets;
use crate::icons;
use crate::icons::Icon;
use crate::shortcuts::Command;
use crate::tab_drag::{DragTab, TabDropTargets};
use crate::theme::{paint_focus, Palette};
use eframe::egui;
use eframe::egui::{Color32, FontId, Sense, Stroke, Vec2};
use std::time::Duration;

pub(super) fn tab_strip(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    index: usize,
    actions: &mut Vec<Action>,
    targets: &mut DropTargets,
    tab_targets: &mut TabDropTargets,
    idle: bool,
) -> (egui::Rect, Option<egui::Rect>) {
    const HEIGHT: f32 = 34.0;
    const PLUS_WIDTH: f32 = 35.0;
    const GAP: f32 = 2.0;
    let colors = Palette::from_ctx(ui.ctx());
    let border = workspace_border(colors);
    let active_id = ui.scope_id().with("last_active_tab");
    let tab_id = pane.tab().id;
    let changed = ui.data_mut(|data| {
        let previous = data.get_temp::<(u64, usize)>(active_id);
        let current = (tab_id, pane.current);
        data.insert_temp(active_id, current);
        previous != Some(current)
    });
    let mut visible_tabs = Vec::new();
    let mut active_tab = None;
    let mut clicked_tab = None;
    let mut tab_context_menu = None;
    let strip = ui
        .horizontal(|ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = GAP;
            let available = (ui.available_width() - PLUS_WIDTH - GAP).max(20.0);
            let scroll = egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .max_width(available)
                .scroll_source(
                    egui::scroll_area::ScrollSource::SCROLL_BAR
                        | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
                )
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([true, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    let viewport = ui.clip_rect();
                    let dragging = egui::DragAndDrop::has_payload_of_type::<DragTab>(ui.ctx());
                    let group = ui.horizontal(|ui| {
                        for i in 0..pane.tabs.len() {
                            let tab = &pane.tabs[i];
                            let path = &tab.path;
                            let selected = pane.current == i;
                            let name = file_name(path);
                            let text = ui.painter().layout_no_wrap(
                                name.clone(),
                                FontId::proportional(13.0),
                                colors.ink,
                            );
                            let width = (text.size().x + 66.0).clamp(108.0, 182.0);
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::hover());
                            let clipped = rect.intersect(viewport);
                            if clipped.width() > 0.0 {
                                visible_tabs.push((i, clipped));
                            }
                            let close_rect = egui::Rect::from_center_size(
                                rect.right_center() - Vec2::new(16.0, 0.0),
                                Vec2::splat(24.0),
                            );
                            let can_close = pane.can_close_tab(i);
                            let body = if can_close {
                                egui::Rect::from_min_max(
                                    rect.min,
                                    egui::pos2(close_rect.left(), rect.bottom()),
                                )
                            } else {
                                rect
                            };
                            let response = ui.interact(
                                body,
                                ui.scope_id().with(("tab", tab.id)),
                                Sense::click_and_drag(),
                            );
                            response.widget_info(|| {
                                let label = if tab.pinned {
                                    crate::tr_format!("{name} (закреплена)", "{name} (pinned)")
                                } else {
                                    name.clone()
                                };
                                egui::WidgetInfo::selected(
                                    egui::Role::Button,
                                    true,
                                    selected,
                                    label,
                                )
                            });
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_role(egui::accesskit::Role::Tab);
                                node.set_selected(selected);
                            });
                            if selected {
                                active_tab = Some(rect);
                            } else {
                                let fill = if response.hovered() {
                                    colors.bg.lerp_to_gamma(colors.raised, 0.5)
                                } else {
                                    colors.surface
                                };
                                let mut outline = rect;
                                if i == 0 {
                                    outline.min.x -= 1.0;
                                }
                                paint_tab(ui, outline, fill, border);
                            }
                            let icon = if path.remote() {
                                Icon::Server
                            } else {
                                Icon::Folder
                            };
                            icons::paint(
                                ui,
                                egui::Rect::from_center_size(
                                    rect.left_center() + Vec2::new(17.0, 0.0),
                                    Vec2::splat(19.0),
                                ),
                                icon,
                                icons::tint(icon, colors),
                            );
                            let text_right = if tab.pinned {
                                close_rect.left()
                            } else {
                                body.right()
                            };
                            let text_rect = egui::Rect::from_min_max(
                                rect.min + Vec2::new(32.0, 0.0),
                                egui::pos2(text_right - 3.0, rect.bottom()),
                            );
                            let text = if text.size().x > text_rect.width() {
                                let mut job = egui::text::LayoutJob::simple(
                                    name.clone(),
                                    FontId::proportional(13.0),
                                    colors.ink,
                                    text_rect.width(),
                                );
                                job.wrap.max_rows = 1;
                                job.wrap.break_anywhere = true;
                                ui.painter().layout_job(job)
                            } else {
                                text
                            };
                            ui.painter()
                                .with_clip_rect(text_rect.intersect(ui.clip_rect()))
                                .galley(
                                    egui::pos2(
                                        rect.left() + 32.0,
                                        rect.center().y - text.size().y / 2.0,
                                    ),
                                    text,
                                    colors.ink,
                                );
                            targets.add(ui, rect, index, path, true);
                            if (changed && selected) || response.gained_focus() {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                            paint_focus(ui, &response, 5);
                            if idle && response.drag_started_by(egui::PointerButton::Primary) {
                                egui::DragAndDrop::set_payload(
                                    ui.ctx(),
                                    DragTab {
                                        id: tab.id,
                                        pane: index,
                                        label: name.clone(),
                                        pinned: tab.pinned,
                                    },
                                );
                            } else if response.clicked() {
                                clicked_tab = Some(i);
                                actions.push(Action::Focus(index));
                            }
                            if !dragging {
                                response.context_menu(|ui| {
                                    tab_context_menu = Some(response.id);
                                    let pin = if tab.pinned {
                                        crate::i18n::text!("Открепить вкладку")
                                    } else {
                                        crate::i18n::text!("Закрепить вкладку")
                                    };
                                    let reopen = Command::ReopenTab.label();
                                    for (label, action, enabled) in [
                                        (pin, Action::TogglePinTab(index, i), true),
                                        (
                                            Command::DuplicateTab.label(),
                                            Action::DuplicateTab(index, i),
                                            true,
                                        ),
                                        (
                                            Command::CloseTab.label(),
                                            Action::CloseTab(index, i),
                                            can_close,
                                        ),
                                        (
                                            reopen,
                                            Action::ReopenTab(index),
                                            !pane.closed_tabs.is_empty(),
                                        ),
                                    ] {
                                        if ui
                                            .add_enabled(enabled, egui::Button::new(label))
                                            .clicked()
                                        {
                                            actions.push(action);
                                            ui.close();
                                        }
                                    }
                                });
                                response
                                    .on_hover_text(path.display())
                                    .on_hover_cursor(egui::CursorIcon::Grab);
                            }
                            if tab.pinned {
                                icons::paint(ui, close_rect.shrink(5.0), Icon::Pin, colors.mauve);
                            } else if can_close {
                                let close = ui.interact(
                                    close_rect,
                                    ui.scope_id().with(("close_tab", tab.id)),
                                    Sense::click(),
                                );
                                close.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::Role::Button,
                                        true,
                                        crate::tr_format!(
                                            "Закрыть вкладку {name}",
                                            "Close tab {name}"
                                        ),
                                    )
                                });
                                if close.hovered() {
                                    ui.painter().rect_filled(close_rect, 5, colors.selection);
                                }
                                icons::paint(ui, close_rect.shrink(5.0), Icon::Close, colors.muted);
                                if close.gained_focus() {
                                    close.scroll_to_me(Some(egui::Align::Center));
                                }
                                paint_focus(ui, &close, 5);
                                if close.clicked() {
                                    actions.push(Action::CloseTab(index, i));
                                }
                            }
                        }
                    });
                    // Scroll overflowing strips while hovering an edge with a tab.
                    if dragging {
                        let pointer = ui.input(|i| {
                            i.pointer
                                .primary_down()
                                .then(|| i.pointer.hover_pos())
                                .flatten()
                        });
                        if let Some(pos) = pointer.filter(|pos| viewport.contains(*pos)) {
                            let delta = if pos.x < viewport.left() + 18.0
                                && group.response.rect.left() < viewport.left()
                            {
                                12.0
                            } else if pos.x > viewport.right() - 18.0
                                && group.response.rect.right() > viewport.right()
                            {
                                -12.0
                            } else {
                                0.0
                            };
                            if delta != 0.0 {
                                ui.scroll_with_delta_animation(
                                    egui::vec2(delta, 0.0),
                                    egui::style::ScrollAnimation::none(),
                                );
                                ui.ctx().request_repaint_after(Duration::from_millis(16));
                            }
                        }
                    }
                });
            active_tab = active_tab
                .map(|rect| rect.intersect(scroll.inner_rect))
                .filter(|rect| rect.is_positive());
            let (plus, response) =
                ui.allocate_exact_size(Vec2::new(PLUS_WIDTH, HEIGHT), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::Button,
                    true,
                    crate::i18n::text!("Новая вкладка"),
                )
            });
            paint_tab(
                ui,
                plus,
                if response.hovered() {
                    colors.raised
                } else {
                    colors.surface
                },
                border,
            );
            icons::paint(
                ui,
                egui::Rect::from_center_size(plus.center(), Vec2::splat(18.0)),
                Icon::Plus,
                colors.ink,
            );
            paint_focus(ui, &response, 5);
            if response
                .on_hover_text(crate::i18n::text!("Новая вкладка"))
                .clicked()
            {
                actions.push(Action::NewTab(index));
            }
        })
        .response
        .rect;
    if let Some(index) = clicked_tab {
        pane.select_tab(index);
    }
    pane.tab_context_menu = tab_context_menu;
    tab_targets.add_strip(
        ui,
        index,
        strip,
        pane.tabs.len(),
        pane.pinned_count(),
        visible_tabs,
    );
    (strip, active_tab)
}

pub(super) fn paint_tab(ui: &egui::Ui, rect: egui::Rect, fill: Color32, line: Color32) {
    ui.painter().rect(
        rect,
        egui::CornerRadius {
            nw: 9,
            ne: 9,
            sw: 0,
            se: 0,
        },
        fill,
        Stroke::new(1.0, line),
        egui::StrokeKind::Inside,
    );
}

pub(super) fn paint_pane_outline(
    painter: &egui::Painter,
    background: egui::layers::ShapeIdx,
    body: egui::Rect,
    active_tab: Option<egui::Rect>,
    fill: Color32,
    stroke: Stroke,
) {
    use egui::epaint::tessellator::path::add_circle_quadrant;
    use egui::{pos2, Shape};

    const PANE_RADIUS: u8 = 12;
    let body_corners = egui::CornerRadius {
        nw: 0,
        ..PANE_RADIUS.into()
    };
    let Some(mut tab) = active_tab else {
        painter.set(background, Shape::rect_filled(body, body_corners, fill));
        painter.rect_stroke(body, body_corners, stroke, egui::StrokeKind::Middle);
        return;
    };
    let flush_left = tab.left() - body.left() <= 1.0;
    if flush_left {
        tab.min.x = body.left();
    }
    let tab_radius = 9.0_f32.min((tab.width() / 2.0 - 0.5).max(0.0)) as u8;
    tab.max.y = body.top() + 1.0;
    // Fill the two rectangles separately because the shared outline is concave.
    painter.set(
        background,
        Shape::Vec(vec![
            Shape::rect_filled(body, body_corners, fill),
            Shape::rect_filled(
                tab,
                egui::CornerRadius {
                    nw: tab_radius,
                    ne: tab_radius,
                    sw: 0,
                    se: 0,
                },
                fill,
            ),
        ]),
    );

    let mut points = Vec::new();
    let r = f32::from(tab_radius);
    let p = f32::from(PANE_RADIUS);
    add_circle_quadrant(&mut points, tab.left_top() + Vec2::new(r, r), r, 2.0);
    add_circle_quadrant(&mut points, tab.right_top() + Vec2::new(-r, r), r, 3.0);
    points.push(pos2(tab.right(), body.top()));
    add_circle_quadrant(&mut points, body.right_top() + Vec2::new(-p, p), p, 3.0);
    add_circle_quadrant(&mut points, body.right_bottom() - Vec2::splat(p), p, 0.0);
    add_circle_quadrant(&mut points, body.left_bottom() + Vec2::new(p, -p), p, 1.0);
    if !flush_left {
        points.push(body.left_top());
        points.push(pos2(tab.left(), body.top()));
    }
    painter.add(Shape::closed_line(points, stroke));
}
