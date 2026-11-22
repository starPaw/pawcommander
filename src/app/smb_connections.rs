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
    pub(super) fn endpoint(&self) -> Result<smb::Endpoint, smb::SmbError> {
        smb::Endpoint::parse(&self.address.url, &self.address.user, self.address.guest)
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus: bool) {
        let label = ui.label(text!("Адрес SMB"));
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
        ui.checkbox(&mut self.address.guest, text!("Гостевой доступ"));
        ui.add_enabled_ui(!self.address.guest, |ui| {
            let name = ui.label(text!("Имя пользователя"));
            ui.add(egui::TextEdit::singleline(&mut self.address.user).desired_width(f32::INFINITY))
                .labelled_by(name.id);
            let password = ui.label(text!("Пароль"));
            ui.add(
                egui::TextEdit::singleline(&mut *self.password)
                    .password(true)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(password.id);
            ui.checkbox(
                &mut self.remember,
                text!("Сохранить пароль в Связке ключей"),
            );
            ui.small(text!(
                "Пустое поле пароля использует сохранённый пароль для этого адреса и пользователя.",
            ));
        });
        if !self.address.url.is_empty()
            && let Err(error) = self.endpoint()
        {
            ui.colored_label(ui.visuals().error_fg_color, crate::i18n::smb_error(&error));
        }
    }
}

struct Target {
    pane: usize,
    tab: u64,
    folder: Location,
}

impl Target {
    fn active(app: &PawCommander) -> Self {
        Self {
            pane: app.active,
            tab: app.tab().id,
            folder: app.tab().path.clone(),
        }
    }

    fn navigate(self, app: &mut PawCommander, path: PathBuf, ctx: &egui::Context) {
        let pane = &mut app.panes[self.pane];
        if (app.settings.split || self.pane == 0)
            && pane.tab().id == self.tab
            && pane.tab().path == self.folder
        {
            pane.tab_mut().navigate(path.into(), ctx);
        }
    }
}

enum Change {
    Connected(Box<smb::Connected>, Target),
    Opened(PathBuf, Target),
    Disconnected(PathBuf, Option<smb::SmbError>),
}

pub(super) struct Connections {
    shares: Vec<smb::MountedShare>,
    discovery: Option<Receiver<smb::Discovery>>,
    discovery_warnings: smb::DiscoveryWarnings,
    pending: Option<Receiver<Result<Change, smb::SmbError>>>,
    last: Address,
}

