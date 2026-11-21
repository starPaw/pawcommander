use super::SftpForm;
use crate::{i18n::text, ssh_config::ResolvedHost, vfs::Endpoint};
use eframe::egui;
use std::{path::PathBuf, sync::mpsc};

impl SftpForm {
    pub(in crate::app) fn start_ssh_config(
        &mut self,
        ctx: &egui::Context,
        read: impl FnOnce(&str) -> Result<ResolvedHost, String> + Send + 'static,
    ) {
        let alias = self.ssh_config.as_ref().map_or_else(
            || self.host.trim().to_owned(),
            |config| config.alias.clone(),
        );
        let (send, result) = mpsc::channel();
        let ctx = ctx.clone();
        self.error = None;
        match std::thread::Builder::new()
            .name("pawcommander-ssh-config".into())
            .spawn(move || {
                let _ = send.send(read(&alias));
                ctx.request_repaint();
            }) {
            Ok(_) => self.ssh_config_job = Some(result),
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Не удалось запустить чтение SSH config ({error})",
                    "Could not start reading SSH config ({error})"
                ));
            }
        }
    }

    pub(super) fn poll_ssh_config(&mut self) {
        let Some(job) = &self.ssh_config_job else {
            return;
        };
        let result = match job.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(text("Чтение SSH config завершилось неожиданно").into())
            }
        };
        self.ssh_config_job = None;
        match result {
            Ok(config) => {
                #[cfg(target_os = "macos")]
                {
                    self.show_identity_choices = false;
                }
                if config.matched {
                    self.host.clone_from(&config.host);
                    self.user.clone_from(&config.user);
                    self.port = config.port;
                    self.key = config
                        .identities
                        .first()
                        .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
                    self.password.clear();
                    self.passphrase.clear();
                    self.save_key = false;
                    self.use_saved = true;
                }
                self.ssh_config = Some(config);
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn imported_key(&self) -> Option<&PathBuf> {
        self.ssh_config
            .as_ref()?
            .identities
            .iter()
            .find(|path| path.to_string_lossy() == self.key)
    }

    pub(super) fn clear_ssh_config(&mut self) {
        if self.imported_key().is_some() {
            self.key.clear();
            self.passphrase.clear();
            self.save_key = false;
        }
        if self
            .ssh_config
            .as_ref()
            .is_some_and(|config| config.matched)
        {
            self.password.clear();
        }
        self.ssh_config = None;
        #[cfg(target_os = "macos")]
        {
            self.show_identity_choices = false;
        }
    }

    pub(in crate::app) fn endpoint(&self) -> Endpoint {
        let key = if self.key.is_empty() {
            None
        } else if let Some(path) = self.imported_key() {
            Some(path.clone())
        } else if let Some(rest) = self.key.strip_prefix("~/") {
            Some(dirs::home_dir().unwrap_or_default().join(rest))
        } else {
            Some(PathBuf::from(&self.key))
        };
        Endpoint {
            host: self.host.trim().into(),
            port: self.port,
            user: self.user.trim().into(),
            key,
        }
    }

    pub(super) fn show_identity_files(&mut self, ui: &mut egui::Ui) {
        let Some(config) = &self.ssh_config else {
            return;
        };
        if config.identities.len() < 2 {
            return;
        }
        let title = text("Файлы ключей из SSH config");
        #[cfg(target_os = "macos")]
        {
            if ui
                .selectable_label(self.show_identity_choices, title)
                .clicked()
            {
                self.show_identity_choices = !self.show_identity_choices;
            }
            if !self.show_identity_choices {
                return;
            }
        }
        let choose = |ui: &mut egui::Ui| {
            for path in &config.identities {
                let label = path.to_string_lossy();
                if ui
                    .selectable_label(label == self.key, label.as_ref())
                    .clicked()
                    && label != self.key
                {
                    self.key = label.into_owned();
                    self.passphrase.clear();
                    #[cfg(target_os = "macos")]
                    {
                        self.show_identity_choices = false;
                    }
                }
            }
        };
        #[cfg(target_os = "macos")]
        egui::ScrollArea::vertical()
            .id_salt("ssh_config_identity_choices")
            .max_height(105.0)
            .show(ui, choose);
        #[cfg(not(target_os = "macos"))]
        egui::ComboBox::from_id_salt("ssh_config_identity_files")
            .selected_text(title)
            .show_ui(ui, choose);
    }

    pub(super) fn show_config_result(&self, ui: &mut egui::Ui) {
        let Some(config) = &self.ssh_config else {
            return;
        };
        let alias = &config.alias;
        let status = if config.matched {
            crate::tr_format!("Загружен SSH-алиас {alias}", "Loaded SSH alias {alias}")
        } else {
            crate::tr_format!(
                "Для {alias} в SSH config не найдены параметры",
                "No settings for {alias} found in SSH config"
            )
        };
        ui.label(status);
        if config.unsupported.is_empty() {
            return;
        }
        ui.colored_label(
            ui.visuals().warn_fg_color,
            text("Неподдерживаемые директивы SSH config"),
        );
        let row_height = ui
            .text_style_height(&egui::TextStyle::Body)
            .max(ui.spacing().interact_size.y);
        egui::ScrollArea::vertical()
            .id_salt("ssh_config_warnings")
            .max_height(105.0)
            .show_rows(ui, row_height, config.unsupported.len(), |ui, rows| {
                for warning in &config.unsupported[rows] {
                    let keyword = &warning.keyword;
                    let line = warning.line;
                    ui.horizontal(|ui| {
                        ui.set_min_height(row_height);
                        ui.add(
                            egui::Label::new(crate::tr_format!(
                                "{keyword} · строка {line}",
                                "{keyword} · line {line}"
                            ))
                            .truncate(),
                        );
                    });
                }
            });
    }
}
