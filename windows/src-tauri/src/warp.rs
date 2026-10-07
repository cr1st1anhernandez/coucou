// "Open terminal" with Warp: jump to the tab and pane the Claude Code session
// runs in, through the deep link Warp exports as WARP_FOCUS_URL. Without one,
// bring the Warp window that is already open to the front, exactly as it was
// left; only when Warp isn't running is it launched. No new tab, no new window.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_MENU, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
};

/// Release channels Warp registers a URL scheme for.
const FOCUS_SCHEMES: [&str; 4] = ["warp", "warppreview", "warpdev", "warposs"];

/// `warp://session/<32 lowercase hex>`, the shape of WARP_FOCUS_URL. Anything
/// else is refused: the island must not be able to open arbitrary URLs.
pub fn is_focus_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return false };
    let Some(id) = rest.strip_prefix("session/") else { return false };
    FOCUS_SCHEMES.contains(&scheme)
        && id.len() == 32
        && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Switches Warp to the session's own tab. Warp ignores a stale id (a closed
/// tab) and simply comes to the front, which is the old behaviour anyway.
pub fn focus_session(url: &str) -> bool {
    if !is_focus_url(url) {
        return false;
    }
    // Raise the window ourselves first: the deep link is handed to Warp by a
    // helper process, which Windows may not let take the foreground.
    if let Some(hwnd) = find_window() {
        focus(hwnd);
    }
    Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .creation_flags(crate::CREATE_NO_WINDOW)
        .spawn()
        .is_ok()
}

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

/// Brings Warp to the front and presses Ctrl+V in it — never Enter: what was
/// pasted waits there for the user. Pastes only once Warp really is the
/// foreground window, so the keystroke can't land in some other app.
pub fn paste(path: Option<&str>) -> Result<(), String> {
    let Some(hwnd) = find_window() else {
        // A Warp that is only now starting has no prompt to paste into yet.
        return if focus_or_launch(path) {
            Err("Abrí Warp; cuando cargue, pega con Ctrl+V.".into())
        } else {
            Err("No encontré Warp. Pega con Ctrl+V donde quieras.".into())
        };
    };
    focus(hwnd);
    for _ in 0..30 {
        if unsafe { GetForegroundWindow() } == hwnd {
            // Give Warp a beat to put the caret back in its input.
            std::thread::sleep(std::time::Duration::from_millis(120));
            if unsafe { GetForegroundWindow() } != hwnd {
                break;
            }
            press_ctrl_v();
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err("Warp no pasó al frente; pega con Ctrl+V.".into())
}

fn press_ctrl_v() {
    let key = |vk, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    };
    let inputs = [
        key(VK_CONTROL, Default::default()),
        key(VK_V, Default::default()),
        key(VK_V, KEYEVENTF_KEYUP),
        key(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// The default per-user install, then the machine-wide one, then %PATH%.
pub(crate) fn find_exe() -> Option<PathBuf> {
    let installs = [
        ("LOCALAPPDATA", r"Programs\Warp\warp.exe"),
        ("ProgramFiles", r"Warp\warp.exe"),
    ];
    for (var, rel) in installs {
        if let Some(base) = std::env::var_os(var) {
            let exe = PathBuf::from(base).join(rel);
            if exe.is_file() {
                return Some(exe);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_warp_session_links_are_opened() {
        assert!(is_focus_url("warp://session/acc59f11bff545c2a3ac99d4842ad916"));
        assert!(is_focus_url("warppreview://session/acc59f11bff545c2a3ac99d4842ad916"));
        assert!(!is_focus_url("warp://session/ACC59F11BFF545C2A3AC99D4842AD916"));
        assert!(!is_focus_url("warp://session/acc59f11"));
        assert!(!is_focus_url("warp://action/new_tab?path=C:/"));
        assert!(!is_focus_url("https://session/acc59f11bff545c2a3ac99d4842ad916"));
        assert!(!is_focus_url("warp://session/acc59f11bff545c2a3ac99d4842ad916&x"));
    }
}
