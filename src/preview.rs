#[cfg(test)]
use crate::test_ui::ContextExt as _;
mod code;
mod encoding;
#[cfg(target_os = "macos")]
mod helper;
mod large_text;
mod markup;
#[cfg(target_os = "macos")]
mod media;
#[cfg(target_os = "macos")]
mod pdf;
mod source;
mod table;

use self::encoding::Codec;
use crate::operations::Cancellation;
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
#[cfg(test)]
pub(crate) use large_text::check_sftp_large_text;
#[cfg(target_os = "macos")]
pub(crate) use media::{run_media_worker, MEDIA_WORKER_ARG};
#[cfg(target_os = "macos")]
pub(crate) use pdf::{run_pdf_worker, PDF_WORKER_ARG};

const TEXT_LIMIT: usize = 32 * 1024;
const IMAGE_LIMIT: u64 = 20 * 1024 * 1024;
const IMAGE_EDGE_LIMIT: u32 = 8192;

struct Request {
    generation: u64,
    path: Location,
    context: egui::Context,
    viewport: egui::ViewportId,
    cancel: Cancellation,
    encoding: Option<Codec>,
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
        encoding: Codec,
        body: String,
        truncated: bool,
        code: code::Code,
        markup: Option<markup::Document>,
    },
    Image {
        size: [usize; 2],
        original_size: [u32; 2],
        rgba: Vec<u8>,
    },
    #[cfg(target_os = "macos")]
    Pdf(Box<pdf::Document>),
    #[cfg(target_os = "macos")]
    Media(Box<media::Player>),
    Table(Box<table::Document>),
    LargeText(Box<large_text::Document>),
    TextError(Codec, String),
    Message(String),
}

impl Content {
    fn encoding(&self) -> Option<Codec> {
        match self {
            Self::Text { encoding, .. } | Self::TextError(encoding, _) => Some(*encoding),
            Self::Table(document) => Some(document.encoding()),
            Self::LargeText(document) => Some(document.encoding()),
            _ => None,
        }
    }
}

/// A read-only preview with one background reader and a bounded image decoder.
pub struct Preview {
    requests: Sender<Request>,
    responses: Receiver<Response>,
    path: Option<Location>,
    generation: u64,
    result: Option<Result<Loaded, String>>,
    texture: Option<egui::TextureHandle>,
    text: TextView,
    encoding: Option<Codec>,
    dirty: bool,
    fingerprint: Option<(u64, Option<std::time::SystemTime>)>,
    cancel: Cancellation,
}

#[derive(Default)]
struct TextView {
    find: code::Find,
    source: bool,
    revision: u64,
}

impl Preview {
    pub fn new() -> Self {
        Self::with_quick_look(false)
    }

    pub(crate) fn new_quick_look() -> Self {
        Self::with_quick_look(true)
    }

