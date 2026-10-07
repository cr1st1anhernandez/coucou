//! The little bit of Win32 the relay needs: who we are, who is on the other
//! end of the pipe, and which Claude Code process we belong to.
//!
//! Named pipes live in a machine-wide namespace, so `\\.\pipe\coucou-<name>` can
//! be created by *any* account that gets there first. Two defences, both cheap:
//! the pipe name carries our SID, and once connected we check the server process
//! really belongs to us before sending anything.

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, LocalFree, HLOCAL,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleMode, GetConsoleTitleW, WriteConsoleInputW, CONSOLE_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
    KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The SID of the account this process runs as, as `S-1-5-21-…`.
pub fn current_user_sid() -> Option<String> {
    unsafe { token_sid(GetCurrentProcess()) }
}

/// True when the process serving `handle` runs as the same user we do.
///
/// A failure to answer is treated as "not ours": refusing to talk to a pipe we
/// cannot vouch for costs one hook event, while trusting it could hand another
/// account on this machine the contents of every tool call.
pub fn pipe_server_is_same_user(handle: HANDLE) -> bool {
    let Some(mine) = current_user_sid() else { return false };
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeServerProcessId(handle, &mut pid).is_err() || pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let theirs = token_sid(process);
        let _ = CloseHandle(process);
        theirs.as_deref() == Some(mine.as_str())
    }
}

/// The user SID behind a process handle. `process` is borrowed, never closed.
unsafe fn token_sid(process: HANDLE) -> Option<String> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

    // First call sizes the buffer, second fills it.
    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = GetTokenInformation(
        token,
        TokenUser,
        Some(buf.as_mut_ptr().cast()),
        needed,
        &mut needed,
    )
    .is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return None;
    }

    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut text = PWSTR::null();
    ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
    let sid = text.to_string().ok();
    let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
    sid
}

// ── The session's own process, and typing into its console ────────────────────

/// Process names that run Claude Code itself: the native build, or the npm one.
const CLAUDE_EXES: &[&str] = &["claude.exe", "node.exe"];

/// The Claude Code process that ran this hook, as `(pid, creation time)`.
///
/// Claude Code starts hooks through a shell (`bash.exe`, sometimes several
/// deep, or `cmd.exe`), so we walk up the parents to the first `claude.exe` —
/// or `node.exe` for an npm install. The creation time travels with the pid so
/// `inject` can tell that process from a later one that reused its number.
pub fn claude_process() -> Option<(u32, u64)> {
    let procs = process_table()?;
    let mut pid = std::process::id();
    for _ in 0..16 {
        let parent = procs.iter().find(|p| p.0 == pid)?.1;
        let name = &procs.iter().find(|p| p.0 == parent)?.2;
        if CLAUDE_EXES.iter().any(|exe| name.eq_ignore_ascii_case(exe)) {
            return Some((parent, process_started(parent)?));
        }
        pid = parent;
    }
    None
}

/// `(pid, parent pid, exe name)` for every process on the machine.
fn process_table() -> Option<Vec<(u32, u32, String)>> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut table = Vec::new();
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let exe = &entry.szExeFile;
            let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
            table.push((entry.th32ProcessID, entry.th32ParentProcessID, String::from_utf16_lossy(&exe[..len])));
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        Some(table)
    }
}

/// When a process started, as a FILETIME count. None once it is gone.
fn process_started(pid: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        let ok = GetProcessTimes(process, created, exited, kernel, user).is_ok();
        let _ = CloseHandle(process);
        ok.then(|| (times[0].dwHighDateTime as u64) << 32 | times[0].dwLowDateTime as u64)
    }
}

/// Whether `pid` is still the process we were told about, and runs as us.
fn same_process(pid: u32, started: u64) -> bool {
    let Some(mine) = current_user_sid() else { return false };
    if process_started(pid) != Some(started) {
        return false;
    }
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let theirs = token_sid(process);
        let _ = CloseHandle(process);
        theirs.as_deref() == Some(mine.as_str())
    }
}

