use super::{human_size, node_detail, node_name, row_size, text, Analysis, Coverage, Kind, NodeId};
use crate::theme::Palette;
use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Vec2};

const MAX_TILES: usize = 14;

struct Tile {
    id: Option<NodeId>,
    bytes: u64,
    caption: String,
}

pub(super) fn show(
    ui: &mut egui::Ui,
    analysis: &Analysis,
    folder: NodeId,
    run: u64,
    height: f32,
) -> NodeId {
    let node = &analysis.nodes[folder];
    let mut selected = folder;
    ui.horizontal(|ui| {
        if let Some(parent) = node.parent
            && ui.button(text!("В родительскую папку")).clicked()
        {
            selected = parent;
        }
        if node.parent.is_some() && node.coverage == Coverage::Partial {
            ui.label(row_size(node))
                .on_hover_text(node_detail(analysis, node));
        }
        let label = analysis
            .folder
            .join(&node.path)
            .display()
            .escape_debug()
            .to_string();
        ui.add(egui::Label::new(&label).truncate().selectable(true))
            .on_hover_text(&label);
    });
    let tiles = tiles(analysis, folder);
    if tiles.is_empty() {
        ui.label(text!("В этой папке пока нет учтённых блоков"));
        return selected;
    }
    let size = Vec2::new(ui.available_width(), (height - 36.0).max(60.0));
    let (bounds, _) = ui.allocate_exact_size(size, Sense::hover());
    let palette = Palette::from_ctx(ui.ctx());
    let accents = [
        palette.blue,
        palette.teal,
        palette.mauve,
        palette.peach,
        palette.success,
        palette.yellow,
    ];
    let weights: Vec<_> = tiles.iter().map(|tile| tile.bytes).collect();
    partition(&weights, 0, bounds, &mut |index, bounds| {
        let tile = &tiles[index];
        let rect = bounds.shrink(2.0);
        if !rect.is_positive() {
            return;
        }
        let folder_id = tile
            .id
            .filter(|&id| analysis.nodes[id].kind == Kind::Folder);
        let sense = if folder_id.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let mut response = ui.interact(
            rect,
            ui.make_persistent_id(("disk-map", run, folder, index)),
            sense,
        );
        let fill = if tile.id.is_some() {
            palette.raised.lerp_to_gamma(
                accents[index % accents.len()],
                if response.hovered() { 0.45 } else { 0.24 },
            )
        } else {
            palette.raised
        };
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 5.0, fill);
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                5.0,
                egui::Stroke::new(2.0, palette.ink),
                egui::StrokeKind::Inside,
            );
        }
        let size = tile.id.map_or_else(
            || human_size(tile.bytes),
            |id| row_size(&analysis.nodes[id]),
        );
        if rect.width() >= 40.0 && rect.height() >= 24.0 {
            let origin = rect.left_top() + Vec2::new(8.0, 7.0);
            painter.text(
                origin,
                Align2::LEFT_TOP,
                &tile.caption,
                FontId::proportional(14.0),
                palette.ink,
            );
            if rect.height() >= 47.0 {
                painter.text(
                    origin + Vec2::new(0.0, 19.0),
                    Align2::LEFT_TOP,
                    &size,
                    FontId::proportional(12.0),
                    palette.ink,
                );
            }
        }
        if let Some(id) = folder_id {
            response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::Button,
                    true,
                    format!("{} · {size}", tile.caption),
                )
            });
            if response.clicked() {
                selected = id;
            }
        }
        response.on_hover_text(tile.id.map_or_else(
            || format!("{} · {size}", tile.caption),
            |id| node_detail(analysis, &analysis.nodes[id]),
        ));
    });
    selected
}

fn tiles(analysis: &Analysis, folder: NodeId) -> Vec<Tile> {
    let node = &analysis.nodes[folder];
    let mut children = node
        .children
        .iter()
        .copied()
        .filter(|&id| analysis.nodes[id].allocated != 0);
    let mut tiles: Vec<_> = children
        .by_ref()
        .take(MAX_TILES)
        .map(|id| Tile {
            id: Some(id),
            bytes: analysis.nodes[id].allocated,
            caption: node_name(analysis, &analysis.nodes[id]),
        })
        .collect();
    let other = children.fold(0u64, |sum, id| {
        sum.saturating_add(analysis.nodes[id].allocated)
    });
    if other > 0 {
        tiles.push(Tile {
            id: None,
            bytes: other,
            caption: text!("Остальные объекты").into(),
        });
    }
    if node.own_blocks > 0 {
        tiles.push(Tile {
            id: None,
            bytes: node.own_blocks,
            caption: text!("Блоки самой папки").into(),
        });
    }
    tiles
}

/// Divide the longest side near half of the remaining weight, preserving area ratios.
fn partition(weights: &[u64], offset: usize, rect: Rect, paint: &mut impl FnMut(usize, Rect)) {
    if weights.len() == 1 {
        paint(offset, rect);
        return;
    }
    let total: f64 = weights.iter().map(|&weight| weight as f64).sum();
    let mut prefix = 0.0;
    let mut nearest = (1, weights[0] as f64);
    for (index, &weight) in weights[..weights.len() - 1].iter().enumerate() {
        prefix += weight as f64;
        if (prefix - total / 2.0).abs() < (nearest.1 - total / 2.0).abs() {
            nearest = (index + 1, prefix);
        }
    }
    let fraction = (nearest.1 / total) as f32;
    let (first, second) = if rect.width() >= rect.height() {
        let x = rect.left() + rect.width() * fraction;
        (
            Rect::from_min_max(rect.min, Pos2::new(x, rect.bottom())),
            Rect::from_min_max(Pos2::new(x, rect.top()), rect.max),
        )
    } else {
        let y = rect.top() + rect.height() * fraction;
        (
            Rect::from_min_max(rect.min, Pos2::new(rect.right(), y)),
            Rect::from_min_max(Pos2::new(rect.left(), y), rect.max),
        )
    };
    partition(&weights[..nearest.0], offset, first, paint);
    partition(&weights[nearest.0..], offset + nearest.0, second, paint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_tiles_cover_the_bounds_in_proportion_to_allocated_bytes() {
        let weights = [40, 30, 20, 9, 1];
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 240.0));
        let area = bounds.width() * bounds.height();
        let mut rectangles: Vec<Rect> = Vec::new();
        partition(&weights, 0, bounds, &mut |index, rect| {
            assert_eq!(index, rectangles.len());
            let percent = 100.0 * rect.width() * rect.height() / area;
            assert!((percent - weights[index] as f32).abs() < 0.01);
            assert!(bounds.contains_rect(rect));
            assert!(rectangles
                .iter()
                .all(|prior| !prior.intersect(rect).is_positive()));
            rectangles.push(rect);
        });
        assert_eq!(rectangles.len(), weights.len());
    }
}
