use super::{Action, PawCommander};
use crate::{i18n::text, smb, vfs::Location};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use zeroize::Zeroizing;

#[derive(Clone, Default)]
struct Address {
    url: String,
    user: String,
    guest: bool,
}

pub(super) struct Form {
    address: Address,
    password: Zeroizing<String>,
    remember: bool,
}

impl Form {
    pub(super) fn endpoint(&self) -> Result<smb::Endpoint, String> {
        smb::Endpoint::parse(&self.address.url, &self.address.user, self.address.guest)
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus: bool) {
        let label = ui.label(text("Адрес SMB"));
        let address = ui
            .add(
                egui::TextEdit::singleline(&mut self.address.url)
                    .desired_width(f32::INFINITY)
                    .hint_text("smb://nas/Share"),
            )
            .labelled_by(label.id);
        if focus && !ui.is_sizing_pass() {
            address.request_focus();
        }
        ui.checkbox(&mut self.address.guest, text("Гостевой доступ"));
        ui.add_enabled_ui(!self.address.guest, |ui| {
            let name = ui.label(text("Имя пользователя"));
            ui.add(egui::TextEdit::singleline(&mut self.address.user).desired_width(f32::INFINITY))
                .labelled_by(name.id);
            let password = ui.label(text("Пароль"));
            ui.add(
                egui::TextEdit::singleline(&mut *self.password)
                    .password(true)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(password.id);
            ui.checkbox(&mut self.remember, text("Сохранить пароль в Связке ключей"));
            ui.small(text(
                "Пустое поле пароля использует сохранённый пароль для этого адреса и пользователя.",
            ));
        });
        if !self.address.url.is_empty()
            && let Err(error) = self.endpoint()
        {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}

struct Target {
    pane: usize,
    tab: u64,
    folder: Location,
}

enum Change {
    Connected(Box<smb::Connected>, Target),
    Disconnected(PathBuf, Option<String>),
}

pub(super) struct Connections {
    pub(super) shares: Vec<smb::MountedShare>,
    discovery: Option<Receiver<Vec<smb::MountedShare>>>,
    pending: Option<Receiver<Result<Change, String>>>,
    last: Address,
}

impl Connections {
    pub(super) fn load(ctx: &egui::Context) -> Self {
        // Unit tests use isolated application state. The native fixture covers discovery.
        let discovery = if cfg!(test) {
            None
        } else {
            spawn(ctx, smb::discover).ok()
        };
        Self {
            shares: Vec::new(),
            discovery,
            pending: None,
            last: Address::default(),
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn form(&self) -> Form {
        Form {
            address: self.last.clone(),
            password: Zeroizing::new(String::new()),
            remember: false,
        }
    }

    fn add(&mut self, share: smb::MountedShare) {
        if !self
            .shares
            .iter()
            .any(|existing| existing.path == share.path)
        {
            self.shares.push(share);
        }
    }
}

fn spawn<T: Send + 'static>(
    ctx: &egui::Context,
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<Receiver<T>> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("pawcommander-smb".into())
        .spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        })?;
    Ok(rx)
}

impl PawCommander {
    pub(super) fn smb_sidebar(
        &self,
        ui: &mut egui::Ui,
        current: &Location,
        actions: &mut Vec<Action>,
    ) {
        if ui
            .add_enabled(
                !self.smb.busy(),
                egui::Button::new(text("SMB · Подключиться")),
            )
            .clicked()
        {
            actions.push(Action::ConnectSmb);
        }
        let can_disconnect = !self.smb.busy() && self.job.is_none() && self.pending_jobs.is_empty();
        for (index, share) in self.smb.shares.iter().enumerate() {
            let endpoint = share.endpoint();
            let selected = current
                .local()
                .is_some_and(|path| path.starts_with(&share.path));
            let response = ui
                .add(
                    egui::Button::new(&endpoint.address)
                        .selected(selected)
                        .truncate(),
                )
                .on_hover_text(&endpoint.address);
            if response.clicked() {
                actions.push(Action::OpenSmb(index));
            }
            response.context_menu(|ui| {
                if ui
                    .add_enabled(can_disconnect, egui::Button::new(text("Отключить SMB")))
                    .clicked()
                {
                    actions.push(Action::DisconnectSmb(index));
                    ui.close();
                }
            });
        }
    }

    pub(super) fn connect_smb(&mut self, form: Form, ctx: &egui::Context) {
        if self.smb.busy() {
            return;
        }
        let endpoint = match form.endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.notice = Some((error, true));
                return;
            }
        };
        let guest = endpoint.user.is_none();
        let password = if guest {
            Zeroizing::new(String::new())
        } else {
            form.password
        };
        let remember = !guest && form.remember;
        self.smb.last = form.address;
        let target = Target {
            pane: self.active,
            tab: self.tab().id,
            folder: self.tab().path.clone(),
        };
        self.start_smb(ctx, text("Подключение к SMB…"), move || {
            smb::connect(endpoint, password, remember)
                .map(|share| Change::Connected(Box::new(share), target))
        });
    }