/// Types `text` into the console of `pid` and presses Enter, as if the user had.
///
/// `AttachConsole` + `WriteConsoleInputW` put key events straight into that
/// console's input buffer: no focus, no clipboard, any terminal behind ConPTY,
/// and it works with the screen locked. It changes this whole process's
/// console, which is why it lives in the relay and not in Coucou.
///
/// A pid that no longer belongs to the same process (Claude Code exited and
/// Windows gave the number to, say, a shell) is refused: typing a prompt there
/// could run it as a command.
///
/// `wait_ready` is for a shell in a Warp tab that just opened: Warp types its
/// own bootstrap line into the shell once it starts, and anything typed before
/// that runs first and swallows it. Once bootstrapped, Warp's prompt hook sets
/// the console title to the current folder; until then it is the shell's own
/// path. So we wait, up to `wait_ready`, for the title to stop ending in `.exe`.
///
/// `clear` first empties Claude Code's input box, so a prompt never lands
/// behind something left half-typed at the PC (see `CLEAR_PRESSES`).
///
/// An empty `text` only presses Enter: Coucou's second try when a typed
/// prompt is still sitting in the input box, unsent.
pub fn inject(
    pid: u32,
    started: u64,
    text: &str,
    wait_ready: Option<std::time::Duration>,
    clear: bool,
) -> Result<(), String> {
    if !same_process(pid, started) {
        return Err(format!("process {pid} is no longer that Claude Code session"));
    }
    // An Enter in the middle would send half the prompt: newlines become spaces.
    let text = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let typed: Vec<INPUT_RECORD> = text
        .encode_utf16()
        .flat_map(|unit| [key(true, unit), key(false, unit)])
        .collect();
    unsafe {
        // Coucou starts us without a console; this is only in case it didn't.
        let _ = FreeConsole();
        AttachConsole(pid).map_err(|e| format!("AttachConsole: {e}"))?;
        if let Some(limit) = wait_ready {
            if !wait_for_title(limit) {
                let _ = FreeConsole();
                return Err("the shell never finished starting".into());
            }
        }
        let result = CreateFileW(
            w!("CONIN$"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| format!("CONIN$: {e}"))
        .and_then(|input| {
            let vt = vt_input(input);
            let enter = enter_for(vt);
            // Only Claude Code itself (VT input) knows Ctrl+U; elsewhere the
            // bare character could end up typed.
            let wipe: Vec<INPUT_RECORD> = if clear && vt {
                (0..CLEAR_PRESSES).flat_map(|_| [key(true, CTRL_U), key(false, CTRL_U)]).collect()
            } else {
                Vec::new()
            };
            let sent = write_input(input, &wipe).and_then(|()| write_input(input, &typed)).and_then(|()| {
                // A beat before Enter, so the text doesn't arrive as one paste
                // that swallows the Enter with it. Claude Code takes longer to
                // digest a long prompt, so the beat grows with it.
                if !typed.is_empty() {
                    std::thread::sleep(enter_delay(typed.len() / 2));
                }
                write_input(input, &enter)
            });
            let _ = CloseHandle(input);
            sent
        });
        let _ = FreeConsole();
        result
    }
}

/// The pause between the last typed character and Enter: 100 ms was enough
/// for a short prompt, but Claude Code ate the Enter after ~150 characters.
fn enter_delay(units: usize) -> std::time::Duration {
    const BASE_MS: u64 = 150;
    const PER_UNIT_MS: u64 = 2;
    const MAX_MS: u64 = 1_500;
    std::time::Duration::from_millis((BASE_MS + PER_UNIT_MS * units as u64).min(MAX_MS))
}

/// Polls the attached console's title until it is no longer a program path.
unsafe fn wait_for_title(limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let mut buf = [0u16; 1024];
        let len = GetConsoleTitleW(&mut buf) as usize;
        let title = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
        if !title.is_empty() && !title.to_ascii_lowercase().ends_with(".exe") {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Ctrl+U: Claude Code deletes the current line of its input box. On an empty
/// box it does nothing (the grey suggested prompt stays a suggestion).
const CTRL_U: u16 = 0x15;
/// One press per line: enough for anything half-typed, multi-line included.
const CLEAR_PRESSES: usize = 8;

/// Whether the console is in VT input mode: Claude Code's is, a shell's own
/// line editor isn't.
unsafe fn vt_input(input: HANDLE) -> bool {
    let mut mode = CONSOLE_MODE(0);
    GetConsoleMode(input, &mut mode).is_ok() && mode.contains(ENABLE_VIRTUAL_TERMINAL_INPUT)
}

/// Enter, the way this console takes it.
///
/// A console in VT input mode (Claude Code itself) gets the bare character CR:
/// in a Warp tab Warp has set up, a VK_RETURN event reaches Claude Code as a
/// sequence it reads as "new line", not "send". A console without it (a
/// shell's own line editor) wants the real VK_RETURN key: PowerShell ignores a
/// bare CR there.
fn enter_for(vt: bool) -> [INPUT_RECORD; 2] {
    const VK_RETURN: u16 = 0x0D;
    const SCAN_RETURN: u16 = 0x1C;
    let (vk, scan) = if vt { (0, 0) } else { (VK_RETURN, SCAN_RETURN) };
    [key_event(true, vk, scan, 13), key_event(false, vk, scan, 13)]
}

/// One key event carrying a UTF-16 unit, with no virtual key behind it.
fn key(down: bool, unit: u16) -> INPUT_RECORD {
    key_event(down, 0, 0, unit)
}

fn key_event(down: bool, vk: u16, scan: u16, unit: u16) -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: down.into(),
                wRepeatCount: 1,
                wVirtualKeyCode: vk,
                wVirtualScanCode: scan,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                dwControlKeyState: 0,
            },
        },
    }
}

/// `WriteConsoleInputW` may take fewer records than offered: loop until done.
fn write_input(input: HANDLE, records: &[INPUT_RECORD]) -> Result<(), String> {
    let mut done = 0;
    while done < records.len() {
        let mut written = 0u32;
        unsafe { WriteConsoleInputW(input, &records[done..], &mut written) }
            .map_err(|e| format!("WriteConsoleInputW: {e}"))?;
        if written == 0 {
            return Err("WriteConsoleInputW wrote nothing".into());
        }
        done += written as usize;
    }
    Ok(())
}
