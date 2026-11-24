use super::actions::Action;
use super::bookmarks::{
    assign_bookmark_shortcut, bookmark_peer, bookmark_slot_key, move_bookmark, BookmarkEditor,
};
use super::pane_view::workspace_card;
use super::state::FolderViewChange;
use super::PawCommander;
use crate::icons::Icon;
use crate::shortcuts::Command;
use crate::theme::{paint_focus, Palette};
use crate::vfs::Location;
use crate::{icons, shortcuts};
use eframe::egui;
use eframe::egui::{Color32, FontId, RichText, Sense, Stroke, Vec2};
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;

pub(super) fn place_button(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let colors = Palette::from_ctx(ui.ctx());
    use crate::i18n::key;

    let icon = [
        (key!("Домашняя"), Icon::Home),
        (key!("Рабочий стол"), Icon::Desktop),
        (key!("Документы"), Icon::Document),
        (key!("Загрузки"), Icon::Download),
        (key!("Изображения"), Icon::Image),
        (key!("Музыка"), Icon::Music),
        (key!("Видео"), Icon::Video),
        (key!("Файловая система"), Icon::Drive),
        (key!("Тома и диски"), Icon::Drive),
        (key!("Подключённые тома…"), Icon::Drive),
        (key!("Носители"), Icon::Drive),
    ]
    .into_iter()
    .find_map(|(name, icon)| name.matches(label).then_some(icon))
    .unwrap_or(if label.starts_with("SFTP") {
        Icon::Server
    } else {
        Icon::Folder
    });
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 31.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, ui.is_enabled(), selected, label)
    });
    if selected || response.hovered() {
        ui.painter().rect(
            rect,
            8,
            if selected {
                colors.selection
            } else {
                colors.raised
            },
            Stroke::new(
                1.0,
                if selected {
                    colors.mauve.gamma_multiply(0.7)
                } else {
                    Color32::TRANSPARENT
                },
            ),
            egui::StrokeKind::Inside,
        );
    }
    icons::paint(
        ui,
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(20.0, 0.0), Vec2::splat(21.0)),
        icon,
        icons::tint(icon, colors),
    );
    let text_rect = rect.shrink2(Vec2::new(8.0, 0.0));
    ui.painter().with_clip_rect(text_rect).text(
        rect.left_center() + Vec2::new(40.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        colors.ink,
    );
    if response.gained_focus() {
        response.scroll_to_me(Some(egui::Align::Center));
    }
    paint_focus(ui, &response, 6);
    response.on_hover_text(label).clicked()
}

