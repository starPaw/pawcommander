//! Resolution-independent, palette-aware icons. No emoji or platform icon fonts.
use crate::theme::Palette;
use eframe::egui::{self, pos2, vec2, Color32, Rect, Response, Sense, Shape, Stroke, Vec2};

#[derive(Clone, Copy)]
pub enum Icon {
    Folder,
    Document,
    Image,
    Music,
    Video,
    Archive,
    Code,
    Back,
    Forward,
    Up,
    Refresh,
    Search,
    Server,
    NewFolder,
    Copy,
    Paste,
    Home,
    Desktop,
    Download,
    Drive,
    Star,
    Grid,
    List,
    Split,
    Info,
    Settings,
    ChevronDown,
    Breadcrumbs,
    Plus,
    Close,
}

pub fn file_icon(name: &str, folder: bool) -> Icon {
    if folder {
        return Icon::Folder;
    }
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "heic" | "avif" | "ico" => Icon::Image,
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" => Icon::Music,
        "mp4" | "mov" | "mkv" | "webm" | "avi" => Icon::Video,
        "zip" | "gz" | "tar" | "7z" | "rar" | "xz" | "bz2" => Icon::Archive,
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "json" | "toml" | "yaml" | "yml" | "html"
        | "css" | "sh" | "swift" => Icon::Code,
        _ => Icon::Document,
    }
}

pub fn tint(icon: Icon, p: Palette) -> Color32 {
    match icon {
        Icon::Archive | Icon::Star => p.yellow,
        Icon::Folder | Icon::NewFolder => p.blue,
        Icon::Image | Icon::Server | Icon::List => p.teal,
        Icon::Music => p.mauve,
        Icon::Video | Icon::Paste => p.peach,
        Icon::Document | Icon::Code | Icon::Download | Icon::Grid => p.blue,
        _ => p.ink,
    }
}

pub fn logo(size: f32) -> egui::Image<'static> {
    egui::Image::new(egui::include_image!("../assets/logo.png"))
        .fit_to_exact_size(Vec2::splat(size))
        .alt_text("PawCommander")
}

pub const TOOLBAR_BUTTON_SIZE: f32 = 32.0;

pub fn button(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool) -> Response {
    button_width(ui, icon, label, selected, 0.0)
}

pub fn button_width(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    min_width: f32,
) -> Response {
    let p = Palette::from_ctx(ui.ctx());
    let text = ui
        .painter()
        .layout_no_wrap(label.into(), egui::FontId::proportional(13.0), p.ink);
    let width = if label.is_empty() {
        TOOLBAR_BUTTON_SIZE
    } else {
        text.size().x + 38.0
    };
    let (r, response) = ui.allocate_exact_size(
        vec2(width.max(min_width), TOOLBAR_BUTTON_SIZE),
        Sense::click(),
    );
    let accessible_label = if label.is_empty() {
        match icon {
            Icon::Back => crate::i18n::text("Назад"),
            Icon::Forward => crate::i18n::text("Вперёд"),
            Icon::Up => crate::i18n::text("Родительская папка"),
            Icon::Refresh => crate::i18n::text("Обновить"),
            Icon::Search => crate::i18n::text("Поиск файлов"),
            Icon::NewFolder => crate::i18n::text("Создать папку"),
            Icon::Copy => crate::i18n::text("Копировать"),
            Icon::Paste => crate::i18n::text("Вставить"),
            Icon::Grid => crate::i18n::text("Сетка"),
            Icon::List => crate::i18n::text("Список"),
            Icon::Split => crate::i18n::text("Две панели"),
            Icon::Info => crate::i18n::text("Сведения"),
            _ => crate::i18n::text("Действие"),
        }
    } else {
        label
    };
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            selected,
            accessible_label,
        )
    });
    let color = tint(icon, p);
    let fill = if selected {
        p.bg.lerp_to_gamma(color, 0.24)
    } else if response.hovered() {
        p.raised
    } else {
        p.surface
    };
    ui.painter().rect(
        r,
        8,
        fill,
        Stroke::new(1.0, if selected { color } else { p.line }),
        egui::StrokeKind::Inside,
    );
    paint(
        ui,
        Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), Vec2::splat(19.0)),
        icon,
        color,
    );
    if !label.is_empty() {
        ui.painter().galley(
            pos2(r.left() + 30.0, r.center().y - text.size().y / 2.0),
            text,
            p.ink,
        );
    }
    response
}