impl Connections {
    pub(super) fn load(ctx: &egui::Context) -> Self {
        // Unit tests use isolated application state. The native fixture covers discovery.
        let mut discovery_warnings = smb::DiscoveryWarnings::default();
        let discovery = if cfg!(test) {
            None
        } else {
            match spawn(ctx, smb::discover) {
                Ok(receiver) => Some(receiver),
                Err(error) => {
                    discovery_warnings.push(smb::SmbError::DiscoveryWorkerStart(error));
                    None
                }
            }
        };
        Self {
            shares: Vec::new(),
            discovery,
            discovery_warnings,
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
        let idle = !self.smb.busy();
        if ui
            .add_enabled(idle, egui::Button::new(text!("SMB · Подключиться")))
            .clicked()
        {
            actions.push(Action::ConnectSmb);
        }
        let can_disconnect = idle && !self.jobs.is_busy();
        for (index, share) in self.smb.shares.iter().enumerate() {
            let endpoint = share.endpoint();
            let selected = current
                .local()
                .is_some_and(|path| path.starts_with(&share.path));
            let response = ui
                .add_enabled(
                    idle,
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
                    .add_enabled(can_disconnect, egui::Button::new(text!("Отключить SMB")))
                    .clicked()
                {
                    actions.push(Action::DisconnectSmb(index));
                    ui.close();
                }
            });
        }
        let warnings = &self.smb.discovery_warnings;
        if !warnings.entries.is_empty() {
            let title = crate::tr_format!(
                "SMB warnings ({count})",
                count = warnings.entries.len().saturating_add(warnings.omitted)
            );
            let width =
                (ui.available_width() - ui.spacing().indent - ui.spacing().button_padding.x)
                    .max(0.0);
            let label = egui::WidgetText::from(title).into_galley(
                ui,
                Some(egui::TextWrapMode::Wrap),
                width,
                egui::TextStyle::Button,
            );
            egui::CollapsingHeader::new(label)
                .id_salt("smb_discovery_warnings")
                .show(ui, |ui| {
                    for error in &warnings.entries {
                        ui.add(
                            egui::Label::new(crate::i18n::smb_error(error))
                                .selectable(true)
                                .wrap(),
                        );
                    }
                    if warnings.omitted > 0 {
                        ui.label(crate::tr_format!(
                            "{count} additional warnings omitted",
                            count = warnings.omitted
                        ));
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
                self.notice = Some((crate::i18n::smb_error(&error), true));
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
        let target = Target::active(self);
        self.start_smb(
            ctx,
            Some(text!("Подключение к SMB…")),
            move || {
                smb::connect(endpoint, password, remember)
                    .map(|share| Change::Connected(Box::new(share), target))
            },
        );
    }

    pub(super) fn open_smb(&mut self, index: usize, ctx: &egui::Context) {
        if self.smb.busy() {
            return;
        }
        if let Some(share) = self.smb.shares.get(index).cloned() {
            let target = Target::active(self);
            self.start_smb(ctx, None, move || {
                share.check_open()?;
                Ok(Change::Opened(share.path, target))
            });
        }
    }

    pub(super) fn disconnect_smb(&mut self, index: usize, ctx: &egui::Context) {
        if self.smb.busy() || self.jobs.is_busy() {
            return;
        }
        if let Some(share) = self.smb.shares.get(index).cloned() {
            self.start_smb(ctx, Some(text!("Отключение SMB…")), move || {
                let warning = share.disconnect()?;
                Ok(Change::Disconnected(share.path, warning))
            });
        }
    }

    fn start_smb(
        &mut self,
        ctx: &egui::Context,
        notice: Option<&str>,
        work: impl FnOnce() -> Result<Change, smb::SmbError> + Send + 'static,
    ) {
        if self.volumes.ejecting() {
            self.notice = Some((super::volumes::wait_for_eject().into(), true));
            return;
        }
        match spawn(ctx, work) {
            Ok(result) => {
                self.smb.pending = Some(result);
                if let Some(notice) = notice {
                    self.notice = Some((notice.into(), false));
                }
            }
            Err(error) => {
                self.notice = Some((
                    crate::tr_format!("Could not start the SMB worker ({error})", error = error),
                    true,
                ));
            }
        }
    }

    pub(super) fn poll_smb(&mut self, ctx: &egui::Context) {
        if let Some(discovery) = &self.smb.discovery {
            match discovery.try_recv() {
                Ok(report) => {
                    self.smb.discovery = None;
                    self.smb.discovery_warnings = report.warnings;
                    for share in report.shares {
                        self.smb.add(share);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.smb.discovery = None;
                    self.smb
                        .discovery_warnings
                        .push(smb::SmbError::WorkerDisconnected);
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some(pending) = &self.smb.pending else {
            return;
        };
        let result = match pending.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(smb::SmbError::WorkerDisconnected),
        };
        self.smb.pending = None;
        match result {
            Ok(Change::Connected(connected, target)) => {
                let path = connected.share.path.clone();
                self.smb.add(connected.share);
                target.navigate(self, path, ctx);
                self.notice = Some(match connected.warning {
                    Some(warning) => (crate::i18n::smb_error(&warning), true),
                    None => (text!("Папка SMB подключена.").into(), false),
                });
            }
            Ok(Change::Opened(path, target)) => {
                target.navigate(self, path, ctx);
                return;
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
                    Some(warning) => (crate::i18n::smb_error(&warning), true),
                    None => (text!("Папка SMB отключена.").into(), false),
                });
            }
            Err(error) => self.notice = Some((crate::i18n::smb_error(&error), true)),
        }
        self.directory_tree.refresh();
    }
}
