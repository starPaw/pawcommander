use super::{Location, PawCommander, TabTarget};
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::{i18n::text, preview::Preview, shortcuts, theme::paint_focus, vfs::FileEntry};
use eframe::egui::{self, Key, Modifiers};

pub(super) struct QuickLook {
    origin: TabTarget,
    original_file: Location,
    file: Location,
    position: usize,
    wait_for_opening_space: bool,
    closing_key: Option<(egui::ViewportId, Key)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Previous,
    Next,
    Close,
}

impl QuickLook {
    fn viewport_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("quick-look")
    }

    fn window_builder() -> egui::ViewportBuilder {
        egui::ViewportBuilder::default()
            .with_title(text("Быстрый просмотр"))
            .with_inner_size([760.0, 540.0])
            .with_min_inner_size([360.0, 240.0])
    }

    fn close_invalidated_window(ctx: &egui::Context) -> bool {
        let viewport = Self::viewport_id();
        if ctx.embed_viewports() || !ctx.input(|input| input.raw.viewports.contains_key(&viewport))
        {
            return false;
        }
        // The main window's copy of child focus may be from the previous frame.
        let focused =
            ctx.show_viewport_immediate(viewport, Self::window_builder(), |ctx, class| {
                class != egui::ViewportClass::EmbeddedWindow
                    && ctx.input(|input| input.viewport().focused == Some(true))
            });
        ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Close);
        focused
    }

    fn take_keys(&mut self, ctx: &egui::Context, typing: bool, seeking: bool) -> Option<Action> {
        let mut action = None;
        let viewport = ctx.viewport_id();
        ctx.input_mut(|input| {
            if focus_lost(input) {
                if self
                    .closing_key
                    .is_some_and(|(window, _)| window == viewport)
                {
                    self.closing_key = None;
                }
                return;
            }
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    modifiers,
                    pressed,
                    repeat,
                    ..
                } = *event
                else {
                    return true;
                };
                if !pressed {
                    if self.closing_key == Some((viewport, key)) {
                        self.closing_key = None;
                        action = Some(Action::Close);
                    }
                    return true;
                }
                if shortcuts::chord(key, modifiers).modifiers != Modifiers::NONE {
                    return true;
                }
                if typing {
                    return true;
                }
                match key {
                    Key::Space | Key::Escape => {
                        // Close on release so a held key cannot act in the main window.
                        if !repeat {
                            self.closing_key = Some((viewport, key));
                        }
                    }
                    Key::ArrowLeft if !seeking => {
                        action.get_or_insert(Action::Previous);
                    }
                    Key::ArrowRight if !seeking => {
                        action.get_or_insert(Action::Next);
                    }
                    _ => return true,
                }
                false
            });
        });
        action
    }

    fn show(
        &mut self,
        ctx: &egui::Context,
        preview: &mut Preview,
        entry: &FileEntry,
        has_previous: bool,
        has_next: bool,
    ) -> Option<Action> {
        let viewport = Self::viewport_id();
        let (action, native) = if ctx.embed_viewports() {
            // egui now wraps embedded viewports in its own Window. Keep our
            // modal fallback outside that API so it owns the backdrop/focus.
            let keys = self.take_keys(ctx, preview.text_input(ctx), preview.seek_focused(ctx));
            let modal = egui::Modal::new(egui::Id::unique("quick-look")).show(ctx, |ui| {
                let size = ctx.content_rect().size() * 0.8;
                ui.set_width(size.x.min(760.0));
                ui.set_height(size.y.min(540.0));
                ui.heading(text("Быстрый просмотр"));
                Self::show_content(ui, preview, entry, has_previous, has_next)
            });
            let action = if modal.backdrop_response.clicked() {
                Some(Action::Close)
            } else {
                keys.or(modal.inner)
            };
            (action, false)
        } else {
            ctx.show_viewport_immediate(viewport, Self::window_builder(), |ui, class| {
                let viewport_ctx = ui.ctx().clone();
                // Immediate viewports bypass App::raw_input_hook.
                #[cfg(target_os = "macos")]
                if class != egui::ViewportClass::EmbeddedWindow {
                    viewport_ctx.input_mut(|input| {
                        super::clipboard::macos_clipboard_events(&mut input.events);
                    });
                }
                let keys = self.take_keys(
                    &viewport_ctx,
                    preview.text_input(&viewport_ctx),
                    preview.seek_focused(&viewport_ctx),
                );
                let buttons = egui::CentralPanel::default()
                    .show(ui, |ui| {
                        Self::show_content(ui, preview, entry, has_previous, has_next)
                    })
                    .inner;
                let action = if viewport_ctx.input(|input| input.viewport().close_requested()) {
                    Some(Action::Close)
                } else {
                    keys.or(buttons)
                };
                (action, class != egui::ViewportClass::EmbeddedWindow)
            })
        };
        let mut action = action;
        if native {
            // The OS may activate the main window while Quick Look remains open.
            action =
                action.or(self.take_keys(ctx, preview.text_input(ctx), preview.seek_focused(ctx)));
            if action != Some(Action::Close) {
                let backdrop = egui::Modal::new(egui::Id::unique("quick-look-backdrop"))
                    .frame(egui::Frame::new())
                    .backdrop_color(egui::Color32::TRANSPARENT)
                    .show(ctx, |_| {});
                if backdrop.backdrop_response.clicked() {
                    action = Some(Action::Close);
                }
            }
        }
        if action == Some(Action::Close) {
            // A click on Close during a keyboard close can wait for the same key-up.
            if self.closing_key.is_some() {
                return None;
            }
            if native {
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Close);
            }
        }
        action
    }

    fn show_content(
        ui: &mut egui::Ui,
        preview: &mut Preview,
        entry: &FileEntry,
        has_previous: bool,
        has_next: bool,
    ) -> Option<Action> {
        let action = ui
            .horizontal(|ui| {
                let previous = ui
                    .add_enabled(has_previous, egui::Button::new(text("Предыдущий файл")))
                    .on_hover_text(ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                        Modifiers::NONE,
                        Key::ArrowLeft,
                    )));
                let next = ui
                    .add_enabled(has_next, egui::Button::new(text("Следующий файл")))
                    .on_hover_text(ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                        Modifiers::NONE,
                        Key::ArrowRight,
                    )));
                let close = ui.button(text("Закрыть"));
                for button in [&previous, &next, &close] {
                    paint_focus(ui, button, 5);
                    if button.has_focus() {
                        ui.memory_mut(|memory| {
                            memory.set_focus_lock_filter(
                                button.id,
                                egui::EventFilter {
                                    horizontal_arrows: true,
                                    ..Default::default()
                                },
                            );
                        });
                    }
                }
                if close.clicked() {
                    Some(Action::Close)
                } else if previous.clicked() {
                    Some(Action::Previous)
                } else if next.clicked() {
                    Some(Action::Next)
                } else {
                    None
                }
            })
            .inner;
        ui.separator();
        preview.show_quick_look(ui, &entry.path, (entry.size, entry.modified));
        action
    }
}

