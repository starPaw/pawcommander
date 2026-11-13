//! Windows drive discovery stays off the UI thread. A stalled OS query can
//! delay a refresh but cannot stall painting or input handling.

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(super) struct WindowsDrives {
    paths: Vec<PathBuf>,
    receive: Option<Receiver<Option<Vec<PathBuf>>>>,
    last_started: Option<Instant>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl WindowsDrives {
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(receive) = &self.receive {
            match receive.try_recv() {
                Ok(Some(paths)) => {
                    self.paths = paths;
                    self.receive = None;
                    ctx.request_repaint();
                }
                Ok(None) | Err(TryRecvError::Disconnected) => self.receive = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if self.receive.is_some()
            || self
                .last_started
                .is_some_and(|started| started.elapsed() < REFRESH_INTERVAL)
        {
            return;
        }
        self.last_started = Some(Instant::now());
        let (send, receive) = mpsc::channel();
        let worker_ctx = ctx.clone();
        if std::thread::Builder::new()
            .name("pawcommander-windows-drives".into())
            .spawn(move || {
                // SAFETY: GetLogicalDrives has no pointer arguments and returns
                // a bitmask of logical drive letters (zero signals failure).
                #[cfg(target_os = "windows")]
                let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
                #[cfg(not(target_os = "windows"))]
                let mask = 0;
                let paths = (mask != 0).then(|| paths_from_mask(mask));
                let _ = send.send(paths);
                worker_ctx.request_repaint();
            })
            .is_ok()
        {
            self.receive = Some(receive);
        }
        ctx.request_repaint_after(REFRESH_INTERVAL);
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

fn paths_from_mask(mask: u32) -> Vec<PathBuf> {
    (0u8..26)
        .filter(|index| mask & (1u32 << index) != 0)
        .map(|index| PathBuf::from(format!("{}:\\", (b'A' + index) as char)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_drive_mask_preserves_letter_order() {
        assert_eq!(
            paths_from_mask((1 << 25) | (1 << 2) | (1 << 0)),
            vec![
                PathBuf::from("A:\\"),
                PathBuf::from("C:\\"),
                PathBuf::from("Z:\\")
            ]
        );
    }
}
