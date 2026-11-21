use super::actions::Action;
use super::pane_view::sort_controls;
use super::state::{Density, FolderViewChange, PaneOrientation};
use super::PawCommander;
use crate::icons;
use crate::icons::Icon;
use crate::shortcuts::{Command, ShortcutEditor};
use crate::theme::{appearance_menu, paint_focus, Palette};
use eframe::egui;
use eframe::egui::{Sense, Vec2};

impl PawCommander {
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let ctx = &ui.ctx().clone();
        let colors = Palette::from_ctx(ctx);
        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(colors.surface).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    if cfg!(target_os = "macos") {
                        // Keep the native close, minimize and zoom controls clear.
                        ui.add_space((88.0 / ctx.zoom_factor() - 12.0).max(0.0));
                    }
                    let tab = self.tab();
                    let writable = !tab.path.is_archive();
                    for (icon, tip, enabled, action) in [
                        (Icon::Back, Command::Back, tab.history_index > 0, Action::Back),
                        (Icon::Forward, Command::Forward, tab.history_index + 1 < tab.history.len(), Action::Forward),
                        (Icon::Up, Command::Parent, tab.path.parent().is_some(), Action::Up),
                        (Icon::Refresh, Command::Refresh, true, Action::Refresh),
                    ] {
                        let tip = self.settings.shortcuts.hint(ctx, tip, tip.label());
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.separator();
                    let search_width = (ui.available_width() - 432.0).clamp(icons::TOOLBAR_BUTTON_SIZE, 420.0);
                    let search_label = if search_width >= 150.0 {
                        crate::i18n::text!("Поиск файлов…")
                    } else if search_width >= 95.0 {
                        crate::i18n::text!("Поиск…")
                    } else {
                        ""
                    };
                    let search_enabled = !self.keyboard_blocked(ctx);
                    if ui.add_enabled_ui(search_enabled, |ui| icons::button_width(ui, Icon::Search, search_label, false, search_width)).inner
                        .on_hover_text(self.settings.shortcuts.hint(ctx, Command::Search, Command::Search.label())).clicked() {
                        self.open_search();
                    }
                    if ui.add_enabled_ui(self.job.is_none(), |ui| icons::button(ui, Icon::Server, "SFTP", self.tab().path.remote())).inner
                        .on_hover_text(crate::i18n::text!("Подключиться по SFTP")).clicked() {
                        actions.push(Action::ConnectSftp);
                    }
                    for (icon, tip, enabled, action) in [
                        (Icon::NewFolder, self.settings.shortcuts.hint(ctx, Command::NewFolder, Command::NewFolder.label()), writable && !self.conflict_open(), Action::NewFolder),
                        (Icon::Copy, crate::i18n::text!("Копировать · ⌘/Ctrl+C").into(), writable && !self.tab().selected.is_empty(), Action::Copy(false)),
                        (Icon::Paste, crate::i18n::text!("Вставить · ⌘/Ctrl+V").into(), writable && !self.conflict_open(), Action::Paste(false)),
                    ] {
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let menu_id = ui.scope_id().with("settings_menu");
                        let settings_label = if ctx.content_rect().width() >= 760.0 {
                            crate::i18n::text!("Настройки")
                        } else {
                            ""
                        };
                        let button = icons::button(ui, Icon::Settings, settings_label, false);
                        if self.focus_toolbar && !self.show_accessibility {
                            button.request_focus();
                            paint_focus(ui, &button, 8);
                            self.focus_toolbar = false;
                        }
                        let menu_was_open = egui::Popup::is_id_open(ctx, menu_id);
                        let opened = button.clicked() && !menu_was_open;
                        let focus_entry = ui.data_mut(|data| {
                            if opened {
                                data.insert_temp(menu_id.with("focus_entry"), true);
                            }
                            data.get_temp::<bool>(menu_id.with("focus_entry")).unwrap_or(false)
                        });
                        let settings_items = |ui: &mut egui::Ui| {
                            let response = ui.button(crate::i18n::text!("Доступность"));
                            if focus_entry && !ui.is_sizing_pass() {
                                response.request_focus();
                                ui.data_mut(|data| data.remove::<bool>(menu_id.with("focus_entry")));
                            }
                            paint_focus(ui, &response, 5);
                            if response.clicked() {
                                self.show_accessibility = true;
                                self.accessibility_focus = true;
                                ui.close();
                            }
                            let shortcuts = ui.button(crate::i18n::text!("Горячие клавиши"));
                            paint_focus(ui, &shortcuts, 5);
                            if shortcuts.clicked() {
                                self.shortcut_editor = Some(ShortcutEditor::new(&self.settings.shortcuts));
                                ctx.memory_mut(|memory| {
                                    if let Some(id) = memory.focused() {
                                        memory.surrender_focus(id);
                                    }
                                });
                                ui.close();
                            }
                            let palette_label = Command::Palette.label();
                            let palette = ui.button(self.settings.shortcuts.hint(ctx, Command::Palette, palette_label));
                            paint_focus(ui, &palette, 5);
                            if palette.clicked() {
                                self.open_command_palette(ctx, Some(button.id));
                                ui.close();
                            }
                            for (command, action, enabled) in [
                                #[cfg(target_os = "macos")]
                                (Command::NewFile, Action::NewFile(Some(button.id)), self.can_create_file()),
                                #[cfg(target_os = "macos")]
                                (Command::CompatibleNames, Action::CompatibleNames(Some(button.id)), self.can_preview_compatible_names()),
                                (
                                    Command::CompareFolders,
                                    Action::CompareFolders(cfg!(target_os = "macos").then_some(button.id)),
                                    self.can_compare_folders(),
                                ),
                                (
                                    Command::CompareFiles,
                                    Action::CompareFiles(cfg!(target_os = "macos").then_some(button.id)),
                                    self.files_to_compare().is_some(),
                                ),
                                #[cfg(target_os = "macos")]
                                (Command::FindDuplicates, Action::FindDuplicates(Some(button.id)), self.can_find_duplicates()),
                                #[cfg(target_os = "macos")]
                                (Command::DiskUsage, Action::DiskUsage(Some(button.id)), self.can_analyze_disk_usage()),
                                #[cfg(target_os = "macos")]
                                (Command::Collections, Action::Collections(Some(button.id)), !self.conflict_open()),
                                #[cfg(target_os = "macos")]
                                (Command::SyncFolders, Action::SyncFolders(Some(button.id)), self.can_open_folder_sync()),
                            ] {
                                let compare = ui.add_enabled(enabled, egui::Button::new(
                                    self.settings.shortcuts.hint(ctx, command, command.label()),
                                ));
                                paint_focus(ui, &compare, 5);
                                if compare.clicked() {
                                    actions.push(action);
                                    ui.close();
                                }
                            }
                            let about = ui.button(crate::i18n::text!("О PawCommander"));
                            paint_focus(ui, &about, 5);
                            if about.clicked() { self.show_help = true; ui.close(); }
                            let language_menu = ui.menu_button(crate::i18n::text!("Язык"), |ui| {
                                for (language, label) in [
                                    (crate::i18n::Language::English, "English"),
                                    (crate::i18n::Language::Russian, "Русский"),
                                    #[cfg(target_os = "macos")]
                                    (crate::i18n::Language::Czech, "Čeština"),
                                ] {
                                    if ui.selectable_value(&mut self.settings.language, language, label).changed() {
                                        crate::i18n::set_language(language);
                                        self.preview.refresh();
                                        ctx.request_repaint();
                                    }
                                }
                            });
                            paint_focus(ui, &language_menu.response, 5);
                            ui.separator();
                            let speed = ui.button(crate::i18n::text!("Скорость передачи…"));
                            paint_focus(ui, &speed, 5);
                            if speed.clicked() {
                                self.open_transfer_limits(ctx);
                                ui.close();
                            }
                            let verification = ui.checkbox(
                                &mut self.settings.verify_copies,
                                crate::i18n::text!("Проверять копии (SHA-256)"),
                            ).on_hover_text(crate::i18n::text!("Повторно читает временные файлы перед публикацией. Применяется к новым заданиям копирования; перемещение использует переименование."));
                            paint_focus(ui, &verification, 5);
                            ui.separator();
                            appearance_menu(ui, &mut self.settings.theme, self.settings.high_contrast);
                            ui.separator();
                            ui.label(crate::i18n::text!("Расположение панелей"));
                            ui.horizontal(|ui| {
                                for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
                                    let response = ui.radio_value(
                                        &mut self.settings.pane_layout.orientation,
                                        orientation,
                                        orientation.label(),
                                    );
                                    paint_focus(ui, &response, 5);
                                }
                            });
                            ui.separator();
                            if ui.button(crate::tr_format!("Crash recovery")).clicked() {
                                self.recovery.reopen(ctx);
                                ui.close();
                            }
                            ui.separator();
                            ui.label(crate::i18n::text!("Вид текущей папки"));
                            let view = self.folder_view(self.active);
                            let mut hidden = view.hidden;
                            let response = ui.checkbox(&mut hidden, crate::i18n::text!("Скрытые файлы"));
                            paint_focus(ui, &response, 5);
                            if response.changed() {
                                actions.push(self.folder_view_action(self.active, FolderViewChange::Hidden(hidden)));
                            }
                            ui.horizontal(|ui| {
                                ui.label(crate::i18n::text!("Плотность"));
                                for density in [Density::Standard, Density::Compact] {
                                    let response = ui.radio(view.density == density, density.label());
                                    paint_focus(ui, &response, 5);
                                    if response.clicked() {
                                        actions.push(self.folder_view_action(self.active, FolderViewChange::Density(density)));
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(crate::i18n::text!("Сортировка"));
                                sort_controls(ui, view.order, |sort| {
                                    actions.push(self.folder_view_action(self.active, FolderViewChange::Sort(sort)));
                                });
                            });
                            let columns = ui.button(crate::i18n::text!("Столбцы списка"));
                            paint_focus(ui, &columns, 5);
                            if columns.clicked() {
                                self.show_list_columns = true;
                                self.list_columns_focus = true;
                                ui.close();
                            }
                            ui.separator();
                            let tools_menu = ui.menu_button(crate::i18n::text!("Инструменты"), |ui| {
                                #[cfg(target_os = "macos")]
                                if ui.button(crate::i18n::text!("Пользовательские действия…")).clicked() {
                                    actions.push(Action::ManageCustomActions);
                                    ui.close();
                                }
                                #[cfg(target_os = "macos")]
                                if ui.button(crate::i18n::text!("Внешние редакторы…")).clicked() {
                                    actions.push(Action::ConfigureExternalEditors);
                                    ui.close();
                                }
                                #[cfg(target_os = "macos")]
                                if self.remote_editor.available() && ui.button(crate::i18n::text!("Правка SFTP")).clicked() {
                                    self.remote_editor.reveal();
                                    ui.close();
                                }
                                for (name, path) in [("locate", &self.tools.locate), ("Zed", &self.tools.zed), ("FileMerge", &self.tools.diff)] {
                                    if let Some(path) = path {
                                        ui.label(crate::tr_format!("{name}: found", name = name)).on_hover_text(path.display().to_string());
                                    } else {
                                        ui.colored_label(ui.visuals().warn_fg_color, crate::tr_format!("{name}: not found", name = name));
                                    }
                                }
                                ui.small(crate::i18n::text!("locate: установите locate/plocate и создайте индекс средствами ОС."));
                                ui.small(crate::i18n::text!("Zed: установите редактор; для нестандартного пути — cli: install cli binary."));
                                let diff = ui.label(crate::i18n::text!("Внешний инструмент сравнения"));
                                ui.add(egui::TextEdit::singleline(&mut self.settings.diff_tool)
                                    .hint_text(crate::i18n::text!("Авто (FileMerge при наличии)"))
                                    .desired_width(290.0)).labelled_by(diff.id);
                                let configured = self.settings.diff_tool.trim();
                                if !configured.is_empty() && !std::path::Path::new(configured).is_absolute() {
                                    ui.colored_label(ui.visuals().warn_fg_color, crate::i18n::text!("Укажите абсолютный путь к исполняемому файлу"));
                                }
                                if ui.button(crate::i18n::text!("Проверить снова")).clicked() { self.tools = crate::tools::Tools::detect(); }
                            });
                            paint_focus(ui, &tools_menu.response, 5);
                            let focused = ui.memory(|memory| memory.focused());
                            if let Some(response) = focused.and_then(|id| ctx.read_response(id))
                                && response.layer_id == ui.layer_id()
                                && response.gained_focus()
                                && !ui.clip_rect().contains_rect(response.rect)
                            {
                                response.scroll_to_me(None);
                            }
                        };
                        let shown = egui::Popup::menu(&button).id(menu_id).show(|ui| {
                            let frame_height = egui::Frame::popup(ui.style()).total_margin().sum().y;
                            let height = (ctx.content_rect().bottom() - button.rect.bottom() - frame_height)
                                .max(1.0);
                            let mut scroll = egui::ScrollArea::vertical()
                                .id_salt("settings_scroll")
                                .max_height(height);
                            if opened {
                                scroll = scroll.vertical_scroll_offset(0.0);
                            }
                            scroll.show(ui, settings_items);
                        });
                        if shown.is_some() && !egui::Popup::is_id_open(ctx, menu_id) && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                            button.request_focus();
                        }
                        let menu_open = egui::Popup::is_id_open(ctx, menu_id);
                        ctx.accesskit_node_builder(button.id, |node| node.set_expanded(menu_open));
                        if icons::button(ui, Icon::Info, "", self.settings.preview).on_hover_text(self.settings.shortcuts.hint(ctx, Command::Preview, Command::Preview.label())).clicked() {
                            self.settings.preview = !self.settings.preview;
                        }
                        let split_icon = match self.settings.pane_layout.orientation {
                            PaneOrientation::SideBySide => Icon::Split,
                            PaneOrientation::Stacked => Icon::SplitStacked,
                        };
                        if icons::button(ui, split_icon, "", self.settings.split).on_hover_text(self.settings.shortcuts.hint(ctx, Command::Split, Command::Split.label())).clicked() {
                            self.toggle_split();
                        }
                        let sidebar = self.settings.pane_layout.sidebar_visible;
                        let hint = if sidebar {
                            crate::i18n::text!("Скрыть боковую панель")
                        } else {
                            crate::i18n::text!("Показать боковую панель")
                        };
                        if icons::button(ui, Icon::Sidebar, "", sidebar).on_hover_text(hint).clicked() {
                            self.settings.pane_layout.sidebar_visible = !sidebar;
                        }
                        if cfg!(target_os = "macos") && ui.available_width() > 0.0 {
                            let (_, drag) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), icons::TOOLBAR_BUTTON_SIZE),
                                Sense::drag(),
                            );
                            if drag.drag_started_by(egui::PointerButton::Primary) {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        }
                    });
                });
            });
    }
}