fn focus_lost(input: &egui::InputState) -> bool {
    !input.focused
        || input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::WindowFocused(false)))
}

impl PawCommander {
    pub(super) fn quick_look_entry(&self, focused: Option<&Location>) -> Option<&FileEntry> {
        let tab = self.tab();
        let visible = tab.visible(self.folder_view(self.active));
        let entries = || visible.iter().map(|&index| &tab.entries[index]);
        if let Some(path) = focused.or(tab.cursor.as_ref()) {
            if let Some(entry) = entries().find(|entry| &entry.path == path) {
                return (!entry.is_dir).then_some(entry);
            }
            if focused.is_some() {
                return None;
            }
        }
        entries().find(|entry| !entry.is_dir && tab.selected.contains(&entry.path))
    }

    pub(super) fn open_quick_look(&mut self, ctx: &egui::Context) {
        if ctx.input(focus_lost) {
            return;
        }
        let tab = self.tab();
        let focused = self
            .file_row_focus(ctx)
            .filter(|(pane, _)| *pane == self.active);
        let Some(entry) = self.quick_look_entry(focused.map(|(_, path)| path)) else {
            return;
        };
        self.quick_look = Some(QuickLook {
            origin: TabTarget {
                pane: self.active,
                tab: tab.id,
                location: tab.path.clone(),
            },
            original_file: entry.path.clone(),
            file: entry.path.clone(),
            position: 0,
            wait_for_opening_space: ctx.input(|input| input.key_down(Key::Space)),
            closing_key: None,
        });
        Self::restore_focus(ctx, None);
        ctx.request_repaint();
    }

