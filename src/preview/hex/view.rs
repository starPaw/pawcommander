use super::{
    parse_offset, parse_pattern,
    worker::{self, Direction, Page, Pending, ResultPage, Work, Worker},
    BYTES_PER_ROW, PAGE_BYTES, PATTERN_LIMIT,
};
use crate::{
    i18n::{file_error, text},
    icons::{self, Icon},
    operations::TransferError,
    preview::source::Version,
    theme::Palette,
    vfs::Location,
};
use eframe::egui::{self, text::LayoutJob, FontId, TextFormat};
use std::ops::Range;

pub(in crate::preview) struct Document {
    worker: Worker,
    pending: Option<Pending>,
    version: Option<Version>,
    page: Option<Page>,
    searching: bool,
    offset: String,
    query: String,
    pattern: Option<Vec<u8>>,
    notice: Option<crate::i18n::MessageKey>,
    error: Option<TransferError>,
    reset_scroll: bool,
    focus: Option<(egui::ViewportId, [egui::Id; 2], bool)>,
}

impl Document {
    pub(in crate::preview) fn start(
        location: Location,
        ctx: &egui::Context,
    ) -> Result<Self, TransferError> {
        let worker = worker::start(location, ctx)?;
        let pending = worker.load((Work::Page(0), None))?;
        Ok(Self {
            worker,
            pending: Some(pending),
            version: None,
            page: None,
            searching: false,
            offset: "0".into(),
            query: String::new(),
            pattern: None,
            notice: None,
            error: None,
            reset_scroll: false,
            focus: None,
        })
    }

    pub(in crate::preview) fn text_input(&self, ctx: &egui::Context) -> bool {
        self.focus.is_some_and(|(viewport, ids, was_focused)| {
            viewport == ctx.viewport_id()
                && ctx.memory(|memory| memory.focused().map_or(was_focused, |id| ids.contains(&id)))
        })
    }