impl PawCommander {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let ctx = &ui.ctx().clone();
        // Rebuild this from visible menu responses every frame. The keyboard
        // guard checks the row ID too, so a removed bookmark cannot leave it set.
        self.bookmark_menu_open = None;
        if !self.settings.pane_layout.sidebar_visible {
            self.directory_tree.inactive();
            return;
        }
        let colors = Palette::from_ctx(ctx);
        egui::Panel::left("places")
            .exact_size(if ctx.content_rect().width() < 900.0 {
                148.0
            } else {
                204.0
            })
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(colors.surface))
            .show(ui, |panel_ui| {
                let mut card = workspace_card(colors)
                    .inner_margin(egui::Margin::symmetric(12, 14))
                    .outer_margin(egui::Margin {
                        left: 8,
                        right: 0,
                        top: 8,
                        bottom: 8,
                    })
                    .begin(panel_ui);
                let ui = &mut card.content_ui;
                ui.set_min_size(ui.available_size());
                let footer_height = 66.0;
                let places_height = (ui.available_height() - footer_height).max(0.0);
                egui::ScrollArea::vertical()
                    .id_salt("places_scroll")
                    .max_height(places_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let jump = ui.add_enabled(
                            !self.keyboard_blocked(ctx),
                            egui::Button::new(crate::i18n::text!("К папке…")),
                        );
                        paint_focus(ui, &jump, 5);
                        if jump
                            .on_hover_text(self.settings.shortcuts.hint(
                                ctx,
                                Command::FolderJump,
                                Command::FolderJump.label(),
                            ))
                            .clicked()
                        {
                            actions.push(Action::Shortcut(Command::FolderJump));
                        }
                        let tab = self.tab();
                        let current = tab.path.clone();
                        let tab_id = tab.id;
                        let waiting_for_sftp = current.remote() && tab.blocking_load();
                        let tree_location = current
                            .archive_source()
                            .and_then(Path::parent)
                            .map(|parent| Location::Local(parent.to_owned()));
                        let show_hidden = self.folder_view(self.active).hidden;
                        let tree = ui.add_enabled_ui(!self.keyboard_blocked(ctx), |ui| {
                            self.directory_tree.show(
                                ui,
                                tab_id,
                                tree_location.as_ref().unwrap_or(&current),
                                show_hidden,
                                waiting_for_sftp,
                            )
                        });
                        if let Some(location) = tree.inner {
                            actions.push(Action::Navigate(self.active, location));
                        }
                        self.workspace_places(ui, actions);
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text!("Ваши файлы"))
                            .id_salt("places_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                let places = [
                                    (crate::i18n::text!("Домашняя"), dirs::home_dir()),
                                    (crate::i18n::text!("Рабочий стол"), dirs::desktop_dir()),
                                    (crate::i18n::text!("Документы"), dirs::document_dir()),
                                    (crate::i18n::text!("Загрузки"), dirs::download_dir()),
                                    (crate::i18n::text!("Изображения"), dirs::picture_dir()),
                                    (crate::i18n::text!("Музыка"), dirs::audio_dir()),
                                    (crate::i18n::text!("Видео"), dirs::video_dir()),
                                ];
                                for (label, path) in places {
                                    if let Some(path) = path
                                        && place_button(
                                            ui,
                                            label,
                                            current.local() == Some(path.as_path()),
                                        )
                                    {
                                        actions.push(Action::Navigate(self.active, path.into()));
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text!("Подключения"))
                            .id_salt("remote_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                if place_button(
                                    ui,
                                    crate::i18n::text!("SFTP · Подключиться"),
                                    current.remote(),
                                ) {
                                    actions.push(Action::ConnectSftp);
                                }
                                for profile in &self.settings.sftp_profiles {
                                    let label = profile.name();
                                    #[cfg(target_os = "macos")]
                                    let label = profile.with_sftp_route_label(label);
                                    if place_button(ui, &label, current == *profile) {
                                        actions
                                            .push(Action::Navigate(self.active, profile.clone()));
                                    }
                                }
                                #[cfg(target_os = "macos")]
                                self.smb_sidebar(ui, &current, actions);
                                #[cfg(target_os = "macos")]
                                if place_button(ui, crate::i18n::text!("WebDAV · Просмотр"), false)
                                {
                                    actions.push(Action::BrowseWebDav);
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text!("Избранное"))
                            .id_salt("bookmarks_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                let groups: Vec<(String, Vec<usize>)> = {
                                    let mut groups = Vec::<(String, Vec<usize>)>::new();
                                    for (index, bookmark) in self.bookmarks.iter().enumerate() {
                                        let label = bookmark.group.trim().to_owned();
                                        if let Some((_, indices)) =
                                            groups.iter_mut().find(|(name, _)| *name == label)
                                        {
                                            indices.push(index);
                                        } else {
                                            groups.push((label, vec![index]));
                                        }
                                    }
                                    groups
                                };
                                let mut remove = None;
                                let mut pending_move = None;
                                for (group, indices) in groups {
                                    if !group.is_empty() {
                                        ui.small(RichText::new(&group).color(colors.muted));
                                    }
                                    for i in indices {
                                        let (id, name, location, shortcut) = {
                                            let bookmark = &self.bookmarks[i];
                                            (
                                                bookmark.id,
                                                bookmark.name.clone(),
                                                bookmark.location.clone(),
                                                bookmark.shortcut,
                                            )
                                        };
                                        ui.horizontal(|ui| {
                                            let menu_size = Vec2::new(34.0, 29.0);
                                            let name_width = (ui.available_width()
                                                - menu_size.x
                                                - ui.spacing().item_spacing.x)
                                                .max(0.0);
                                            let mut button =
                                                egui::Button::new(format!("      {name}"))
                                                    .selected(current == location)
                                                    .truncate();
                                            if let Some(slot) = shortcut {
                                                button =
                                                    button.shortcut_text(format!("Alt+{slot}"));
                                            }
                                            let response = ui
                                                .add_sized([name_width, menu_size.y], button)
                                                .on_hover_text(location.display().to_string());
                                            icons::paint(
                                                ui,
                                                egui::Rect::from_center_size(
                                                    response.rect.left_center()
                                                        + Vec2::new(16.0, 0.0),
                                                    Vec2::splat(18.0),
                                                ),
                                                Icon::Star,
                                                colors.yellow,
                                            );
                                            if response.clicked() {
                                                actions.push(Action::Navigate(
                                                    self.active,
                                                    location.clone(),
                                                ));
                                            }
                                            let menu_button =
                                                egui::containers::menu::MenuButton::from_button(
                                                    egui::Button::new("…")
                                                        .min_size(menu_size)
                                                        .truncate(),
                                                );
                                            let (menu, _) = menu_button.ui(ui, |ui| {
                                                self.bookmark_menu_open = Some(id);
                                                if ui
                                                    .button(crate::i18n::text!(
                                                        "Изменить избранное"
                                                    ))
                                                    .clicked()
                                                {
                                                    self.bookmark_editor = Some(
                                                        BookmarkEditor::new(&self.bookmarks[i]),
                                                    );
                                                    ui.close();
                                                }
                                                ui.separator();
                                                ui.menu_button(
                                                    crate::i18n::text!("Сочетание"),
                                                    |ui| {
                                                        if ui
                                                            .selectable_label(
                                                                self.bookmarks[i]
                                                                    .shortcut
                                                                    .is_none(),
                                                                crate::i18n::text!("Не назначено"),
                                                            )
                                                            .clicked()
                                                        {
                                                            assign_bookmark_shortcut(
                                                                &mut self.bookmarks,
                                                                i,
                                                                None,
                                                                &self.settings.shortcuts,
                                                            );
                                                            ui.close();
                                                        }
                                                        for slot in 1..=9 {
                                                            let occupied_by_command = self
                                                                .settings
                                                                .shortcuts
                                                                .command(shortcuts::chord(
                                                                    bookmark_slot_key(slot)
                                                                        .unwrap(),
                                                                    egui::Modifiers::ALT,
                                                                ))
                                                                .is_some();
                                                            let occupied_by_bookmark = self
                                                                .bookmarks
                                                                .iter()
                                                                .enumerate()
                                                                .any(|(other, item)| {
                                                                    other != i
                                                                        && item.shortcut
                                                                            == Some(slot)
                                                                });
                                                            let response = ui.add_enabled(
                                                                !occupied_by_command
                                                                    && !occupied_by_bookmark,
                                                                egui::Button::selectable(
                                                                    self.bookmarks[i].shortcut
                                                                        == Some(slot),
                                                                    format!("Alt+{slot}"),
                                                                ),
                                                            );
                                                            if response.clicked() {
                                                                assign_bookmark_shortcut(
                                                                    &mut self.bookmarks,
                                                                    i,
                                                                    Some(slot),
                                                                    &self.settings.shortcuts,
                                                                );
                                                                ui.close();
                                                            }
                                                        }
                                                    },
                                                );
                                                ui.separator();
                                                ui.horizontal(|ui| {
                                                    let previous =
                                                        bookmark_peer(&self.bookmarks, i, -1);
                                                    if ui
                                                        .add_enabled(
                                                            previous.is_some(),
                                                            egui::Button::new(crate::i18n::text!(
                                                                "Вверх",
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_move =
                                                            previous.map(|previous| (i, previous));
                                                        ui.close();
                                                    }
                                                    let next = bookmark_peer(&self.bookmarks, i, 1);
                                                    if ui
                                                        .add_enabled(
                                                            next.is_some(),
                                                            egui::Button::new(crate::i18n::text!(
                                                                "Вниз",
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_move = next.map(|next| (i, next));
                                                        ui.close();
                                                    }
                                                });
                                                if ui
                                                    .button(crate::i18n::text!(
                                                        "Убрать из избранного",
                                                    ))
                                                    .clicked()
                                                {
                                                    remove = Some(i);
                                                    ui.close();
                                                }
                                            });
                                            let menu_label = crate::tr_format!(
                                                "Actions for {name}",
                                                name = name
                                            );
                                            ui.ctx().accesskit_node_builder(menu.id, |node| {
                                                node.set_label(menu_label.clone());
                                            });
                                            paint_focus(ui, &menu.on_hover_text(menu_label), 5);
                                        });
                                    }
                                }
                                if let Some((from, to)) = pending_move {
                                    move_bookmark(&mut self.bookmarks, from, to);
                                }
                                if let Some(i) = remove {
                                    self.bookmarks.remove(i);
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(crate::i18n::text!("+ Текущая папка"))
                                                .color(colors.muted),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    actions.push(Action::Bookmark);
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text!("Устройства"))
                            .id_salt("devices_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                #[cfg(not(target_os = "windows"))]
                                if place_button(
                                    ui,
                                    crate::i18n::text!("Файловая система"),
                                    current.local() == Some(Path::new("/")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/").into(),
                                    ));
                                }
                                #[cfg(target_os = "macos")]
                                if place_button(
                                    ui,
                                    crate::i18n::text!("Тома и диски"),
                                    current.local() == Some(Path::new("/Volumes")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/Volumes").into(),
                                    ));
                                }
                                #[cfg(target_os = "macos")]
                                if place_button(ui, Command::MountedVolumes.label(), false) {
                                    actions.push(Action::MountedVolumes);
                                }
                                #[cfg(target_os = "linux")]
                                if place_button(
                                    ui,
                                    crate::i18n::text!("Носители"),
                                    current.local() == Some(Path::new("/media")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/media").into(),
                                    ));
                                }
                                #[cfg(target_os = "windows")]
                                for path in self.windows_drives.paths() {
                                    let drive =
                                        path.to_string_lossy().chars().next().unwrap_or('?');
                                    if place_button(
                                        ui,
                                        &crate::tr_format!("Drive {}:", drive),
                                        current.local() == Some(path.as_path()),
                                    ) {
                                        actions.push(Action::Navigate(
                                            self.active,
                                            path.clone().into(),
                                        ));
                                    }
                                }
                            });
                    });
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(concat!("PawCommander  /  ", env!("CARGO_PKG_VERSION")))
                            .size(10.0)
                            .color(colors.muted),
                    );
                    ui.add_space(10.0);
                    let mut hidden = self.folder_view(self.active).hidden;
                    let response = ui
                        .checkbox(&mut hidden, crate::i18n::text!("Скрытые файлы"))
                        .on_hover_text(self.settings.shortcuts.hint(
                            ctx,
                            Command::Hidden,
                            Command::Hidden.label(),
                        ));
                    paint_focus(ui, &response, 5);
                    if response.changed() {
                        actions.push(
                            self.folder_view_action(self.active, FolderViewChange::Hidden(hidden)),
                        );
                    }
                });
                card.end(panel_ui);
            });
    }

    pub(super) fn workspace_places(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::CollapsingHeader::new(crate::i18n::text!("Пространства"))
            .id_salt("workspaces_group")
            .default_open(true)
            .show_unindented(ui, |ui| {
                ui.add_enabled_ui(self.drop_enabled(), |ui| {
                    let save = ui.button(crate::i18n::text!("Сохранить пару папок…"));
                    paint_focus(ui, &save, 5);
                    if save.clicked() {
                        actions.push(Action::SaveWorkspace);
                    }
                    for (index, workspace) in self.workspaces.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                let open = ui
                                    .add_sized(
                                        [(ui.available_width() - 34.0).max(0.0), 29.0],
                                        egui::Button::new(&workspace.name).truncate(),
                                    )
                                    .on_hover_text(&workspace.name);
                                paint_focus(ui, &open, 5);
                                if open.gained_focus() {
                                    open.scroll_to_me(Some(egui::Align::Center));
                                }
                                if open.clicked() {
                                    actions.push(Action::OpenWorkspace(index));
                                }
                                let menu = ui.menu_button("…", |ui| {
                                    if ui.button(crate::i18n::text!("Переименовать…")).clicked()
                                    {
                                        actions.push(Action::RenameWorkspace(index));
                                        ui.close();
                                    }
                                    if ui
                                        .button(crate::i18n::text!("Удалить пространство"))
                                        .clicked()
                                    {
                                        actions.push(Action::RemoveWorkspace(index));
                                        ui.close();
                                    }
                                });
                                let label = crate::tr_format!("Actions for {}", workspace.name);
                                ui.ctx().accesskit_node_builder(menu.response.id, |node| {
                                    node.set_label(label.clone());
                                });
                                paint_focus(ui, &menu.response.on_hover_text(label), 5);
                            });
                        });
                    }
                });
            });
    }
}
