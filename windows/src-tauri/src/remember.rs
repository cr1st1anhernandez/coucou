// Claude Code sessions survive a Coucou restart.
//
// The island only learns about a session from its hook events, so after a
// restart (an update, Windows starting up) a session sitting idle at its prompt
// was invisible: not on the island, not on the iPhone, and nothing could wake
// it until someone typed in it at the PC. So Rust keeps, per session, what
// "wake" needs — its Claude Code process, folder and last known state — plus
// the prompts queued for it, in %APPDATA%\Coucou\sessions.json.
//
// On launch only sessions whose process is still the same one (pid and
// creation time) and that were active in the last 3 hours come back, the same
// age the island drops them at. The file is written when something that
// matters changes (a new session, its process, its state, its queue), not on
// every tool call.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::log;
use crate::pipe::Queue;

/// STALE_MS in sessions.ts: older sessions leave the island anyway.
const MAX_AGE_MS: i64 = 3 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Remembered {
    pid: u32,
    /// Creation time of `pid` (FILETIME): tells it from a later process that
    /// got the same number.
    started: u64,
    cwd: String,
    root: Option<String>,
    focus_url: Option<String>,
    /// The island status the last event left it in.
    status: String,
    last_event_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    sessions: HashMap<String, Remembered>,
    queues: HashMap<String, VecDeque<String>>,
}

#[derive(Default)]
pub struct Memory {
    sessions: Mutex<HashMap<String, Remembered>>,
    /// Held while writing, so the last write always carries the newest state.
    saving: Mutex<()>,
}

/// A session brought back for the island (`sessions_restore`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredSession {
    id: String,
    cwd: String,
    root: Option<String>,
    focus_url: Option<String>,
    status: String,
    last_event_at: i64,
    queue: Vec<String>,
}

fn path() -> PathBuf {
    crate::settings::config_dir().join("sessions.json")
}

/// The sessions and queues still worth having, read once at launch.
pub fn load() -> (Memory, Queue) {
    let file: File = std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now = crate::phone::now_ms();
    let sessions: HashMap<String, Remembered> = file
        .sessions
        .into_iter()
        .filter(|(_, s)| {
            now - s.last_event_at <= MAX_AGE_MS && crate::launch::process_started(s.pid) == Some(s.started)
        })
        .collect();
    let queues = file.queues.into_iter().filter(|(id, q)| sessions.contains_key(id) && !q.is_empty()).collect();
    if !sessions.is_empty() {
        log::line(format!("restored {} session(s) from before the restart", sessions.len()));
    }
    (Memory { sessions: Mutex::new(sessions), saving: Mutex::new(()) }, Queue(Mutex::new(queues)))
}

/// The island status a hook event leaves a session in; None when it doesn't
/// change it (a Notification, a subagent finishing…). `queued`: a Stop that is
/// about to be answered with a queued prompt, so the turn goes on.
fn status_after(event: &str, payload: &Value, queued: bool) -> Option<&'static str> {
    Some(match event {
        "SessionStart" => "idle",
        "Stop" if queued => "working",
        "Stop" => "finished",
        "StopFailure" if payload.get("error_type").and_then(Value::as_str) == Some("rate_limit") => "ratelimit",
        "StopFailure" => "error",
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" => "working",
        _ => return None,
    })
}

/// pipe.rs, on every hook event but SessionEnd.
pub fn note(app: &AppHandle, session_id: &str, event: &str, payload: &Value, queued: bool) {
    let text = |key: &str| payload.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let pid = payload.get("coucou_claude_pid").and_then(Value::as_u64);
    let started = payload.get("coucou_claude_started").and_then(Value::as_u64);
    let now = crate::phone::now_ms();
    let changed = {
        let memory = app.state::<Memory>();
        let mut sessions = memory.sessions.lock().unwrap();
        let entry = match (sessions.get_mut(session_id), pid, started) {
            (Some(entry), _, _) => entry,
            // A relay that doesn't say which process: nothing to wake later.
            (None, Some(pid), Some(started)) => sessions.entry(session_id.to_string()).or_insert(Remembered {
                pid: pid as u32,
                started,
                cwd: String::new(),
                root: None,
                focus_url: None,
                status: "idle".into(),
                last_event_at: now,
            }),
            (None, _, _) => return,
        };
        let before = (entry.pid, entry.started, entry.status.clone(), entry.cwd.clone());
        if let (Some(pid), Some(started)) = (pid, started) {
            entry.pid = pid as u32;
            entry.started = started;
        }
        if let Some(cwd) = text("cwd") {
            entry.cwd = cwd;
        }
        entry.root = text("coucou_root").or(entry.root.take());
        entry.focus_url = text("warp_focus_url").or(entry.focus_url.take());
        if let Some(status) = status_after(event, payload, queued) {
            entry.status = status.into();
        }
        entry.last_event_at = now;
        before != (entry.pid, entry.started, entry.status.clone(), entry.cwd.clone()) || event == "SessionStart"
    };
    if changed {
        save(app);
    }
}

/// The session ended: nothing left to wake.
pub fn forget(app: &AppHandle, session_id: &str) {
    let removed = app.state::<Memory>().sessions.lock().unwrap().remove(session_id).is_some();
    if removed {
        save(app);
    }
}

/// The session's Claude Code process: `(pid, creation time)`.
pub fn terminal(app: &AppHandle, session_id: &str) -> Option<(u32, u64)> {
    let memory = app.state::<Memory>();
    let sessions = memory.sessions.lock().unwrap();
    sessions.get(session_id).map(|s| (s.pid, s.started))
}

/// Writes the file in the background. Called after any change worth keeping,
/// queues included (pipe.rs).
pub fn save(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let memory = app.state::<Memory>();
        let _writing = memory.saving.lock().unwrap();
        let file = File {
            sessions: memory.sessions.lock().unwrap().clone(),
            queues: app.state::<Queue>().0.lock().unwrap().clone(),
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&file) else { return };
        let path = path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
            log::line("could not save sessions.json");
        }
    });
}

/// What the island shows again after a restart, with each session's queue.
pub fn restored(app: &AppHandle) -> Vec<RestoredSession> {
    let queues = app.state::<Queue>().0.lock().unwrap().clone();
    let memory = app.state::<Memory>();
    let sessions = memory.sessions.lock().unwrap();
    sessions
        .iter()
        .filter(|(_, s)| !s.cwd.is_empty())
        .map(|(id, s)| RestoredSession {
            id: id.clone(),
            cwd: s.cwd.clone(),
            root: s.root.clone(),
            focus_url: s.focus_url.clone(),
            status: s.status.clone(),
            last_event_at: s.last_event_at,
            queue: queues.get(id).map(|q| q.iter().cloned().collect()).unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_follow_the_turn() {
        let none = json!({});
        assert_eq!(status_after("SessionStart", &none, false), Some("idle"));
        assert_eq!(status_after("Stop", &none, false), Some("finished"));
        assert_eq!(status_after("Stop", &none, true), Some("working"));
        assert_eq!(status_after("StopFailure", &json!({ "error_type": "rate_limit" }), false), Some("ratelimit"));
        assert_eq!(status_after("StopFailure", &none, false), Some("error"));
        assert_eq!(status_after("PreToolUse", &none, false), Some("working"));
        assert_eq!(status_after("Notification", &none, false), None);
    }
}
