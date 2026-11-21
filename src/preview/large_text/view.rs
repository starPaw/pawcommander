use super::{
    worker::{self, Page, Pending, ResultPage, Work, Worker},
    Codec, Direction, Position, QUERY_CHARS,
};
use crate::{
    i18n::text,
    icons::{self, Icon},
    preview::{code::Highlights, markup, source::Version},
    vfs::Location,
};
use eframe::egui;

pub(in crate::preview) struct Document {
    location: Location,
    markup: Option<markup::Document>,
    encoding: Codec,
    source: bool,
    viewer: Option<Result<Viewer, String>>,
}

impl Document {
    pub(in crate::preview) fn new(
        location: Location,
        markup: Option<markup::Document>,
        encoding: Codec,
    ) -> Self {
        Self {
            location,
            source: markup.is_none(),
            markup,
            encoding,
            viewer: None,
        }
    }

    pub(in crate::preview) fn encoding(&self) -> Codec {
        self.encoding
    }

    pub(in crate::preview) fn show(&mut self, ui: &mut egui::Ui) {
        if let Some(markup) = &self.markup {
            let previous = self.source;
            super::super::source_controls(ui, &mut self.source);
            let changed = previous != self.source;
            if changed {
                self.viewer = None;
            }
            if !self.source {
                super::super::draw_text_limit(ui);
                let mut scroll = egui::ScrollArea::vertical().id_salt("large-markup");
                if changed {
                    scroll = scroll.vertical_scroll_offset(0.0);
                }
                scroll.show(ui, |ui| markup.show(ui, 14.0));
                return;
            }
        }
        let viewer = self
            .viewer
            .get_or_insert_with(|| Viewer::new(self.location.clone(), self.encoding, ui.ctx()));
        match viewer {
            Ok(viewer) => viewer.show(ui),
            Err(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error.as_str());
            }
        }
    }

    pub(in crate::preview) fn text_input(&self, ctx: &egui::Context) -> bool {
        self.viewer
            .as_ref()
            .and_then(|viewer| viewer.as_ref().ok())
            .is_some_and(|viewer| viewer.text_input(ctx))
    }
}

struct Viewer {
    worker: Worker,
    version: Option<Version>,
    pending: Option<Pending>,
    searching: bool,
    page: Option<Box<Page>>,
    reset_page: bool,
    query: String,
    line: String,
    notice: Option<crate::i18n::MessageKey>,
    error: Option<String>,
    focus: Option<(egui::ViewportId, [egui::Id; 3], bool)>,
}

impl Viewer {
    fn new(location: Location, encoding: Codec, ctx: &egui::Context) -> Result<Self, String> {
        let worker = worker::start(location, encoding, ctx)?;
        let pending = worker.load((
            Work::Page {
                index: 0,
                near: Position::START,
            },
            None,
        ))?;
        Ok(Self {
            worker,
            version: None,
            pending: Some(pending),
            searching: false,
            page: None,
            reset_page: false,
            query: String::new(),
            line: String::new(),
            notice: None,
            error: None,
            focus: None,
        })
    }

