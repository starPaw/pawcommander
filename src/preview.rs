use crate::vfs::{self, Location};
use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use crate::{
    icons::{self, Icon},
    theme::Palette,
};
use chrono::{DateTime, Local};
use eframe::egui::{self, RichText};

const TEXT_LIMIT: usize = 32 * 1024;
const IMAGE_LIMIT: u64 = 20 * 1024 * 1024;
const IMAGE_EDGE_LIMIT: u32 = 8192;

struct Request {
    generation: u64,
    path: Location,
    context: egui::Context,
}

struct Response {
    generation: u64,
    result: Result<Loaded, String>,
}

struct Loaded {
    kind: String,
    size: Option<u64>,
    modified: String,
    is_dir: bool,
    content: Content,
}

enum Content {
    Directory,
    Text {
        body: String,
        truncated: bool,
    },
    Image {
        size: [usize; 2],
        original_size: [u32; 2],
        rgba: Vec<u8>,
    },
    Message(String),
}

/// A read-only preview with one background reader and a bounded image decoder.
pub struct Preview {
    requests: Sender<Request>,
    responses: Receiver<Response>,
    path: Option<Location>,
    generation: u64,
    result: Option<Result<Loaded, String>>,
    texture: Option<egui::TextureHandle>,
    dirty: bool,
    fingerprint: Option<(u64, Option<std::time::SystemTime>)>,
}

