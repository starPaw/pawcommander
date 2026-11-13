//! Headless frames deliberately discard GPU uploads while retaining UI output.
use eframe::egui;

pub trait ContextExt {
    fn run_test_ui(&self, input: egui::RawInput, ui: impl FnMut(&mut egui::Ui))
        -> egui::FullOutput;
}

impl ContextExt for egui::Context {
    fn run_test_ui(
        &self,
        input: egui::RawInput,
        ui: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let mut output = self.run_ui(input, ui);
        // egui requires integrations to acknowledge texture updates. Tests have
        // no renderer; shapes, accessibility output and input handling remain.
        output.textures_delta.clear();
        output
    }
}