pub fn paint(ui: &egui::Ui, rect: Rect, icon: Icon, color: Color32) {
    let painter = ui.painter();
    let p = Palette::from_ctx(ui.ctx());
    let pt = |x: f32, y: f32| {
        pos2(
            rect.left() + x * rect.width() / 24.0,
            rect.top() + y * rect.height() / 24.0,
        )
    };
    let rr = |x, y, w, h| Rect::from_min_max(pt(x, y), pt(x + w, y + h));
    let stroke = Stroke::new((rect.width() / 15.0).clamp(1.3, 3.0), color);
    let line = |points: &[(f32, f32)]| {
        painter.add(Shape::line(
            points.iter().map(|&(x, y)| pt(x, y)).collect(),
            stroke,
        ))
    };
    let outline =
        |r: Rect, radius| painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Inside);
    match icon {
        Icon::Folder | Icon::NewFolder => {
            let blue = if ui.visuals().dark_mode {
                color.lerp_to_gamma(Color32::from_rgb(97, 163, 252), 0.55)
            } else {
                color
            };
            let outline = Stroke::new((rect.width() * 0.054).max(1.2), blue);
            for (i, path) in folder_paths().iter().enumerate() {
                let points = path.iter().map(|p| pt(p.x, p.y)).collect();
                painter.add(if i == 0 {
                    Shape::closed_line(points, outline)
                } else {
                    Shape::line(points, outline)
                });
            }
            if matches!(icon, Icon::NewFolder) {
                painter.line_segment([pt(12.0, 12.4), pt(12.0, 17.6)], outline);
                painter.line_segment([pt(9.4, 15.0), pt(14.6, 15.0)], outline);
            }
        }
        Icon::ChevronDown => {
            line(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]);
        }
        Icon::Breadcrumbs => {
            line(&[(3.0, 6.0), (9.0, 12.0), (3.0, 18.0)]);
            line(&[(13.0, 6.0), (19.0, 12.0), (13.0, 18.0)]);
        }
        Icon::Plus => {
            line(&[(12.0, 4.0), (12.0, 20.0)]);
            line(&[(4.0, 12.0), (20.0, 12.0)]);
        }
        Icon::Close => {
            line(&[(6.0, 6.0), (18.0, 18.0)]);
            line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Document | Icon::Code | Icon::Archive => {
            painter.add(Shape::convex_polygon(
                vec![
                    pt(5.0, 2.0),
                    pt(14.0, 2.0),
                    pt(20.0, 8.0),
                    pt(20.0, 22.0),
                    pt(5.0, 22.0),
                ],
                color,
                Stroke::NONE,
            ));
            painter.add(Shape::convex_polygon(
                vec![pt(14.0, 2.0), pt(14.0, 8.0), pt(20.0, 8.0)],
                color.lerp_to_gamma(Color32::WHITE, 0.4),
                Stroke::NONE,
            ));
            let s = Stroke::new(stroke.width, p.surface);
            if matches!(icon, Icon::Archive) {
                for y in (4..19).step_by(3) {
                    painter.rect_filled(
                        rr(if y % 2 == 0 { 10.0 } else { 12.0 }, y as f32, 2.2, 2.0),
                        0,
                        p.surface,
                    );
                }
            } else if matches!(icon, Icon::Code) {
                painter.add(Shape::line(
                    vec![pt(10.0, 12.0), pt(7.0, 15.0), pt(10.0, 18.0)],
                    s,
                ));
                painter.add(Shape::line(
                    vec![pt(14.0, 12.0), pt(17.0, 15.0), pt(14.0, 18.0)],
                    s,
                ));
            } else {
                for (y, w) in [(11.0, 9.0), (14.5, 9.0), (18.0, 6.0)] {
                    painter.line_segment([pt(8.0, y), pt(8.0 + w, y)], s);
                }
            }
        }
        Icon::Image => {
            outline(rr(2.0, 3.0, 20.0, 18.0), 3);
            painter.circle_filled(pt(8.0, 8.5), rect.width() / 12.0, color);
            line(&[
                (3.0, 18.0),
                (10.0, 12.0),
                (14.0, 16.0),
                (17.0, 13.0),
                (21.0, 17.0),
            ]);
        }
        Icon::Video => {
            outline(rr(2.0, 3.0, 20.0, 18.0), 3);
            painter.add(Shape::convex_polygon(
                vec![pt(10.0, 7.0), pt(17.0, 12.0), pt(10.0, 17.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Music => {
            line(&[(9.0, 18.0), (9.0, 5.0), (20.0, 3.0), (20.0, 16.0)]);
            line(&[(9.0, 9.0), (20.0, 7.0)]);
            painter.circle_filled(pt(6.5, 18.0), rect.width() * 0.14, color);
            painter.circle_filled(pt(17.5, 16.0), rect.width() * 0.14, color);
        }
        Icon::Back => {
            line(&[(11.0, 5.0), (4.0, 12.0), (11.0, 19.0)]);
            line(&[(4.0, 12.0), (20.0, 12.0)]);
        }
        Icon::Forward => {
            line(&[(13.0, 5.0), (20.0, 12.0), (13.0, 19.0)]);
            line(&[(4.0, 12.0), (20.0, 12.0)]);
        }
        Icon::Up => {
            line(&[(4.0, 11.0), (12.0, 3.0), (20.0, 11.0)]);
            line(&[(12.0, 3.0), (12.0, 22.0)]);
        }
        Icon::Refresh => {
            for sign in [1.0, -1.0] {
                let point = |angle: f32| {
                    let (sin, cos) = angle.to_radians().sin_cos();
                    (12.0 + sign * 9.0 * cos, 12.0 + sign * 9.0 * sin)
                };
                let arc: Vec<_> = (0..=20)
                    .map(|i| point(-165.0 + i as f32 * 135.0 / 20.0))
                    .collect();
                line(&arc);
                let (x, y) = point(-30.0);
                line(&[(x, y - sign * 5.0), (x, y), (x - sign * 5.0, y)]);
            }
        }
        Icon::Search => {
            painter.circle_stroke(pt(10.0, 10.0), rect.width() * 0.32, stroke);
            line(&[(16.0, 16.0), (22.0, 22.0)]);
        }
        Icon::Server => {
            for y in [3.0, 14.0] {
                outline(rr(2.0, y, 20.0, 7.0), 2);
                painter.circle_filled(pt(6.0, y + 3.5), rect.width() / 24.0, color);
                line(&[(11.0, y + 3.5), (18.0, y + 3.5)]);
            }
        }
        Icon::Copy => {
            outline(rr(3.0, 2.0, 13.0, 16.0), 2);
            painter.rect_filled(rr(8.0, 6.0, 13.0, 16.0), 2, p.surface);
            outline(rr(8.0, 6.0, 13.0, 16.0), 2);
        }
        Icon::Paste => {
            outline(rr(5.0, 4.0, 15.0, 18.0), 2);
            painter.rect_filled(rr(9.0, 2.0, 7.0, 5.0), 2, p.surface);
            outline(rr(9.0, 2.0, 7.0, 5.0), 2);
        }
        Icon::Home => {
            line(&[(2.0, 11.0), (12.0, 2.0), (22.0, 11.0)]);
            line(&[
                (5.0, 9.0),
                (5.0, 22.0),
                (10.0, 22.0),
                (10.0, 15.0),
                (15.0, 15.0),
                (15.0, 22.0),
                (20.0, 22.0),
                (20.0, 9.0),
            ]);
        }
        Icon::Desktop => {
            outline(rr(2.0, 3.0, 20.0, 14.0), 2);
            line(&[(12.0, 17.0), (12.0, 22.0)]);
            line(&[(7.0, 22.0), (17.0, 22.0)]);
        }
        Icon::Download => {
            line(&[(12.0, 2.0), (12.0, 16.0)]);
            line(&[(6.0, 10.0), (12.0, 16.0), (18.0, 10.0)]);
            line(&[(3.0, 16.0), (3.0, 22.0), (21.0, 22.0), (21.0, 16.0)]);
        }
        Icon::Drive => {
            line(&[(2.0, 16.0), (6.0, 3.0), (18.0, 3.0), (22.0, 16.0)]);
            outline(rr(2.0, 15.0, 20.0, 7.0), 2);
            painter.circle_filled(pt(18.0, 18.5), rect.width() / 24.0, color);
        }
        Icon::Star => {
            let pts = (0..10)
                .map(|i| {
                    let a = i as f32 * std::f32::consts::PI / 5.0 - std::f32::consts::FRAC_PI_2;
                    let r = if i % 2 == 0 { 10.5 } else { 4.8 };
                    pt(12.0 + r * a.cos(), 12.0 + r * a.sin())
                })
                .collect();
            painter.add(Shape::closed_line(pts, stroke));
        }
        Icon::Grid => {
            for x in [3.0, 14.0] {
                for y in [3.0, 14.0] {
                    outline(rr(x, y, 7.0, 7.0), 1);
                }
            }
        }
        Icon::List => {
            for y in [5.0, 12.0, 19.0] {
                painter.circle_filled(pt(4.0, y), rect.width() / 16.0, color);
                line(&[(9.0, y), (22.0, y)]);
            }
        }
        Icon::Split => {
            outline(rr(2.0, 3.0, 20.0, 18.0), 2);
            line(&[(12.0, 3.0), (12.0, 21.0)]);
        }
        Icon::Info => {
            painter.circle_stroke(pt(12.0, 12.0), rect.width() * 0.43, stroke);
            line(&[(12.0, 10.0), (12.0, 18.0)]);
            painter.circle_filled(pt(12.0, 6.5), rect.width() / 18.0, color);
        }
        Icon::Settings => {
            let pts = (0..32)
                .map(|i| {
                    let a = i as f32 * std::f32::consts::TAU / 32.0;
                    let r = if i % 4 == 1 || i % 4 == 2 { 11.0 } else { 8.5 };
                    pt(12.0 + r * a.cos(), 12.0 + r * a.sin())
                })
                .collect();
            painter.add(Shape::closed_line(pts, stroke));
            painter.circle_stroke(pt(12.0, 12.0), rect.width() * 0.17, stroke);
        }
    }
}

fn folder_paths() -> &'static [Vec<egui::Pos2>; 4] {
    use egui::epaint::CubicBezierShape;
    use std::sync::OnceLock;

    static PATHS: OnceLock<[Vec<egui::Pos2>; 4]> = OnceLock::new();
    PATHS.get_or_init(|| {
        let path = |start: (f32, f32), curves: &[[(f32, f32); 3]]| {
            let mut current = pos2(start.0, start.1);
            let mut points = vec![current];
            for &[a, b, end] in curves {
                let next = pos2(end.0, end.1);
                let curve = CubicBezierShape::from_points_stroke(
                    [current, pos2(a.0, a.1), pos2(b.0, b.1), next],
                    false,
                    Color32::TRANSPARENT,
                    Stroke::NONE,
                );
                points.extend(curve.flatten(Some(0.025)).into_iter().skip(1));
                current = next;
            }
            points
        };

        // The outside line joins the folder lip to the back of the paw.
        let mut outside = path(
            (3.6, 7.1),
            &[
                [(5.0, 7.1), (6.1, 7.1), (7.4, 7.1)],
                [(8.2, 7.1), (8.5, 7.8), (9.2, 8.5)],
                [(8.9, 5.7), (10.5, 3.6), (12.6, 3.6)],
                [(13.4, 3.6), (14.1, 3.9), (14.6, 4.3)],
                [(16.0, 3.5), (17.6, 3.8), (18.5, 4.9)],
                [(20.7, 4.8), (21.8, 6.6), (21.2, 9.2)],
                [(22.0, 9.5), (22.4, 10.2), (22.2, 11.2)],
                [(22.0, 13.5), (21.6, 16.4), (21.4, 18.2)],
                [(21.3, 19.4), (20.7, 20.0), (19.7, 20.0)],
                [(15.0, 20.0), (8.6, 20.0), (4.0, 20.0)],
                [(2.9, 20.0), (2.2, 19.4), (2.2, 18.3)],
                [(2.1, 15.7), (1.9, 11.1), (1.8, 9.0)],
                [(1.8, 7.8), (2.5, 7.1), (3.6, 7.1)],
            ],
        );
        outside.pop(); // closed_line joins the last point to the first.
        let toes = path(
            (9.2, 8.5),
            &[
                [(9.9, 10.4), (11.5, 10.7), (12.8, 9.4)],
                [(13.3, 11.3), (15.1, 11.6), (16.6, 9.9)],
                [(17.3, 11.8), (19.4, 11.8), (20.0, 9.9)],
                [(20.3, 9.0), (20.7, 8.9), (21.2, 9.2)],
            ],
        );
        let first_toe = path((12.8, 9.4), &[[(12.8, 8.4), (13.0, 7.6), (13.5, 7.0)]]);
        let second_toe = path((16.6, 9.9), &[[(16.5, 8.9), (16.8, 8.1), (17.3, 7.6)]]);
        [outside, toes, first_toe, second_toe]
    })
}

pub const COMPACT_SIZE: f32 = 28.0;

/// Small controls for the pane chrome, with useful accessibility labels.
pub fn compact_button(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let p = Palette::from_ctx(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(COMPACT_SIZE), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, label)
    });
    let color = tint(icon, p);
    ui.painter().rect(
        rect,
        6,
        if selected {
            p.bg.lerp_to_gamma(color, 0.25)
        } else if response.hovered() {
            p.raised
        } else {
            Color32::TRANSPARENT
        },
        Stroke::new(
            1.0,
            if selected {
                color
            } else if response.hovered() {
                p.line
            } else {
                Color32::TRANSPARENT
            },
        ),
        egui::StrokeKind::Inside,
    );
    paint(
        ui,
        rect.shrink(5.0),
        icon,
        if selected { color } else { p.muted },
    );
    response.on_hover_text(label)
}
