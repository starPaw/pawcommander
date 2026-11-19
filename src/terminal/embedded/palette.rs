use alacritty_terminal::term::{cell::Flags, color::Colors};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use eframe::egui::Color32;

const ANSI: [u32; 16] = [
    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de, 0x585b70,
    0xf28fad, 0xb5eca9, 0xffe9bf, 0x9ac3ff, 0xf8cdf0, 0xa1eddf, 0xf5f5ff,
];

pub(super) fn base(index: usize) -> Rgb {
    match index {
        0..=15 => rgb(ANSI[index]),
        16..=231 => {
            let color = index - 16;
            let level = |value| {
                if value == 0 {
                    0
                } else {
                    (55 + value * 40) as u8
                }
            };
            Rgb {
                r: level(color / 36),
                g: level(color / 6 % 6),
                b: level(color % 6),
            }
        }
        232..=255 => {
            let shade = (8 + (index - 232) * 10) as u8;
            Rgb {
                r: shade,
                g: shade,
                b: shade,
            }
        }
        257 => rgb(0x11111b),
        258 => rgb(0xf5e0dc),
        259..=266 => dim(rgb(ANSI[index - 259])),
        267 => rgb(0xf5f5ff),
        268 => dim(rgb(0xcdd6f4)),
        _ => rgb(0xcdd6f4),
    }
}

pub(super) fn indexed(colors: &Colors, index: usize) -> Color32 {
    let color = colors[index].unwrap_or_else(|| base(index));
    Color32::from_rgb(color.r, color.g, color.b)
}

pub(super) fn cell(colors: &Colors, color: Color, flags: Flags) -> Color32 {
    let dimmed = flags.contains(Flags::DIM);
    let color = match color {
        Color::Named(name) => {
            let name = if dimmed {
                name.to_dim()
            } else if flags.contains(Flags::BOLD) {
                name.to_bright()
            } else {
                name
            };
            let index = name as usize;
            colors[index].unwrap_or_else(|| base(index))
        }
        Color::Indexed(index) => {
            let index = usize::from(index);
            let color = colors[index].unwrap_or_else(|| base(index));
            if dimmed {
                dim(color)
            } else {
                color
            }
        }
        Color::Spec(color) => {
            if dimmed {
                dim(color)
            } else {
                color
            }
        }
    };
    Color32::from_rgb(color.r, color.g, color.b)
}

pub(super) fn background(colors: &Colors) -> Color32 {
    indexed(colors, NamedColor::Background as usize)
}

fn rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

fn dim(color: Rgb) -> Rgb {
    let lower = |value| (u16::from(value) * 2 / 3) as u8;
    Rgb {
        r: lower(color.r),
        g: lower(color.g),
        b: lower(color.b),
    }
}
