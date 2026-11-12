//! Docked search results and asynchronous validation for their actions.
use super::{Job, Results};
use crate::{i18n::text, theme::paint_focus, vfs::Location};
use eframe::egui;
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    time::{Duration, Instant},
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct TabTarget {
    pub pane: usize,
    pub tab: u64,
    pub location: Location,
}

pub(crate) enum ReadyAction {
    Open {
        path: PathBuf,
        directory: bool,
        target: TabTarget,
    },
    Reveal {
        path: PathBuf,
        target: TabTarget,
    },
    Copy {
        paths: Vec<PathBuf>,
        destination: Location,
    },
}

enum Action {
    Open(PathBuf, TabTarget),
    Reveal(PathBuf, TabTarget),
    Copy(Vec<PathBuf>, Location),
}

impl Action {
    fn paths(&self) -> &[PathBuf] {
        match self {
            Self::Open(path, _) | Self::Reveal(path, _) => std::slice::from_ref(path),
            Self::Copy(paths, _) => paths,
        }
    }

    fn validate(self, cancel: &AtomicBool) -> Option<Validation> {
        let mut missing = Vec::new();
        let mut error = None;
        let mut directory = false;
        for path in self.paths() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            match path.symlink_metadata() {
                Ok(metadata) if matches!(self, Self::Open(..)) => {
                    if metadata.is_symlink() {
                        match path.metadata() {
                            Ok(target) => directory = target.is_dir(),
                            Err(cause) => error = Some(path_error(path, &cause)),
                        }
                    } else {
                        directory = metadata.is_dir();
                    }
                }
                Ok(_) => {}
                Err(cause)
                    if matches!(
                        cause.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    missing.push(path.clone());
                }
                Err(cause) => {
                    error.get_or_insert_with(|| path_error(path, &cause));
                }
            }
        }
        // A selection change must be visible to the user before copying a different group.
        if !missing.is_empty() || error.is_some() {
            return Some(Validation::Unavailable { missing, error });
        }
        Some(Validation::Ready(match self {
            Self::Open(path, target) => ReadyAction::Open {
                path,
                directory,
                target,
            },
            Self::Reveal(path, target) => ReadyAction::Reveal { path, target },
            Self::Copy(paths, destination) => ReadyAction::Copy { paths, destination },
        }))
    }
}

fn path_error(path: &Path, error: &io::Error) -> String {
    let path = path.display();
    crate::tr_format!(
        "Не удалось проверить результат поиска {path}: {error}",
        "Could not check search result {path}: {error}"
    )
}

enum Validation {
    Ready(ReadyAction),
    Unavailable {
        missing: Vec<PathBuf>,
        error: Option<String>,
    },
}

struct Check {
    // Keep the worker slot occupied while a cancelled or timed-out call drains.
    cancel: Arc<AtomicBool>,
    started: Instant,
    receiver: Receiver<(Instant, Validation)>,
}

impl Drop for Check {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct Snapshot {
    results: Results,
    matching_lines: bool,
    query: String,
    scroll_to_top: bool,
    selected: BTreeSet<PathBuf>,
}

#[derive(Default)]
pub(crate) struct Panel {
    snapshot: Option<Snapshot>,
    check: Option<Check>,
    launch: Option<Job<io::Result<()>>>,
    error: Option<String>,
}

impl Panel {
    pub(crate) fn is_open(&self) -> bool {
        self.snapshot.is_some()
    }

    pub(super) fn replace(&mut self, results: Results, matching_lines: bool, query: String) {
        self.close();
        self.snapshot = Some(Snapshot {
            results,
            matching_lines,
            query,
            scroll_to_top: true,
            selected: BTreeSet::new(),
        });
    }

    fn close(&mut self) {
        self.cancel_pending();
        if let Some(launch) = &self.launch {
            launch.cancel();
        }
        self.snapshot = None;
        self.error = None;
    }

