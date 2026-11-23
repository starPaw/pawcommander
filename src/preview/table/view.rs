use super::{
    worker::{Bookmark, Scan, Settings, Worker},
    Codec, Position, Row, Separator,
};
use crate::{
    i18n::{file_error, text},
    operations::TransferError,
    preview::source::Target,
};
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::time::{Duration, Instant};

pub(in crate::preview) struct Document {
    target: Target,
    preferred: Separator,
    encoding: Codec,
    settings: Settings,
    filter_id: Option<egui::Id>,
    filter_due: Option<Instant>,
    worker: Option<Worker>,
    scan: Option<Scan>,
    page: Page,
    previous: Vec<Position>,
    reset_scroll: bool,
}

#[derive(Default)]
struct Page {
    start: Option<Bookmark>,
    rows: Vec<Row>,
    columns: usize,
    scanned: u64,
    next: Option<Position>,
    error: Option<TransferError>,
}

impl Document {
    pub(in crate::preview) fn new(target: Target, preferred: Separator, encoding: Codec) -> Self {
        Self {
            target,
            preferred,
            encoding,
            settings: Settings {
                separator: None,
                header: true,
                query: String::new(),
            },
            filter_id: None,
            filter_due: None,
            worker: None,
            scan: None,
            page: Page::default(),
            previous: Vec::new(),
            reset_scroll: true,
        }
    }

    pub(in crate::preview) fn encoding(&self) -> Codec {
        self.encoding
    }

    pub(in crate::preview) fn text_input(&self, ctx: &egui::Context) -> bool {
        self.filter_id
            .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
    }

    pub(in crate::preview) fn show(&mut self, ui: &mut egui::Ui) {
        let format_changed = self.format_controls(ui);
        let filter_id = ui.scope_id().with("table-filter");
        self.filter_id = Some(filter_id);
        let filter_changed = ui
            .add(
                egui::TextEdit::singleline(&mut self.settings.query)
                    .id(filter_id)
                    .hint_text(text!("Фильтр строк…"))
                    .desired_width(f32::INFINITY)
                    .char_limit(256),
            )
            .changed();
        if format_changed || filter_changed {
            self.previous.clear();
            self.scan = None;
            self.page = Page::default();
            self.filter_due =
                (!format_changed).then(|| Instant::now() + Duration::from_millis(180));
        }
        if let Some(due) = self.filter_due {
            let wait = due.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                self.filter_due = None;
                self.load(ui.ctx(), None);
            } else {
                ui.ctx().request_repaint_after(wait);
            }
        } else if format_changed || (self.worker.is_none() && self.page.error.is_none()) {
            self.load(ui.ctx(), None);
        }
        self.update();
        self.navigation(ui);
        if let Some(error) = &self.page.error {
            ui.colored_label(ui.visuals().error_fg_color, file_error(error));
        } else if self.scan.is_none() && self.filter_due.is_none() && self.page.rows.is_empty() {
            ui.label(if self.settings.query.is_empty() {
                text!("В таблице нет строк данных.")
            } else {
                text!("Нет строк, подходящих под фильтр.")
            });
        }
        if let Some(start) = &self.page.start
            && self.page.columns != 0
        {
            show_table(ui, &self.page, &start.layout.headers, self.reset_scroll);
            self.reset_scroll = false;
        }
    }

    fn format_controls(&mut self, ui: &mut egui::Ui) -> bool {
        let previous = self.settings.separator;
        let mut header_changed = false;
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(text!("Разделитель"));
            let caption = match self.settings.separator {
                Some(separator) => separator.label().into(),
                None => self.page.start.as_ref().map_or_else(
                    || text!("Авто").into(),
                    |start| crate::tr_format!("Auto ({})", start.layout.separator.label()),
                ),
            };
            egui::ComboBox::from_id_salt("table-separator")
                .selected_text(caption)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.settings.separator, None, text!("Авто"));
                    for separator in Separator::ALL {
                        ui.selectable_value(
                            &mut self.settings.separator,
                            Some(separator),
                            separator.label(),
                        );
                    }
                })
                .response
                .labelled_by(label.id);
            header_changed = ui
                .checkbox(
                    &mut self.settings.header,
                    text!("Первая строка — заголовки"),
                )
                .changed();
        });
        previous != self.settings.separator || header_changed
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        let waiting = self.scan.is_some() || self.filter_due.is_some();
        let (previous, next) = ui
            .horizontal_wrapped(|ui| {
                let previous = ui.add_enabled(
                    !waiting && self.page.start.is_some() && !self.previous.is_empty(),
                    egui::Button::new(text!("Предыдущие строки")),
                );
                let next = ui.add_enabled(
                    !waiting && self.page.next.is_some(),
                    egui::Button::new(text!("Следующие строки")),
                );
                if waiting {
                    ui.spinner();
                }
                if self.page.scanned != 0 {
                    ui.label(crate::tr_format!(
                        "Rows on page: {} · records read: {}",
                        self.page.rows.len(),
                        self.page.scanned,
                    ));
                } else if waiting {
                    ui.label(text!("Чтение таблицы…"));
                }
                (previous.clicked(), next.clicked())
            })
            .inner;
        if let Some(start) = &self.page.start {
            let position = if previous {
                self.previous.pop()
            } else if next {
                self.previous.push(start.position);
                self.page.next
            } else {
                None
            };
            if let Some(position) = position {
                self.load(
                    ui.ctx(),
                    Some(Bookmark {
                        layout: start.layout.clone(),
                        position,
                    }),
                );
            }
        }
    }

    fn load(&mut self, ctx: &egui::Context, start: Option<Bookmark>) {
        self.scan = None;
        self.page = Page::default();
        self.reset_scroll = true;
        let result = (|| {
            if self.worker.is_none() {
                self.worker = Some(Worker::start(
                    self.target.clone(),
                    self.preferred,
                    self.encoding,
                    ctx,
                )?);
            }
            self.worker
                .as_ref()
                .unwrap()
                .load(self.settings.clone(), start)
        })();
        match result {
            Ok(scan) => self.scan = Some(scan),
            Err(error) => self.page.error = Some(error),
        }
    }

    fn update(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        let update = scan.take_update();
        if let Some(start) = update.ready {
            self.page.columns = start.layout.headers.len();
            self.page.start = Some(start);
        }
        for row in &update.rows {
            self.page.columns = self.page.columns.max(row.cells.len());
        }
        self.page.rows.extend(update.rows);
        self.page.scanned = self.page.scanned.max(update.scanned);
        if let Some(result) = update.finished {
            self.scan = None;
            match result {
                Ok(next) => self.page.next = next,
                Err(error) => {
                    #[cfg(target_os = "macos")]
                    if crate::preview::source::is_smb_mount_error(&error) {
                        self.page = Page::default();
                    }
                    self.page.error = Some(error);
                }
            }
        }
    }
}

