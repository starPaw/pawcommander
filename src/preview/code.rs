//! Bounded lexical highlighting: deliberately not a parser or semantic analysis.
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::{
    i18n::text,
    icons::{self, Icon},
    theme::Palette,
};
use eframe::egui::{self, text::LayoutJob, FontId, RichText, TextFormat};
use std::{ops::Range, path::Path};

pub(super) const LINE_LIMIT: usize = 1000;
pub(super) const LINE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    Plain,
    Rust,
    JavaScript,
    Python,
    Json,
}

pub(super) struct Code {
    language: Language,
    spans: Vec<(Range<usize>, Kind)>,
    numbers: String,
}

pub(super) struct Highlights<'a> {
    pub ranges: &'a [Range<usize>],
    pub current: usize,
    pub jump_to: Option<usize>,
}

pub(super) fn bound(body: &mut String) -> bool {
    let mut start = 0;
    for (index, line) in body.split_inclusive('\n').enumerate() {
        let end = if index == LINE_LIMIT {
            Some(start)
        } else if line.len() > LINE_BYTES {
            let mut end = start + LINE_BYTES;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            Some(end)
        } else {
            None
        };
        if let Some(end) = end {
            body.truncate(end);
            return true;
        }
        start += line.len();
    }
    false
}

impl Code {
    pub(super) fn new(path: &Path, body: &str) -> Self {
        Self::numbered(path, body, 1)
    }

    pub(super) fn numbered(path: &Path, body: &str, first: u64) -> Self {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let language = match ext.as_str() {
            "rs" => Language::Rust,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => Language::JavaScript,
            "py" | "pyw" => Language::Python,
            "json" => Language::Json,
            _ => Language::Plain,
        };
        Self {
            language,
            spans: lex(body, language),
            numbers: (0..body.split('\n').count())
                .map(|n| (first + n as u64).to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
    pub(super) fn name(&self) -> &'static str {
        match self.language {
            Language::Plain => text!("Текст"),
            Language::Rust => "Rust",
            Language::JavaScript => "JavaScript / TypeScript",
            Language::Python => "Python",
            Language::Json => "JSON",
        }
    }
    fn job(
        &self,
        body: &str,
        font_size: f32,
        p: &Palette,
        matches: &[Range<usize>],
        current: usize,
    ) -> LayoutJob {
        let mut job = LayoutJob::default();
        // Split only at token and match boundaries; UTF-8 indices remain byte-based.
        let mut points = vec![0, body.len()];
        for (r, _) in &self.spans {
            points.extend([r.start, r.end]);
        }
        for r in matches {
            points.extend([r.start, r.end]);
        }
        points.sort_unstable();
        points.dedup();
        let mut token = 0;
        let mut found = 0;
        for pair in points.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            while token + 1 < self.spans.len() && self.spans[token].0.end <= start {
                token += 1;
            }
            while found < matches.len() && matches[found].end <= start {
                found += 1;
            }
            let kind = self.spans.get(token).map_or(Kind::Plain, |s| s.1);
            let color = match kind {
                Kind::Plain => p.ink,
                Kind::Keyword => p.mauve,
                Kind::String => p.success,
                Kind::Comment => p.muted,
                Kind::Number => p.peach,
            };
            let matched = matches.get(found).is_some_and(|r| r.contains(&start));
            job.append(
                &body[start..end],
                0.0,
                TextFormat {
                    font_id: FontId::monospace(font_size),
                    color: if matched { p.bg } else { color },
                    background: if matched {
                        if found == current {
                            p.blue
                        } else {
                            p.yellow
                        }
                    } else {
                        egui::Color32::TRANSPARENT
                    },
                    ..Default::default()
                },
            );
        }
        job
    }

    pub(super) fn show_text(
        &self,
        ui: &mut egui::Ui,
        body: &str,
        font_size: f32,
        highlights: Highlights<'_>,
        reset: bool,
    ) -> Option<egui::Response> {
        if body.is_empty() {
            ui.label(text!("Пустой файл"));
            return None;
        }
        let palette = Palette::from_ctx(ui.ctx());
        let mut scroll = egui::ScrollArea::both()
            .id_salt("code-preview")
            .max_height(ui.available_height().max(100.0))
            .auto_shrink([false, true]);
        if reset {
            scroll = scroll
                .horizontal_scroll_offset(0.0)
                .vertical_scroll_offset(0.0);
        }
        let output = scroll.show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&self.numbers)
                            .monospace()
                            .size(font_size)
                            .color(palette.muted),
                    )
                    .selectable(false)
                    .extend(),
                );
                let mut layouter = |ui: &egui::Ui, _: &dyn egui::TextBuffer, _: f32| {
                    ui.fonts_mut(|fonts| {
                        fonts.layout_job(self.job(
                            body,
                            font_size,
                            &palette,
                            highlights.ranges,
                            highlights.current,
                        ))
                    })
                };
                // An immutable TextBuffer supports selection and copying.
                let mut buffer = body;
                let id = ui.make_persistent_id("preview-text-editor");
                if reset {
                    egui::text_edit::TextEditState::default().store(ui.ctx(), id);
                    ui.memory_mut(|memory| memory.surrender_focus(id));
                }
                let output = egui::TextEdit::multiline(&mut buffer)
                    .id(id)
                    .font(FontId::monospace(font_size))
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Vec2::ZERO)
                    .show(ui);
                if let Some(byte) = highlights.jump_to {
                    let index = body[..byte].chars().count();
                    let cursor = egui::text::CCursor::new(index);
                    let rect = output
                        .galley
                        .pos_from_cursor(cursor)
                        .translate(output.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect.expand(20.0), Some(egui::Align::Center));
                }
                output.response
            })
            .inner
        });
        Some(output.inner.response)
    }
}

