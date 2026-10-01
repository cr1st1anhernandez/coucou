// "Open terminal" with Warp: bring the Warp window that is already open to the
// front, exactly as it was left (same tab, same Claude Code session). Only when
// Warp isn't running is it launched. No new tab, no new window.

use std::path::PathBuf;
use std::process::Command;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
};

/// Focuses the open Warp window, or launches Warp. False when Warp isn't installed.
pub fn focus_or_launch(path: Option<&str>) -> bool {
    if let Some(hwnd) = find_window() {
        focus(hwnd);
        return true;
    }
    let Some(exe) = find_exe() else { return false };
    let mut cmd = Command::new(exe);
    if let Some(p) = path.filter(|p| !p.is_empty()) {
        cmd.current_dir(p);
    }
    cmd.spawn().is_ok()
}

/// The default per-user install, then %PATH%.
fn find_exe() -> Option<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let exe = PathBuf::from(local).join(r"Programs\Warp\warp.exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    crate::find_on_path("warp")
}

/// EnumWindows walks top-level windows in Z-order, so the first hit is the Warp
/// window used most recently.
fn find_window() -> Option<HWND> {
    let mut found = HWND::default();
    unsafe {
        let _ = EnumWindows(Some(match_warp), LPARAM(&mut found as *mut HWND as isize));
    }
    (!found.is_invalid()).then_some(found)
}

unsafe extern "system" fn match_warp(hwnd: HWND, out: LPARAM) -> BOOL {
    // Visible (minimised still counts) and not owned: a real app window, not a
    // tooltip or a hidden helper.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
    if visible && !owned && is_warp_process(hwnd) {
        unsafe { *(out.0 as *mut HWND) = hwnd };
        return false.into();
    }
    true.into()
}

fn is_warp_process(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        if ok.is_err() {
            return false;
        }
        let image = String::from_utf16_lossy(&buf[..len as usize]);
        image.rsplit('\\').next().is_some_and(|n| n.eq_ignore_ascii_case("warp.exe"))
    }
}

fn focus(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }
        // The island never takes focus, so Windows may refuse to hand the
        // foreground to another app. A synthetic Alt tap counts as fresh input
        // and lifts that lock; it's the usual workaround.
        tap_alt();
        let _ = SetForegroundWindow(hwnd);
    }
}

fn tap_alt() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VK_MENU, dwFlags: flags, ..Default::default() },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}
