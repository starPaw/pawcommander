//! Transient, read-only WebDAV browsing. This state never enters profile storage.
use super::{footer::human_size, PawCommander};
use crate::{
    i18n::text,
    webdav::{self, Endpoint, Entry, WebDavError},
};
use eframe::egui;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use zeroize::Zeroizing;

pub(super) struct Browser {
    address: String,
    user: String,
    password: Zeroizing<String>,
    history: Vec<Endpoint>,
    entries: Vec<Entry>,
    worker: Option<Worker>,
    error: Option<WebDavError>,
    focus_address: bool,
    previous_focus: Option<egui::Id>,
}

fn field_id(name: &str) -> egui::Id {
    egui::Id::unique(("webdav_input", name))
}

fn clear_input_state(ctx: &egui::Context) {
    for name in ["address", "user", "password"] {
        let id = field_id(name);
        ctx.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id));
        ctx.memory_mut(|memory| memory.surrender_focus(id));
    }
}

struct Worker {
    reply: Receiver<Result<Vec<Entry>, WebDavError>>,
    cancelled: Arc<AtomicBool>,
    history: Vec<Endpoint>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Browser {
    fn new(previous_focus: Option<egui::Id>) -> Self {
        Self {
            address: String::new(),
            user: String::new(),
            password: Zeroizing::new(String::new()),
            history: Vec::new(),
            entries: Vec::new(),
            worker: None,
            error: None,
            focus_address: true,
            previous_focus,
        }
    }

    fn start(&mut self, ctx: &egui::Context, history: Vec<Endpoint>) {
        if self.worker.is_some() {
            return;
        }
        let Some(endpoint) = history.last().cloned() else {
            return;
        };
        let password = self.password.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_worker = cancelled.clone();
        let (send, reply) = mpsc::channel();
        let ctx = ctx.clone();
        self.error = None;
        match std::thread::Builder::new()
            .name("pawcommander-webdav".into())
            .spawn(move || {
                let result = webdav::browse(&endpoint, &password, &|| {
                    cancel_worker.load(Ordering::Relaxed)
                });
                let _ = send.send(result);
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.worker = Some(Worker {
                    reply,
                    cancelled,
                    history,
                })
            }
            Err(_) => self.error = Some(WebDavError::StartFailed),
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        match Endpoint::parse(&self.address, &self.user) {
            Ok(endpoint) => self.start(ctx, vec![endpoint]),
            Err(error) => self.error = Some(error),
        }
    }

    fn poll(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let result = match worker.reply.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(WebDavError::Io),
        };
        let worker = self.worker.take().expect("completed WebDAV worker");
        match result {
            Ok(entries) => {
                self.history.clone_from(&worker.history);
                self.entries = entries;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn show(&mut self, ctx: &egui::Context) -> bool {
        self.poll();
        let idle = self.worker.is_none();
        let mut close = false;
        let mut requested = None;
        let mut connect = false;
        let modal = egui::Modal::new(egui::Id::unique("webdav_browser")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(250.0, 640.0));
            ui.heading(text!("WebDAV / Nextcloud · Только просмотр"));
            ui.add_space(8.0);
            if self.history.is_empty() {
                ui.add_enabled_ui(idle, |ui| {
                    let label = ui.label(text!("Адрес WebDAV"));
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.address)
                            .id(field_id("address"))
                            .desired_width(f32::INFINITY)
                            .hint_text("https://cloud.example/remote.php/dav/files/user/"),
                    ).labelled_by(label.id);
                    if self.focus_address && !ui.is_sizing_pass() {
                        response.request_focus();
                        self.focus_address = false;
                    }
                    let label = ui.label(text!("Имя пользователя"));
                    ui.add(egui::TextEdit::singleline(&mut self.user).id(field_id("user")).desired_width(f32::INFINITY))
                        .labelled_by(label.id);
                    let label = ui.label(text!("Пароль приложения"));
                    let mut password = egui::TextEdit::singleline(&mut *self.password)
                        .id(field_id("password"))
                        .password(true).desired_width(f32::INFINITY)
                        .show(ui);
                    password.response.response.labelled_by(label.id);
                    // egui records plain text in its undoer even for masked fields.
                    password.state.clear_undoer();
                    password.state.store(ctx, field_id("password"));
                    ui.small(text!("Для Nextcloud используйте WebDAV-адрес из настроек файлов и пароль приложения."));
                    connect = ui.button(text!("Подключиться")).clicked();
                });
            } else {
                let endpoint = self.history.last().expect("connected WebDAV endpoint");
                ui.add(egui::Label::new(endpoint.address()).wrap().selectable(true));
                ui.horizontal(|ui| {
                    if ui.add_enabled(idle && self.history.len() > 1,
                        egui::Button::new(text!("Родительская папка"))).clicked() {
                        requested = Some(self.history[..self.history.len() - 1].to_vec());
                    }
                    if ui.add_enabled(idle, egui::Button::new(text!("Обновить"))).clicked() {
                        requested = Some(self.history.clone());
                    }
                });
                ui.separator();
                let row_height = ui.spacing().interact_size.y;
                let name_width = (ui.available_width() - 116.0).max(80.0);
                ui.horizontal(|ui| {
                    ui.add_sized([name_width, row_height], egui::Label::new(text!("Имя")).truncate());
                    ui.strong(text!("Размер"));
                });
                egui::ScrollArea::vertical()
                    .id_salt(("webdav_entries", endpoint.address()))
                    .max_height((ctx.content_rect().height() * 0.46).max(100.0))
                    .show_rows(ui, row_height, self.entries.len(), |ui, rows| {
                        for row in rows {
                            let entry = &self.entries[row];
                            ui.push_id(&entry.address, |ui| {
                                ui.horizontal(|ui| {
                                    if entry.is_dir {
                                        let button = ui.add_enabled_ui(idle, |ui| {
                                            ui.add_sized([name_width, row_height],
                                                egui::Button::new(format!("▸ {}", entry.name)).truncate())
                                        }).inner;
                                        if button.on_hover_text(&entry.name).clicked() {
                                            match endpoint.child(entry) {
                                                Ok(child) => {
                                                    let mut history = self.history.clone();
                                                    history.push(child);
                                                    requested = Some(history);
                                                }
                                                Err(error) => self.error = Some(error),
                                            }
                                        }
                                        ui.label(text!("Папка"));
                                    } else {
                                        ui.add_sized([name_width, row_height],
                                            egui::Label::new(&entry.name).truncate().selectable(true))
                                            .on_hover_text(&entry.name);
                                        ui.label(entry.size.map(human_size).unwrap_or_else(|| "—".into()));
                                    }
                                });
                            });
                        }
                    });
                if self.entries.is_empty() {
                    ui.label(text!("Папка пуста"));
                }
            }
            if !idle {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text!("Чтение папки WebDAV…"));
                });
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, crate::i18n::webdav_error(error));
            }
            ui.separator();
            ui.small(text!("Адрес, учётные данные и результаты не сохраняются. Закрытие окна отменяет запрос и очищает сеанс."));
            close = ui.button(text!("Закрыть")).clicked();
        });
        if close || modal.should_close() {
            clear_input_state(ctx);
            return true;
        }
        if connect {
            self.connect(ctx);
        } else if let Some(history) = requested {
            self.start(ctx, history);
        }
        false
    }
}

