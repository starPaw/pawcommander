use super::*;

#[test]
fn dragging_rows_and_tiles_preserves_visible_group_and_transfers_files() {
    for grid in [false, true] {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        for name in ["a.txt", "b.txt", ".hidden"] {
            std::fs::write(source.join(name), name).unwrap();
        }
        let mut tab = Tab::new(source.into(), &ctx);
        tab.loading = None;
        tab.entries
            .replace(fs_ops::read_directory(&tab.path, &|| false).unwrap());
        tab.selected = tab.entries.iter().map(|e| e.path.clone()).collect();
        let mut source_pos = egui::Pos2::ZERO;
        let mut target_pos = egui::Pos2::ZERO;
        let mut run = |events| {
            let mut drop = None;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(800.0, 500.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                let mut targets = DropTargets::default();
                let mut actions = vec![];
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.columns(2, |cols| {
                        let target_rect = cols[0].available_rect_before_wrap();
                        target_pos = target_rect.center();
                        targets.add(&cols[0], target_rect, 0, &destination.clone().into(), true);
                        source_pos = cols[1].available_rect_before_wrap().min + Vec2::splat(10.0);
                        let visible = visible_rows(&tab, false);
                        entry_widget(
                            &mut cols[1],
                            &mut tab,
                            0,
                            &visible,
                            EntryOptions {
                                columns: None,
                                thumbnails: None,
                                pane: 1,
                                split: true,
                                grid,
                                folder_columns: false,
                                compact: false,
                                idle: true,
                                zed_available: true,
                            },
                            &mut actions,
                            &mut targets,
                        );
                    });
                });
                drop = targets.finish(ctx, true);
            });
            (source_pos, target_pos, drop)
        };
        let (start, end, _) = run(vec![]);
        run(vec![egui::Event::PointerMoved(start)]);
        run(vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        run(vec![egui::Event::PointerMoved(end)]);
        let (_, _, result) = run(vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let request = result.expect("actual entry widget starts a drag").unwrap();
        assert_eq!(
            request.sources.len(),
            2,
            "hidden selection must not be dragged"
        );
        assert_eq!(request.destination, Location::Local(destination.clone()));
        assert_eq!(request.kind, TransferKind::Copy);
        fs_ops::transfer(&request.sources, &request.destination, request.kind).unwrap();
        for name in ["a.txt", "b.txt"] {
            assert_eq!(
                std::fs::read_to_string(destination.join(name)).unwrap(),
                name
            );
            assert!(tab.path.join(name).path().exists());
        }
        assert!(!destination.join(".hidden").exists());
        // Dragging an unselected item replaces the group without interpreting
        // Shift (which controls the drop operation) as range selection.
        tab.selected.clear();
        let visible = visible_rows(&tab, false);
        let paths = tab.drag_paths(visible[0], &visible);
        assert_eq!(paths.len(), 1);
        assert_eq!(tab.selected.len(), 1);
    }
}