    fn with_quick_look(quick_look: bool) -> Self {
        let (requests, receiver) = mpsc::channel::<Request>();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut request) = receiver.recv() {
                // Fast navigation should not queue a read for every visited item.
                while let Ok(newer) = receiver.try_recv() {
                    request = newer;
                }
                if request.cancel.is_requested() {
                    continue;
                }
                let result =
                    load_location(&request.path, &request.cancel, quick_look, request.encoding);
                if request.cancel.is_requested() {
                    continue;
                }
                if sender
                    .send(Response {
                        generation: request.generation,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
                request.context.request_repaint_of(request.viewport);
            }
        });
        Self {
            requests,
            responses,
            path: None,
            generation: 0,
            result: None,
            texture: None,
            text: TextView::default(),
            encoding: None,
            dirty: false,
            fingerprint: None,
            cancel: Cancellation::default(),
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
        self.update(ui.ctx(), path, fingerprint);
        let muted = Palette::from_ctx(ui.ctx()).muted;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(crate::i18n::text("СВЕДЕНИЯ"))
                    .size(11.0)
                    .strong()
                    .color(muted),
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
        if let Some(path) = path {
            self.upload_texture(ui.ctx());
            self.show_details(ui, path);
        } else {
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
                    .color(muted),
                );
            });
        }
    }

    pub fn show_quick_look(
        &mut self,
        ui: &mut egui::Ui,
        path: &Location,
        fingerprint: (u64, Option<std::time::SystemTime>),
    ) {
        let changed_path = self.path.as_ref() != Some(path);
        self.update(ui.ctx(), Some(path), Some(fingerprint));
        self.upload_texture(ui.ctx());
        ui.add(egui::Label::new(RichText::new(path.name()).heading()).truncate());
        ui.add_space(8.0);
        if self.encoding_picker(ui) {
            return;
        }
        if let Some(Ok(loaded)) = &mut self.result {
            match &mut loaded.content {
                Content::Table(table) => {
                    ui.push_id(("table-preview", self.generation), |ui| table.show(ui));
                    return;
                }
                Content::LargeText(document) => {
                    ui.push_id(("large-text", self.generation), |ui| document.show(ui));
                    return;
                }
                #[cfg(target_os = "macos")]
                Content::Pdf(document) => {
                    ui.push_id(("pdf-preview", self.generation), |ui| document.show(ui));
                    return;
                }
                #[cfg(target_os = "macos")]
                Content::Media(player) => {
                    ui.push_id(("media-preview", self.generation), |ui| {
                        player.show(ui, &self.cancel)
                    });
                    return;
                }
                _ => {}
            }
        }
        let image_height = ui.available_height().max(1.0);
        let mut scroll = egui::ScrollArea::vertical().id_salt((
            "quick-look-content",
            self.generation,
            self.text.revision,
        ));
        if changed_path {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        scroll
            .auto_shrink([false, false])
            .show(ui, |ui| match &self.result {
                None => draw_loading(ui),
                Some(Err(error)) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                Some(Ok(loaded)) => match &loaded.content {
                    Content::Text {
                        body,
                        truncated,
                        code,
                        markup,
                        ..
                    } => {
                        ui.push_id(self.generation, |ui| {
                            draw_text(
                                ui,
                                body,
                                *truncated,
                                code,
                                markup.as_ref(),
                                &mut self.text,
                                14.0,
                            )
                        });
                    }
                    Content::Image { .. } => {
                        if let Some(texture) = &self.texture {
                            draw_image(ui, texture, image_height);
                        }
                    }
                    Content::Message(message) => {
                        ui.label(message);
                    }
                    Content::TextError(_, error) => {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    Content::Directory => {
                        ui.label(&loaded.kind);
                    }
                    Content::Table(_) | Content::LargeText(_) => {}
                    #[cfg(target_os = "macos")]
                    Content::Pdf(_) | Content::Media(_) => {}
                },
            });
    }

    pub fn text_input(&self, ctx: &egui::Context) -> bool {
        if let Some(Ok(loaded)) = &self.result {
            match &loaded.content {
                Content::Table(table) => return table.text_input(ctx),
                Content::LargeText(document) => return document.text_input(ctx),
                #[cfg(target_os = "macos")]
                Content::Pdf(document) => return document.text_input(ctx),
                _ => {}
            }
        }
        self.text.find.text_input(ctx)
    }

    pub fn seek_focused(&self, ctx: &egui::Context) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(Ok(Loaded {
            content: Content::Media(player),
            ..
        })) = &self.result
        {
            return player.seek_focused(ctx);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = ctx;
        false
    }

    pub fn clear(&mut self, ctx: &egui::Context) {
        self.update(ctx, None, None);
    }

    fn update(
        &mut self,
        ctx: &egui::Context,
        path: Option<&Location>,
        fingerprint: Option<(u64, Option<std::time::SystemTime>)>,
    ) {
        let changed_path = self.path.as_ref() != path;
        if changed_path {
            self.encoding = None;
        }
        if changed_path || self.fingerprint != fingerprint || self.dirty {
            self.cancel.request();
            self.cancel = Cancellation::default();
            self.fingerprint = fingerprint;
            self.dirty = false;
            self.path = path.cloned();
            self.generation = self.generation.wrapping_add(1);
            self.result = None;
            self.text.find.clear(ctx);
            self.text = TextView::default();
            self.texture = None;
            if let Some(path) = path {
                if path.is_archive() {
                    self.result = Some(Err(crate::archive::readonly().into()));
                } else if self
                    .requests
                    .send(Request {
                        generation: self.generation,
                        path: path.clone(),
                        context: ctx.clone(),
                        viewport: ctx.viewport_id(),
                        cancel: self.cancel.clone(),
                        encoding: self.encoding,
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
    }

    fn encoding_picker(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(actual) = self
            .result
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(|loaded| loaded.content.encoding())
        else {
            return false;
        };
        let before = self.encoding;
        let caption = before.map_or_else(
            || crate::tr_format!("Авто ({})", "Auto ({})", actual.name()),
            |codec| codec.name().to_owned(),
        );
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(crate::i18n::text("Кодировка"));
            egui::ComboBox::from_id_salt("preview-text-encoding")
                .selected_text(caption)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.encoding, None, crate::i18n::text("Авто"));
                    for encoding in Codec::ALL {
                        ui.selectable_value(&mut self.encoding, Some(encoding), encoding.name());
                    }
                })
                .response
                .labelled_by(label.id);
        });
        if self.encoding != before {
            self.refresh();
            ui.ctx().request_repaint();
            return true;
        }
        false
    }

    fn upload_texture(&mut self, ctx: &egui::Context) {
        // Upload once; subsequent frames reuse the texture.
        if self.texture.is_none()
            && let Some(Ok(Loaded {
                content: Content::Image { size, rgba, .. },
                ..
            })) = &mut self.result
            && !rgba.is_empty()
        {
            let image = egui::ColorImage::from_rgba_unmultiplied(*size, rgba);
            self.texture = Some(ctx.load_texture(
                format!("file-preview-{}", self.generation),
                image,
                egui::TextureOptions::LINEAR,
            ));
            // The texture owns its pixels now; keep only the small metadata.
            *rgba = Vec::new();
        }
    }

    fn show_details(&mut self, ui: &mut egui::Ui, path: &Location) {
        let muted = Palette::from_ctx(ui.ctx()).muted;
        egui::ScrollArea::vertical()
            .id_salt(("file-preview-details", self.generation, self.text.revision))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                match (&mut self.result, &self.texture) {
                    #[cfg(target_os = "macos")]
                    (
                        Some(Ok(Loaded {
                            content: Content::Pdf(document),
                            ..
                        })),
                        _,
                    ) => {
                        ui.push_id(("pdf-details", self.generation), |ui| {
                            document.thumbnail(ui)
                        });
                    }
                    (Some(Ok(_)), Some(texture)) => draw_image(ui, texture, 220.0),
                    (Some(Ok(loaded)), _) => draw_icon(ui, loaded.is_dir, &path.name()),
                    _ => draw_icon(ui, false, &path.name()),
                }
                ui.add_space(18.0);
                let name = path.name();
                ui.label(RichText::new(name).size(19.0).strong());
                ui.add_space(4.0);

                match &self.result {
                    None => draw_loading(ui),
                    Some(Err(error)) => {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    Some(Ok(loaded)) => {
                        ui.colored_label(muted, &loaded.kind);
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
                if self.encoding_picker(ui) {
                    return;
                }

                if let Some(Ok(loaded)) = &self.result {
                    match &loaded.content {
                        Content::Text {
                            body,
                            truncated,
                            code,
                            markup,
                            ..
                        } => {
                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(crate::i18n::text("ПРЕДПРОСМОТР"))
                                    .size(11.0)
                                    .strong()
                                    .color(muted),
                            );
                            ui.add_space(8.0);
                            ui.push_id(self.generation, |ui| {
                                draw_text(
                                    ui,
                                    body,
                                    *truncated,
                                    code,
                                    markup.as_ref(),
                                    &mut self.text,
                                    11.0,
                                )
                            });
                        }
                        Content::Message(message) => {
                            ui.add_space(12.0);
                            ui.colored_label(muted, message);
                        }
                        Content::TextError(_, error) => {
                            ui.add_space(12.0);
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                        #[cfg(target_os = "macos")]
                        Content::Media(_) => {
                            ui.add_space(12.0);
                            ui.colored_label(
                                muted,
                                crate::i18n::text(
                                    "Откройте быстрый просмотр, чтобы воспроизвести файл.",
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            });
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_loading(ui: &mut egui::Ui) {
    let muted = Palette::from_ctx(ui.ctx()).muted;
    ui.horizontal(|ui| {
        ui.spinner();
        ui.colored_label(muted, crate::i18n::text("Загрузка сведений…"));
    });
}

fn draw_image(ui: &mut egui::Ui, texture: &egui::TextureHandle, max_height: f32) {
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Image::new((texture.id(), texture.size_vec2()))
                .max_width(ui.available_width())
                .max_height(max_height)
                .shrink_to_fit()
                .corner_radius(8),
        );
    });
}

fn draw_text(
    ui: &mut egui::Ui,
    body: &str,
    truncated: bool,
    code: &code::Code,
    markup: Option<&markup::Document>,
    view: &mut TextView,
    font_size: f32,
) {
    if truncated {
        draw_text_limit(ui);
    }
    if let Some(document) = markup {
        let old_source = view.source;
        source_controls(ui, &mut view.source);
        if old_source != view.source {
            view.find.clear(ui.ctx());
            view.revision = view.revision.wrapping_add(1);
        }
        if !view.source {
            ui.push_id(view.revision, |ui| document.show(ui, font_size));
            return;
        }
    }
    ui.push_id(view.revision, |ui| {
        view.find.show(ui, body, code, font_size)
    });
}

fn source_controls(ui: &mut egui::Ui, source: &mut bool) {
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(source, false, crate::i18n::text("Форматированный"));
        ui.selectable_value(source, true, crate::i18n::text("Исходный текст"));
    });
}

fn draw_text_limit(ui: &mut egui::Ui) {
    ui.colored_label(
        Palette::from_ctx(ui.ctx()).muted,
        crate::i18n::text("Показано начало файла: до 32 КиБ, 1000 строк и 8 КиБ на строку."),
    );
}

fn detail(ui: &mut egui::Ui, label: &str, value: &str) {
    let muted = Palette::from_ctx(ui.ctx()).muted;
    let label = ui.label(RichText::new(label).size(11.0).color(muted));
    ui.add(egui::Label::new(value).wrap().selectable(true))
        .labelled_by(label.id);
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

fn load_location(
    location: &Location,
    cancel: &Cancellation,
    quick_look: bool,
    encoding: Option<Codec>,
) -> Result<Loaded, String> {
    if let Some(path) = location.local() {
        return load(path, cancel, quick_look, encoding);
    }
    let separator = quick_look
        .then(|| table::Separator::from_path(location.path()))
        .flatten();
    let limit = if separator.is_some() {
        3
    } else if image_path_format(location.path()).is_some() {
        IMAGE_LIMIT as usize
    } else {
        TEXT_LIMIT
    };
    #[cfg(target_os = "macos")]
    let limit = if media::supported(location.path()) {
        0
    } else if pdf::is_pdf(location.path()) {
        pdf::FILE_LIMIT
    } else {
        limit
    };
    let (stat, bytes) = vfs::preview_data(location, limit)?;
    let content = if stat.is_dir() {
        Content::Directory
    } else if let Some(separator) = separator {
        Content::Table(Box::new(table::Document::new(
            location.clone(),
            separator,
            Codec::choose(encoding, &bytes),
        )))
    } else {
        let length = stat.size.unwrap_or(bytes.len() as u64);
        remote_file_preview(location, bytes, length, cancel, quick_look, encoding)
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

fn remote_file_preview(
    location: &Location,
    bytes: Vec<u8>,
    length: u64,
    cancel: &Cancellation,
    quick_look: bool,
    encoding: Option<Codec>,
) -> Result<Content, String> {
    #[cfg(target_os = "macos")]
    if media::supported(location.path()) {
        return Ok(Content::Media(Box::new(media::Player::new(
            location.clone(),
        ))));
    }
    #[cfg(target_os = "macos")]
    if pdf::is_pdf(location.path()) {
        return pdf_content(bytes, length, cancel);
    }
    if image_path_format(location.path()).is_none() {
        return Ok(text_content(location, bytes, quick_look, encoding));
    }
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = directory.path().join(location.name());
    fs::write(&path, bytes).map_err(|e| e.to_string())?;
    load_content(&path, length, cancel, false, None)
}

fn load(
    path: &Path,
    cancel: &Cancellation,
    quick_look: bool,
    encoding: Option<Codec>,
) -> Result<Loaded, String> {
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
    } else if quick_look && let Some(separator) = table::Separator::from_path(path) {
        match read_bounded(path, 3) {
            Ok(prefix) => Content::Table(Box::new(table::Document::new(
                path.to_path_buf().into(),
                separator,
                Codec::choose(encoding, &prefix),
            ))),
            Err(error) => Content::Message(error),
        }
    } else {
        load_content(path, metadata.len(), cancel, quick_look, encoding)
            .unwrap_or_else(Content::Message)
    };
    Ok(Loaded {
        kind: kind.into(),
        size: metadata.is_file().then_some(metadata.len()),
        modified,
        is_dir,
        content,
    })
}

pub(crate) fn image_path_format(path: &Path) -> Option<image::ImageFormat> {
    use image::ImageFormat;
    let extension = path.extension()?;
    [
        ("png", ImageFormat::Png),
        ("jpg", ImageFormat::Jpeg),
        ("jpeg", ImageFormat::Jpeg),
        ("gif", ImageFormat::Gif),
        ("webp", ImageFormat::WebP),
    ]
    .into_iter()
    .find_map(|(name, format)| extension.eq_ignore_ascii_case(name).then_some(format))
}

fn load_content(
    path: &Path,
    length: u64,
    cancel: &Cancellation,
    quick_look: bool,
    encoding: Option<Codec>,
) -> Result<Content, String> {
    cancel.check().map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    if media::supported(path) {
        return Ok(Content::Media(Box::new(media::Player::new(
            Location::Local(path.into()),
        ))));
    }
    #[cfg(target_os = "macos")]
    if pdf::is_pdf(path) {
        if length > pdf::FILE_LIMIT as u64 {
            return Err(pdf_too_large());
        }
        let file = open_regular(path)?;
        let mut bytes = Vec::new();
        file.take(pdf::FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        return pdf_content(bytes, length, cancel);
    }
    if image_path_format(path).is_some() {
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
    Ok(text_content(
        &Location::Local(path.into()),
        read_bounded(path, TEXT_LIMIT)?,
        quick_look,
        encoding,
    ))
}

fn text_content(
    location: &Location,
    mut bytes: Vec<u8>,
    quick_look: bool,
    encoding: Option<Codec>,
) -> Content {
    let truncated = bytes.len() > TEXT_LIMIT;
    bytes.truncate(TEXT_LIMIT);
    let encoding = Codec::choose(encoding, &bytes);
    let Some(mut body) = encoding
        .prefix(&bytes, truncated)
        .filter(|text| preview_text_chars(text))
    else {
        return Content::TextError(encoding, encoding.error());
    };
    let truncated = code::bound(&mut body) || truncated;
    let markup = markup::parse(location.path(), &body);
    if quick_look && truncated {
        return Content::LargeText(Box::new(large_text::Document::new(
            location.clone(),
            markup,
            encoding,
        )));
    }
    let code = code::Code::new(location.path(), &body);
    Content::Text {
        encoding,
        body,
        truncated,
        code,
        markup,
    }
}

#[cfg(target_os = "macos")]
fn pdf_content(bytes: Vec<u8>, length: u64, cancel: &Cancellation) -> Result<Content, String> {
    if length > pdf::FILE_LIMIT as u64 || bytes.len() > pdf::FILE_LIMIT {
        return Err(pdf_too_large());
    }
    pdf::Document::open(bytes, cancel).map(|document| Content::Pdf(Box::new(document)))
}

#[cfg(target_os = "macos")]
fn pdf_too_large() -> String {
    crate::i18n::text("PDF больше 64 МиБ. Откройте его во внешнем приложении.").into()
}

fn open_regular(path: &Path) -> Result<File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    // The name can change after the metadata lookup. Opening a FIFO must not
    // wait for a writer while the preview verifies the opened file type.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(crate::i18n::text("Предпросмотр специальных файлов недоступен.").into());
    }
    Ok(file)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = open_regular(path).map_err(|e| {
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

fn preview_text_chars(text: &str) -> bool {
    text.chars()
        .all(|ch| (!ch.is_control() && ch != '\u{fffe}') || matches!(ch, '\n' | '\r' | '\t'))
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
    fn markup_load_uses_the_bounded_prefix_without_modifying_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.MARKDOWN");
        let original = format!("# Заголовок\n\n{}", "я".repeat(TEXT_LIMIT));
        fs::write(&path, &original).unwrap();
        let loaded = load(&path, &Cancellation::default(), false, None).unwrap();
        assert!(matches!(
            loaded.content,
            Content::Text {
                truncated: true,
                markup: Some(_),
                ..
            }
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn changed_selected_file_reloads_preview_without_changing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "old").unwrap();
        let location: Location = path.clone().into();
        let ctx = egui::Context::default();
        let mut preview = Preview::new();
        let render = |preview: &mut Preview, size| {
            let _ = ctx.run_test_ui(egui::RawInput::default(), |root_ui| {
                egui::CentralPanel::default().show(root_ui, |ui| {
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
        assert_eq!(Codec::Utf8.prefix(&[b'a', 0xd0], true).unwrap(), "a");
        assert!(Codec::Utf8.prefix(&[b'a', 0xd0], false).is_none());
        assert!(Codec::Utf8.prefix(&[b'a', 0xff], true).is_none());
        assert!(!preview_text_chars("binary\0data"));
    }

    #[test]
    fn quick_look_encoding_menu_recovers_legacy_text_and_resets_on_file_change() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = directory.path().join("legacy.txt");
        let next = directory.path().join("next.txt");
        let original = b"\xcf\xf0\xe8\xe2\xe5\xf2";
        fs::write(&legacy, original).unwrap();
        fs::write(&next, "A new UTF-8 file").unwrap();
        let legacy_location = Location::Local(legacy.clone());
        let next_location = Location::Local(next);
        let ctx = egui::Context::default();
        let mut preview = Preview::new_quick_look();
        let draw = |preview: &mut Preview, location: &Location, events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 540.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| preview.show_quick_look(ui, location, (0, None)),
            )
        };
        let settle = |preview: &mut Preview, location: &Location| {
            for _ in 0..200 {
                let output = draw(preview, location, vec![]);
                if preview.result.is_some() && !preview.dirty {
                    return output;
                }
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
            panic!("preview did not load the selected encoding");
        };
        fn label(output: egui::FullOutput, matches: impl Fn(&str) -> bool) -> egui::Pos2 {
            output
                .shapes
                .into_iter()
                .find_map(|clipped| match clipped.shape {
                    egui::Shape::Text(text) if matches(&text.galley.job.text) => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .expect("encoding menu label")
        }
        let click = |preview: &mut Preview, pos| {
            draw(
                preview,
                &legacy_location,
                vec![egui::Event::PointerMoved(pos)],
            );
            for pressed in [true, false] {
                draw(
                    preview,
                    &legacy_location,
                    vec![egui::Event::PointerButton {
                        pos,
                        pressed,
                        button: egui::PointerButton::Primary,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
            }
        };
        let initial = settle(&mut preview, &legacy_location);
        assert!(matches!(
            &preview.result,
            Some(Ok(Loaded {
                content: Content::TextError(Codec::Utf8, _),
                ..
            }))
        ));
        click(
            &mut preview,
            label(initial, |text| text.ends_with(" (UTF-8)")),
        );
        let popup = draw(&mut preview, &legacy_location, vec![]);
        click(
            &mut preview,
            label(popup, |text| text == Codec::Windows1251.name()),
        );
        settle(&mut preview, &legacy_location);
        assert!(matches!(
            &preview.result,
            Some(Ok(Loaded { content: Content::Text { encoding: Codec::Windows1251, body, .. }, .. }))
            if body == "Привет"
        ));
        preview.refresh();
        settle(&mut preview, &legacy_location);
        assert_eq!(preview.encoding, Some(Codec::Windows1251));
        settle(&mut preview, &next_location);
        assert_eq!(preview.encoding, None);
        assert_eq!(fs::read(legacy).unwrap(), original);
    }

    #[test]
    fn text_read_is_bounded_and_preserves_utf8() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.txt");
        fs::write(&path, "я".repeat(TEXT_LIMIT)).unwrap();
        let loaded = load(&path, &Cancellation::default(), false, None).unwrap();
        match loaded.content {
            Content::Text {
                body, truncated, ..
            } => {
                assert!(truncated);
                assert_eq!(body.len(), code::LINE_BYTES);
                assert_eq!(body.chars().count(), code::LINE_BYTES / 2);
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
        let loaded = load(&path, &Cancellation::default(), false, None).unwrap();
        assert!(matches!(loaded.content, Content::Message(_)));
    }

    #[test]
    fn directory_does_not_get_a_recursive_size_or_text_preview() {
        let directory = tempfile::tempdir().unwrap();
        let loaded = load(directory.path(), &Cancellation::default(), false, None).unwrap();
        assert!(loaded.is_dir);
        assert!(loaded.size.is_none());
        assert!(matches!(loaded.content, Content::Directory));
    }
}
