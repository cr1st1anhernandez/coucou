// Small pieces of the Windows shell the island needs: the file picker behind
// "Suelta tus archivos aquí", the clipboard and "open" for the library, and how long the
// user has been away from the keyboard and mouse.

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, ShellExecuteW, DROPFILES, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// The standard Windows "Open" dialog, owned by the island so it comes up in
/// front of it. Blocks until the user picks a file or cancels; run it off the
/// main thread. None on cancel.
pub fn pick_file(owner: Option<HWND>, title: &str) -> Option<String> {
    unsafe {
        // The dialog is a COM object and wants its own single-threaded apartment.
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let _ = dialog.SetTitle(&HSTRING::from(title));
            dialog.Show(owner).ok()?;
            let item = dialog.GetResult().ok()?;
            let raw: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = raw.to_string().ok();
            CoTaskMemFree(Some(raw.0 as *const _));
            path
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Puts text on the clipboard as CF_UNICODETEXT.
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    set_clipboard(CF_UNICODETEXT.0 as u32, &wide_bytes(&wide))
}

/// Puts files on the clipboard as CF_HDROP, the way Explorer's Copy does: Ctrl+V
/// pastes them as files in Explorer, or as attachments in a mail or a chat.
pub fn set_clipboard_files(paths: &[String]) -> Result<(), String> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.encode_utf16());
        list.push(0);
    }
    list.push(0);
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    let mut bytes = unsafe {
        std::slice::from_raw_parts(&header as *const DROPFILES as *const u8, std::mem::size_of::<DROPFILES>())
    }
    .to_vec();
    bytes.extend(wide_bytes(&list));
    set_clipboard(CF_HDROP.0 as u32, &bytes)
}

fn wide_bytes(wide: &[u16]) -> Vec<u8> {
    wide.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// Replaces the clipboard with `bytes` in `format`.
fn set_clipboard(format: u32, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        // Another app may hold the clipboard for a moment; try a few times.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            return Err("El portapapeles está ocupado. Intenta de nuevo.".into());
        }
        let result = (|| {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(mem) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(mem));
                return Err("No se pudo usar el portapapeles.".to_string());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(mem);
            // On success the clipboard owns the memory; only free it on failure.
            if let Err(e) = SetClipboardData(format, Some(HANDLE(mem.0))) {
                let _ = GlobalFree(Some(mem));
                return Err(e.to_string());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Opens a file with its default app (a .docx in Word), like a double-click.
pub fn open_file(path: &str) -> Result<(), String> {
    let done = unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(path), None, None, SW_SHOWNORMAL)
    };
    // Anything above 32 is success.
    if done.0 as isize > 32 {
        Ok(())
    } else {
        Err("Windows no pudo abrir ese archivo.".into())
    }
}

/// Seconds since the last keyboard or mouse input anywhere on the machine.
pub fn idle_seconds() -> u64 {
    unsafe {
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if !GetLastInputInfo(&mut info).as_bool() {
            return 0;
        }
        // Both are 32-bit tick counts; wrapping_sub keeps it right across the
        // 49-day rollover.
        (GetTickCount().wrapping_sub(info.dwTime) / 1000) as u64
    }
}
