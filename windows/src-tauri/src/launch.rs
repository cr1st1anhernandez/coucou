// "launch": the iPhone opens a new Claude Code session on the PC (contract v1.1).
//
// The phone only ever picks a folder from `folders()` — the direct subfolders
// of the roots set in the settings, plus the folders sessions have started in —
// and only one Claude Code already trusts. Coucou builds the command itself:
// `claude.exe`, plus `--dangerously-skip-permissions` when the phone asks for it
// *and* the setting "Permitir que el iPhone abra sesiones sin permisos" is on.
//
// The terminal:
//   * Warp, when installed: `warp://action/new_tab?path=<cwd>` opens a tab, we
//     spot that tab's shell (`new_warp_tab_shell`), wait for Warp to finish
//     setting it up, and type the command into its console (the same keystroke
//     injection as "wake", pipe.rs).
//   * Windows Terminal otherwise: `wt.exe -w 0 nt -d <cwd> <claude.exe> [flag]`.
//
// The session counts as started when its SessionStart arrives from that folder,
// from a Claude Code process that wasn't running before (under the new Warp
// shell, for Warp). Then the phones hear `{ t: "launch", status: "started" }`,
// and the prompt, if any, is typed into it. 45 s without one → `timeout`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

use crate::log;

/// No SessionStart by then: the launch failed.
const START_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the new Warp tab gets to show its shell.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
/// A launch per phone at most this often.
const LAUNCH_SPACING: Duration = Duration::from_secs(10);
/// Between SessionStart and Claude Code's prompt being ready for keystrokes.
const PROMPT_DELAY: Duration = Duration::from_millis(1_500);
/// The session reaches the phones' list with the island's next publish.
const LISTED_TIMEOUT: Duration = Duration::from_secs(5);
/// Shells a Warp tab can run, and how each is told to start Claude Code.
const SHELLS: &[&str] = &["powershell.exe", "pwsh.exe", "cmd.exe"];
/// A new Warp tab's shell is bootstrapped (Warp's own setup line has run)
/// about 4 s after it opens; this leaves room for a slow machine.
const SHELL_READY: Duration = Duration::from_secs(20);

// ── Folders ───────────────────────────────────────────────────────────────────

/// PhoneFolder in the API contract.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneFolder {
    pub path: String,
    pub name: String,
    pub trusted: bool,
    pub last_used_at: Option<i64>,
}

/// Folders a session has started in, with when: `{ "<path>": epoch ms }`.
fn recent_path() -> PathBuf {
    crate::settings::config_dir().join("recent-folders.json")
}

fn load_recent() -> HashMap<String, i64> {
    std::fs::read(recent_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// A session started in `cwd` (SessionStart): it's a recent folder now.
pub fn note_folder(cwd: &str) {
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return;
    }
    let _guard = recent_lock().lock().unwrap();
    let mut recent = load_recent();
    recent.retain(|path, _| key(path) != key(cwd));
    recent.insert(display(cwd), crate::phone::now_ms());
    let Ok(bytes) = serde_json::to_vec_pretty(&recent) else { return };
    let path = recent_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
        log::line("could not save recent-folders.json");
    }
}

fn recent_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// How two spellings of one folder compare: `/`, no trailing slash, any case.
fn key(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// How a folder is shown and sent back: Windows separators, no trailing one.
fn display(path: &str) -> String {
    let p = path.replace('/', "\\");
    let trimmed = p.trim_end_matches('\\');
    // `C:\` keeps its slash.
    if trimmed.ends_with(':') { format!("{trimmed}\\") } else { trimmed.to_string() }
}

/// Folders Claude Code trusts: `projects[path].hasTrustDialogAccepted` in
/// ~/.claude.json, folder by folder (a trusted parent doesn't count).
fn trusted_folders() -> HashSet<String> {
    let Some(home) = std::env::var_os("USERPROFILE") else { return HashSet::new() };
    let Ok(bytes) = std::fs::read(PathBuf::from(home).join(".claude.json")) else { return HashSet::new() };
    let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else { return HashSet::new() };
    doc.get("projects")
        .and_then(Value::as_object)
        .map(|projects| {
            projects
                .iter()
                .filter(|(_, p)| p.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true))
                .map(|(path, _)| key(path))
                .collect()
        })
        .unwrap_or_default()
}