#[derive(Default)]
pub(super) struct Find {
    query: String,
    matches: Vec<Range<usize>>,
    current: usize,
    jump: bool,
    editors: Option<(egui::ViewportId, [egui::Id; 2])>,
    focused: bool,
}

impl Find {
    pub(super) fn clear(&mut self, ctx: &egui::Context) {
        if let Some((viewport, ids)) = self.editors
            && viewport == ctx.viewport_id()
        {
            for id in ids {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
        }
        *self = Self::default();
    }

    pub(super) fn text_input(&self, ctx: &egui::Context) -> bool {
        self.editors.is_some_and(|(viewport, ids)| {
            viewport == ctx.viewport_id() && {
                let focused = ctx.memory(|m| m.focused());
                // Escape can clear focus at frame start, before Quick Look handles keys.
                focused.map_or(self.focused, |id| ids.contains(&id))
            }
        })
    }

    fn update(&mut self, body: &str) {
        self.matches = if self.query.is_empty() {
            Vec::new()
        } else {
            body.match_indices(&self.query)
                .map(|(start, s)| start..start + s.len())
                .collect()
        };
        self.current = 0;
        self.jump = true;
    }
    fn step(&mut self, previous: bool) {
        if !self.matches.is_empty() {
            self.current = if previous {
                (self.current + self.matches.len() - 1) % self.matches.len()
            } else {
                (self.current + 1) % self.matches.len()
            };
            self.jump = true;
        }
    }
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        body: &str,
        code: &Code,
        font_size: f32,
    ) -> egui::Id {
        ui.label(
            RichText::new(code.name())
                .small()
                .color(Palette::from_ctx(ui.ctx()).muted),
        );
        let search = ui
            .horizontal_wrapped(|ui| {
                let id = ui.make_persistent_id("code-find");
                let focused = ui.memory(|m| m.has_focus(id));
                // Consume before TextEdit handles Enter and surrenders keyboard focus.
                let previous = focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter));
                let enter = focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(id)
                        .hint_text(text!("Найти в предпросмотре"))
                        .desired_width(150.0)
                        .char_limit(256),
                );
                ui.ctx().accesskit_node_builder(response.id, |node| {
                    node.set_label(text!("Найти в предпросмотре"));
                });
                if response.changed() {
                    self.update(body);
                }
                if ui
                    .add_enabled_ui(!self.matches.is_empty(), |ui| {
                        icons::compact_button(ui, Icon::Up, text!("Предыдущее совпадение"), false)
                    })
                    .inner
                    .clicked()
                    || previous
                {
                    self.step(true);
                }
                if ui
                    .add_enabled_ui(!self.matches.is_empty(), |ui| {
                        icons::compact_button(
                            ui,
                            Icon::ChevronDown,
                            text!("Следующее совпадение"),
                            false,
                        )
                    })
                    .inner
                    .clicked()
                    || enter
                {
                    self.step(false);
                }
                ui.label(format!(
                    "{} / {}",
                    if self.matches.is_empty() {
                        0
                    } else {
                        self.current + 1
                    },
                    self.matches.len()
                ));
                id
            })
            .inner;
        self.focused = ui.memory(|m| m.has_focus(search));
        self.editors = Some((ui.ctx().viewport_id(), [search, egui::Id::NULL]));
        let jump_to = self
            .matches
            .get(self.current)
            .filter(|_| self.jump)
            .map(|range| range.start);
        let highlights = Highlights {
            ranges: &self.matches,
            current: self.current,
            jump_to,
        };
        if let Some(response) = code.show_text(ui, body, font_size, highlights, false) {
            self.editors = Some((ui.ctx().viewport_id(), [search, response.id]));
            self.focused |= response.has_focus();
        }
        self.jump = false;
        search
    }
}

