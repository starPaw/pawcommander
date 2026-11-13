mod accessibility;
mod address;
mod bookmarks;
mod clipboard;
mod directory;
mod drag_drop;
mod jobs;
mod keyboard;
mod layout;
mod navigation;
mod recovery;
mod search;
mod selection;
mod sessions;
mod tabs;

use super::*;

pub(super) fn wait_for_directory(tab: &mut Tab) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while tab.loading.is_some() && Instant::now() < deadline {
        tab.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(tab.loading.is_none(), "directory read did not finish");
}

#[derive(Default)]
struct MemoryStorage(std::collections::HashMap<String, String>);
impl eframe::Storage for MemoryStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.into(), value);
    }
    fn flush(&mut self) {}
}

fn operation_footer_frame(
    app: &mut PawCommander,
    ctx: &egui::Context,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
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
            app.transfer_stop_shortcut(ctx);
            app.footer(ctx);
        },
    )
}

fn operation_footer_text(output: &egui::FullOutput) -> String {
    output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hold_queue(app: &mut PawCommander) -> mpsc::Sender<Result<String, String>> {
    let (send, receive) = mpsc::channel();
    let (_, progress) = Progress::tracked();
    app.job = Some(Job {
        label: "held operation".into(),
        started: Instant::now(),
        result: receive,
        progress,
    });
    send
}

fn poll_queue_until(
    app: &mut PawCommander,
    ctx: &egui::Context,
    condition: impl Fn(&PawCommander) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition(app) && Instant::now() < deadline {
        app.poll(ctx);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(condition(app), "queue did not reach expected state");
}

fn answer_queue_conflict(app: &mut PawCommander, ctx: &egui::Context, label: &str) {
    let mut draw = |events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(800.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                let transfer = app.transfer.as_mut().unwrap();
                if transfer.conflict.as_mut().unwrap().show(ctx) {
                    transfer.conflict = None;
                }
            },
        )
    };
    draw(vec![]);
    let output = draw(vec![]);
    let point = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("conflict decision button");
    for pressed in [true, false] {
        draw(vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
}

fn visible_rows(tab: &Tab, hidden: bool) -> Rc<Vec<usize>> {
    tab.visible(FolderView {
        hidden,
        order: tab.legacy_order(),
        ..Default::default()
    })
}