impl Preview {
    pub fn new() -> Self {
        let (requests, receiver) = mpsc::channel::<Request>();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut request) = receiver.recv() {
                // Fast navigation should not queue a read for every visited item.
                while let Ok(newer) = receiver.try_recv() {
                    request = newer;
                }
                let result = load_location(&request.path);
                if sender
                    .send(Response {
                        generation: request.generation,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                request.context.request_repaint();
            }
        });
        Self {
            requests,
            responses,
            path: None,
            generation: 0,
            result: None,
            texture: None,
            dirty: false,
            fingerprint: None,
        }
    }

    /// Call after an operation that changes the selected file without changing its path.
    pub fn refresh(&mut self) {
        self.dirty = true;
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        path: Option<&Location>,
        fingerprint: Option<(u64, Option<std::time::SystemTime>)>,
    ) {
        if self.path.as_ref() != path || self.fingerprint != fingerprint || self.dirty {
            self.fingerprint = fingerprint;
            self.dirty = false;
            self.path = path.cloned();
            self.generation = self.generation.wrapping_add(1);
            self.result = None;
            self.texture = None;
            if let Some(path) = path {
                if self
                    .requests
                    .send(Request {
                        generation: self.generation,
                        path: path.clone(),
                        context: ui.ctx().clone(),
                    })
                    .is_err()
                {
                    self.result = Some(Err(
                        crate::i18n::text("Служба предпросмотра недоступна").into()
                    ));
                }
            }
        }
        while let Ok(response) = self.responses.try_recv() {
            if response.generation == self.generation {
                self.result = Some(response.result);
                self.texture = None;
            }
        }

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(crate::i18n::text("СВЕДЕНИЯ"))
                    .size(11.0)
                    .strong()
                    .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled_ui(path.is_some(), |ui| {
                        icons::compact_button(
                            ui,
                            Icon::Refresh,
                            crate::i18n::text("Обновить предпросмотр"),
                            false,
                        )
                    })
                    .inner
                    .clicked()
                {
                    self.refresh();
                    ui.ctx().request_repaint();
                }
            });
        });
        ui.add_space(12.0);
        let Some(path) = path else {
            ui.add_space(45.0);
            draw_icon(ui, false, "");
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(crate::i18n::text("Ничего не выбрано")).strong());
                ui.add_space(6.0);
                ui.label(
                    RichText::new(crate::i18n::text(
                        "Выберите файл или папку,\nчтобы увидеть подробности.",
                    ))
                    .weak(),
                );
            });
            return;
        };

        // Upload once; subsequent frames reuse the texture.
        if self.texture.is_none() {
            if let Some(Ok(Loaded {
                content: Content::Image { size, rgba, .. },
                ..
            })) = &mut self.result
            {
                if !rgba.is_empty() {
                    let image = egui::ColorImage::from_rgba_unmultiplied(*size, rgba);
                    self.texture = Some(ui.ctx().load_texture(
                        format!("file-preview-{}", self.generation),
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                    // The texture owns its pixels now; keep only the small metadata.
                    *rgba = Vec::new();
                }
            }
        }

        egui::ScrollArea::vertical()
            .id_salt("file-preview-details")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                match (&self.result, &self.texture) {
                    (Some(Ok(_)), Some(texture)) => {
                        ui.vertical_centered(|ui| {
                            ui.add(
                                egui::Image::new((texture.id(), texture.size_vec2()))
                                    .max_width(ui.available_width())
                                    .max_height(220.0)
                                    .shrink_to_fit()
                                    .corner_radius(8),
                            );
                        });
                    }
                    (Some(Ok(loaded)), _) => draw_icon(ui, loaded.is_dir, &path.name()),
                    _ => draw_icon(ui, false, &path.name()),
                }
                ui.add_space(18.0);
                let name = path.name();
                ui.label(RichText::new(name).size(19.0).strong());
                ui.add_space(4.0);

                match &self.result {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(crate::i18n::text("Загрузка сведений…"));
                        });
                    }
                    Some(Err(error)) => {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    Some(Ok(loaded)) => {
                        ui.weak(&loaded.kind);
                        ui.add_space(18.0);
                        ui.separator();
                        ui.add_space(10.0);
                        if let Some(size) = loaded.size {
                            detail(ui, crate::i18n::text("Размер"), &format_size(size));
                        }
                        detail(ui, crate::i18n::text("Изменён"), &loaded.modified);
                        if let Content::Image { original_size, .. } = &loaded.content {
                            detail(
                                ui,
                                crate::i18n::text("Разрешение"),
                                &format!("{} × {}", original_size[0], original_size[1]),
                            );
                        }
                    }
                }
                detail(ui, crate::i18n::text("Расположение"), &path.display());

                if let Some(Ok(loaded)) = &self.result {
                    match &loaded.content {
                        Content::Text { body, truncated } => {
                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(crate::i18n::text("ПРЕДПРОСМОТР"))
                                    .size(11.0)
                                    .strong()
                                    .weak(),
                            );
                            ui.add_space(8.0);
                            egui::Frame::new()
                                .fill(ui.visuals().faint_bg_color)
                                .corner_radius(8)
                                .inner_margin(10)
                                .show(ui, |ui| {
                                    if body.is_empty() {
                                        ui.weak(crate::i18n::text("Пустой файл"));
                                    } else {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(body).monospace().size(11.0),
                                            )
                                            .wrap()
                                            .selectable(true),
                                        );
                                    }
                                });
                            if *truncated {
                                ui.add_space(6.0);
                                ui.weak(crate::i18n::text("Показано начало файла (до 32 КиБ)."));
                            }
                        }
                        Content::Message(message) => {
                            ui.add_space(12.0);
                            ui.weak(message);
                        }
                        _ => {}
                    }
                }
            });
    }
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

fn detail(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(11.0).weak());
    ui.add(egui::Label::new(value).wrap().selectable(true));
    ui.add_space(10.0);
}

fn draw_icon(ui: &mut egui::Ui, is_dir: bool, name: &str) {
    let colors = Palette::from_ctx(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 168.0),
        egui::Sense::hover(),
    );
    ui.painter().rect(
        rect,
        12,
        colors.raised,
        egui::Stroke::new(1.0, colors.line),
        egui::StrokeKind::Inside,
    );
    if name.is_empty() {
        icons::logo(96.0).paint_at(
            ui,
            egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(96.0)),
        );
        return;
    }
    let icon = icons::file_icon(name, is_dir);
    icons::paint(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(108.0)),
        icon,
        icons::tint(icon, colors),
    );
}