fn lex(body: &str, language: Language) -> Vec<(Range<usize>, Kind)> {
    let mut spans: Vec<(Range<usize>, Kind)> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let mut kind = Kind::Plain;
        if language == Language::Plain {
            i = bytes.len();
        } else if (language == Language::Python && bytes[i] == b'#')
            || (matches!(language, Language::Rust | Language::JavaScript)
                && body[i..].starts_with("//"))
        {
            kind = Kind::Comment;
            i = body[i..].find('\n').map_or(bytes.len(), |n| i + n);
        } else if matches!(language, Language::Rust | Language::JavaScript)
            && body[i..].starts_with("/*")
        {
            kind = Kind::Comment;
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if language == Language::Rust && body[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if body[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += body[i..].chars().next().unwrap().len_utf8();
                }
            }
        } else if language == Language::Rust && bytes[i] == b'r' && raw_string(body, i).is_some() {
            kind = Kind::String;
            i = raw_string(body, i).unwrap();
        } else if bytes[i] == b'"'
            || (language != Language::Json
                && bytes[i] == b'\''
                && (language != Language::Rust || rust_char(body, i)))
            || (language == Language::JavaScript && bytes[i] == b'`')
        {
            kind = Kind::String;
            let quote = bytes[i];
            let triple = language == Language::Python
                && bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]);
            let delimiter = if triple { 3 } else { 1 };
            i += delimiter;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < bytes.len() {
                        i += body[i..].chars().next().unwrap().len_utf8();
                    }
                } else if bytes
                    .get(i..i + delimiter)
                    .is_some_and(|s| s.iter().all(|&b| b == quote))
                {
                    i += delimiter;
                    break;
                } else {
                    i += body[i..].chars().next().unwrap().len_utf8();
                }
            }
        } else if bytes[i].is_ascii_digit() {
            kind = Kind::Number;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
        } else if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &body[start..i];
            if keywords(language).split_whitespace().any(|w| w == word) {
                kind = Kind::Keyword;
            }
        } else {
            i += body[i..].chars().next().unwrap().len_utf8();
        }
        if let Some((range, last)) = spans.last_mut()
            && *last == kind
        {
            range.end = i;
            continue;
        }
        spans.push((start..i, kind));
    }
    spans
}

