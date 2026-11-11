//! Persistent list column preferences and shared header/row geometry.

use crate::i18n::text;
use crate::theme::paint_focus;
use crate::vfs::FileEntry;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Column {
    Name,
    Size,
    Modified,
    Extension,
    Permissions,
    Owner,
    Created,
    Accessed,
}

impl Column {
    const OPTIONAL: [Self; 7] = [
        Self::Size,
        Self::Modified,
        Self::Extension,
        Self::Permissions,
        Self::Owner,
        Self::Created,
        Self::Accessed,
    ];

    pub(crate) fn label(self) -> &'static str {
        text(match self {
            Self::Name => "Имя",
            Self::Size => "Размер",
            Self::Modified => "Изменён",
            Self::Extension => "Расширение",
            Self::Permissions => "Права",
            Self::Owner => "Владелец",
            Self::Created => "Создан",
            Self::Accessed => "Последний доступ",
        })
    }

    fn default_width(self) -> u16 {
        match self {
            Self::Name => 220,
            Self::Size | Self::Extension | Self::Owner => 90,
            Self::Permissions => 100,
            Self::Modified | Self::Created | Self::Accessed => 125,
        }
    }

    pub(crate) fn metadata(self, entry: &FileEntry) -> Option<Cow<'_, str>> {
        match self {
            Self::Extension if !entry.is_dir => Path::new(&entry.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| !extension.is_empty())
                .map(Cow::Borrowed),
            Self::Permissions => entry.metadata.mode.map(|mode| format!("{mode:04o}").into()),
            Self::Owner => entry.metadata.owner_id.map(|id| format!("UID {id}").into()),
            Self::Modified => format_date(entry.modified),
            Self::Created => format_date(entry.metadata.created),
            Self::Accessed => format_date(entry.metadata.accessed),
            Self::Name | Self::Size | Self::Extension => None,
        }
    }
}

