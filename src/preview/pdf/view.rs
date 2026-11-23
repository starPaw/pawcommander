use super::{protocol::Session, Answer, Command, MAX_EDGE, QUERY_LIMIT};
use crate::{
    file_error::{PdfAction, PdfError},
    i18n::{file_error, text},
    operations::{Cancellation, TransferError},
    vfs::Location,
};
use eframe::egui;
use std::{sync::mpsc, thread};

pub(in crate::preview) struct Document {
    location: Location,
    pages: usize,
    page: usize,
    page_field: Option<egui::Id>,
    zoom: Option<u16>,
    search: Search,
    requests: mpsc::Sender<Request>,
    replies: mpsc::Receiver<Event>,
    ticket: u64,
    pending: Option<(u64, Raster)>,
    frame: Option<Page>,
    failed_page: Option<(Raster, TransferError)>,
    stopped: Option<TransferError>,
}

#[derive(Default)]
struct Search {
    field: Option<egui::Id>,
    draft: String,
    query: String,
    ticket: Option<u64>,
    matches: Option<Vec<usize>>,
    error: Option<TransferError>,
}

#[derive(Clone, PartialEq, Eq)]
struct Raster {
    page: usize,
    edge: u32,
    highlight: String,
}

struct Page {
    raster: Raster,
    points: egui::Vec2,
    texture: egui::TextureHandle,
}

struct Request {
    command: Command,
    ticket: u64,
    context: egui::Context,
    viewport: egui::ViewportId,
}

enum Event {
    Result(u64, Answer),
    Stopped(TransferError),
}

impl Document {
    pub(in crate::preview) fn open(
        location: &Location,
        bytes: Vec<u8>,
        cancel: &Cancellation,
    ) -> Result<Self, TransferError> {
        let session = Session::open(&bytes, cancel).map_err(|error| error.at(location))?;
        let pages = session.pages();
        let (tx, requests) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let cancel = cancel.clone();
        let worker_location = location.clone();
        thread::Builder::new()
            .name("pawcommander-pdf".into())
            .spawn(move || serve(session, &worker_location, &requests, &replies, &cancel))
            .map_err(|source| {
                PdfError::Io {
                    action: PdfAction::StartReader,
                    source,
                }
                .at(location)
            })?;
        Ok(Self::new(location.clone(), pages, tx, rx))
    }

    fn new(
        location: Location,
        pages: usize,
        requests: mpsc::Sender<Request>,
        replies: mpsc::Receiver<Event>,
    ) -> Self {
        Self {
            location,
            pages,
            page: 0,
            page_field: None,
            zoom: None,
            search: Search::default(),
            requests,
            replies,
            ticket: 0,
            pending: None,
            frame: None,
            failed_page: None,
            stopped: None,
        }
    }

