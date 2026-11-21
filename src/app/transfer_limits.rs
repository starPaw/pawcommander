use super::{dialogs::Dialog, PawCommander};
use crate::{
    i18n::text,
    operations::TransferLimits,
    vfs::{Endpoint, Location},
};
use eframe::egui;

impl PawCommander {
    pub(super) fn open_transfer_limits(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || self.conflict_open() {
            return;
        }
        let active = self
            .job
            .as_ref()
            .map(|job| job.progress.transfer_endpoints())
            .unwrap_or_default();
        let locations = self
            .panes
            .iter()
            .flat_map(|pane| pane.tabs.iter().map(|tab| &tab.path))
            .chain(&self.settings.sftp_profiles)
            .chain(self.bookmarks.iter().map(|bookmark| &bookmark.location));
        let mut endpoints: Vec<Endpoint> = Vec::new();
        for endpoint in active
            .iter()
            .flatten()
            .chain(locations.filter_map(Location::sftp_endpoint))
            .chain(self.settings.transfer_limits.endpoints())
        {
            if !endpoints.iter().any(|known| known.same_authority(endpoint)) {
                endpoints.push(endpoint.clone());
            }
        }
        self.remember_dialog_file_focus(ctx);
        self.dialog = Some(Dialog::TransferLimits {
            draft: self.settings.transfer_limits.clone(),
            endpoints,
        });
        self.dialog_focus = true;
        ctx.request_repaint();
    }

    pub(super) fn apply_transfer_limits(&mut self, limits: TransferLimits, ctx: &egui::Context) {
        self.settings.transfer_limits = limits;
        if let Some(job) = &self.job {
            job.progress
                .set_transfer_limits(&self.settings.transfer_limits);
        }
        ctx.request_repaint();
    }
}

pub(super) fn show_fields(ui: &mut egui::Ui, draft: &mut TransferLimits, endpoints: &[Endpoint]) {
    rate_field(ui, text!("Общий лимит"), &mut draft.global_kib);
    ui.weak(text!("0 — без ограничения"));
    if !endpoints.is_empty() {
        ui.separator();
        ui.label(text!("Подключения SFTP"));
        egui::ScrollArea::vertical()
            .max_height(240.0)
            .show(ui, |ui| {
                for (index, endpoint) in endpoints.iter().enumerate() {
                    let label = Location::Sftp {
                        endpoint: endpoint.clone(),
                        path: "/".into(),
                    }
                    .display();
                    let mut kib = draft.connection_kib(endpoint);
                    if ui
                        .push_id(index, |ui| rate_field(ui, &label, &mut kib))
                        .inner
                        .changed()
                    {
                        draft.set_connection_kib(endpoint, kib);
                    }
                }
            });
    }
}

fn rate_field(ui: &mut egui::Ui, caption: &str, kib: &mut u32) -> egui::Response {
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        row_size,
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            let response = ui.add(
                egui::DragValue::new(kib)
                    .speed(1)
                    .range(0..=u32::MAX)
                    .suffix(format!(" {}", text!("КиБ/с"))),
            );
            let caption = ui
                .add(egui::Label::new(caption).truncate())
                .on_hover_text(caption);
            response.labelled_by(caption.id)
        },
    )
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;

    #[test]
    fn transfer_speed_field_reserves_only_one_row() {
        let ctx = egui::Context::default();
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                let top = ui.next_widget_position().y;
                let mut limit = 96;
                let field = rate_field(ui, "Global limit", &mut limit);
                let used = ui.next_widget_position().y - top;
                let row_height = field.rect.height() + ui.spacing().item_spacing.y;
                assert!(
                    used <= row_height + 1.0,
                    "Speed field used {used} pt for a {row_height} pt row"
                );
            },
        );
    }
}