/// The roots whose subfolders the phone may open, from the settings.
pub fn default_roots() -> Vec<String> {
    std::env::var_os("USERPROFILE")
        .map(|home| vec![PathBuf::from(home).join("Projects").to_string_lossy().into_owned()])
        .unwrap_or_default()
}

/// `GET /api/folders`: the roots' direct subfolders plus the recent folders,
/// each once, only those that exist.
pub fn folders(app: &AppHandle) -> Vec<PhoneFolder> {
    let roots = app.state::<crate::Shared>().settings.lock().unwrap().phone_launch_roots.clone();
    let trusted = trusted_folders();
    let recent = load_recent();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |path: String, last_used_at: Option<i64>| {
        if !seen.insert(key(&path)) {
            return;
        }
        let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
        out.push(PhoneFolder { trusted: trusted.contains(&key(&path)), name, path, last_used_at });
    };
    let recent_at = |path: &str| recent.iter().find(|(p, _)| key(p) == key(path)).map(|(_, at)| *at);
    for (path, at) in &recent {
        if Path::new(path).is_dir() {
            add(display(path), Some(*at));
        }
    }
    for root in roots.iter().filter(|r| !r.trim().is_empty()) {
        let Ok(entries) = std::fs::read_dir(root.trim()) else { continue };
        for entry in entries.flatten() {
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if hidden || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = display(&entry.path().to_string_lossy());
            let at = recent_at(&path);
            add(path, at);
        }
    }
    out
}

// ── Processes ─────────────────────────────────────────────────────────────────

/// `pid → (parent pid, exe name)` for every process on the machine.
fn process_table() -> HashMap<u32, (u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut table = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return table };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let exe = &entry.szExeFile;
            let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
            table.insert(entry.th32ProcessID, (entry.th32ParentProcessID, String::from_utf16_lossy(&exe[..len])));
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    table
}

/// When a process started (FILETIME), the identity `coucou-hook inject` checks.
fn process_started(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        let ok = GetProcessTimes(process, created, exited, kernel, user).is_ok();
        let _ = CloseHandle(process);
        ok.then(|| (times[0].dwHighDateTime as u64) << 32 | times[0].dwLowDateTime as u64)
    }
}

fn named(table: &HashMap<u32, (u32, String)>, names: &[&str]) -> HashSet<u32> {
    table
        .iter()
        .filter(|(_, (_, exe))| names.iter().any(|n| exe.eq_ignore_ascii_case(n)))
        .map(|(pid, _)| *pid)
        .collect()
}

/// Processes of these names whose parent is a warp.exe.
fn warp_children(table: &HashMap<u32, (u32, String)>, names: &[&str]) -> HashSet<u32> {
    let warps = named(table, &["warp.exe"]);
    named(table, names).into_iter().filter(|pid| warps.contains(&table[pid].0)).collect()
}

/// The shell of a Warp tab that just opened, if it's there yet.
///
/// Warp keeps starting short-lived PowerShells of its own in the background
/// (`powershell -NoProfile -c "…"`: git status, completions, prompt context),
/// a dozen a second while a tab opens, so "a new shell under warp.exe" is not
/// enough. A tab's shell is the one Warp starts interactive: `-NoExit`.
fn new_warp_tab_shell(before: &HashSet<u32>) -> Result<Option<(u32, String, u64)>, &'static str> {
    let table = process_table();
    let tabs: Vec<u32> = warp_children(&table, SHELLS)
        .difference(before)
        .copied()
        .filter(|&pid| command_line(pid).is_some_and(|c| c.to_ascii_lowercase().contains("-noexit")))
        .collect();
    match tabs.as_slice() {
        [] => Ok(None),
        [pid] => Ok(process_started(*pid).map(|at| (*pid, table[pid].1.to_lowercase(), at))),
        // Two tabs at once (the user opened one too): can't tell which is ours,
        // and typing into the wrong one is not an option.
        _ => {
            log::line("phone: launch: two Warp tabs opened at once, not typing into either");
            Err("terminal")
        }
    }
}