fn raw_string(body: &str, start: usize) -> Option<usize> {
    let tail = &body[start + 1..];
    let hashes = tail.bytes().take_while(|&b| b == b'#').count();
    if tail.as_bytes().get(hashes) != Some(&b'"') {
        return None;
    }
    let content = start + 2 + hashes;
    let close = format!("\"{}", "#".repeat(hashes));
    Some(
        body[content..]
            .find(&close)
            .map_or(body.len(), |n| content + n + close.len()),
    )
}
fn rust_char(body: &str, start: usize) -> bool {
    let tail = &body[start + 1..];
    if let Some(escaped) = tail.strip_prefix('\\') {
        escaped.find('\'').is_some_and(|n| n <= 10)
    } else {
        tail.chars()
            .next()
            .is_some_and(|c| tail[c.len_utf8()..].starts_with('\''))
    }
}
fn keywords(language: Language) -> &'static str {
    match language {
        Language::Rust => "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while",
        Language::JavaScript => "as async await break case catch class const continue debugger default delete do else enum export extends false finally for from function if implements import in instanceof interface let new null of private protected public return static super switch this throw true try type typeof undefined var void while with yield",
        Language::Python => "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield",
        Language::Json => "true false null", Language::Plain => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn code_preview_lexing_preserves_strings_comments_and_utf8() {
        for (name, body) in [
            (
                "a.rs",
                "let x = r##\"// let я\"##; /* fn /* x */ */ fn f<'a>() {}",
            ),
            ("a.py", "x = '''# def я\nif'''\n# class\ndef f(): pass"),
            ("a.ts", "const x = `// if я`; /* let */ function f() {}"),
            ("a.json", "{\"true\": false}"),
        ] {
            let code = Code::new(Path::new(name), body);
            assert_eq!(
                code.spans
                    .iter()
                    .map(|(r, _)| &body[r.clone()])
                    .collect::<String>(),
                body
            );
            assert!(code.spans.iter().any(|(_, k)| *k == Kind::String));
            assert!(code.spans.iter().any(|(_, k)| *k == Kind::Keyword));
            assert!(!code
                .spans
                .iter()
                .any(|(r, k)| *k == Kind::Keyword && body[r.clone()].contains("я")));
        }
    }

    #[test]
    fn code_preview_find_scrolls_and_enter_keeps_focus() {
        let ctx = egui::Context::default();
        ctx.global_style_mut(|s| s.animation_time = 0.0);
        let body = format!(
            "{}target\n{}target",
            "line\n".repeat(200),
            "line\n".repeat(200)
        );
        let code = Code::new(Path::new("a.rs"), &body);
        let mut find = Find {
            query: "target".into(),
            ..Default::default()
        };
        find.update(&body);
        let mut scroll_id = egui::Id::NULL;
        let mut search_id = egui::Id::NULL;
        for frame in 0..25 {
            if frame == 1 {
                ctx.memory_mut(|m| m.request_focus(search_id));
            }
            let events = if frame == 10 || frame == 20 || frame == 22 {
                vec![egui::Event::Key {
                    key: if frame == 22 {
                        egui::Key::Escape
                    } else {
                        egui::Key::Enter
                    },
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: if frame == 20 {
                        egui::Modifiers::SHIFT
                    } else {
                        egui::Modifiers::NONE
                    },
                }]
            } else {
                vec![]
            };
            let _ = ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 300.0),
                    )),
                    time: Some(frame as f64 * 0.1),
                    events,
                    ..Default::default()
                },
                |root_ui| {
                    let ctx = &root_ui.ctx().clone();
                    egui::CentralPanel::default().show(root_ui, |ui| {
                        if frame == 22 {
                            assert!(
                                find.text_input(ctx),
                                "Escape must be delivered to a previously focused search field"
                            );
                        }
                        if frame == 23 {
                            assert!(
                                !find.text_input(ctx),
                                "Escape releases search focus for the next shortcut"
                            );
                        }
                        scroll_id = ui.make_persistent_id(egui::IdSalt::new("code-preview"));
                        search_id = find.show(ui, &body, &code, 14.0);
                    });
                },
            );
            if frame == 9 || frame == 19 {
                assert!(
                    egui::scroll_area::State::load(&ctx, scroll_id)
                        .unwrap()
                        .clamped_offset()
                        .y
                        > 1000.0
                );
            }
            if frame == 10 {
                assert_eq!(find.current, 1);
                assert!(ctx.memory(|m| m.has_focus(search_id)));
            }
            if frame == 20 {
                assert_eq!(find.current, 0);
                assert!(ctx.memory(|m| m.has_focus(search_id)));
            }
        }
    }

    #[test]
    fn code_preview_does_not_capture_focused_navigation_buttons() {
        let ctx = egui::Context::default();
        let find = Find {
            editors: Some((
                ctx.viewport_id(),
                [egui::Id::unique("search"), egui::Id::unique("code")],
            )),
            focused: true,
            ..Default::default()
        };
        let _ = ctx.run_test_ui(egui::RawInput::default(), |root_ui| {
            let ctx = &root_ui.ctx().clone();
            egui::CentralPanel::default().show(root_ui, |ui| {
                ui.button("Next file").request_focus();
                assert!(!find.text_input(ctx));
            });
        });
    }

    #[test]
    fn leaving_source_view_clears_search_focus_and_input_capture() {
        let ctx = egui::Context::default();
        let body = "target text";
        let code = Code::new(Path::new("document.md"), body);
        let mut find = Find {
            query: "target".into(),
            ..Default::default()
        };
        find.update(body);
        let _ = ctx.run_test_ui(egui::RawInput::default(), |root_ui| {
            let ctx = &root_ui.ctx().clone();
            egui::CentralPanel::default().show(root_ui, |ui| {
                let id = find.show(ui, body, &code, 14.0);
                ui.memory_mut(|m| m.request_focus(id));
                assert!(find.text_input(ctx));
                find.clear(ctx);
                assert!(!find.text_input(ctx));
                assert!(!ctx.memory(|m| m.has_focus(id)));
                assert!(find.query.is_empty());
                assert!(find.matches.is_empty());
                ui.button("Next file").request_focus();
                let navigation = ctx.memory(|m| m.focused());
                find.clear(ctx);
                assert_eq!(ctx.memory(|m| m.focused()), navigation);
            });
        });
    }

    #[test]
    fn code_preview_limits_and_literal_find_are_utf8_safe() {
        let mut body = "я".repeat(LINE_BYTES);
        assert!(bound(&mut body));
        assert_eq!(body.len(), LINE_BYTES);
        let mut lines = "x\n".repeat(LINE_LIMIT + 5);
        assert!(bound(&mut lines));
        assert_eq!(lines, "x\n".repeat(LINE_LIMIT));
        let mut find = Find {
            query: "я.*".into(),
            ..Default::default()
        };
        find.update("я.* я.*");
        assert_eq!(find.matches, vec![0..4, 5..9]);
        find.step(true);
        assert_eq!(find.current, 1);
        find.step(false);
        assert_eq!(find.current, 0);
        find.query.clear();
        find.update("anything");
        assert!(find.matches.is_empty());
    }
}