    pub(crate) fn cancel_pending(&mut self) {
        if let Some(check) = &self.check {
            check.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn reject(&mut self, error: impl Into<String>, ctx: &egui::Context) {
        self.error = Some(error.into());
        ctx.request_repaint();
    }

    pub(crate) fn open_file(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.launch_file(ctx, move || open::that_detached(path));
    }

    fn launch_file(
        &mut self,
        ctx: &egui::Context,
        launch: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) {
        if self.launch.is_some() || self.snapshot.is_none() {
            return;
        }
        let (sender, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_ctx = ctx.clone();
        match std::thread::Builder::new().spawn(move || {
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = sender.send(launch());
            }
            drop(sender);
            worker_ctx.request_repaint();
        }) {
            Ok(_) => {
                self.launch = Some(Job { cancel, result });
                self.error = None;
                ctx.request_repaint();
            }
            Err(error) => self.report_launch_error(error, ctx),
        }
    }

    fn poll_launch(&mut self, ctx: &egui::Context) {
        let Some(launch) = &self.launch else {
            return;
        };
        let result = match launch.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(io::ErrorKind::Interrupted.into()),
        };
        let cancelled = launch.cancelled();
        self.launch = None;
        ctx.request_repaint();
        if let (false, Err(error)) = (cancelled, result) {
            self.report_launch_error(error, ctx);
        }
    }

    fn report_launch_error(&mut self, e: io::Error, ctx: &egui::Context) {
        self.reject(
            crate::tr_format!("Не удалось открыть файл: {e}", "Could not open file: {e}"),
            ctx,
        );
    }

    fn start(&mut self, action: Action, ctx: &egui::Context) {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let started = Instant::now();
        let worker_ctx = ctx.clone();
        match std::thread::Builder::new().spawn(move || {
            if let Some(validation) = action.validate(&worker_cancel) {
                let _ = sender.send((Instant::now(), validation));
            }
            drop(sender);
            worker_ctx.request_repaint();
        }) {
            Ok(_) => {
                self.check = Some(Check {
                    cancel,
                    started,
                    receiver,
                });
                self.error = None;
                ctx.request_repaint_after(CHECK_TIMEOUT);
            }
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Не удалось запустить проверку результатов поиска: {error}",
                    "Could not start checking search results: {error}"
                ));
            }
        }
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Option<ReadyAction> {
        self.poll_launch(ctx);
        let check = self.check.as_mut()?;
        let validation = match check.receiver.try_recv() {
            Ok((finished, _)) if finished.duration_since(check.started) >= CHECK_TIMEOUT => None,
            Ok((_, validation)) => Some(validation),
            Err(mpsc::TryRecvError::Empty) => {
                if check.started.elapsed() >= CHECK_TIMEOUT
                    && !check.cancel.swap(true, Ordering::Relaxed)
                {
                    self.error = Some(
                        text("Файловая система не ответила на проверку результатов поиска.").into(),
                    );
                    ctx.request_repaint();
                }
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => Some(Validation::Unavailable {
                missing: Vec::new(),
                error: Some(text("Проверка результатов поиска завершилась неожиданно.").into()),
            }),
        };
        let check = self.check.take()?;
        ctx.request_repaint();
        if check.cancel.load(Ordering::Relaxed) {
            return None;
        }
        match validation {
            Some(Validation::Ready(action)) => return Some(action),
            Some(Validation::Unavailable { missing, error }) => {
                self.remove_missing(missing, error);
            }
            None => {
                self.error = Some(
                    text("Файловая система не ответила на проверку результатов поиска.").into(),
                );
            }
        }
        None
    }

    fn remove_missing(&mut self, missing: Vec<PathBuf>, error: Option<String>) {
        let missing: BTreeSet<_> = missing.into_iter().collect();
        if let Some(snapshot) = &mut self.snapshot {
            snapshot
                .results
                .matches
                .retain(|hit| !missing.contains(&hit.path));
            snapshot.selected.retain(|path| !missing.contains(path));
        }
        self.error = match missing.len() {
            0 => error,
            1 => {
                let path = missing.first().expect("one missing path").display();
                Some(crate::tr_format!(
                    "Результат поиска больше не существует: {path}",
                    "Search result no longer exists: {path}"
                ))
            }
            count => Some(crate::tr_format!(
                "Объектов больше нет на прежнем месте: {count}. Они удалены из результатов поиска.",
                "{count} items no longer exist at their original paths and were removed from the search results."
            )),
        };
    }

    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        target: TabTarget,
        destination: Option<Location>,
        enabled: bool,
        can_copy: bool,
    ) {
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        let max_height = (ctx.screen_rect().height() * 0.44).max(150.0);
        let action_enabled = enabled && self.check.is_none();
        let mut close = false;
        let mut action = None;
        egui::TopBottomPanel::bottom("docked_search_results")
            .resizable(true)
            .default_height(210.0_f32.min(max_height))
            .height_range(150.0..=max_height)
            .show(ctx, |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(text("Результаты поиска"));
                        let count = snapshot.results.matches.len();
                        let limit = if snapshot.results.limited {
                            text("+ · Уточните запрос, показаны первые 1000")
                        } else {
                            ""
                        };
                        ui.label(if snapshot.matching_lines {
                            crate::tr_format!(
                                "Строк с совпадениями: {count}{limit}",
                                "Matching lines: {count}{limit}"
                            )
                        } else {
                            crate::tr_format!("Найдено: {count}{limit}", "Found: {count}{limit}")
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let button = ui.button(text("Закрыть результаты поиска"));
                            if button.clicked() {
                                button.surrender_focus();
                                close = true;
                            }
                            if !snapshot.query.is_empty() {
                                ui.add(
                                    egui::Label::new(&snapshot.query)
                                        .truncate()
                                        .selectable(false),
                                )
                                .on_hover_text(&snapshot.query);
                            }
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        let single = (snapshot.selected.len() == 1)
                            .then(|| snapshot.selected.first())
                            .flatten();
                        if ui
                            .add_enabled(
                                action_enabled && single.is_some() && self.launch.is_none(),
                                egui::Button::new(text("Открыть")),
                            )
                            .clicked()
                        {
                            if let Some(path) = single {
                                action = Some(Action::Open(path.clone(), target.clone()));
                            }
                        }
                        if ui
                            .add_enabled(
                                action_enabled && single.is_some(),
                                egui::Button::new(text("Показать в активной панели")),
                            )
                            .clicked()
                        {
                            if let Some(path) = single {
                                action = Some(Action::Reveal(path.clone(), target.clone()));
                            }
                        }
                        let copy = ui.add_enabled(
                            action_enabled
                                && can_copy
                                && !snapshot.selected.is_empty()
                                && destination.is_some(),
                            egui::Button::new(text("Копировать в соседнюю панель")),
                        );
                        if let Some(destination) = destination {
                            let display = destination.display();
                            let copy = copy.on_hover_text(&display);
                            if copy.clicked() {
                                action = Some(Action::Copy(
                                    snapshot.selected.iter().cloned().collect(),
                                    destination,
                                ));
                            }
                            let label = crate::tr_format!(
                                "Папка назначения: {display}",
                                "Destination: {display}"
                            );
                            ui.add(egui::Label::new(&label).truncate().selectable(false))
                                .on_hover_text(label);
                        }
                    });
                    let count = snapshot.selected.len();
                    ui.small(crate::tr_format!(
                        "Выбрано объектов: {count}",
                        "Selected items: {count}"
                    ));
                    if let Some(error) = &self.error {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(error).color(ui.visuals().error_fg_color),
                            )
                            .truncate(),
                        )
                        .on_hover_text(error);
                    } else if self
                        .check
                        .as_ref()
                        .is_some_and(|check| !check.cancel.load(Ordering::Relaxed))
                    {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text("Проверка результатов поиска…"));
                        });
                    } else if self
                        .launch
                        .as_ref()
                        .is_some_and(|launch| !launch.cancelled())
                    {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text("Открытие файла…"));
                        });
                    } else if !snapshot.results.warning.is_empty() {
                        let warning = &snapshot.results.warning;
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(warning).color(ui.visuals().warn_fg_color),
                            )
                            .truncate(),
                        )
                        .on_hover_text(warning);
                    }
                    snapshot.rows(ui);
                });
            });
        if close {
            self.close();
            ctx.request_repaint();
        } else if let Some(action) = action {
            self.start(action, ctx);
        }
    }
}