fn load_location(location: &Location) -> Result<Loaded, String> {
    if let Some(path) = location.local() {
        return load(path);
    }
    let image = matches!(
        location
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    );
    let limit = if image {
        IMAGE_LIMIT as usize
    } else {
        TEXT_LIMIT
    };
    let (stat, bytes) = vfs::preview_data(location, limit)?;
    let content = if stat.is_dir() {
        Content::Directory
    } else {
        let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = directory.path().join(location.name());
        fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        load_content(&path, stat.size.unwrap_or(bytes.len() as u64))
            .unwrap_or_else(Content::Message)
    };
    Ok(Loaded {
        kind: if stat.is_dir() {
            crate::i18n::text("Папка SFTP")
        } else {
            crate::i18n::text("Файл SFTP")
        }
        .into(),
        size: if stat.is_dir() { None } else { stat.size },
        modified: stat
            .mtime
            .and_then(|t| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(t)))
            .map(|t| {
                DateTime::<Local>::from(t)
                    .format("%d.%m.%Y, %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| crate::i18n::text("Недоступно").into()),
        is_dir: stat.is_dir(),
        content,
    })
}

fn load(path: &Path) -> Result<Loaded, String> {
    let link_metadata = fs::symlink_metadata(path).map_err(|e| {
        crate::tr_format!(
            "Не удалось прочитать сведения: {e}",
            "Could not read details: {e}"
        )
    })?;
    let is_link = link_metadata.file_type().is_symlink();
    let metadata = fs::metadata(path).map_err(|e| {
        crate::tr_format!("Не удалось открыть объект: {e}", "Could not open item: {e}")
    })?;
    let is_dir = metadata.is_dir();
    let kind = if is_link {
        if is_dir {
            crate::i18n::text("Ссылка на папку")
        } else {
            crate::i18n::text("Ссылка на файл")
        }
    } else if is_dir {
        crate::i18n::text("Папка")
    } else if metadata.is_file() {
        crate::i18n::text("Файл")
    } else {
        crate::i18n::text("Специальный файл")
    };
    let modified = metadata
        .modified()
        .map(|time| {
            DateTime::<Local>::from(time)
                .format("%d.%m.%Y, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| crate::i18n::text("Недоступно").into());
    let content = if is_dir {
        Content::Directory
    } else if !metadata.is_file() {
        // Do not open sockets, devices or named pipes: a read may never finish.
        Content::Message(crate::i18n::text("Предпросмотр специальных файлов недоступен.").into())
    } else {
        load_content(path, metadata.len()).unwrap_or_else(Content::Message)
    };
    Ok(Loaded {
        kind: kind.into(),
        size: metadata.is_file().then_some(metadata.len()),
        modified,
        is_dir,
        content,
    })
}

fn load_content(path: &Path, length: u64) -> Result<Content, String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        if length > IMAGE_LIMIT {
            return Err(crate::i18n::text(
                "Изображение больше 20 МиБ. Откройте его во внешнем приложении.",
            )
            .into());
        }
        let bytes = read_bounded(path, IMAGE_LIMIT as usize)?;
        if bytes.len() > IMAGE_LIMIT as usize {
            return Err(crate::i18n::text("Изображение больше 20 МиБ.").into());
        }
        let mut reader = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| {
                crate::tr_format!(
                    "Не удалось определить формат изображения: {e}",
                    "Could not detect image format: {e}"
                )
            })?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(IMAGE_EDGE_LIMIT);
        limits.max_image_height = Some(IMAGE_EDGE_LIMIT);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|e| {
            crate::tr_format!(
                "Предпросмотр изображения недоступен: {e}",
                "Image preview unavailable: {e}"
            )
        })?;
        let original_size = [decoded.width(), decoded.height()];
        let thumbnail = decoded.thumbnail(1200, 1200).to_rgba8();
        return Ok(Content::Image {
            size: [thumbnail.width() as usize, thumbnail.height() as usize],
            original_size,
            rgba: thumbnail.into_raw(),
        });
    }
    let mut bytes = read_bounded(path, TEXT_LIMIT)?;
    let truncated = bytes.len() > TEXT_LIMIT;
    bytes.truncate(TEXT_LIMIT);
    let body = utf8_prefix(&bytes, truncated)?;
    Ok(Content::Text { body, truncated })
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| {
        crate::tr_format!("Не удалось прочитать файл: {e}", "Could not read file: {e}")
    })?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            crate::tr_format!("Не удалось прочитать файл: {e}", "Could not read file: {e}")
        })?;
    Ok(bytes)
}

