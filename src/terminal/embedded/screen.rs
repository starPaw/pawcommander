use super::{
    palette,
    session::{Events, Size},
};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor};
use alacritty_terminal::Term;
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};

pub(super) struct Screen {
    pub(super) size: Size,
    rect: Rect,
    cell: Vec2,
    font: FontId,
}

impl Screen {
    pub(super) fn measure(ui: &egui::Ui, rect: Rect) -> Self {
        let font = FontId::monospace(13.0);
        let pixel = ui.ctx().pixels_per_point();
        let glyph = ui
            .painter()
            .layout_no_wrap("M".into(), font.clone(), Color32::WHITE);
        let cell = Vec2::new(glyph.size().x.max(1.0), glyph.size().y.max(1.0));
        let size = Size {
            columns: (rect.width() / cell.x).floor().clamp(2.0, 512.0) as u16,
            rows: (rect.height() / cell.y).floor().clamp(2.0, 160.0) as u16,
            cell_width: (cell.x * pixel).round().max(1.0) as u16,
            cell_height: (cell.y * pixel).round().max(1.0) as u16,
        };
        Self {
            size,
            rect,
            cell,
            font,
        }
    }

    pub(super) fn interact(
        &self,
        ui: &egui::Ui,
        response: &egui::Response,
        terminal: &mut Term<Events>,
        wheel_remainder: &mut f32,
    ) {
        if response.hovered() {
            let scroll = ui.input_mut(|input| std::mem::take(&mut input.smooth_scroll_delta.y));
            *wheel_remainder += scroll / self.cell.y;
            let lines = wheel_remainder.trunc() as i32;
            if lines != 0 {
                *wheel_remainder -= lines as f32;
                terminal.scroll_display(Scroll::Delta(lines));
            }
        }
        if response.clicked() {
            terminal.selection = None;
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(origin) = ui.input(|input| input.pointer.press_origin())
        {
            let (point, side) = self.point(origin, terminal);
            terminal.selection = Some(Selection::new(SelectionType::Simple, point, side));
        }
        if response.dragged_by(egui::PointerButton::Primary)
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let (point, side) = self.point(pointer, terminal);
            if let Some(selection) = &mut terminal.selection {
                selection.update(point, side);
            }
        }
    }

    fn point(&self, position: Pos2, terminal: &Term<Events>) -> (Point, Side) {
        let x = ((position.x - self.rect.min.x) / self.cell.x)
            .clamp(0.0, terminal.columns() as f32 - 0.01);
        let row = ((position.y - self.rect.min.y) / self.cell.y)
            .floor()
            .clamp(0.0, terminal.screen_lines() as f32 - 1.0) as i32;
        let line = Line(row - terminal.grid().display_offset() as i32);
        (
            Point::new(line, Column(x as usize)),
            if x.fract() >= 0.5 {
                Side::Right
            } else {
                Side::Left
            },
        )
    }

    fn cell_rect(&self, line: i32, column: usize) -> Rect {
        let start =
            self.rect.min + Vec2::new(column as f32 * self.cell.x, line as f32 * self.cell.y);
        Rect::from_min_size(start, self.cell)
    }

    pub(super) fn paint(&self, ui: &egui::Ui, session: &super::session::Session, focused: bool) {
        // Snapshot the visible cells and release the PTY parser lock before
        // the glyph layout work on the UI thread.
        let (colors, cursor, offset, cells) = {
            let terminal = session.terminal.lock();
            let content = terminal.renderable_content();
            let selection = content.selection;
            let cells = content
                .display_iter
                .map(|entry| {
                    let selected = selection
                        .as_ref()
                        .is_some_and(|span| span.contains(entry.point));
                    (entry.point, entry.cell.clone(), selected)
                })
                .collect::<Vec<_>>();
            (
                *content.colors,
                content.cursor,
                content.display_offset as i32,
                cells,
            )
        };
        let painter = ui
            .painter()
            .with_clip_rect(ui.clip_rect().intersect(self.rect.expand(1.0)));
        let default_bg = palette::background(&colors);
        painter.rect_filled(self.rect, 3.0, default_bg);
        let selection = Color32::from_rgb(64, 71, 111);
        let mut glyphs = Vec::new();
        for (point, cell, selected) in cells {
            let rect = self.cell_rect(point.line.0 + offset, point.column.0);
            if !rect.intersects(self.rect) {
                continue;
            }
            let mut fg = palette::cell(&colors, cell.fg, cell.flags);
            let mut bg = palette::cell(&colors, cell.bg, Flags::empty());
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selected {
                bg = selection;
                fg = Color32::WHITE;
            }
            if bg != default_bg {
                painter.rect_filled(rect, 0.0, bg);
            }
            if cell.flags.intersects(
                Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            if cell.c != ' '
                || cell
                    .flags
                    .intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
            {
                glyphs.push((rect, cell, fg));
            }
        }
        for (rect, cell, fg) in glyphs {
            if cell.c != ' ' {
                let mut text = String::from(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
                painter.text(
                    rect.left_top(),
                    egui::Align2::LEFT_TOP,
                    text,
                    self.font.clone(),
                    fg,
                );
            }
            let width = self.cell.x
                * if cell.flags.contains(Flags::WIDE_CHAR) {
                    2.0
                } else {
                    1.0
                };
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                let bottom = rect.left_bottom() - Vec2::new(0.0, 1.0);
                painter.line_segment(
                    [bottom, bottom + Vec2::new(width, 0.0)],
                    Stroke::new(1.0, fg),
                );
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                let middle = rect.left_center();
                painter.line_segment(
                    [middle, middle + Vec2::new(width, 0.0)],
                    Stroke::new(1.0, fg),
                );
            }
        }
        if !session.state.finished() && cursor.shape != CursorShape::Hidden {
            let cursor_rect = self.cell_rect(cursor.point.line.0 + offset, cursor.point.column.0);
            if cursor_rect.intersects(self.rect) {
                let color = palette::indexed(&colors, NamedColor::Cursor as usize);
                match cursor.shape {
                    CursorShape::Beam => painter.line_segment(
                        [cursor_rect.left_top(), cursor_rect.left_bottom()],
                        Stroke::new(1.5, color),
                    ),
                    CursorShape::Underline => painter.line_segment(
                        [cursor_rect.left_bottom(), cursor_rect.right_bottom()],
                        Stroke::new(1.5, color),
                    ),
                    _ => painter.rect_stroke(
                        cursor_rect,
                        0.0,
                        Stroke::new(1.0, color),
                        StrokeKind::Inside,
                    ),
                };
                if focused {
                    ui.ctx().output_mut(|output| {
                        output.ime = Some(egui::output::IMEOutput {
                            purpose: Default::default(),
                            rect: self.rect,
                            cursor_rect,
                            should_interrupt_composition: false,
                        });
                    });
                }
            }
        }
        if focused {
            painter.rect_stroke(
                self.rect,
                3.0,
                Stroke::new(1.0, crate::theme::Palette::from_ctx(ui.ctx()).blue),
                StrokeKind::Inside,
            );
        }
    }
}
