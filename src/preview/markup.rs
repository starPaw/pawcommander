//! Passive markup: a bounded, flat text model, with no URLs, resource loaders or actions.
use crate::{i18n::text, theme::Palette};
use eframe::egui::{self, text::LayoutJob, FontId, TextFormat};
use html5ever::tokenizer::{
    states::RawKind, BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::{cell::RefCell, path::Path};

const DEPTH_LIMIT: usize = 32;
const EVENT_LIMIT: usize = 8192;
const RUN_LIMIT: usize = 4096;
const BLOCK_LIMIT: usize = 1024;
const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Style {
    strong: bool,
    italic: bool,
    mono: bool,
    strike: bool,
    link: bool,
    heading: u8,
    quote: usize,
    indent: usize,
    pre: bool,
    hidden: bool,
}

#[derive(Debug)]
struct Run {
    text: String,
    style: Style,
}

#[derive(Debug, Default)]
struct Block {
    runs: Vec<Run>,
    rule: bool,
}

#[derive(Debug, Default)]
pub(super) struct Document {
    blocks: Vec<Block>,
    limited: bool,
}

#[derive(Default)]
struct Builder {
    document: Document,
    current: Block,
    style: Style,
    events: usize,
    runs: usize,
    bytes: usize,
}

impl Builder {
    fn event(&mut self) -> bool {
        self.events += 1;
        if self.events > EVENT_LIMIT {
            self.document.limited = true;
        }
        !self.document.limited
    }

    fn append(&mut self, value: &str) {
        if self.style.hidden || self.document.limited || value.is_empty() {
            return;
        }
        let remaining = OUTPUT_LIMIT - self.bytes;
        let mut end = value.len().min(remaining);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let last = self.current.runs.last_mut();
        if let Some(last) = last.filter(|last| last.style == self.style) {
            last.text.push_str(&value[..end]);
        } else if self.runs < RUN_LIMIT {
            self.current.runs.push(Run {
                text: value[..end].to_owned(),
                style: self.style,
            });
            self.runs += 1;
        } else {
            self.document.limited = true;
        }
        self.bytes += end;
        if end < value.len() {
            self.document.limited = true;
        }
    }

    fn flush(&mut self) {
        if !self.current.runs.is_empty() || self.current.rule {
            if self.document.blocks.len() < BLOCK_LIMIT {
                self.document.blocks.push(std::mem::take(&mut self.current));
            } else {
                self.current = Block::default();
                self.document.limited = true;
            }
        }
    }

    fn rule(&mut self) {
        self.flush();
        self.current.rule = true;
        self.flush();
    }

    fn finish(mut self) -> Document {
        self.flush();
        self.document
    }
}

pub(super) fn parse(path: &Path, body: &str) -> Option<Document> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => Some(markdown(body)),
        "html" | "htm" => Some(html(body)),
        _ => None,
    }
}

