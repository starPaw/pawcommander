use super::FileList;
use clipboard_win::{formats::CF_HDROP, raw, Clipboard};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

const DROP_EFFECT_COPY: u32 = 1;
const DROP_EFFECT_MOVE: u32 = 2;

pub struct SystemClipboard {
    hwnd: Option<isize>,
}

impl SystemClipboard {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let hwnd = cc
            .window_handle()
            .ok()
            .and_then(|handle| match handle.as_raw() {
                RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                _ => None,
            });
        Self { hwnd }
    }

    pub fn write(&mut self, paths: &[PathBuf], cut: bool) -> Result<(), String> {
        let hwnd = self.hwnd.ok_or(crate::i18n::text(
            "Окно PawCommander недоступно для записи в буфер Windows",
        ))?;
        let preferred = raw::register_format("Preferred DropEffect").ok_or(crate::i18n::text(
            "Не удалось зарегистрировать Preferred DropEffect",
        ))?;
        // DROPFILES: pFiles, POINT, fNC, fWide. The filenames are UTF-16 followed by a double NUL.
        let mut files = Vec::new();
        for word in [20u32, 0, 0, 0, 1] {
            files.extend(word.to_le_bytes());
        }
        for path in paths {
            for word in path.as_os_str().encode_wide() {
                if word == 0 {
                    return Err(crate::i18n::text("Путь содержит нулевой символ").into());
                }
                files.extend(word.to_le_bytes());
            }
            files.extend(0u16.to_le_bytes());
        }
        files.extend(0u16.to_le_bytes());
        let _clipboard =
            Clipboard::new_attempts_for(hwnd as *mut _, 3).map_err(|error| error.to_string())?;
        let effect = if cut {
            DROP_EFFECT_MOVE
        } else {
            DROP_EFFECT_COPY
        };
        raw::set(CF_HDROP, &files).map_err(|error| error.to_string())?;
        if let Err(error) = raw::set_without_clear(preferred.get(), &effect.to_le_bytes()) {
            let _ = raw::empty();
            return Err(error.to_string());
        }
        Ok(())
    }

    pub fn read(&mut self) -> Result<Option<FileList>, String> {
        let _clipboard = Clipboard::new_attempts(3).map_err(|error| error.to_string())?;
        if !raw::is_format_avail(CF_HDROP) {
            return Ok(None);
        }
        let mut paths = Vec::new();
        raw::get_file_list_path(&mut paths).map_err(|error| error.to_string())?;
        let mut effect = Vec::new();
        let cut = raw::register_format("Preferred DropEffect").is_some_and(|format| {
            raw::is_format_avail(format.get())
                && raw::get_vec(format.get(), &mut effect).is_ok()
                && effect.get(..4) == Some(DROP_EFFECT_MOVE.to_le_bytes().as_slice())
        });
        let revision = raw::seq_num().map(|sequence| sequence.to_string());
        FileList::new(paths, cut, revision)
    }
}