fn show_table(ui: &mut egui::Ui, page: &Page, headers: &[String], reset_scroll: bool) {
    let height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
    let available = (ui.available_height() - height - ui.spacing().item_spacing.y).max(1.0);
    egui::ScrollArea::horizontal()
        .id_salt("table-columns")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut table = TableBuilder::new(ui)
                .id_salt("table-rows")
                .column(Column::initial(55.0).at_least(36.0).clip(true))
                .columns(
                    Column::initial(156.0)
                        .at_least(76.0)
                        .clip(true)
                        .resizable(true),
                    page.columns,
                )
                .striped(true)
                .min_scrolled_height(0.0)
                .max_scroll_height(available)
                .auto_shrink([true, false]);
            if reset_scroll {
                table = table.vertical_scroll_offset(0.0);
            }
            table
                .header(height, |mut row| {
                    row.col(|ui| {
                        ui.strong("#");
                    });
                    for column in 0..page.columns {
                        row.col(|ui| {
                            if let Some(label) =
                                headers.get(column).filter(|label| !label.is_empty())
                            {
                                cell(ui, label, true);
                            } else {
                                ui.strong(crate::tr_format!("Column {}", column + 1));
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(height, page.rows.len(), |mut row| {
                        let data = &page.rows[row.index()];
                        row.col(|ui| {
                            ui.weak(data.number.to_string());
                        });
                        for column in 0..page.columns {
                            row.col(|ui| {
                                if let Some(value) = data.cells.get(column) {
                                    cell(ui, value, false);
                                }
                            });
                        }
                    });
                });
        });
}

fn cell(ui: &mut egui::Ui, value: &str, header: bool) {
    if !ui.is_rect_visible(ui.max_rect()) {
        return;
    }
    let line = value.split(['\n', '\r']).next().unwrap_or_default();
    let prefix = &line[..line.floor_char_boundary(256)];
    let mut label = if prefix.len() < value.len() {
        egui::RichText::new(format!("{prefix}…"))
    } else {
        egui::RichText::new(prefix)
    };
    if header {
        label = label.strong();
    }
    let response = ui.add(egui::Label::new(label).truncate().selectable(false));
    if prefix.len() < value.len() {
        response.on_hover_text(&value[..value.floor_char_boundary(4096)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;

    #[test]
    fn wide_table_keeps_column_resize_when_it_can_scroll_horizontally() {
        let ctx = egui::Context::default();
        let page = Page {
            columns: 8,
            rows: vec![Row {
                number: 2,
                cells: vec!["Value".into()],
            }],
            ..Page::default()
        };
        let headers = vec!["First".into(), "Second".into()];
        let frame = |events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(680.0, 460.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| show_table(ui, &page, &headers, false),
            )
        };
        let second_header = |frame: egui::FullOutput| {
            frame
                .shapes
                .into_iter()
                .find_map(|clipped| match clipped.shape {
                    egui::Shape::Text(text) if text.galley.job.text == "Second" => Some(text.pos),
                    _ => None,
                })
                .expect("visible header")
        };
        frame(vec![]);
        let before = second_header(frame(vec![]));
        let spacing = ctx.style_of(ctx.theme()).spacing.item_spacing.x;
        let divider = before + egui::vec2(-spacing * 0.5, 9.0);
        let wider = divider + egui::vec2(48.0, 0.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(divider)]);
        frame(vec![button(divider, true)]);
        frame(vec![egui::Event::PointerMoved(wider)]);
        let after = second_header(frame(vec![button(wider, false)]));
        assert!(after.x > before.x + 35.0, "{before:?} -> {after:?}");
    }
}