fn markdown(body: &str) -> Document {
    let mut b = Builder::default();
    let mut stack = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    for event in Parser::new_ext(body, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        if !b.event() {
            break;
        }
        match event {
            Event::Start(tag) => {
                if stack.len() >= DEPTH_LIMIT {
                    b.document.limited = true;
                    break;
                }
                stack.push(b.style);
                match tag {
                    Tag::Paragraph => {}
                    Tag::Heading { level, .. } => {
                        b.flush();
                        b.style.heading = level as u8;
                        b.style.strong = true;
                    }
                    Tag::BlockQuote(_) => {
                        b.flush();
                        b.style.quote += 1;
                    }
                    Tag::CodeBlock(_) => {
                        b.flush();
                        b.style.mono = true;
                        b.style.pre = true;
                    }
                    Tag::List(start) => {
                        b.flush();
                        lists.push(start);
                        b.style.indent += 1;
                    }
                    Tag::Item => {
                        b.flush();
                        if let Some(Some(number)) = lists.last_mut() {
                            b.append(&format!("{number}. "));
                            *number = number.saturating_add(1);
                        } else {
                            b.append("* ");
                        }
                    }
                    Tag::Emphasis => b.style.italic = true,
                    Tag::Strong => b.style.strong = true,
                    Tag::Strikethrough => b.style.strike = true,
                    Tag::Link { .. } => b.style.link = true,
                    Tag::Image { .. } => {
                        b.append(text("[Изображение: "));
                    }
                    Tag::Table(_) | Tag::TableRow => b.flush(),
                    Tag::TableHead => {
                        b.flush();
                        b.style.strong = true;
                    }
                    Tag::TableCell if !b.current.runs.is_empty() => b.append(" | "),
                    _ => {}
                }
            }
            Event::End(tag) => {
                if tag == TagEnd::Image {
                    b.append("]");
                }
                if matches!(tag, TagEnd::List(_)) {
                    lists.pop();
                }
                if matches!(
                    tag,
                    TagEnd::Paragraph
                        | TagEnd::Heading(_)
                        | TagEnd::BlockQuote(_)
                        | TagEnd::CodeBlock
                        | TagEnd::Item
                        | TagEnd::List(_)
                        | TagEnd::TableHead
                        | TagEnd::TableRow
                        | TagEnd::Table
                ) {
                    b.flush();
                }
                b.style = stack.pop().unwrap_or_default();
            }
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                // Raw HTML in Markdown is displayed literally, never interpreted.
                b.append(&value);
            }
            Event::Code(value) => {
                let old = b.style;
                b.style.mono = true;
                b.append(&value);
                b.style = old;
            }
            Event::SoftBreak => b.append(" "),
            Event::HardBreak => b.append("\n"),
            Event::Rule => b.rule(),
            _ => {}
        }
    }
    b.finish()
}

struct HtmlState {
    builder: Builder,
    stack: Vec<(String, Style)>,
    lists: Vec<Option<i64>>,
}

struct HtmlSink(RefCell<HtmlState>);

fn hidden_element(name: &str) -> bool {
    matches!(
        name,
        "head"
            | "title"
            | "script"
            | "style"
            | "iframe"
            | "object"
            | "applet"
            | "form"
            | "button"
            | "select"
            | "textarea"
            | "audio"
            | "video"
            | "svg"
            | "math"
            | "template"
            | "noscript"
            | "canvas"
    )
}

fn close_html(
    b: &mut Builder,
    stack: &mut Vec<(String, Style)>,
    lists: &mut Vec<Option<i64>>,
    index: usize,
) {
    // Optional or malformed ancestor closures must not escape an active suppressed
    // subtree. Its own closing tag may close ordinary descendants, but cannot
    // discard a nested suppression boundary that has not itself been closed.
    if stack
        .iter()
        .rposition(|(name, _)| hidden_element(name))
        .is_some_and(|floor| index < floor)
    {
        return;
    }
    if is_block(&stack[index].0) && !b.style.hidden {
        b.flush();
    }
    for (name, _) in &stack[index..] {
        if matches!(name.as_str(), "ul" | "ol") {
            lists.pop();
        }
    }
    b.style = stack[index].1;
    stack.truncate(index);
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
            | "ul"
            | "ol"
            | "li"
            | "table"
            | "tr"
            | "dl"
            | "dt"
            | "dd"
    )
}