    fn text_input(&self, ctx: &egui::Context) -> bool {
        self.focus.is_some_and(|(viewport, ids, was_focused)| {
            viewport == ctx.viewport_id()
                && ctx.memory(|memory| memory.focused().map_or(was_focused, |id| ids.contains(&id)))
        })
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let line = self.page_controls(ui);
        let query = self.find_controls(ui);
        self.update();
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        } else if let Some(notice) = self.notice {
            ui.label(text(notice));
        }
        let mut editor = egui::Id::NULL;
        if let Some(page) = &mut self.page {
            let highlights = Highlights {
                ranges: page.hit_range.as_slice(),
                current: 0,
                jump_to: page.jump.take(),
            };
            ui.separator();
            let reset = std::mem::take(&mut self.reset_page);
            let response = page
                .code
                .show_text(ui, &page.text.text, 14.0, highlights, reset);
            if let Some(response) = response {
                editor = response.id;
            }
        }
        let ids = [line, query, editor];
        let has_focus = ui.memory(|memory| memory.focused().is_some_and(|id| ids.contains(&id)));
        self.focus = Some((ui.ctx().viewport_id(), ids, has_focus));
    }

    fn page_controls(&mut self, ui: &mut egui::Ui) -> egui::Id {
        let id = ui.make_persistent_id("large-text-line");
        let available = self.pending.is_none();
        let page = self.page.as_ref();
        let action = ui
            .horizontal_wrapped(|ui| {
                let previous = ui
                    .add_enabled(
                        available && page.is_some_and(|page| page.text.index > 0),
                        egui::Button::new(text!("Предыдущая страница")),
                    )
                    .clicked();
                let next = ui
                    .add_enabled(
                        available && page.is_some_and(|page| page.text.has_next),
                        egui::Button::new(text!("Следующая страница")),
                    )
                    .clicked();
                if let Some(page) = page {
                    ui.weak(crate::tr_format!(
                        "Страница {}",
                        "Page {}",
                        page.text.index + 1
                    ));
                }
                let enter = ui.memory(|m| m.has_focus(id))
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                ui.add(
                    egui::TextEdit::singleline(&mut self.line)
                        .id(id)
                        .hint_text(text!("Номер строки"))
                        .desired_width(100.0)
                        .char_limit(20),
                );
                let number = self
                    .line
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|&number| number > 0);
                let go = ui
                    .add_enabled(
                        available && page.is_some() && number.is_some(),
                        egui::Button::new(text!("Перейти к строке")),
                    )
                    .clicked();
                let page = page?;
                let near = page.text.start;
                if previous {
                    Some(Work::Page {
                        index: page.text.index - 1,
                        near,
                    })
                } else if next {
                    Some(Work::Page {
                        index: page.text.index + 1,
                        near,
                    })
                } else if available && (go || enter) {
                    number.map(|number| Work::Line { number, near })
                } else {
                    None
                }
            })
            .inner;
        if let Some(action) = action {
            self.submit(action);
        }
        id
    }

    fn find_controls(&mut self, ui: &mut egui::Ui) -> egui::Id {
        let id = ui.make_persistent_id("large-text-find");
        let direction = ui
            .horizontal_wrapped(|ui| {
                let focused = ui.memory(|m| m.has_focus(id));
                let shift_enter = focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter));
                let enter = focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(id)
                        .hint_text(text!("Найти в файле"))
                        .desired_width(165.0)
                        .char_limit(QUERY_CHARS),
                );
                if response.changed() {
                    self.notice = None;
                    if let Some(page) = self.page.as_mut() {
                        page.hit = None;
                        page.hit_range = None;
                    }
                    if self.searching {
                        self.pending = None;
                        self.searching = false;
                    }
                }
                let enabled =
                    self.pending.is_none() && self.page.is_some() && !self.query.is_empty();
                let previous = ui
                    .add_enabled_ui(enabled, |ui| {
                        icons::compact_button(ui, Icon::Up, text!("Предыдущее совпадение"), false)
                    })
                    .inner
                    .clicked();
                let next = ui
                    .add_enabled_ui(enabled, |ui| {
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
                } else if let Some(hit) = self.page.as_ref().and_then(|page| page.hit) {
                    let line = hit.start.line;
                    ui.weak(crate::tr_format!(
                        "Совпадение в строке {line}",
                        "Match on line {line}"
                    ));
                }
                if enabled && (previous || shift_enter) {
                    Some(Direction::Backward)
                } else if enabled && (next || enter) {
                    Some(Direction::Forward)
                } else {
                    None
                }
            })
            .inner;
        if let Some(direction) = direction
            && let Some(page) = &self.page
        {
            let from = page.hit.map_or(page.text.start, |hit| match direction {
                Direction::Forward => hit.end,
                Direction::Backward => hit.start,
            });
            self.submit(Work::Match {
                query: self.query.clone(),
                from,
                direction,
            });
        }
        id
    }

    fn submit(&mut self, work: Work) {
        self.notice = None;
        self.error = None;
        self.pending = None;
        self.searching = matches!(work, Work::Match { .. });
        match self.worker.load((work, self.version)) {
            Ok(pending) => self.pending = Some(pending),
            Err(error) => {
                self.error = Some(error);
                self.searching = false;
            }
        }
    }

    fn update(&mut self) {
        let Some(result) = self.pending.as_ref().and_then(Pending::poll) else {
            return;
        };
        self.pending = None;
        self.searching = false;
        match result {
            Ok((version, result)) => {
                self.version = Some(version);
                match result {
                    ResultPage::Page(page) => {
                        self.page = Some(page);
                        // Keep one scroll and selection state even after
                        // visiting many pages of a long file.
                        self.reset_page = true;
                    }
                    ResultPage::NoMatch => self.notice = Some(crate::i18n::key!("Совпадений нет.")),
                    ResultPage::NoLine => {
                        self.notice = Some(crate::i18n::key!("Такой строки нет."))
                    }
                }
            }
            Err(error) => self.error = Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;
    use eframe::egui::{Event, Key, Modifiers};
    use std::{io::Write, thread, time::Duration};

    #[test]
    fn keyboard_search_and_line_jump_display_results_beyond_the_first_page() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in 1..=3800 {
            writeln!(file, "{line:04} a normal line for the large text preview").unwrap();
            if line == 1500 || line == 3600 {
                writeln!(file, "needle at {line}").unwrap();
            }
        }
        let ctx = egui::Context::default();
        let mut document =
            Document::new(Location::Local(file.path().to_owned()), None, Codec::Utf8);
        let draw = |document: &mut Document, events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 540.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| document.show(ui),
            );
        };
        let settle = |document: &mut Document| {
            for _ in 0..250 {
                draw(document, vec![]);
                let viewer = document.viewer.as_ref().unwrap().as_ref().unwrap();
                if viewer.pending.is_none() {
                    return;
                }
                thread::sleep(Duration::from_millis(4));
            }
            panic!("text preview did not finish its request");
        };
        fn current(document: &Document) -> &Page {
            document
                .viewer
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .page
                .as_ref()
                .unwrap()
        }
        let enter = |modifiers| {
            [true, false].map(|pressed| Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            })
        };

        settle(&mut document);
        assert_eq!(current(&document).text.index, 0);
        let [line_input, query_input, editor] = document
            .viewer
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .focus
            .unwrap()
            .1;
        ctx.memory_mut(|memory| memory.request_focus(query_input));
        draw(&mut document, vec![Event::Text("needle".into())]);
        draw(&mut document, enter(Modifiers::NONE).to_vec());
        settle(&mut document);
        assert_eq!(current(&document).hit.unwrap().start.line, 1501);
        assert!(current(&document).text.index > 0);
        draw(&mut document, enter(Modifiers::SHIFT).to_vec());
        settle(&mut document);
        assert_eq!(current(&document).hit.unwrap().start.line, 3602);

        ctx.memory_mut(|memory| memory.request_focus(line_input));
        draw(&mut document, vec![Event::Text("2700".into())]);
        draw(&mut document, enter(Modifiers::NONE).to_vec());
        settle(&mut document);
        let page = current(&document);
        assert!(page.hit.is_none());
        assert!(page.text.text.contains("2699 a normal line"));
        assert_eq!(
            document
                .viewer
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .focus
                .unwrap()
                .1[2],
            editor
        );
    }
}
