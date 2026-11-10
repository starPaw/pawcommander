use crate::theme::Palette;
use eframe::egui::{self, Id, LayerId, Pos2, Rect, Stroke, StrokeKind};

pub struct DragTab {
    pub id: u64,
    pub pane: usize,
    pub label: String,
}

pub struct TabDrop {
    pub source: usize,
    pub id: u64,
    pub destination: usize,
    /// Insertion slot in the destination before the source tab is removed.
    pub slot: usize,
}

struct Strip {
    pane: usize,
    layer: LayerId,
    rect: Rect,
    pane_rect: Rect,
    count: usize,
    tabs: Vec<(usize, Rect)>,
}

impl Strip {
    fn slot(&self, pos: Pos2) -> (usize, f32) {
        for &(index, rect) in &self.tabs {
            if pos.x < rect.center().x {
                return (index, rect.left());
            }
            if pos.x <= rect.right() {
                return (index + 1, rect.right());
            }
        }
        (
            self.count,
            self.tabs
                .last()
                .map_or(self.rect.left(), |(_, r)| r.right()),
        )
    }
}

#[derive(Default)]
pub struct TabDropTargets(Vec<Strip>);

impl TabDropTargets {
    pub fn add_strip(
        &mut self,
        ui: &egui::Ui,
        pane: usize,
        rect: Rect,
        count: usize,
        tabs: Vec<(usize, Rect)>,
    ) {
        self.0.push(Strip {
            pane,
            layer: ui.layer_id(),
            rect: rect.intersect(ui.clip_rect()),
            pane_rect: Rect::NOTHING,
            count,
            tabs,
        });
    }

    pub fn set_pane_rect(&mut self, ui: &egui::Ui, pane: usize, rect: Rect) {
        if let Some(strip) = self.0.iter_mut().find(|s| s.pane == pane) {
            strip.pane_rect = rect.intersect(ui.clip_rect());
        }
    }

    pub fn finish(&self, ctx: &egui::Context, enabled: bool) -> Option<TabDrop> {
        let drag = egui::DragAndDrop::payload::<DragTab>(ctx)?;
        let (pos, released, focused) = ctx.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.button_released(egui::PointerButton::Primary),
                i.focused,
            )
        });
        if released || !enabled || !focused {
            egui::DragAndDrop::clear_payload(ctx);
        }
        let pos = pos.filter(|_| enabled && focused)?;
        let layer = ctx.layer_id_at(pos);
        let strip = self.0.iter().find(|s| {
            Some(s.layer) == layer
                && (s.rect.contains(pos) || (s.pane != drag.pane && s.pane_rect.contains(pos)))
        });
        let colors = Palette::from_ctx(ctx);
        let painter = ctx.layer_painter(LayerId::new(egui::Order::Tooltip, Id::new("tab_drop")));
        let target = strip.map(|strip| {
            let slot = if strip.rect.contains(pos) {
                let (slot, x) = strip.slot(pos);
                painter.vline(
                    x.clamp(strip.rect.left() + 1.0, strip.rect.right() - 1.0),
                    strip.rect.y_range().shrink(2.0),
                    Stroke::new(2.5, colors.blue),
                );
                slot
            } else {
                painter.rect_stroke(
                    strip.pane_rect.shrink(1.0),
                    10,
                    Stroke::new(2.0, colors.blue),
                    StrokeKind::Inside,
                );
                strip.count
            };
            TabDrop {
                source: drag.pane,
                id: drag.id,
                destination: strip.pane,
                slot,
            }
        });
        if !released {
            ctx.set_cursor_icon(if target.is_some() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::NoDrop
            });
            let text = painter.layout(
                drag.label.clone(),
                egui::FontId::proportional(13.0),
                colors.ink,
                180.0,
            );
            let rect = Rect::from_min_size(
                pos + egui::vec2(12.0, 16.0),
                text.size() + egui::vec2(18.0, 10.0),
            );
            let rect = rect.translate(egui::vec2(
                (ctx.screen_rect().right() - rect.right()).min(0.0),
                (ctx.screen_rect().bottom() - rect.bottom()).min(0.0),
            ));
            painter.rect(
                rect,
                6,
                colors.raised,
                Stroke::new(1.0, colors.line),
                StrokeKind::Inside,
            );
            painter.galley(rect.min + egui::vec2(9.0, 5.0), text, colors.ink);
        }
        target.filter(|_| released)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_uses_visible_tab_indices_and_can_append_after_hidden_tabs() {
        let rect = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 32.0));
        let strip = Strip {
            pane: 1,
            layer: LayerId::background(),
            rect,
            pane_rect: rect,
            count: 6,
            tabs: vec![
                (2, Rect::from_min_max(rect.min, egui::pos2(44.0, 32.0))),
                (
                    3,
                    Rect::from_min_max(egui::pos2(44.0, 0.0), egui::pos2(145.0, 32.0)),
                ),
            ],
        };
        assert_eq!(strip.slot(egui::pos2(10.0, 16.0)), (2, 0.0));
        assert_eq!(strip.slot(egui::pos2(35.0, 16.0)), (3, 44.0));
        assert_eq!(strip.slot(egui::pos2(120.0, 16.0)), (4, 145.0));
        assert_eq!(strip.slot(egui::pos2(180.0, 16.0)).0, 6);
    }
}