impl PawCommander {
    pub(super) fn open_webdav(&mut self, ctx: &egui::Context) {
        clear_input_state(ctx);
        self.webdav = Some(Browser::new(ctx.memory(|memory| memory.focused())));
    }

    pub(super) fn show_webdav(&mut self, ctx: &egui::Context) {
        let Some(browser) = &mut self.webdav else {
            return;
        };
        if browser.show(ctx) {
            let previous_focus = browser.previous_focus;
            self.webdav = None;
            if let Some(id) = previous_focus {
                ctx.memory_mut(|memory| memory.request_focus(id));
            } else {
                self.focus_toolbar = true;
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt;

    fn endpoint(path: &str) -> Endpoint {
        Endpoint::parse(&format!("https://cloud.example/{path}/"), "sample").unwrap()
    }

    fn pending(history: Vec<Endpoint>) -> (Worker, mpsc::Sender<Result<Vec<Entry>, WebDavError>>) {
        let (send, reply) = mpsc::channel();
        (
            Worker {
                reply,
                history,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            send,
        )
    }

    #[test]
    fn closing_browser_cancels_inflight_listing() {
        let mut browser = Browser::new(None);
        let (worker, _send) = pending(vec![endpoint("root")]);
        let cancellation = worker.cancelled.clone();
        browser.password = Zeroizing::new("disposable-app-password".into());
        browser.worker = Some(worker);
        drop(browser);
        assert!(cancellation.load(Ordering::Relaxed));
    }

    #[test]
    fn failed_navigation_preserves_current_folder_and_history() {
        let mut browser = Browser::new(None);
        browser.history = vec![endpoint("root")];
        let (worker, send) = pending(vec![endpoint("root"), endpoint("root/denied")]);
        browser.worker = Some(worker);
        send.send(Err(WebDavError::HttpStatus(403))).unwrap();
        browser.poll();
        assert!(browser.worker.is_none());
        assert_eq!(browser.history.len(), 1);
        assert_eq!(browser.history[0].address(), "https://cloud.example/root/");
        assert!(matches!(browser.error, Some(WebDavError::HttpStatus(403))));
    }

    #[test]
    fn successful_navigation_publishes_only_the_requested_history() {
        let mut browser = Browser::new(None);
        browser.history = vec![endpoint("root")];
        let (worker, send) = pending(vec![endpoint("root"), endpoint("root/child")]);
        browser.worker = Some(worker);
        send.send(Ok(Vec::new())).unwrap();
        browser.poll();
        assert!(browser.worker.is_none());
        assert_eq!(browser.history.len(), 2);
        assert_eq!(
            browser.history[1].address(),
            "https://cloud.example/root/child/"
        );
    }

    #[test]
    fn closing_clears_text_edit_caches_and_reopening_cannot_undo_credentials() {
        let ctx = egui::Context::default();
        let draw = |browser: &mut Browser, events| {
            let mut closed = false;
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 680.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| closed = browser.show(ui.ctx()),
            );
            closed
        };
        let mut browser = Browser::new(None);
        browser.address = "https://private.example/dav/".into();
        browser.user = "private-user".into();
        browser.password = Zeroizing::new("private-app-password".into());
        assert!(!draw(&mut browser, Vec::new()));
        for name in ["address", "user", "password"] {
            assert!(egui::TextEdit::load_state(&ctx, field_id(name)).is_some());
        }
        let password_state = egui::TextEdit::load_state(&ctx, field_id("password")).unwrap();
        assert!(!password_state
            .undoer()
            .has_undo(&(egui::text::CCursorRange::default(), String::new())));
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(draw(&mut browser, vec![escape]));
        drop(browser);
        for name in ["address", "user", "password"] {
            assert!(egui::TextEdit::load_state(&ctx, field_id(name)).is_none());
            assert_ne!(ctx.memory(|memory| memory.focused()), Some(field_id(name)));
        }
        let mut reopened = Browser::new(None);
        reopened.focus_address = false;
        for name in ["address", "user", "password"] {
            ctx.memory_mut(|memory| memory.request_focus(field_id(name)));
            let undo = egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    command: true,
                    mac_cmd: true,
                    ..Default::default()
                },
            };
            assert!(!draw(&mut reopened, vec![undo]));
        }
        assert!(reopened.address.is_empty());
        assert!(reopened.user.is_empty());
        assert!(reopened.password.is_empty());
    }
}
