use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{os::windows::ffi::OsStrExt, path::PathBuf};
use windows::{
    core::{implement, w, Ref, Result as WinResult, BOOL, HRESULT},
    Win32::{
        Foundation::{self::*, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS},
        System::{
            Com::*,
            DataExchange::RegisterClipboardFormatW,
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            Ole::{
                DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize,
                CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
            },
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
        },
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON},
            Shell::{SHCreateStdEnumFmtEtc, DROPFILES},
            WindowsAndMessaging::{GetAncestor, GetCursorPos, WindowFromPoint, GA_ROOT},
        },
    },
};

#[derive(Default)]
pub struct Platform {
    result: Option<Result<bool, String>>,
}

impl Platform {
    pub fn prepare(&mut self, _: &egui::Context, _: &eframe::Frame) {}

    pub fn poll(&mut self) -> Option<Result<bool, String>> {
        self.result.take()
    }

    pub fn start(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        paths: Vec<PathBuf>,
    ) -> Result<(), String> {
        let handle = frame.window_handle().map_err(|e| e.to_string())?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err(crate::tr_format!(
                "Окно не поддерживает системное перетаскивание",
                "The window does not support native drag and drop"
            ));
        };
        let owner = HWND(handle.hwnd.get() as *mut _);
        let files = drop_files(&paths);
        // SAFETY: the HWND is borrowed from the live eframe window; this call
        // runs on its main STA thread. OLE pumps native events until completion.
        let result = unsafe {
            OleInitialize(None).map_err(|e| e.to_string())?;
            let _ole = OleGuard;
            if GetAsyncKeyState(i32::from(VK_LBUTTON.0)) >= 0 {
                return Err(
                    crate::i18n::text("Перетаскивание отменено: кнопка мыши уже отпущена").into(),
                );
            }
            let preferred = RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16;
            if preferred == 0 {
                return Err(windows::core::Error::from_thread().to_string());
            }
            let data: IDataObject = FileData { files, preferred }.into();
            let source: IDropSource = DropSource { owner }.into();
            // winit captured this press; OLE now owns its continuation/release.
            let _ = ReleaseCapture();
            let mut effect = DROPEFFECT_NONE;
            let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
            if hr == DRAGDROP_S_CANCEL {
                Ok(false)
            } else {
                hr.ok()
                    .map(|()| hr == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY)
                    .map_err(|e| e.to_string())
            }
        };
        self.result = Some(result);
        ctx.request_repaint();
        Ok(())
    }
}

struct OleGuard;
impl Drop for OleGuard {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful OleInitialize, including S_FALSE.
        unsafe { OleUninitialize() }
    }
}

#[implement(IDropSource)]
struct DropSource {
    owner: HWND,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 != 0 {
            return S_OK;
        }
        // Returning to our own window cancels this handoff. Native incoming
        // events must not turn the exported selection into a duplicate job.
        let mut point = POINT::default();
        // SAFETY: valid stack output and a borrowed live owner HWND.
        let own_window = unsafe {
            GetCursorPos(&mut point).is_ok()
                && GetAncestor(WindowFromPoint(point), GA_ROOT) == GetAncestor(self.owner, GA_ROOT)
        };
        if own_window {
            DRAGDROP_S_CANCEL
        } else {
            DRAGDROP_S_DROP
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[implement(IDataObject)]
struct FileData {
    files: Vec<u8>,
    preferred: u16,
}

impl FileData {
    fn formats(&self) -> [FORMATETC; 2] {
        [CF_HDROP.0, self.preferred].map(|cf_format| FORMATETC {
            cfFormat: cf_format,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        })
    }

    fn valid(&self, request: &FORMATETC) -> bool {
        (request.cfFormat == CF_HDROP.0 || request.cfFormat == self.preferred)
            && request.dwAspect == DVASPECT_CONTENT.0
            && request.lindex == -1
            && request.tymed & TYMED_HGLOBAL.0 as u32 != 0
            && request.ptd.is_null()
    }
}

impl IDataObject_Impl for FileData_Impl {
    fn GetData(&self, format: *const FORMATETC) -> WinResult<STGMEDIUM> {
        // SAFETY: OLE supplies a readable FORMATETC; reject null defensively.
        let format =
            unsafe { format.as_ref() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        if !self.valid(format) {
            return Err(DV_E_FORMATETC.into());
        }
        let copy = DROPEFFECT_COPY.0.to_le_bytes();
        let bytes = if format.cfFormat == CF_HDROP.0 {
            &self.files
        } else {
            &copy[..]
        };
        // SAFETY: allocate an independent movable block for each request. OLE
        // owns the successful STGMEDIUM and frees it with ReleaseStgMedium.
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
            let target = GlobalLock(memory);
            if target.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast(), bytes.len());
            let _ = GlobalUnlock(memory);
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: memory },
                ..Default::default()
            })
        }
    }
    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        // SAFETY: same borrowed OLE input as GetData; no allocation or I/O.
        if unsafe { format.as_ref() }.is_some_and(|f| self.valid(f)) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }
    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: the shell copies this temporary format array into its enumerator.
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = vec![0; std::mem::size_of::<DROPFILES>()];
    let offset = bytes.len() as u32;
    bytes[..4].copy_from_slice(&offset.to_le_bytes());
    // DROPFILES has a fixed 20-byte layout on both Win32 and Win64; fWide is BOOL.
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        bytes.extend(
            path.as_os_str()
                .encode_wide()
                .chain([0])
                .flat_map(u16::to_le_bytes),
        );
    }
    bytes.extend([0, 0]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::ReleaseStgMedium;

    #[test]
    fn native_file_data_round_trips_unicode_and_advertises_only_copy() {
        let paths = vec![
            PathBuf::from(r"C:\QA\Привет #100% two words.txt"),
            PathBuf::from(r"C:\QA\folder"),
        ];
        let data: IDataObject = FileData {
            files: drop_files(&paths),
            preferred: 0xc001,
        }
        .into();
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        // SAFETY: COM implementation owns fresh blocks; release each medium.
        unsafe {
            let mut medium = data.GetData(&format).unwrap();
            let h = windows::Win32::UI::Shell::HDROP(medium.u.hGlobal.0);
            let count = windows::Win32::UI::Shell::DragQueryFileW(h, u32::MAX, None);
            assert_eq!(count, 2);
            for (i, path) in paths.iter().enumerate() {
                let mut name = vec![0; 512];
                let len = windows::Win32::UI::Shell::DragQueryFileW(h, i as u32, Some(&mut name));
                assert_eq!(
                    String::from_utf16(&name[..len as usize]).unwrap(),
                    path.to_str().unwrap()
                );
            }
            ReleaseStgMedium(&mut medium);
            let effect = FORMATETC {
                cfFormat: 0xc001,
                ..format
            };
            let mut medium = data.GetData(&effect).unwrap();
            let ptr = GlobalLock(medium.u.hGlobal);
            assert_eq!(
                std::ptr::read_unaligned(ptr.cast::<u32>()),
                DROPEFFECT_COPY.0
            );
            let _ = GlobalUnlock(medium.u.hGlobal);
            ReleaseStgMedium(&mut medium);
            assert!(data
                .QueryGetData(&FORMATETC {
                    cfFormat: 1,
                    ..format
                })
                .is_err());
        }
    }
}