fn format_date(time: Option<SystemTime>) -> Option<Cow<'static, str>> {
    let elapsed = match time?.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => chrono::TimeDelta::from_std(duration).ok()?,
        Err(error) => -chrono::TimeDelta::from_std(error.duration()).ok()?,
    };
    let local = chrono::DateTime::UNIX_EPOCH
        .checked_add_signed(elapsed)?
        .with_timezone(&chrono::Local);
    Some(local.format("%d.%m.%y %H:%M").to_string().into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ColumnSetting {
    column: Column,
    width: u16,
    visible: bool,
}

impl ColumnSetting {
    fn new(column: Column) -> Self {
        Self {
            column,
            width: column.default_width(),
            visible: matches!(column, Column::Size | Column::Modified),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ListColumns {
    custom: bool,
    name_width: u16,
    columns: Vec<ColumnSetting>,
}

impl Default for ListColumns {
    fn default() -> Self {
        Self {
            custom: false,
            name_width: Column::Name.default_width(),
            columns: Column::OPTIONAL
                .into_iter()
                .map(ColumnSetting::new)
                .collect(),
        }
    }
}

impl ListColumns {
    const NAME_WIDTH: std::ops::RangeInclusive<u16> = 100..=800;
    const DETAIL_WIDTH: std::ops::RangeInclusive<u16> = 60..=600;

    pub(crate) fn is_custom(&self) -> bool {
        self.custom
    }

    pub(crate) fn validate(&mut self) {
        self.name_width = self
            .name_width
            .clamp(*Self::NAME_WIDTH.start(), *Self::NAME_WIDTH.end());
        let mut missing = Column::OPTIONAL.to_vec();
        self.columns.retain_mut(|setting| {
            let Some(index) = missing.iter().position(|&column| column == setting.column) else {
                return false;
            };
            missing.remove(index);
            setting.width = setting
                .width
                .clamp(*Self::DETAIL_WIDTH.start(), *Self::DETAIL_WIDTH.end());
            true
        });
        self.columns
            .extend(missing.into_iter().map(ColumnSetting::new));
    }

    pub(crate) fn layout(&self, available: f32) -> ColumnLayout {
        if !self.custom {
            return ColumnLayout::responsive(available);
        }
        let mut columns = vec![(Column::Name, f32::from(self.name_width))];
        columns.extend(
            self.columns
                .iter()
                .filter(|setting| setting.visible)
                .map(|setting| (setting.column, f32::from(setting.width))),
        );
        let content_width = columns.iter().map(|&(_, width)| width).sum::<f32>();
        ColumnLayout {
            columns,
            width: available.max(content_width),
        }
    }

    pub(crate) fn dialog(&mut self, ctx: &egui::Context, focus: &mut bool) -> bool {
        let response = egui::Modal::new(egui::Id::new("list_columns")).show(ctx, |ui| {
            ui.set_width((ctx.screen_rect().width() - 28.0).clamp(1.0, 340.0));
            ui.heading(text("Столбцы списка"));
            egui::ScrollArea::both()
                .max_height((ctx.screen_rect().height() - 125.0).max(40.0))
                .show(ui, |ui| self.controls(ui, focus));
            ui.separator();
            let close = ui.button(text("Закрыть"));
            paint_focus(ui, &close, 4);
            close.clicked()
        });
        response.inner || response.should_close()
    }

    fn controls(&mut self, ui: &mut egui::Ui, focus: &mut bool) {
        let mut changed = false;
        let mut move_column = None;
        egui::Grid::new("list_column_settings")
            .num_columns(4)
            .show(ui, |ui| {
                ui.label(text("Имя"));
                let width = width_control(ui, Column::Name, &mut self.name_width, Self::NAME_WIDTH);
                if *focus && !ui.is_sizing_pass() {
                    width.request_focus();
                    *focus = false;
                }
                changed |= width.changed();
                ui.end_row();
                let count = self.columns.len();
                for (index, setting) in self.columns.iter_mut().enumerate() {
                    ui.push_id(setting.column as u8, |ui| {
                        let label = setting.column.label();
                        let response = ui.checkbox(&mut setting.visible, label);
                        paint_focus(ui, &response, 4);
                        changed |= response.changed();
                    });
                    changed |= ui
                        .push_id(("width", setting.column as u8), |ui| {
                            ui.add_enabled_ui(setting.visible, |ui| {
                                width_control(
                                    ui,
                                    setting.column,
                                    &mut setting.width,
                                    Self::DETAIL_WIDTH,
                                )
                                .changed()
                            })
                            .inner
                        })
                        .inner;
                    for (target, symbol, hint) in [
                        (index.checked_sub(1), "↑", text("Переместить выше")),
                        (
                            (index + 1 < count).then_some(index + 1),
                            "↓",
                            text("Переместить ниже"),
                        ),
                    ] {
                        let title = format!("{hint} · {}", setting.column.label());
                        let response = ui
                            .push_id((setting.column as u8, symbol), |ui| {
                                ui.add_enabled(target.is_some(), egui::Button::new(symbol))
                            })
                            .inner
                            .on_hover_text(&title);
                        ui.ctx()
                            .accesskit_node_builder(response.id, |node| node.set_label(title));
                        paint_focus(ui, &response, 4);
                        if response.clicked() {
                            move_column = target.map(|target| (index, target));
                        }
                    }
                    ui.end_row();
                }
            });
        if let Some((index, target)) = move_column {
            self.columns.swap(index, target);
            changed = true;
        }
        self.custom |= changed;
        let reset = ui.add_enabled(self.custom, egui::Button::new(text("Сбросить столбцы")));
        paint_focus(ui, &reset, 4);
        if reset.clicked() {
            *self = Self::default();
        }
    }
}

fn width_control(
    ui: &mut egui::Ui,
    column: Column,
    width: &mut u16,
    range: std::ops::RangeInclusive<u16>,
) -> egui::Response {
    let title = format!("{} · {}", text("Ширина столбца"), column.label());
    let response = ui
        .add(egui::DragValue::new(width).range(range))
        .on_hover_text(&title);
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(title));
    paint_focus(ui, &response, 4);
    response
}

pub(crate) struct ColumnLayout {
    columns: Vec<(Column, f32)>,
    pub(crate) width: f32,
}

impl ColumnLayout {
    pub(crate) fn responsive(available: f32) -> Self {
        let mut columns = Vec::with_capacity(3);
        let (size_width, modified_width) = if available > 380.0 {
            (74.0, 108.0)
        } else if available >= 200.0 {
            (72.0, 0.0)
        } else {
            (0.0, 0.0)
        };
        columns.push((Column::Name, available - size_width - modified_width));
        if size_width > 0.0 {
            columns.push((Column::Size, size_width));
        }
        if modified_width > 0.0 {
            columns.push((Column::Modified, modified_width));
        }
        Self {
            columns,
            width: available,
        }
    }

    pub(crate) fn cells(
        &self,
        rect: egui::Rect,
    ) -> impl Iterator<Item = (Column, egui::Rect)> + '_ {
        let mut left = rect.left();
        self.columns.iter().map(move |&(column, width)| {
            let cell = egui::Rect::from_min_max(
                egui::pos2(left, rect.top()),
                egui::pos2(left + width, rect.bottom()),
            );
            left += width;
            (column, cell)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_layout_keeps_narrow_defaults() {
        for (width, expected) in [
            (199.0, &[Column::Name][..]),
            (200.0, &[Column::Name, Column::Size][..]),
            (381.0, &[Column::Name, Column::Size, Column::Modified][..]),
        ] {
            let layout = ListColumns::default().layout(width);
            let kinds: Vec<_> = layout.columns.iter().map(|&(kind, _)| kind).collect();
            assert_eq!(kinds, expected);
            assert_eq!(layout.width, width);
        }
    }

    #[test]
    fn custom_layout_uses_saved_width_order_and_visibility() {
        let mut settings = ListColumns {
            custom: true,
            name_width: 250,
            ..ListColumns::default()
        };
        settings.columns[0].visible = false;
        settings.columns[4].visible = true;
        settings.columns[4].width = 110;
        settings.columns.swap(1, 4);
        let serialized = serde_json::to_string(&settings).unwrap();
        let restored: ListColumns = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, settings);
        let layout = restored.layout(250.0);
        assert_eq!(
            layout.columns,
            vec![
                (Column::Name, 250.0),
                (Column::Owner, 110.0),
                (Column::Modified, 125.0)
            ]
        );
        assert_eq!(layout.width, 485.0);
        assert_eq!(restored.layout(500.0).columns[0].1, 250.0);
    }

    #[cfg(unix)]
    #[test]
    fn local_listing_reports_extensions_permissions_and_owner() {
        use std::fs;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("report.tar.gz");
        fs::write(&file, b"report").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(folder.path().join(".hidden"), b"dotfile").unwrap();
        fs::create_dir(folder.path().join("example.dir")).unwrap();
        let entries =
            crate::vfs::read_directory(&folder.path().to_path_buf().into(), &|| false).unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.name == "report.tar.gz")
            .unwrap();
        assert_eq!(Column::Extension.metadata(entry).as_deref(), Some("gz"));
        assert_eq!(Column::Permissions.metadata(entry).as_deref(), Some("0640"));
        let uid = fs::metadata(&file).unwrap().uid();
        assert_eq!(Column::Owner.metadata(entry).unwrap(), format!("UID {uid}"));
        assert!(Column::Modified.metadata(entry).is_some());
        assert!(Column::Accessed.metadata(entry).is_some());
        for entry in entries.iter().filter(|entry| entry.name != "report.tar.gz") {
            assert!(Column::Extension.metadata(entry).is_none());
        }
    }

    #[test]
    fn unavailable_or_out_of_range_dates_are_blank() {
        assert!(format_date(None).is_none());
        assert!(format_date(
            SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(10_000_000_000_000))
        )
        .is_none());
        assert!(
            format_date(SystemTime::UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(1)))
                .is_some()
        );
    }
}
