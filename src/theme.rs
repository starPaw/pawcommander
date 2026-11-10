use eframe::egui::{self, Color32, FontId, Stroke, Vec2};
use serde::{Deserialize, Serialize};

/// The legacy names remain readable so existing tabs and preferences survive upgrades.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Latte,
    #[default]
    Frappe,
    Macchiato,
    Mocha,
    Light,
    Dark,
    System,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Self::Latte | Self::Light => "Latte",
            Self::Frappe | Self::Dark => "Frappé",
            Self::Macchiato => "Macchiato",
            Self::Mocha => "Mocha",
            Self::System => crate::i18n::text("Как в системе"),
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        ctx.data_mut(|data| data.insert_temp(egui::Id::new("catppuccin.theme"), self));
        ctx.set_theme(match self {
            Self::Latte | Self::Light => egui::ThemePreference::Light,
            Self::System => egui::ThemePreference::System,
            _ => egui::ThemePreference::Dark,
        });
        for mode in [egui::Theme::Light, egui::Theme::Dark] {
            let p = Palette::new(self.resolved(mode == egui::Theme::Dark));
            ctx.style_mut_of(mode, |style| {
                let v = &mut style.visuals;
                v.override_text_color = Some(p.ink);
                v.panel_fill = p.bg;
                v.window_fill = p.surface;
                v.window_stroke = Stroke::new(1.0, p.line);
                v.window_corner_radius = 12.into();
                v.menu_corner_radius = 10.into();
                v.extreme_bg_color = p.bg;
                v.faint_bg_color = p.raised;
                v.selection.bg_fill = p.selection;
                v.selection.stroke = Stroke::new(1.0, p.ink);
                v.hyperlink_color = p.blue;
                v.error_fg_color = p.error;
                v.warn_fg_color = p.peach;
                for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive] {
                    w.bg_fill = p.raised;
                    w.weak_bg_fill = p.surface;
                    w.bg_stroke = Stroke::new(1.0, p.line);
                    w.fg_stroke = Stroke::new(1.4, p.ink);
                    w.corner_radius = 8.into();
                }
                for w in [
                    &mut v.widgets.hovered,
                    &mut v.widgets.active,
                    &mut v.widgets.open,
                ] {
                    w.bg_fill = p.selection;
                    w.weak_bg_fill = p.selection;
                    w.bg_stroke = Stroke::new(1.0, p.mauve);
                    w.fg_stroke = Stroke::new(1.5, p.ink);
                    w.corner_radius = 8.into();
                    w.expansion = 0.0;
                }
                style.spacing.item_spacing = Vec2::new(7.0, 6.0);
                style.spacing.button_padding = Vec2::new(10.0, 7.0);
                style.spacing.interact_size = Vec2::new(36.0, 30.0);
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, FontId::proportional(14.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, FontId::proportional(14.0));
            });
        }
        ctx.request_repaint();
    }

    fn resolved(self, dark: bool) -> Self {
        match self {
            Self::Light => Self::Latte,
            Self::Dark => Self::Frappe,
            Self::System if dark => Self::Frappe,
            Self::System => Self::Latte,
            other => other,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub blue: Color32,
    pub muted: Color32,
    pub ink: Color32,
    pub line: Color32,
    pub bg: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub selection: Color32,
    pub error: Color32,
    pub success: Color32,
    pub mauve: Color32,
    pub teal: Color32,
    pub peach: Color32,
    pub yellow: Color32,
}

impl Palette {
    pub fn from_ctx(ctx: &egui::Context) -> Self {
        let theme = ctx
            .data(|d| d.get_temp::<Theme>(egui::Id::new("catppuccin.theme")))
            .unwrap_or_default();
        Self::new(theme.resolved(ctx.style().visuals.dark_mode))
    }

    pub fn new(theme: Theme) -> Self {
        // Official Catppuccin palette: https://github.com/catppuccin/palette
        let hex = match theme {
            Theme::Latte | Theme::Light => [
                0xeff1f5, 0xe6e9ef, 0xccd0da, 0xbcc0cc, 0x4c4f69, 0x6c6f85, 0x1e66f5, 0x8839ef,
                0x179299, 0xfe640b, 0xdf8e1d, 0xd20f39, 0x40a02b,
            ],
            Theme::Macchiato => [
                0x24273a, 0x1e2030, 0x363a4f, 0x494d64, 0xcad3f5, 0xa5adcb, 0x8aadf4, 0xc6a0f6,
                0x8bd5ca, 0xf5a97f, 0xeed49f, 0xed8796, 0xa6da95,
            ],
            Theme::Mocha => [
                0x1e1e2e, 0x181825, 0x313244, 0x45475a, 0xcdd6f4, 0xa6adc8, 0x89b4fa, 0xcba6f7,
                0x94e2d5, 0xfab387, 0xf9e2af, 0xf38ba8, 0xa6e3a1,
            ],
            _ => [
                0x303446, 0x292c3c, 0x414559, 0x51576d, 0xc6d0f5, 0xa5adce, 0x8caaee, 0xca9ee6,
                0x81c8be, 0xef9f76, 0xe5c890, 0xe78284, 0xa6d189,
            ],
        };
        let c = hex.map(|h| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8));
        Self {
            bg: c[0],
            surface: c[1],
            raised: c[2],
            line: c[3],
            ink: c[4],
            muted: c[5],
            blue: c[6],
            mauve: c[7],
            teal: c[8],
            peach: c[9],
            yellow: c[10],
            error: c[11],
            success: c[12],
            selection: c[0].lerp_to_gamma(c[7], 0.26),
        }
    }
}