    pub(in crate::preview) fn show(&mut self, ui: &mut egui::Ui) {
        let offset = self.navigation(ui);
        let query = self.search(ui);
        self.update();
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, file_error(error));
        } else if let Some(notice) = self.notice {
            ui.label(text(notice));
        }
        if let Some(page) = &self.page {
            if page.bytes.is_empty() {
                ui.label(text!("Пустой файл"));
            } else {
                let last = page.start + page.bytes.len() as u64 - 1;
                ui.weak(crate::tr_format!(
                    "Bytes 0x{:X}–0x{last:X}",
                    page.start,
                    last = last,
                ));
                ui.separator();
                show_bytes(ui, page, std::mem::take(&mut self.reset_scroll));
            }
        }
        let ids = [offset, query];
        let has_focus = ui.memory(|memory| memory.focused().is_some_and(|id| ids.contains(&id)));
        self.focus = Some((ui.ctx().viewport_id(), ids, has_focus));
    }

    fn navigation(&mut self, ui: &mut egui::Ui) -> egui::Id {
        let id = ui.make_persistent_id("hex-offset");
        let available = self.pending.is_none() && self.page.is_some();
        let target = ui
            .horizontal_wrapped(|ui| {
                let previous = ui
                    .add_enabled(
                        available && self.page.as_ref().is_some_and(|page| page.start > 0),
                        egui::Button::new(text!("Предыдущая страница")),
                    )
                    .clicked();
                let next = ui
                    .add_enabled(
                        available && self.page.as_ref().is_some_and(|page| page.has_next),
                        egui::Button::new(text!("Следующая страница")),
                    )
                    .clicked();
                let label = ui.label(text!("Смещение (hex)"));
                let enter = ui.memory(|memory| memory.has_focus(id))
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                let field = ui
                    .add_enabled(
                        available,
                        egui::TextEdit::singleline(&mut self.offset)
                            .id(id)
                            .desired_width(110.0)
                            .char_limit(20),
                    )
                    .labelled_by(label.id);
                if field.changed() {
                    self.notice = None;
                }
                let offset = parse_offset(&self.offset);
                let go = ui
                    .add_enabled(
                        available && offset.is_some(),
                        egui::Button::new(text!("Перейти к смещению")),
                    )
                    .clicked();
                let page = self.page.as_ref()?;
                if previous {
                    Some(page.start - PAGE_BYTES)
                } else if next {
                    page.start.checked_add(PAGE_BYTES)
                } else if available && (go || enter) {
                    if offset.is_none() {
                        self.notice = Some(crate::i18n::key!(
                            "Введите смещение в шестнадцатеричном виде."
                        ));
                    }
                    offset
                } else {
                    None
                }
            })
            .inner;
        if let Some(target) = target {
            self.submit(Work::Page(target));
        }
        id
    }

    fn search(&mut self, ui: &mut egui::Ui) -> egui::Id {
        let id = ui.make_persistent_id("hex-search");
        let direction = ui
            .horizontal_wrapped(|ui| {
                let focused = ui.memory(|memory| memory.has_focus(id));
                let previous = focused
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter)
                    });
                let enter = focused
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                let reply = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(id)
                        .hint_text(text!("Найти байты (DE AD)"))
                        .desired_width(165.0)
                        .char_limit(PATTERN_LIMIT * 3),
                );
                if reply.changed() {
                    self.pattern = parse_pattern(&self.query);
                    self.notice = None;
                    if let Some(page) = &mut self.page {
                        page.hit = None;
                    }
                    if self.searching {
                        self.pending = None;
                        self.searching = false;
                    }
                }
                let available =
                    self.pending.is_none() && self.page.is_some() && self.pattern.is_some();
                let back = ui
                    .add_enabled_ui(available, |ui| {
                        icons::compact_button(ui, Icon::Up, text!("Предыдущее совпадение"), false)
                    })
                    .inner
                    .clicked();
                let next = ui
                    .add_enabled_ui(available, |ui| {
                        icons::compact_button(
                            ui,
                            Icon::ChevronDown,
                            text!("Следующее совпадение"),
                            false,
                        )
                    })
                    .inner
                    .clicked();
                if self.pending.is_some() {
                    ui.spinner();
                    ui.weak(if self.searching {
                        text!("Поиск по файлу…")
                    } else {
                        text!("Чтение страницы…")
                    });
                    if self.searching && ui.button(text!("Отменить поиск")).clicked() {
                        self.pending = None;
                        self.searching = false;
                    }
                } else if let Some(hit) = self.page.as_ref().and_then(|page| page.hit.as_ref()) {
                    let offset = hit.start;
                    ui.weak(crate::tr_format!("Match at 0x{offset:X}", offset = offset,));
                } else if self.pattern.is_none() && !self.query.trim().is_empty() {
                    ui.weak(text!("Введите пары hex-цифр, до 256 байтов."));
                }
                if available && (back || previous) {
                    Some(Direction::Backward)
                } else if available && (next || enter) {
                    Some(Direction::Forward)
                } else {
                    None
                }
            })
            .inner;
        if let (Some(direction), Some(page), Some(pattern)) = (direction, &self.page, &self.pattern)
        {
            let from = page
                .hit
                .as_ref()
                .map_or(page.offset, |hit| match direction {
                    Direction::Forward => hit.start + 1,
                    Direction::Backward => hit.start,
                });
            self.submit(Work::Search {
                pattern: pattern.clone(),
                from,
                direction,
            });
        }
        id
    }

    fn submit(&mut self, work: Work) {
        self.notice = None;
        self.error = None;
        self.searching = matches!(work, Work::Search { .. });
        match self.worker.load((work, self.version)) {
            Ok(pending) => self.pending = Some(pending),
            Err(error) => {
                self.pending = None;
                self.error = Some(error);
                self.searching = false;
            }
        }
    }

    fn update(&mut self) {
        let Some(reply) = self.pending.as_ref().and_then(Pending::poll) else {
            return;
        };
        self.pending = None;
        self.searching = false;
        match reply {
            Ok((version, result)) => {
                self.version = Some(version);
                match result {
                    ResultPage::Page(page) => {
                        self.offset = format!("{:X}", page.offset);
                        self.page = Some(page);
                        self.reset_scroll = true;
                    }
                    ResultPage::NoMatch => self.notice = Some(crate::i18n::key!("Совпадений нет.")),
                    ResultPage::OutsideFile => {
                        self.notice = Some(crate::i18n::key!("Смещение за пределами файла."))
                    }
                }
            }
            Err(error) => self.error = Some(error),
        }
    }
}