fn utf8_prefix(bytes: &[u8], truncated: bool) -> Result<String, String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            // The byte limit can fall in the middle of a UTF-8 character.
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or("")
        }
        Err(_) => {
            return Err(crate::i18n::text(
                "Предпросмотр доступен для текста UTF-8 и изображений PNG, JPEG, GIF, WebP.",
            )
            .into())
        }
    };
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(crate::i18n::text("Для этого формата предпросмотр недоступен.").into());
    }
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

fn format_size(bytes: u64) -> String {
    let units: [&str; 5] = [
        crate::i18n::text("Б"),
        crate::i18n::text("КиБ"),
        crate::i18n::text("МиБ"),
        crate::i18n::text("ГиБ"),
        crate::i18n::text("ТиБ"),
    ];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        crate::tr_format!("{bytes} Б", "{bytes} B")
    } else {
        crate::tr_format!(
            "{value:.1} {} ({bytes} Б)",
            "{value:.1} {} ({bytes} B)",
            units[unit]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_selected_file_reloads_preview_without_changing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "old").unwrap();
        let location: Location = path.clone().into();
        let ctx = egui::Context::default();
        let mut preview = Preview::new();
        let render = |preview: &mut Preview, size| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    preview.show(ui, Some(&location), Some((size, None)));
                });
            });
        };
        render(&mut preview, 3);
        fs::write(&path, "new content").unwrap();
        render(&mut preview, 11);
        for _ in 0..100 {
            render(&mut preview, 11);
            if matches!(&preview.result, Some(Ok(Loaded { content: Content::Text { body, .. }, .. })) if body == "new content")
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("preview did not reload changed file");
    }

    #[test]
    fn truncated_multibyte_character_is_not_reported_as_binary() {
        assert_eq!(utf8_prefix(&[b'a', 0xd0], true).unwrap(), "a");
        assert!(utf8_prefix(&[b'a', 0xd0], false).is_err());
        assert!(utf8_prefix(&[b'a', 0xff], true).is_err());
        assert!(utf8_prefix(b"binary\0data", false).is_err());
    }

    #[test]
    fn text_read_is_bounded_and_preserves_utf8() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.txt");
        fs::write(&path, "я".repeat(TEXT_LIMIT)).unwrap();
        let loaded = load(&path).unwrap();
        match loaded.content {
            Content::Text { body, truncated } => {
                assert!(truncated);
                assert_eq!(body.len(), TEXT_LIMIT);
                assert_eq!(body.chars().count(), TEXT_LIMIT / 2);
            }
            _ => panic!("Expected a text preview"),
        }
    }

    #[test]
    fn rejects_image_with_excessive_dimensions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide.png");
        image::RgbaImage::new(IMAGE_EDGE_LIMIT + 1, 1)
            .save(&path)
            .unwrap();
        let loaded = load(&path).unwrap();
        assert!(matches!(loaded.content, Content::Message(_)));
    }

    #[test]
    fn directory_does_not_get_a_recursive_size_or_text_preview() {
        let directory = tempfile::tempdir().unwrap();
        let loaded = load(directory.path()).unwrap();
        assert!(loaded.is_dir);
        assert!(loaded.size.is_none());
        assert!(matches!(loaded.content, Content::Directory));
    }
}
