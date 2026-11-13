//! Hand an internal drag to the OS as it leaves the window. Keeping the handoff
//! at the boundary preserves egui's internal targets and Shift-move semantics.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::NativeDrag;

#[cfg(any(target_os = "windows", target_os = "linux"))]
mod desktop;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub use desktop::NativeDrag;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
#[derive(Default)]
pub struct NativeDrag;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl NativeDrag {
    pub fn active(&self) -> bool {
        false
    }
    pub fn finish_input(&mut self, _: &mut eframe::egui::RawInput) -> Option<bool> {
        None
    }
    pub fn handoff(
        &mut self,
        _: &eframe::egui::Context,
        _: &eframe::Frame,
        _: bool,
    ) -> Option<Result<(), String>> {
        None
    }
}