/// A process's command line, as it was started.
fn command_line(pid: u32) -> Option<String> {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows::Win32::Foundation::{CloseHandle, UNICODE_STRING};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        // A UNICODE_STRING header followed by the text it points into. u64s
        // keep the buffer aligned for the header.
        let mut buf = vec![0u64; 4096];
        let mut needed = 0u32;
        let status = NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut needed,
        );
        let _ = CloseHandle(process);
        if status.is_err() {
            return None;
        }
        let text = &*(buf.as_ptr() as *const UNICODE_STRING);
        let units = std::slice::from_raw_parts(text.Buffer.0, text.Length as usize / 2);
        Some(String::from_utf16_lossy(units))
    }
}

fn descends_from(table: &HashMap<u32, (u32, String)>, mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..16 {
        let Some((parent, _)) = table.get(&pid) else { return false };
        if *parent == ancestor {
            return true;
        }
        pid = *parent;
    }
    false
}

// ── Launching ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Terminal {
    Warp,
    WindowsTerminal,
}

/// The terminal a launch would use now, if any. `COUCOU_LAUNCH_TERMINAL=wt`
/// (or `warp`) in Coucou's environment forces one, for testing.
pub fn terminal() -> Option<Terminal> {
    let warp = crate::warp::find_exe().is_some();
    let wt = crate::find_on_path("wt").is_some();
    match std::env::var("COUCOU_LAUNCH_TERMINAL").as_deref() {
        Ok("wt") => return wt.then_some(Terminal::WindowsTerminal),
        Ok("warp") => return warp.then_some(Terminal::Warp),
        _ => {}
    }
    if warp {
        Some(Terminal::Warp)
    } else if wt {
        Some(Terminal::WindowsTerminal)
    } else {
        None
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchBody {
    pub cwd: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub skip_permissions: bool,
}

/// Why a launch is refused before it starts; the HTTP status in the contract.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// 400: not a listed, trusted folder, or the prompt is too long.
    BadRequest,
    /// 403: skipping permissions isn't allowed on this PC.
    Forbidden,
    /// 409: neither Warp nor Windows Terminal.
    NoTerminal,
    /// 429: this phone launched less than 10 s ago, or is still launching.
    TooSoon,
}

/// What the session has to show to count as the one we launched.
struct Waiting {
    cwd: String,
    /// Claude Code processes that already existed: not ours.
    before: HashSet<u32>,
    /// The Warp shell we typed into; the session runs under it.
    shell: Option<u32>,
    tx: oneshot::Sender<(String, u32, u64)>,
}

#[derive(Default)]
struct Launches {
    waiting: Vec<Waiting>,
    /// Per phone: still launching, and when it last did.
    by_device: HashMap<String, (bool, Instant)>,
}

fn launches() -> &'static Mutex<Launches> {
    static LAUNCHES: OnceLock<Mutex<Launches>> = OnceLock::new();
    LAUNCHES.get_or_init(Default::default)
}

/// Whether ~/.claude/settings.json skips the "Bypass Permissions mode" warning.
/// Without that, the warning's default answer is "No, exit": a prompt typed
/// into it would close the session.
fn bypass_warning_skipped() -> bool {
    let Ok(text) = std::fs::read(crate::hooks::settings_path()) else { return false };
    serde_json::from_slice::<Value>(&text)
        .ok()
        .and_then(|v| v.get("skipDangerousModePermissionPrompt").and_then(Value::as_bool))
        == Some(true)
}