    pub(super) fn show_quick_look(&mut self, ctx: &egui::Context) -> Option<egui::Id> {
        let mut quick_look = self.quick_look.take()?;
        let cancelled_open = quick_look.wait_for_opening_space && ctx.input(focus_lost);
        if quick_look.wait_for_opening_space && !cancelled_open {
            // Create the window after key-up so an opening hold cannot reach it as a close.
            if ctx.input(|input| input.key_down(Key::Space)) {
                self.quick_look = Some(quick_look);
                return None;
            }
            quick_look.wait_for_opening_space = false;
        }
        let mut closed_window = false;
        if !cancelled_open && self.quick_look_origin_visible(&quick_look.origin) {
            let pane = quick_look.origin.pane;
            let tab = self.panes[pane].tab();
            let visible = tab.visible(self.folder_view(pane));
            let entry_at = |position: usize| &tab.entries[visible[position]];
            let is_current = |&index: &usize| {
                let entry = &tab.entries[index];
                !entry.is_dir && entry.path == quick_look.file
            };
            let position = if visible.get(quick_look.position).is_some_and(is_current) {
                Some(quick_look.position)
            } else {
                visible.iter().position(is_current)
            };
            if let Some(position) = position {
                quick_look.position = position;
                // DirectoryView always orders directories before files.
                let previous = position.checked_sub(1).filter(|&row| !entry_at(row).is_dir);
                let next = (position + 1 < visible.len()).then_some(position + 1);
                let preview = self
                    .quick_look_preview
                    .get_or_insert_with(Preview::new_quick_look);
                let action = quick_look.show(
                    ctx,
                    preview,
                    entry_at(position),
                    previous.is_some(),
                    next.is_some(),
                );
                if action != Some(Action::Close) {
                    let target = match action {
                        Some(Action::Previous) => previous,
                        Some(Action::Next) => next,
                        _ => None,
                    };
                    if let Some(target) = target {
                        quick_look.file = entry_at(target).path.clone();
                        quick_look.position = target;
                        ctx.request_repaint();
                    }
                    self.quick_look = Some(quick_look);
                    return None;
                }
                closed_window = true;
            }
        }
        if let Some(preview) = &mut self.quick_look_preview {
            preview.clear(ctx);
        }
        if !ctx.embed_viewports()
            && (closed_window || (!cancelled_open && QuickLook::close_invalidated_window(ctx)))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        ctx.request_repaint();
        self.restore_quick_look_cursor(&quick_look)
    }

    fn quick_look_origin_visible(&self, origin: &TabTarget) -> bool {
        (origin.pane == 0 || self.settings.split) && {
            let tab = self.panes[origin.pane].tab();
            tab.id == origin.tab && tab.path == origin.location
        }
    }

