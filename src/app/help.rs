use super::{
    comparison::{ComparisonBody, ComparisonDialog, ComparisonResponse},
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::{Command, Shortcuts},
    theme::paint_focus,
};
use eframe::egui;

pub(super) struct HelpDialog {
    shortcuts: Shortcuts,
    query: String,
    focus_query: bool,
}

impl HelpDialog {
    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        let modal = egui::Modal::new(egui::Id::unique("pawcommander-help")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 40.0).clamp(160.0, 590.0));
            ui.heading(text!("Справка PawCommander"));
            let query_id = ui.make_persistent_id("query");
            let hint = text!("Поиск команд…");
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id(query_id)
                    .hint_text(hint)
                    .desired_width(ui.available_width())
                    .return_key(None),
            );
            if self.focus_query && !ui.is_sizing_pass() {
                search.request_focus();
                self.focus_query = false;
                ctx.request_repaint();
            }
            ctx.accesskit_node_builder(search.id, |node| node.set_label(hint));

            ui.small(format!(
                "{} · {}",
                text!("Профиль сочетаний"),
                self.shortcuts.profile_label()
            ));
            let mut scroll = egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() - 155.0).clamp(72.0, 420.0))
                .auto_shrink([false, true]);
            if search.changed() {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            scroll.show(ui, |ui| {
                if self.query.trim().is_empty() {
                    ui.strong(text!("Начало работы"));
                    ui.label(crate::tr_format!(
                        "If only one pane is visible, choose “{split}” in the Command Palette. Open a folder in each pane and click the pane you want to work in.",
                        split = Command::Split.label()
                    ));
                    ui.label(crate::tr_format!(
                        "Select files in the active pane. In the Command Palette, choose “{copy}” or “{move_files}”.",
                        copy = Command::CopyToPane.label(),
                        move_files = Command::MoveToPane.label()
                    ));
                    ui.separator();
                }
                let mut commands = Command::matching(&self.query).peekable();
                if commands.peek().is_none() {
                    ui.label(text!("Команды не найдены"));
                    return;
                }
                egui::Grid::new("help-commands")
                    .num_columns(2)
                    .spacing([16.0, 5.0])
                    .max_col_width((ui.available_width() - 20.0) / 2.0)
                    .striped(true)
                    .show(ui, |ui| {
                        for command in commands {
                            ui.label(command.label());
                            let keys = self.shortcuts.keys(ctx, command);
                            if keys.is_empty() {
                                ui.weak(text!("Не назначено"));
                            } else {
                                ui.label(keys);
                            }
                            ui.end_row();
                        }
                    });
            });
            ui.horizontal_wrapped(|ui| {
                let palette = ui.button(text!("Открыть палитру команд"));
                paint_focus(ui, &palette, 4);
                let close = ui.button(text!("Закрыть"));
                paint_focus(ui, &close, 4);
                if palette.clicked() {
                    ComparisonResponse::OpenCommandPalette(std::mem::take(&mut self.query))
                } else if close.clicked() {
                    ComparisonResponse::Close
                } else {
                    ComparisonResponse::Open
                }
            })
            .inner
        });
        if modal.should_close() {
            ComparisonResponse::Close
        } else {
            modal.inner
        }
    }
}

impl PawCommander {
    pub(super) fn open_help(&mut self, focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_some() || self.conflict_open() {
            return;
        }
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::Help(Box::new(HelpDialog {
                shortcuts: self.settings.shortcuts.clone(),
                query: String::new(),
                focus_query: true,
            })),
            focus,
            ctx,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;

    #[test]
    fn help_search_shows_saved_shortcut_and_carries_the_query_to_the_palette() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let binding = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::F9);
        let shortcuts = serde_json::from_value(serde_json::json!({
            "profile": "Commander",
            "overrides": { "NewTab": binding }
        }))
        .unwrap();
        let mut help = HelpDialog {
            shortcuts,
            query: String::new(),
            focus_query: true,
        };
        let frame = |help: &mut HelpDialog, events| {
            let mut response = ComparisonResponse::Open;
            let output = ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| response = help.show(ui.ctx()),
            );
            (response, output.platform_output.accesskit_update.unwrap())
        };
        frame(&mut help, vec![]);
        frame(&mut help, vec![]);
        let query = Command::NewTab.label().to_uppercase();
        let (_, tree) = frame(&mut help, vec![egui::Event::Text(query.clone())]);
        assert_eq!(help.query, query, "the search field should receive focus");
        let labels: Vec<_> = tree
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Label)
            .filter_map(|(_, node)| node.value())
            .collect();
        assert!(labels.contains(&Command::NewTab.label()), "{labels:?}");
        assert!(labels.contains(&ctx.format_shortcut(&binding).as_str()));
        assert!(!labels.contains(&Command::CloseTab.label()));
        let &(target_node, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text!("Открыть палитру команд")))
            .expect("command palette button");
        let (response, _) = frame(
            &mut help,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target_tree: egui::accesskit::TreeId::ROOT,
                    target_node,
                    data: None,
                },
            )],
        );
        assert!(matches!(response, ComparisonResponse::OpenCommandPalette(s) if s == query));
    }
}