/// `POST /api/sessions`. Checks everything that can be checked now, then
/// launches in the background; the outcome reaches the phones as `launch`.
pub fn start(app: &AppHandle, device_id: &str, body: LaunchBody, max_prompt: usize) -> Result<String, Refused> {
    let prompt = body.prompt.trim().to_string();
    if prompt.chars().count() > max_prompt {
        return Err(Refused::BadRequest);
    }
    let folder = folders(app).into_iter().find(|f| f.path == body.cwd).ok_or(Refused::BadRequest)?;
    if !folder.trusted {
        return Err(Refused::BadRequest);
    }
    if body.skip_permissions {
        let allowed = app.state::<crate::Shared>().settings.lock().unwrap().phone_skip_permissions;
        if !allowed || !bypass_warning_skipped() {
            return Err(Refused::Forbidden);
        }
    }
    let terminal = terminal().ok_or(Refused::NoTerminal)?;
    {
        let mut state = launches().lock().unwrap();
        if let Some((busy, last)) = state.by_device.get(device_id) {
            if *busy || last.elapsed() < LAUNCH_SPACING {
                return Err(Refused::TooSoon);
            }
        }
        state.by_device.insert(device_id.to_string(), (true, Instant::now()));
    }

    let launch_id = crate::phone::random_id();
    let (app, id, device) = (app.clone(), launch_id.clone(), device_id.to_string());
    tauri::async_runtime::spawn(async move {
        let outcome = run(&app, &id, terminal, &folder.path, &prompt, body.skip_permissions).await;
        let result = match &outcome {
            Ok(_) => "started".to_string(),
            Err(reason) => format!("failed ({reason})"),
        };
        log::line(format!(
            "phone: launch {id} in {} via {terminal:?}{} — {result}",
            folder.path,
            if body.skip_permissions { " without permissions" } else { "" },
        ));
        let message = match outcome {
            Ok(session_id) => json!({ "t": "launch", "launchId": id, "status": "started", "sessionId": session_id }),
            Err(reason) => json!({ "t": "launch", "launchId": id, "status": "failed", "reason": reason }),
        };
        crate::phone::broadcast(&app, message);
        if let Some(entry) = launches().lock().unwrap().by_device.get_mut(&device) {
            entry.0 = false;
        }
    });
    Ok(launch_id)
}

/// Opens the terminal, waits for the session, types the prompt. The error is
/// the contract's `reason`.
async fn run(
    app: &AppHandle,
    launch_id: &str,
    terminal: Terminal,
    cwd: &str,
    prompt: &str,
    skip: bool,
) -> Result<String, &'static str> {
    let claude = crate::claude_code::find_claude().ok_or("error")?;
    let before = named(&process_table(), &["claude.exe", "node.exe"]);
    let (tx, rx) = oneshot::channel();

    // Registered before Claude Code can possibly start, so its SessionStart
    // can't slip past.
    let wait = |shell: Option<u32>, tx| {
        launches().lock().unwrap().waiting.push(Waiting { cwd: key(cwd), before: before.clone(), shell, tx });
    };
    match terminal {
        Terminal::Warp => {
            let (pid, exe, started) = open_warp_tab(cwd).await?;
            let command = shell_command(&exe, &claude, cwd, skip);
            wait(Some(pid), tx);
            // Typed only once Warp has bootstrapped the shell: Warp types its
            // own setup line into it first, and anything before that would run
            // first and swallow it (claude.exe got it as its first prompt).
            tauri::async_runtime::spawn_blocking(move || crate::pipe::run_inject(pid, started, &command, Some(SHELL_READY)))
                .await
                .map_err(|_| "error")?
                .map_err(|err| {
                    log::line(format!("phone: launch {launch_id}: could not type into the Warp shell: {err}"));
                    "terminal"
                })?;
        }
        Terminal::WindowsTerminal => {
            let mut cmd = std::process::Command::new(crate::find_on_path("wt").ok_or("terminal")?);
            cmd.args(["-w", "0", "nt", "-d", cwd]).arg(&claude);
            if skip {
                cmd.arg("--dangerously-skip-permissions");
            }
            wait(None, tx);
            cmd.spawn().map_err(|_| "terminal")?;
        }
    }

    let (session_id, pid, started) = match tokio::time::timeout(START_TIMEOUT, rx).await {
        Ok(Ok(found)) => found,
        _ => {
            launches().lock().unwrap().waiting.retain(|w| !w.tx.is_closed());
            return Err("timeout");
        }
    };

    // `started` goes out once the phones can find the session in `sessions`.
    let deadline = Instant::now() + LISTED_TIMEOUT;
    while !crate::phone::has_session(app, &session_id) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !prompt.is_empty() {
        let (session_id, prompt) = (session_id.clone(), prompt.to_string());
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(PROMPT_DELAY).await;
            let typed = tauri::async_runtime::spawn_blocking(move || crate::pipe::run_inject(pid, started, &prompt, None))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            match typed {
                Ok(()) => log::line(format!("phone: first prompt typed into {session_id}")),
                Err(err) => log::line(format!("phone: could not type the first prompt into {session_id}: {err}")),
            }
        });
    }
    Ok(session_id)
}