impl Snapshot {
    fn rows(&mut self, ui: &mut egui::Ui) {
        let excerpt_height = ui.text_style_height(&egui::TextStyle::Body);
        let row_height = if self.matching_lines {
            28.0_f32.max(excerpt_height) + excerpt_height + ui.spacing().item_spacing.y
        } else {
            28.0_f32.max(excerpt_height)
        };
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        scroll.show_rows(ui, row_height, self.results.matches.len(), |ui, rows| {
            for index in rows {
                let hit = &self.results.matches[index];
                let path = hit.path.display();
                let line = hit.context.as_ref().map(|context| context.line);
                let label = if let Some(line) = line {
                    crate::tr_format!("Строка {line} · {path}", "Line {line} · {path}")
                } else {
                    path.to_string()
                };
                ui.push_id((&hit.path, line), |ui| {
                    let selected = self.selected.contains(&hit.path);
                    let response = ui
                        .add(
                            egui::Button::new(&label)
                                .small()
                                .selected(selected)
                                .truncate()
                                .min_size(egui::vec2(
                                    ui.available_width(),
                                    28.0_f32.max(excerpt_height),
                                )),
                        )
                        .on_hover_text(&label);
                    if response.clicked() {
                        response.request_focus();
                        if selected {
                            self.selected.remove(&hit.path);
                        } else {
                            self.selected.insert(hit.path.clone());
                        }
                        ui.ctx().request_repaint();
                    }
                    ui.ctx().accesskit_node_builder(response.id, |node| {
                        node.set_label(label);
                        node.set_toggled(if self.selected.contains(&hit.path) {
                            egui::accesskit::Toggled::True
                        } else {
                            egui::accesskit::Toggled::False
                        });
                    });
                    if response.gained_focus() {
                        response.scroll_to_me(None);
                    }
                    paint_focus(ui, &response, 5);
                    if let Some(context) = &hit.context {
                        ui.add(
                            egui::Label::new(&context.excerpt)
                                .truncate()
                                .selectable(false),
                        )
                        .on_hover_text(&context.excerpt);
                    }
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{content::LineMatch, SearchMatch};
    use std::fs;

    fn results(paths: &[&Path]) -> Results {
        Results {
            matches: paths
                .iter()
                .enumerate()
                .map(|(line, path)| SearchMatch {
                    path: path.to_path_buf(),
                    context: Some(LineMatch {
                        line: line + 1,
                        excerpt: "match".into(),
                    }),
                })
                .collect(),
            limited: false,
            warning: String::new(),
        }
    }

    fn check(panel: &mut Panel, started: Instant) -> mpsc::Sender<(Instant, Validation)> {
        let (sender, receiver) = mpsc::channel();
        panel.check = Some(Check {
            cancel: Arc::new(AtomicBool::new(false)),
            started,
            receiver,
        });
        sender
    }

    #[test]
    fn a_missing_copy_source_removes_every_hit_and_keeps_the_other_selection() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("missing.txt");
        let present = root.path().join("present.txt");
        fs::write(&present, "match\nmatch\n").unwrap();
        let destination = Location::from(root.path().join("destination"));
        let mut panel = Panel::default();
        panel.replace(
            results(&[&stale, &present, &stale, &present]),
            true,
            "match".into(),
        );
        let selected = &mut panel.snapshot.as_mut().unwrap().selected;
        selected.extend([stale.clone(), present.clone()]);

        let action = Action::Copy(selected.iter().cloned().collect(), destination.clone());
        let validation = action.validate(&AtomicBool::default()).unwrap();
        check(&mut panel, Instant::now())
            .send((Instant::now(), validation))
            .unwrap();
        assert!(panel.poll(&ctx).is_none());
        assert!(panel.error.as_ref().unwrap().contains("missing.txt"));
        let snapshot = panel.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.results.matches.len(), 2);
        assert!(snapshot
            .results
            .matches
            .iter()
            .all(|hit| hit.path == present));
        assert_eq!(snapshot.selected, BTreeSet::from([present.clone()]));

        let retry = Action::Copy(
            snapshot.selected.iter().cloned().collect(),
            destination.clone(),
        );
        let Some(Validation::Ready(ReadyAction::Copy {
            paths,
            destination: captured,
        })) = retry.validate(&AtomicBool::default())
        else {
            panic!("remaining results should pass validation");
        };
        assert_eq!(paths, [present]);
        assert_eq!(captured, destination);
    }

    #[test]
    fn replacing_or_closing_results_discards_a_pending_action() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        let new = root.path().join("new");
        let mut panel = Panel::default();
        panel.replace(results(&[&old]), true, "old".into());
        let sender = check(&mut panel, Instant::now());
        panel.replace(results(&[&new]), true, "new".into());
        sender
            .send((
                Instant::now(),
                Validation::Unavailable {
                    missing: vec![new.clone()],
                    error: None,
                },
            ))
            .unwrap();
        assert!(panel.poll(&ctx).is_none());
        assert_eq!(
            panel.snapshot.as_ref().unwrap().results.matches[0].path,
            new
        );
        assert!(panel.error.is_none());

        let sender = check(&mut panel, Instant::now());
        panel.close();
        let action = ReadyAction::Copy {
            paths: vec![new],
            destination: root.path().to_path_buf().into(),
        };
        sender
            .send((Instant::now(), Validation::Ready(action)))
            .unwrap();
        assert!(panel.poll(&ctx).is_none());
        assert!(panel.snapshot.is_none());
    }

    #[test]
    fn a_timed_out_worker_cannot_dispatch_after_it_finishes() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut panel = Panel::default();
        let sender = check(&mut panel, Instant::now() - CHECK_TIMEOUT);
        assert!(panel.poll(&ctx).is_none());
        assert!(panel.error.is_some());
        assert!(panel.check.is_some());

        let action = ReadyAction::Copy {
            paths: vec![root.path().join("file")],
            destination: root.path().to_path_buf().into(),
        };
        sender
            .send((Instant::now(), Validation::Ready(action)))
            .unwrap();
        assert!(panel.poll(&ctx).is_none());
        assert!(panel.check.is_none());
    }

    #[test]
    fn file_launch_is_bounded_and_only_current_failures_are_reported() {
        fn drain(panel: &mut Panel, ctx: &egui::Context) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while panel.launch.is_some() && Instant::now() < deadline {
                panel.poll(ctx);
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(panel.launch.is_none());
        }

        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        let mut panel = Panel::default();
        panel.replace(results(&[&path]), true, "old".into());
        let (started, started_rx) = mpsc::channel();
        let (finish, finish_rx) = mpsc::channel();
        panel.launch_file(&ctx, move || {
            started.send(std::thread::current().id()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Err(io::Error::other("retired launcher failure"))
        });
        assert_ne!(
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            std::thread::current().id(),
        );
        panel.close();
        panel.replace(results(&[&path]), true, "new".into());
        panel.launch_file(&ctx, || Err(io::Error::other("unexpected second launch")));
        assert!(panel.launch.as_ref().unwrap().cancelled());
        finish.send(()).unwrap();
        drain(&mut panel, &ctx);
        assert!(panel.error.is_none());

        panel.launch_file(&ctx, || Err(io::Error::other("active launcher failure")));
        drain(&mut panel, &ctx);
        assert!(panel.error.unwrap().contains("active launcher failure"));
    }

    #[test]
    fn result_rows_expose_toggle_state_and_focus_after_accessible_click() {
        use egui::accesskit::{Action, ActionRequest, Toggled};

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        let mut panel = Panel::default();
        panel.replace(results(&[&path, &path]), true, "match".into());
        let mut draw = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    panel.show(
                        ctx,
                        TabTarget {
                            pane: 0,
                            tab: 0,
                            location: root.path().to_path_buf().into(),
                        },
                        None,
                        true,
                        false,
                    );
                },
            )
            .platform_output
            .accesskit_update
            .unwrap()
        };
        let rows = |update: egui::accesskit::TreeUpdate| {
            update
                .nodes
                .into_iter()
                .filter(|(_, node)| {
                    node.label()
                        .is_some_and(|label| label.ends_with("file.txt"))
                        && node.supports_action(Action::Click)
                })
                .collect::<Vec<_>>()
        };
        let unselected = rows(draw(Vec::new()));
        assert_eq!(unselected.len(), 2);
        assert!(unselected
            .iter()
            .all(|(_, node)| node.toggled() == Some(Toggled::False)));
        let target = unselected[0].0;
        let _ = draw(vec![egui::Event::AccessKitActionRequest(ActionRequest {
            action: Action::Click,
            target,
            data: None,
        })]);
        let update = draw(Vec::new());
        assert_eq!(update.focus, target);
        let selected = rows(update);
        assert_eq!(selected.len(), 2);
        assert!(selected
            .iter()
            .all(|(_, node)| node.toggled() == Some(Toggled::True)));
    }
}