    fn restore_quick_look_cursor(&mut self, quick_look: &QuickLook) -> Option<egui::Id> {
        if !self.quick_look_origin_visible(&quick_look.origin) {
            return None;
        }
        self.active = quick_look.origin.pane;
        let view = self.folder_view(self.active);
        let tab = self.tab_mut();
        let visible = tab.visible(view);
        let paths = || visible.iter().map(|&index| &tab.entries[index].path);
        let target = paths()
            .find(|path| **path == quick_look.original_file)
            .or_else(|| paths().find(|path| Some(*path) == tab.cursor.as_ref()))
            .or_else(|| paths().find(|path| tab.rendered_rows.contains_key(&tab.entry_id(path))))
            .cloned();
        tab.cursor.clone_from(&target);
        let target = target?;
        let id = tab.entry_id(&target);
        if !tab.rendered_rows.contains_key(&id) {
            tab.scroll_to = Some(target);
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::wait_for_directory;
    use eframe::App;
    use egui::Event;

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn tap(key_code: Key) -> Vec<Event> {
        vec![
            key(key_code, true, Modifiers::NONE),
            key(key_code, false, Modifiers::NONE),
        ]
    }

    fn render(
        app: &mut PawCommander,
        ctx: &egui::Context,
        frame: &mut eframe::Frame,
        events: Vec<Event>,
    ) {
        let mut input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 860.0),
            )),
            ..Default::default()
        };
        app.raw_input_hook(ctx, &mut input);
        let _ = ctx.run_test_ui(input, |ui| {
            app.logic(ui.ctx(), frame);
            app.ui(ui, frame);
        });
    }

    #[test]
    fn quick_look_preserves_preview_text_field_keys() {
        let directory = tempfile::tempdir().unwrap();
        let location = Location::from(directory.path().to_path_buf());
        let mut quick_look = QuickLook {
            origin: TabTarget {
                pane: 0,
                tab: 0,
                location: location.clone(),
            },
            original_file: location.clone(),
            file: location,
            position: 0,
            wait_for_opening_space: false,
            closing_key: None,
        };
        let ctx = egui::Context::default();
        let mut query = String::from("a");
        let id = egui::Id::unique("preview-search-test");
        let _ = ctx.run_test_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut query).id(id))
                    .request_focus();
            });
        });
        for key_code in [Key::Space, Key::ArrowLeft, Key::ArrowRight, Key::Escape] {
            let events = tap(key_code);
            let _ = ctx.run_test_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    assert_eq!(quick_look.take_keys(ctx, true, false), None);
                    assert_eq!(ctx.input(|i| i.events.len()), 2);
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.add(egui::TextEdit::singleline(&mut query).id(id))
                            .request_focus();
                    });
                },
            );
            assert!(quick_look.closing_key.is_none());
        }
    }

    #[test]
    fn quick_look_browses_visible_files_and_restores_the_originating_pane_focus() {
        let root = tempfile::tempdir().unwrap();
        let location = Location::from(root.path().to_path_buf());
        let a = location.join("a.txt");
        let b = location.join("b.txt");
        let hidden = location.join("c.csv");
        for file in [&a, &b, &hidden] {
            std::fs::write(file.path(), b"fixture").unwrap();
        }
        std::fs::create_dir(location.join("folder.txt").path()).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.active = 1;
        app.panes[1].tab_mut().navigate(location, &ctx);
        wait_for_directory(app.tab_mut());
        app.tab_mut().filter = ".txt".into();
        app.tab_mut().cursor = Some(b.clone());
        app.tab_mut().selected.insert(a.clone());
        app.tab_mut().anchor = Some(a.clone());
        render(&mut app, &ctx, &mut frame, vec![]);
        let origin_id = app.tab().entry_id(&b);
        ctx.memory_mut(|memory| memory.request_focus(origin_id));
        render(
            &mut app,
            &ctx,
            &mut frame,
            vec![key(Key::Space, true, Modifiers::NONE)],
        );
        assert_eq!(app.quick_look.as_ref().unwrap().file, b);
        assert!(app.quick_look_preview.is_none());
        render(
            &mut app,
            &ctx,
            &mut frame,
            vec![key(Key::Space, false, Modifiers::NONE)],
        );
        assert_eq!(app.quick_look.as_ref().unwrap().file, b);
        assert!(app.quick_look_preview.is_some());
        render(&mut app, &ctx, &mut frame, tap(Key::ArrowRight));
        assert_eq!(app.quick_look.as_ref().unwrap().file, b);
        render(&mut app, &ctx, &mut frame, tap(Key::ArrowLeft));
        assert_eq!(app.quick_look.as_ref().unwrap().file, a);
        render(&mut app, &ctx, &mut frame, tap(Key::ArrowRight));
        assert_eq!(app.quick_look.as_ref().unwrap().file, b);
        render(&mut app, &ctx, &mut frame, tap(Key::ArrowLeft));
        render(&mut app, &ctx, &mut frame, tap(Key::Escape));
        assert!(app.quick_look.is_none());
        assert_eq!(app.active, 1);
        assert!(ctx.memory(|memory| memory.has_focus(origin_id)));
        assert_eq!(app.tab().cursor.as_ref(), Some(&b));
        assert_eq!(app.tab().anchor.as_ref(), Some(&a));
        assert_eq!(app.selected(), vec![a.clone()]);

        render(&mut app, &ctx, &mut frame, tap(Key::Space));
        assert!(app.quick_look.is_some());
        render(&mut app, &ctx, &mut frame, tap(Key::Space));
        assert!(app.quick_look.is_none());
        render(
            &mut app,
            &ctx,
            &mut frame,
            vec![key(Key::Space, true, Modifiers::SHIFT)],
        );
        assert!(app.quick_look.is_none());
        assert_eq!(app.selected(), vec![a, b]);
    }

    #[test]
    fn quick_look_keeps_text_input_and_never_switches_to_an_unrelated_listing() {
        let root = tempfile::tempdir().unwrap();
        let location = Location::from(root.path().to_path_buf());
        let file = location.join("file.txt");
        let folder = location.join("folder");
        std::fs::write(file.path(), b"fixture").unwrap();
        std::fs::create_dir(folder.path()).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        wait_for_directory(app.tab_mut());
        app.tab_mut().cursor = Some(file.clone());
        app.tab_mut().focus_filter = true;
        render(&mut app, &ctx, &mut frame, vec![]);
        let focus = ctx.memory(|memory| memory.focused()).unwrap();
        let mut events = tap(Key::Space);
        events.insert(1, Event::Text(" ".into()));
        render(&mut app, &ctx, &mut frame, events);
        assert_eq!(app.tab().filter, " ");
        assert!(app.quick_look.is_none());
        ctx.memory_mut(|memory| memory.surrender_focus(focus));
        app.tab_mut().filter.clear();
        app.tab_mut().cursor = Some(folder.clone());
        app.tab_mut().focus_cursor(&ctx);
        render(&mut app, &ctx, &mut frame, tap(Key::Space));
        assert!(app.quick_look.is_none());

        let missing = location.join("missing.txt");
        app.tab_mut().selected.insert(file.clone());
        app.tab_mut().cursor = Some(missing.clone());
        assert!(app.quick_look_entry(Some(&missing)).is_none());
        assert_eq!(
            app.quick_look_entry(None).map(|entry| &entry.path),
            Some(&file)
        );

        app.tab_mut().cursor = Some(file.clone());
        app.tab_mut().focus_cursor(&ctx);
        render(
            &mut app,
            &ctx,
            &mut frame,
            vec![key(Key::Space, true, Modifiers::NONE)],
        );
        assert!(app.quick_look.is_some());
        render(
            &mut app,
            &ctx,
            &mut frame,
            vec![
                key(Key::Space, false, Modifiers::NONE),
                Event::WindowFocused(false),
            ],
        );
        assert!(app.quick_look.is_none());
        assert!(app.quick_look_preview.is_none());

        app.tab_mut().cursor = Some(file);
        app.tab_mut().focus_cursor(&ctx);
        render(&mut app, &ctx, &mut frame, tap(Key::Space));
        assert!(app.quick_look.is_some());
        app.tab_mut().navigate(folder, &ctx);
        wait_for_directory(app.tab_mut());
        render(&mut app, &ctx, &mut frame, tap(Key::ArrowRight));
        assert!(app.quick_look.is_none());
        assert!(app.tab().cursor.is_none());
    }
}