/// Opens a Warp tab in `cwd` and returns its shell: `(pid, exe, started)`.
async fn open_warp_tab(cwd: &str) -> Result<(u32, String, u64), &'static str> {
    let before: HashSet<u32> = process_table().into_keys().collect();
    let url = format!("warp://action/new_tab?path={}", percent_encode(cwd));
    use std::os::windows::process::CommandExt;
    std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .creation_flags(crate::CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| "terminal")?;

    let deadline = Instant::now() + SHELL_TIMEOUT;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(shell) = new_warp_tab_shell(&before)? {
            return Ok(shell);
        }
    }
    log::line("phone: launch: no new Warp tab within 10 s");
    Err("terminal")
}

/// The line typed into the new tab's shell. Only Coucou's own pieces go in it:
/// a listed folder and the path of claude.exe, quoted for that shell.
fn shell_command(shell: &str, claude: &Path, cwd: &str, skip: bool) -> String {
    let flag = if skip { " --dangerously-skip-permissions" } else { "" };
    let exe = claude.to_string_lossy();
    if shell == "cmd.exe" {
        format!("\"{exe}\"{flag}")
    } else {
        // PowerShell: single quotes are literal; a quote inside doubles.
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        format!("Set-Location -LiteralPath {}; & {}{flag}", q(cwd), q(&exe))
    }
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// pipe.rs, on every SessionStart: is this the session a launch waits for?
pub fn session_started(session_id: &str, cwd: &str, pid: Option<(u32, u64)>) {
    let mut state = launches().lock().unwrap();
    // Launches that already gave up.
    state.waiting.retain(|w| !w.tx.is_closed());
    if state.waiting.is_empty() {
        return;
    }
    let Some((pid, started)) = pid else { return };
    let table = process_table();
    let found = state.waiting.iter().position(|w| {
        w.cwd == key(cwd)
            && !w.before.contains(&pid)
            && w.shell.map_or(true, |shell| descends_from(&table, pid, shell))
    });
    if let Some(i) = found {
        let w = state.waiting.remove(i);
        let _ = w.tx.send((session_id.to_string(), pid, started));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_keys_ignore_separators_and_case() {
        assert_eq!(key(r"C:\Users\dev\Projects\Coucou\"), key("c:/users/dev/projects/coucou"));
        assert_eq!(display("C:/Users/dev/Projects/coucou/"), r"C:\Users\dev\Projects\coucou");
        assert_eq!(display("C:/"), r"C:\");
    }

    #[test]
    fn shell_commands_quote_their_paths() {
        let exe = Path::new(r"C:\Users\o'neil\.local\bin\claude.exe");
        assert_eq!(
            shell_command("powershell.exe", exe, r"C:\p\it's", true),
            r"Set-Location -LiteralPath 'C:\p\it''s'; & 'C:\Users\o''neil\.local\bin\claude.exe' --dangerously-skip-permissions",
        );
        assert_eq!(shell_command("cmd.exe", exe, r"C:\p", false), r#""C:\Users\o'neil\.local\bin\claude.exe""#);
    }

    #[test]
    fn warp_url_path_is_encoded() {
        assert_eq!(percent_encode(r"C:\Users\dev\Mis cosas"), "C%3A%5CUsers%5Cdev%5CMis%20cosas");
    }
}