    pub(super) fn disconnect_smb(&mut self, index: usize, ctx: &egui::Context) {
        if self.smb.busy() || self.job.is_some() || !self.pending_jobs.is_empty() {
            return;
        }
        if let Some(share) = self.smb.shares.get(index).cloned() {
            self.start_smb(ctx, text("Отключение SMB…"), move || {
                let warning = share.disconnect()?;
                Ok(Change::Disconnected(share.path, warning))
            });
        }
    }

    fn start_smb(
        &mut self,
        ctx: &egui::Context,
        notice: &str,
        work: impl FnOnce() -> Result<Change, String> + Send + 'static,
    ) {
        match spawn(ctx, work) {
            Ok(result) => {
                self.smb.pending = Some(result);
                self.notice = Some((notice.into(), false));
            }
            Err(error) => {
                self.notice = Some((
                    crate::tr_format!(
                        "Не удалось запустить задачу SMB ({error})",
                        "Could not start the SMB worker ({error})"
                    ),
                    true,
                ));
            }
        }
    }

    pub(super) fn poll_smb(&mut self, ctx: &egui::Context) {
        if let Some(discovery) = &self.smb.discovery {
            match discovery.try_recv() {
                Ok(shares) => {
                    self.smb.discovery = None;
                    for share in shares {
                        self.smb.add(share);
                    }
                }
                Err(TryRecvError::Disconnected) => self.smb.discovery = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some(pending) = &self.smb.pending else {
            return;
        };
        let result = match pending.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err(text("Задача SMB завершилась без результата.").into())
            }
        };
        self.smb.pending = None;
        match result {
            Ok(Change::Connected(connected, target)) => {
                let location = connected.share.path.clone().into();
                self.smb.add(connected.share);
                let pane = &mut self.panes[target.pane];
                if (self.settings.split || target.pane == 0)
                    && pane.tab().id == target.tab
                    && pane.tab().path == target.folder
                {
                    pane.tab_mut().navigate(location, ctx);
                }
                self.notice = Some(match connected.warning {
                    Some(warning) => (warning, true),
                    None => (text("Папка SMB подключена.").into(), false),
                });
            }
            Ok(Change::Disconnected(path, warning)) => {
                self.smb.shares.retain(|share| share.path != path);
                for pane in &mut self.panes {
                    for tab in &mut pane.tabs {
                        if tab
                            .path
                            .local()
                            .is_some_and(|current| current.starts_with(&path))
                        {
                            tab.reload(ctx);
                        }
                    }
                }
                self.notice = Some(match warning {
                    Some(warning) => (warning, true),
                    None => (text("Папка SMB отключена.").into(), false),
                });
            }
            Err(error) => self.notice = Some((error, true)),
        }
        self.directory_tree.refresh();
    }
}