fn show_bytes(ui: &mut egui::Ui, page: &Page, reset: bool) {
    let font = FontId::monospace(13.0);
    let height = ui.fonts_mut(|fonts| fonts.row_height(&font)) + 2.0;
    let viewport_height = ui.available_height().max(1.0);
    let mut scroll = egui::ScrollArea::both()
        .id_salt("hex-rows")
        .max_height(viewport_height)
        .auto_shrink([false, false]);
    if reset {
        let row = (page.offset - page.start) / BYTES_PER_ROW as u64;
        let y = row as f32 * (height + ui.spacing().item_spacing.y);
        scroll = scroll
            .horizontal_scroll_offset(0.0)
            .vertical_scroll_offset((y - viewport_height * 0.25).max(0.0));
    }
    let palette = Palette::from_ctx(ui.ctx());
    let marker = (page.offset != page.start).then(|| page.offset..page.offset + 1);
    let highlight = page.hit.as_ref().or(marker.as_ref());
    scroll.show_rows(
        ui,
        height,
        page.bytes.len().div_ceil(BYTES_PER_ROW),
        |ui, rows| {
            for row in rows {
                let start = row * BYTES_PER_ROW;
                let end = (start + BYTES_PER_ROW).min(page.bytes.len());
                let offset = page.start + start as u64;
                let job = format_row(offset, &page.bytes[start..end], highlight, &font, &palette);
                ui.add(egui::Label::new(job).extend().selectable(false));
            }
        },
    );
}

fn format_row(
    offset: u64,
    bytes: &[u8],
    hit: Option<&Range<u64>>,
    font: &FontId,
    palette: &Palette,
) -> LayoutJob {
    let style = |color| TextFormat {
        font_id: font.clone(),
        color,
        ..Default::default()
    };
    let normal = style(palette.ink);
    let muted = style(palette.muted);
    let selected = TextFormat {
        background: palette.blue,
        ..style(palette.bg)
    };
    let byte_style = |index| {
        if hit.is_some_and(|range| range.contains(&(offset + index as u64))) {
            selected.clone()
        } else {
            normal.clone()
        }
    };
    let mut row = LayoutJob::default();
    row.append(&format!("{offset:016X}  "), 0.0, muted.clone());
    for index in 0..BYTES_PER_ROW {
        if index == 8 {
            row.append(" ", 0.0, muted.clone());
        }
        if let Some(byte) = bytes.get(index) {
            row.append(&format!("{byte:02X}"), 0.0, byte_style(index));
            row.append(" ", 0.0, normal.clone());
        } else {
            row.append("   ", 0.0, normal.clone());
        }
    }
    row.append("  |", 0.0, muted.clone());
    for (index, &byte) in bytes.iter().enumerate() {
        let value = if matches!(byte, b' '..=b'~') {
            char::from(byte)
        } else {
            '.'
        };
        row.append(value.encode_utf8(&mut [0; 4]), 0.0, byte_style(index));
    }
    for _ in bytes.len()..BYTES_PER_ROW {
        row.append(" ", 0.0, muted.clone());
    }
    row.append("|", 0.0, muted);
    row
}