    pub(in crate::preview) fn text_input(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| {
            [self.search.field, self.page_field]
                .into_iter()
                .flatten()
                .any(|id| memory.has_focus(id))
        })
    }

    pub(in crate::preview) fn thumbnail(&mut self, ui: &mut egui::Ui) {
        self.update(ui.ctx());
        self.draw_page(ui, egui::vec2(ui.available_width(), 220.0), true);
        ui.label(crate::tr_format!("Pages: {}", self.pages));
    }

    pub(in crate::preview) fn show(&mut self, ui: &mut egui::Ui) {
        self.update(ui.ctx());
        self.controls(ui);
        self.search_bar(ui);
        ui.separator();
        let viewport_size = ui.available_size().max(egui::Vec2::splat(1.0));
        egui::ScrollArea::both()
            .id_salt(("pdf-page", self.page))
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_page(ui, viewport_size, false));
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        if !self.text_input(ui.ctx()) {
            let (previous, next) = ui.input_mut(|input| {
                (
                    input.consume_key(egui::Modifiers::NONE, egui::Key::PageUp),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::PageDown),
                )
            });
            if previous {
                self.go(self.page.saturating_sub(1));
            } else if next {
                self.go((self.page + 1).min(self.pages - 1));
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.page > 0,
                    egui::Button::new(text!("Предыдущая страница")),
                )
                .clicked()
            {
                self.go(self.page - 1);
            }
            let mut number = self.page + 1;
            let label = ui.label(text!("Страница"));
            let page_field = ui
                .add(egui::DragValue::new(&mut number).range(1..=self.pages))
                .labelled_by(label.id);
            self.page_field = Some(page_field.id);
            if page_field.changed() {
                self.go(number.saturating_sub(1).min(self.pages - 1));
            }
            ui.label(crate::tr_format!("of {}", self.pages));
            if ui
                .add_enabled(
                    self.page + 1 < self.pages,
                    egui::Button::new(text!("Следующая страница")),
                )
                .clicked()
            {
                self.go(self.page + 1);
            }
            egui::ComboBox::from_id_salt("pdf-zoom")
                .selected_text(self.zoom.map_or_else(
                    || text!("Вписать страницу").into(),
                    |percent| format!("{percent}%"),
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.zoom, None, text!("Вписать страницу"));
                    for percent in [25, 50, 75, 100, 125, 150, 200, 250, 300] {
                        ui.selectable_value(&mut self.zoom, Some(percent), format!("{percent}%"));
                    }
                });
        });
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let id = ui.make_persistent_id("pdf-find");
            self.search.field = Some(id);
            let focus_search =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F));
            let label = ui.label(text!("Поиск по PDF"));
            let input = ui
                .add(
                    egui::TextEdit::singleline(&mut self.search.draft)
                        .id(id)
                        .desired_width(165.0)
                        .char_limit(QUERY_LIMIT),
                )
                .labelled_by(label.id);
            if focus_search {
                input.request_focus();
            }
            let enter = input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            let can_search = self.stopped.is_none() && !self.search.draft.trim().is_empty();
            let find = ui
                .add_enabled(can_search, egui::Button::new(text!("Найти")))
                .clicked();
            if can_search && (enter || find) {
                self.find(ui.ctx());
            }
            if !self.search.query.is_empty() && ui.button(text!("Очистить поиск")).clicked()
            {
                let field = self.search.field;
                self.search = Search {
                    field,
                    ..Search::default()
                };
            }
            let matches = self
                .search
                .matches
                .as_ref()
                .is_some_and(|pages| !pages.is_empty());
            let previous = ui
                .add_enabled(matches, egui::Button::new(text!("Предыдущее совпадение")))
                .clicked();
            let next = ui
                .add_enabled(matches, egui::Button::new(text!("Следующее совпадение")))
                .clicked();
            if (previous || next)
                && let Some(pages) = &self.search.matches
            {
                self.go(matching_page(pages, self.page, previous));
            }
        });
        if self.search.ticket.is_some() {
            ui.label(text!("Поиск по PDF…"));
        } else if let Some(error) = &self.search.error {
            show_error(ui, error);
        } else if let Some(pages) = &self.search.matches {
            if pages.is_empty() {
                ui.label(text!("Совпадений нет"));
            } else {
                ui.label(crate::tr_format!("Matching pages: {}", pages.len()));
            }
        }
    }

    fn find(&mut self, ctx: &egui::Context) {
        let query = self.search.draft.trim().to_owned();
        self.search.query = query.clone();
        self.search.matches = None;
        self.search.error = None;
        self.search.ticket = self.send(Command::Search { query }, ctx);
    }

    fn go(&mut self, page: usize) {
        if page != self.page {
            self.page = page;
            self.pending = None;
            self.failed_page = None;
        }
    }

    fn draw_page(&mut self, ui: &mut egui::Ui, available: egui::Vec2, thumbnail: bool) {
        if let Some(error) = &self.stopped {
            show_error(ui, error);
            return;
        }
        let frame = self
            .frame
            .as_ref()
            .filter(|page| page.raster.page == self.page);
        let display_size = frame.map(|frame| {
            page_extent(
                frame.points,
                available,
                if thumbnail { None } else { self.zoom },
            )
        });
        let desired_edge = display_size.map_or(if thumbnail { 600 } else { 1200 }, |area| {
            let pixels = area.x.max(area.y) * ui.ctx().pixels_per_point();
            [600, 1200, 1800, 2400, MAX_EDGE]
                .into_iter()
                .find(|&edge| edge as f32 >= pixels)
                .unwrap_or(MAX_EDGE)
        });
        let mark = self
            .search
            .matches
            .as_ref()
            .is_some_and(|pages| pages.binary_search(&self.page).is_ok());
        let key = Raster {
            page: self.page,
            edge: desired_edge,
            highlight: if mark {
                self.search.query.clone()
            } else {
                String::new()
            },
        };
        if let Some((failed, error)) = &self.failed_page
            && *failed == key
        {
            show_error(ui, error);
            return;
        }
        let shown = frame.is_some_and(|frame| frame.raster == key);
        if shown {
            self.pending = None;
        } else if self
            .pending
            .as_ref()
            .is_none_or(|(_, pending)| *pending != key)
        {
            let command = Command::Render {
                page: key.page,
                edge: key.edge,
                highlight: key.highlight.clone(),
            };
            self.pending = self.send(command, ui.ctx()).map(|ticket| (ticket, key));
        }
        if let Some(error) = &self.stopped {
            show_error(ui, error);
        } else if let Some((frame, size)) = self
            .frame
            .as_ref()
            .filter(|page| {
                page.raster.page == self.page
                    && if mark {
                        page.raster.highlight == self.search.query
                    } else {
                        page.raster.highlight.is_empty()
                    }
            })
            .zip(display_size)
        {
            let margin = (available.x - size.x).max(0.0) * 0.5;
            let (_, space) = ui.allocate_space(egui::vec2(size.x + margin, size.y));
            let rect = egui::Rect::from_min_size(space.min + egui::vec2(margin, 0.0), size);
            ui.put(
                rect,
                egui::Image::new((frame.texture.id(), size))
                    .alt_text(crate::tr_format!("PDF page {}", self.page + 1)),
            );
        } else {
            super::super::draw_loading(ui);
        }
    }

    fn send(&mut self, command: Command, ctx: &egui::Context) -> Option<u64> {
        self.ticket = self.ticket.wrapping_add(1);
        let ticket = self.ticket;
        let request = Request {
            command,
            ticket,
            context: ctx.clone(),
            viewport: ctx.viewport_id(),
        };
        if self.requests.send(request).is_err() {
            self.update(ctx);
            self.stopped
                .get_or_insert_with(|| PdfError::WorkerUnavailable.at(&self.location).into());
            self.search.ticket = None;
            return None;
        }
        Some(ticket)
    }

    fn update(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.replies.try_recv() {
            match event {
                Event::Stopped(reason) => {
                    self.stopped = Some(reason);
                    self.pending = None;
                    self.search.ticket = None;
                    self.frame = None;
                }
                Event::Result(ticket, answer) if self.search.ticket == Some(ticket) => {
                    self.search.ticket = None;
                    match answer {
                        Answer::Matches(matches) => {
                            if !matches.is_empty() && matches.binary_search(&self.page).is_err() {
                                self.go(matching_page(&matches, self.page, false));
                            }
                            self.search.matches = Some(matches);
                        }
                        Answer::Error(error) => {
                            self.search.error = Some(error.at(&self.location).into())
                        }
                        Answer::Page(_) => {
                            self.search.error = Some(
                                PdfError::InvalidResponse(PdfAction::Search)
                                    .at(&self.location)
                                    .into(),
                            );
                        }
                    }
                }
                Event::Result(ticket, answer)
                    if self.pending.as_ref().is_some_and(|(id, _)| *id == ticket) =>
                {
                    let (_, raster) = self.pending.take().unwrap();
                    match answer {
                        Answer::Page(image) => {
                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                [image.width as usize, image.height as usize],
                                &image.rgba,
                            );
                            let texture =
                                ctx.load_texture("pdf-page", color, egui::TextureOptions::LINEAR);
                            self.frame = Some(Page {
                                raster,
                                texture,
                                points: egui::vec2(image.points[0] as f32, image.points[1] as f32),
                            });
                            self.failed_page = None;
                        }
                        Answer::Error(error) => {
                            self.failed_page = Some((raster, error.at(&self.location).into()))
                        }
                        Answer::Matches(_) => {
                            let error =
                                PdfError::InvalidResponse(PdfAction::RenderPage(raster.page))
                                    .at(&self.location)
                                    .into();
                            self.failed_page = Some((raster, error));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn show_error(ui: &mut egui::Ui, error: &TransferError) {
    ui.colored_label(ui.visuals().error_fg_color, file_error(error));
}

fn page_extent(points: egui::Vec2, available: egui::Vec2, zoom: Option<u16>) -> egui::Vec2 {
    let scale = zoom.map_or_else(
        || (available.x / points.x).min(available.y / points.y),
        |percent| f32::from(percent) / 100.0,
    );
    (points * scale).max(egui::Vec2::splat(1.0))
}

fn matching_page(pages: &[usize], page: usize, previous: bool) -> usize {
    if previous {
        let index = pages.partition_point(|&match_page| match_page < page);
        pages[index.checked_sub(1).unwrap_or(pages.len() - 1)]
    } else {
        let index = pages.partition_point(|&match_page| match_page <= page);
        pages.get(index).copied().unwrap_or(pages[0])
    }
}

fn serve(
    mut session: Session,
    location: &Location,
    requests: &mpsc::Receiver<Request>,
    replies: &mpsc::Sender<Event>,
    cancel: &Cancellation,
) {
    while let Ok(first) = requests.recv() {
        let mut render = None;
        let mut search = None;
        for request in std::iter::once(first).chain(requests.try_iter()) {
            match request.command {
                Command::Render { .. } => render = Some(request),
                Command::Search { .. } => search = Some(request),
            }
        }
        for request in [render, search].into_iter().flatten() {
            if cancel.is_requested() {
                return;
            }
            let result = session.run(&request.command, cancel);
            if cancel.is_requested() {
                return;
            }
            let event = match result {
                Ok(answer) => Event::Result(request.ticket, answer),
                Err(error) => match TransferError::from(error.at(location)) {
                    TransferError::Cancelled => return,
                    error => Event::Stopped(error),
                },
            };
            let terminal = matches!(event, Event::Stopped(_));
            if replies.send(event).is_err() {
                return;
            }
            request.context.request_repaint_of(request.viewport);
            if terminal {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{preview::pdf::Image, test_ui::ContextExt as _};
    use egui::{Event as Input, Key, Modifiers};

    #[test]
    fn pdf_disconnected_reader_keeps_the_raster_action_and_original_cause() {
        use crate::file_error::{Failure, PreviewError};
        use std::{error::Error, io};

        let location = Location::Local("/fixtures/selected.pdf".into());
        let (requests, requested) = mpsc::channel();
        let (replies, received) = mpsc::channel();
        let mut view = Document::new(location.clone(), 3, requests, received);
        let source = io::Error::from_raw_os_error(libc::EIO);
        replies
            .send(Event::Stopped(
                PdfError::Io {
                    action: PdfAction::ReadRaster(1),
                    source,
                }
                .at(&location)
                .into(),
            ))
            .unwrap();
        drop(requested);
        drop(replies);

        let ctx = egui::Context::default();
        assert!(view
            .send(
                Command::Search {
                    query: "fixture text".into()
                },
                &ctx
            )
            .is_none());
        let error = view.stopped.as_ref().unwrap();
        let TransferError::Failed(Failure::Preview(preview)) = error else {
            panic!("expected the original PDF failure");
        };
        assert!(matches!(preview.as_ref(), PreviewError::Pdf {
            location: origin,
            cause: PdfError::Io { action: PdfAction::ReadRaster(1), .. },
        } if origin == &location));
        let source = preview
            .source()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(source.raw_os_error(), Some(libc::EIO));
        let message = file_error(error);
        assert!(message.contains(&location.display()) && message.contains(&source.to_string()));
        assert!(!message.contains("fixture text"));
    }

    #[test]
    fn pdf_view_turns_pages_and_applies_the_current_search_and_scale() {
        let ctx = egui::Context::default();
        let (requests, requested) = mpsc::channel();
        let (replies, received) = mpsc::channel();
        let mut view = Document::new(Location::Local("fixture.pdf".into()), 3, requests, received);
        let draw = |view: &mut Document, events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(860.0, 630.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| view.show(ui),
            );
        };
        let tap = |key, modifiers| {
            [true, false]
                .map(|pressed| Input::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers,
                })
                .to_vec()
        };
        let page = || {
            Answer::Page(Image {
                width: 2,
                height: 2,
                points: [612.0, 792.0],
                rgba: vec![255; 16],
            })
        };
        draw(&mut view, vec![]);
        let first = requested.try_recv().unwrap();
        assert!(matches!(first.command, Command::Render { page: 0, .. }));
        draw(&mut view, tap(Key::PageDown, Modifiers::NONE));
        let second = requested.try_recv().unwrap();
        assert!(matches!(second.command, Command::Render { page: 1, .. }));
        replies.send(Event::Result(first.ticket, page())).unwrap();
        replies.send(Event::Result(second.ticket, page())).unwrap();
        draw(&mut view, vec![]);
        assert_eq!(view.frame.as_ref().unwrap().raster.page, 1);
        // Eat a possible smaller fit raster after learning the page dimensions.
        for _ in requested.try_iter() {}

        draw(&mut view, tap(Key::F, Modifiers::COMMAND));
        assert!(view.text_input(&ctx));
        draw(&mut view, vec![Input::Text("needle".into())]);
        draw(&mut view, tap(Key::Enter, Modifiers::NONE));
        let search = requested
            .try_iter()
            .find(|r| matches!(r.command, Command::Search { .. }))
            .unwrap();
        assert!(matches!(search.command, Command::Search { ref query } if query == "needle"));
        replies
            .send(Event::Result(search.ticket, Answer::Matches(vec![0, 2])))
            .unwrap();
        draw(&mut view, vec![]);
        let highlighted = requested.try_recv().unwrap();
        assert!(
            matches!(highlighted.command, Command::Render { page: 2, ref highlight, .. } if highlight == "needle")
        );
        replies
            .send(Event::Result(highlighted.ticket, page()))
            .unwrap();
        view.zoom = Some(200);
        draw(&mut view, vec![]);
        assert!(matches!(
            requested.try_recv().unwrap().command,
            Command::Render {
                page: 2,
                edge: 1800,
                ..
            }
        ));
        assert_eq!(
            page_extent(
                egui::vec2(612.0, 792.0),
                egui::vec2(800.0, 600.0),
                view.zoom
            ),
            egui::vec2(1224.0, 1584.0)
        );
    }
}