pub fn appearance_menu(ui: &mut egui::Ui, theme: &mut Theme) {
    // Normalize old persisted names without changing their light/dark preference.
    *theme = match *theme {
        Theme::Light => Theme::Latte,
        Theme::Dark => Theme::Frappe,
        other => other,
    };
    ui.label(egui::RichText::new("CATPPUCCIN").strong());
    ui.weak(crate::i18n::text("Мягкие цвета для ваших файлов"));
    ui.add_space(6.0);
    for value in [
        Theme::Latte,
        Theme::Frappe,
        Theme::Macchiato,
        Theme::Mocha,
        Theme::System,
    ] {
        ui.horizontal(|ui| {
            if ui.radio_value(theme, value, value.name()).changed() {
                theme.apply(ui.ctx());
            }
            if value != Theme::System {
                let p = Palette::new(value);
                for color in [p.bg, p.mauve, p.blue, p.teal, p.peach] {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.5, color);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_flavours_updates_widgets_and_custom_painters_together() {
        let ctx = egui::Context::default();
        for theme in [Theme::Latte, Theme::Frappe, Theme::Macchiato, Theme::Mocha] {
            theme.apply(&ctx);
            let p = Palette::from_ctx(&ctx);
            assert_eq!(ctx.style().visuals.panel_fill, p.bg);
            assert_eq!(ctx.style().visuals.override_text_color, Some(p.ink));
            assert_eq!(ctx.style().visuals.dark_mode, theme != Theme::Latte);
            assert_eq!(ctx.style().visuals.selection.bg_fill, p.selection);
            assert_ne!(p.selection, p.bg);
            assert_eq!(
                serde_json::from_str::<Theme>(&serde_json::to_string(&theme).unwrap()).unwrap(),
                theme
            );
        }
    }

    #[test]
    fn legacy_preferences_remain_readable() {
        for (legacy, expected) in [
            (egui::ThemePreference::Light, Theme::Light),
            (egui::ThemePreference::Dark, Theme::Dark),
            (egui::ThemePreference::System, Theme::System),
        ] {
            assert_eq!(
                serde_json::from_str::<Theme>(&serde_json::to_string(&legacy).unwrap()).unwrap(),
                expected
            );
        }
    }
}