impl TokenSink for HtmlSink {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut state = self.0.borrow_mut();
        let HtmlState {
            builder: b,
            stack,
            lists,
        } = &mut *state;
        if !b.event() {
            return TokenSinkResult::Continue;
        }
        match token {
            Token::CharacterTokens(value) => {
                // Non-whitespace directly in head starts the omitted body. Text
                // inside metadata descendants remains suppressed until they close.
                if value.chars().any(|c| !c.is_ascii_whitespace())
                    && stack.last().is_some_and(|(name, _)| name == "head")
                {
                    close_html(b, stack, lists, stack.len() - 1);
                }
                if b.style.pre {
                    b.append(&value);
                } else {
                    // HTML collapses whitespace, including whitespace split over tokens.
                    for part in value.split_inclusive(char::is_whitespace) {
                        let word = part.trim_end_matches(char::is_whitespace);
                        b.append(word);
                        if word.len() < part.len()
                            && b.current
                                .runs
                                .last()
                                .is_some_and(|r| !r.text.ends_with(' '))
                        {
                            b.append(" ");
                        }
                    }
                }
            }
            Token::TagToken(tag) => {
                let name = tag.name.as_ref();
                if tag.kind == TagKind::EndTag {
                    if let Some(index) = stack.iter().rposition(|(n, _)| n == name) {
                        close_html(b, stack, lists, index);
                    }
                    return TokenSinkResult::Continue;
                }
                // Common optional end tags, without constructing a browser DOM.
                if !matches!(
                    name,
                    "head"
                        | "base"
                        | "link"
                        | "meta"
                        | "script"
                        | "style"
                        | "title"
                        | "noscript"
                        | "template"
                ) && let Some(index) = stack.iter().rposition(|(n, _)| n == "head")
                {
                    close_html(b, stack, lists, index);
                }
                let implied = match name {
                    "li" => stack
                        .iter()
                        .rposition(|(n, _)| matches!(n.as_str(), "li" | "ul" | "ol"))
                        .filter(|&i| stack[i].0 == "li"),
                    "tr" => stack
                        .iter()
                        .rposition(|(n, _)| matches!(n.as_str(), "tr" | "table"))
                        .filter(|&i| stack[i].0 == "tr"),
                    "td" | "th" => stack
                        .iter()
                        .rposition(|(n, _)| matches!(n.as_str(), "td" | "th" | "tr" | "table"))
                        .filter(|&i| matches!(stack[i].0.as_str(), "td" | "th")),
                    _ => None,
                };
                if let Some(index) = implied {
                    close_html(b, stack, lists, index);
                }
                if is_block(name)
                    && let Some(index) = stack.iter().rposition(|(n, _)| n == "p")
                {
                    close_html(b, stack, lists, index);
                }
                if !b.style.hidden {
                    if is_block(name) {
                        b.flush();
                    }
                    match name {
                        "br" => b.append("\n"),
                        "hr" => b.rule(),
                        "img" => {
                            b.append(text("[Изображение: "));
                            if let Some(alt) = tag.attrs.iter().find(|a| &*a.name.local == "alt") {
                                b.append(&alt.value);
                            }
                            b.append("]");
                        }
                        "td" | "th" if !b.current.runs.is_empty() => b.append(" | "),
                        _ => {}
                    }
                }
                if matches!(
                    name,
                    "area"
                        | "base"
                        | "br"
                        | "col"
                        | "embed"
                        | "hr"
                        | "img"
                        | "input"
                        | "link"
                        | "meta"
                        | "param"
                        | "source"
                        | "track"
                        | "wbr"
                ) {
                    return TokenSinkResult::Continue;
                }
                if stack.len() >= DEPTH_LIMIT {
                    b.document.limited = true;
                    return TokenSinkResult::Continue;
                }
                stack.push((name.to_owned(), b.style));
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        b.style.heading = name.as_bytes()[1] - b'0';
                        b.style.strong = true;
                    }
                    "b" | "strong" | "th" | "dt" => b.style.strong = true,
                    "i" | "em" => b.style.italic = true,
                    "s" | "strike" | "del" => b.style.strike = true,
                    "code" | "kbd" | "samp" => b.style.mono = true,
                    "pre" => {
                        b.style.mono = true;
                        b.style.pre = true;
                    }
                    "blockquote" => b.style.quote += 1,
                    "a" => b.style.link = true,
                    "ul" | "ol" => {
                        lists.push((name == "ol").then(|| {
                            tag.attrs
                                .iter()
                                .find(|a| &*a.name.local == "start")
                                .and_then(|a| a.value.trim().parse::<i64>().ok())
                                .unwrap_or(1)
                        }));
                        b.style.indent += 1;
                    }
                    "li" => {
                        if let Some(Some(number)) = lists.last_mut() {
                            b.append(&format!("{number}. "));
                            *number = number.saturating_add(1);
                        } else {
                            b.append("* ");
                        }
                    }
                    // Suppress non-document/active/embedded subtrees; resource attributes are ignored.
                    _ if hidden_element(name) => b.style.hidden = true,
                    _ => {}
                }
                return match name {
                    "script" => TokenSinkResult::RawData(RawKind::ScriptData),
                    "style" | "iframe" | "noembed" | "noframes" | "xmp" => {
                        TokenSinkResult::RawData(RawKind::Rawtext)
                    }
                    "textarea" | "title" => TokenSinkResult::RawData(RawKind::Rcdata),
                    "plaintext" => TokenSinkResult::Plaintext,
                    _ => TokenSinkResult::Continue,
                };
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn html(body: &str) -> Document {
    let tokenizer = Tokenizer::new(
        HtmlSink(RefCell::new(HtmlState {
            builder: Builder::default(),
            stack: Vec::new(),
            lists: Vec::new(),
        })),
        TokenizerOpts::default(),
    );
    let input = BufferQueue::default();
    input.push_back(body.into());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.0.into_inner().builder.finish()
}

impl Document {
    pub(super) fn show(&self, ui: &mut egui::Ui, font_size: f32) {
        let p = Palette::from_ctx(ui.ctx());
        ui.colored_label(p.muted, text("Пассивный просмотр: ссылки неактивны, изображения заменены текстом. Скрипты, стили и внешние ресурсы отключены."));
        if self.limited {
            ui.colored_label(p.muted, text("Форматированный просмотр сокращён из-за ограничения сложности. Исходный текст доступен отдельно."));
        }
        ui.add_space(8.0);
        for block in &self.blocks {
            if block.rule {
                ui.separator();
                continue;
            }
            let Some(first) = block.runs.first() else {
                continue;
            };
            let quote = first.style.quote;
            let indent = first.style.indent.saturating_sub(1) + quote;
            let mut job = LayoutJob::default();
            for run in &block.runs {
                let s = run.style;
                let color = if s.link {
                    p.blue
                } else if quote > 0 {
                    p.muted
                } else {
                    p.ink
                };
                let size = if s.heading > 0 {
                    font_size + (7 - s.heading) as f32 * 2.0
                } else {
                    font_size
                };
                job.append(
                    &run.text,
                    0.0,
                    TextFormat {
                        font_id: if s.mono {
                            FontId::monospace(size)
                        } else {
                            FontId::proportional(size)
                        },
                        color,
                        italics: s.italic,
                        extra_letter_spacing: if s.strong { 0.4 } else { 0.0 },
                        underline: if s.link {
                            egui::Stroke::new(1.0, color)
                        } else {
                            egui::Stroke::NONE
                        },
                        strikethrough: if s.strike {
                            egui::Stroke::new(1.0, color)
                        } else {
                            egui::Stroke::NONE
                        },
                        background: if s.mono {
                            p.raised
                        } else {
                            egui::Color32::TRANSPARENT
                        },
                        ..Default::default()
                    },
                );
            }
            ui.horizontal(|ui| {
                ui.add_space((indent as f32 * 12.0).min(ui.available_width() * 0.3));
                if quote > 0 {
                    ui.colored_label(p.muted, "|");
                }
                job.wrap.max_width = ui.available_width().max(1.0);
                ui.add(egui::Label::new(job).wrap().selectable(true));
            });
            ui.add_space(6.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(document: &Document) -> String {
        document
            .blocks
            .iter()
            .map(|block| {
                block
                    .runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_formats_structures_and_discards_resource_destinations() {
        let document = markdown("# Заголовок\n\nA **strong** and *soft* `code` ~~gone~~.\n\n> quote\n\n3. first\n4. second\n\n```rust\nlet x = 1;\n```\n\n---\n\n| A | B |\n| - | - |\n| one | two |\n\n[click](javascript:alert) ![кот](https://private.invalid/image)");
        let output = plain(&document);
        assert!(!document.limited);
        assert!(output.contains("Заголовок"));
        assert!(output.contains("3. first\n4. second"));
        assert!(output.contains("A | B\none | two"));
        assert!(output.contains("click"));
        assert!(output.contains("кот]"));
        assert!(!output.contains("javascript:"));
        assert!(!output.contains("private.invalid"));
        assert!(document.blocks.iter().any(|b| b.rule));
        let runs: Vec<_> = document.blocks.iter().flat_map(|b| &b.runs).collect();
        for (value, predicate) in [
            (
                "Заголовок",
                (|s: Style| s.heading == 1) as fn(Style) -> bool,
            ),
            ("strong", |s| s.strong),
            ("soft", |s| s.italic),
            ("code", |s| s.mono),
            ("gone", |s| s.strike),
            ("quote", |s| s.quote == 1),
            ("let x", |s| s.pre),
        ] {
            assert!(
                runs.iter()
                    .any(|r| r.text.contains(value) && predicate(r.style)),
                "{value}"
            );
        }
    }

    #[test]
    fn html_is_passive_with_entities_attributes_and_raw_text_elements() {
        let document = html(
            r#"<!doctype html><HTML><head><meta http-equiv="refresh" content="0;url=https://private.invalid"><link rel="stylesheet" href="https://private.invalid"><style>body { background: url(https://private.invalid) }</style><title>secret title</title></head><body onload="secret-handler"><h1>Заголовок &amp; &#x1f431;</h1><p>A <strong>bold</strong> <em>soft</em> <a href="javascript:secret-url">click</a><img src="https://private.invalid" srcset="//private.invalid" alt="кот &gt; пёс" onerror="secret-handler"></p><script>if (a < b) { secretScript(); document.write('<p>secret-script</p>'); }</script><iframe srcdoc="<p>secret-frame</p>">secret-frame</iframe><object>secret-object</object><form action="https://private.invalid"><input value="secret-input">secret-form</form><svg><text>secret-svg</text></svg><math>secret-math</math><template>secret-template</template><audio>secret-audio</audio><video>secret-video</video><p>Visible tail &lt;ok&gt;</p></body></HTML>"#,
        );
        let output = plain(&document);
        assert!(!document.limited);
        assert!(output.contains("Заголовок & 🐱"));
        assert!(output.contains("кот > пёс]"));
        assert!(output.contains("click"));
        assert!(output.contains("Visible tail <ok>"));
        assert!(!output.contains("secret"), "{output}");
        assert!(!output.contains("private.invalid"));
        assert!(!output.contains("javascript:"));
    }

    #[test]
    fn html_preserves_preformatted_unicode_and_simple_structures() {
        let document = html("<h2>Title</h2><blockquote>quote</blockquote><ol><li>first</li><li>second</li></ol><pre>  α\n    β</pre><table><tr><th>A</th><th>B</th></tr><tr><td>one</td><td>two</td></tr></table><hr><p title='a > b'>tail");
        let output = plain(&document);
        assert!(!document.limited);
        assert!(output.contains("1. first\n2. second"));
        assert!(output.contains("  α\n    β"));
        assert!(output.contains("A | B\none | two"));
        assert!(output.ends_with("tail"));
        assert!(document.blocks.iter().any(|b| b.rule));
    }

    #[test]
    fn raw_markdown_html_remains_literal_and_never_enters_html_renderer() {
        let source =
            "<script src='https://private.invalid'>secret()</script>\n\ntext <b>inline</b>";
        let output = plain(&markdown(source));
        assert!(output.contains("<script src="));
        assert!(output.contains("<b>inline</b>"));
    }

    #[test]
    fn common_optional_html_end_tags_do_not_hide_body_or_accumulate_depth() {
        let source = format!("<html><head><title>Hidden title</title><body><h1>Visible</h1><ul>{}</ul><p>one<p>two<table><tr><th>A<th>B<tr><td>x<td>y</table>", "<li>item".repeat(100));
        let document = html(&source);
        let output = plain(&document);
        assert!(!document.limited);
        assert!(output.starts_with("Visible\n"));
        assert!(!output.contains("Hidden title"));
        assert_eq!(output.matches("* item").count(), 100);
        assert!(output.contains("one\ntwo\nA | B\nx | y"), "{output}");
    }

    #[test]
    fn html_ancestor_closures_cannot_escape_suppressed_subtrees() {
        for source in [
            "<p>public<form><p>secret form</p></form><p>tail</p>",
            "<div>public<form></div><p>secret form</p></form><p>tail</p>",
            "<ul><li>public<form><li>secret form</li></form></ul><p>tail</p>",
            "<p>public<form><template></form><p>secret template</p></template></form><p>tail</p>",
            "<p>public</p><head><noscript><div>secret noscript</div></noscript><body><p>tail</p>",
            "<p>public</p><head><noscript></head><div>secret noscript</div></noscript><body><p>tail</p>",
        ] {
            let document = html(source);
            let output = plain(&document);
            assert!(!document.limited);
            assert!(output.contains("public"), "{output}");
            assert!(output.contains("tail"), "{output}");
            assert!(!output.contains("secret"), "{output}");
        }
    }

    #[test]
    fn html_non_whitespace_after_metadata_starts_omitted_body_only_outside_descendants() {
        for source in [
            "<!doctype html><html><head><title>secret title</title>Hello world</html>",
            "<html><head> \n<title>secret title</title> \nHello world</html>",
            "<head><meta charset=utf-8><script>secretScript()</script><style>secret style</style><template>secret template</template><noscript>secret noscript</noscript>&#72;ello world",
        ] {
            let document = html(source);
            assert!(!document.limited);
            assert_eq!(plain(&document), "Hello world");
        }
    }

    #[test]
    fn html_ordered_list_start_is_preserved_with_checked_integer_parsing() {
        for (attribute, expected) in [
            ("start='3'", "3. Third\n4. Fourth"),
            ("start=' -2 '", "-2. Third\n-1. Fourth"),
            ("start='0'", "0. Third\n1. Fourth"),
            (
                "start='9223372036854775807'",
                "9223372036854775807. Third\n9223372036854775807. Fourth",
            ),
            ("start='9223372036854775808'", "1. Third\n2. Fourth"),
            ("start='invalid'", "1. Third\n2. Fourth"),
        ] {
            let document = html(&format!("<ol {attribute}><li>Third<li>Fourth</ol>"));
            assert!(!document.limited);
            assert_eq!(plain(&document), expected);
        }
    }

    #[test]
    fn extension_aliases_are_case_insensitive_and_plain_text_stays_plain() {
        for extension in ["md", "MARKDOWN", "mDoWn", "MKD", "HTML", "hTm"] {
            assert!(parse(Path::new(&format!("file.{extension}")), "hello").is_some());
        }
        assert!(parse(Path::new("file.txt"), "# hello").is_none());
        assert!(parse(Path::new("noextension"), "hello").is_none());
        assert!(markdown("").blocks.is_empty());
        assert!(html("").blocks.is_empty());
    }

    #[test]
    fn complexity_limits_keep_flat_documents_bounded() {
        for document in [
            markdown(&format!("{}deep", "> ".repeat(DEPTH_LIMIT + 2))),
            html(&format!("{}deep", "<b>".repeat(DEPTH_LIMIT + 2))),
            html(&"<p>x</p>".repeat(BLOCK_LIMIT + 2)),
            html(&"<b>x</b>x".repeat(RUN_LIMIT + 2)),
            html(&"<br>".repeat(EVENT_LIMIT + 2)),
        ] {
            assert!(document.limited);
            assert!(document.blocks.len() <= BLOCK_LIMIT);
            assert!(document.blocks.iter().map(|b| b.runs.len()).sum::<usize>() <= RUN_LIMIT);
            assert!(plain(&document).len() <= OUTPUT_LIMIT + BLOCK_LIMIT);
        }
        let mut builder = Builder::default();
        builder.append(&"я".repeat(OUTPUT_LIMIT));
        let document = builder.finish();
        assert!(document.limited);
        assert_eq!(plain(&document).len(), OUTPUT_LIMIT);
    }
}
