//! coucou-hook — the relay Claude Code runs on every hook event.
//!
//! Reads the hook JSON on stdin, adds a little terminal context, and hands it to
//! Coucou over the named pipe `\\.\pipe\coucou-<sid>`.
//!
//! Hard rule (docs/CLAUDE.md): **never block Claude Code.**
//! * If the pipe does not exist — Coucou is closed — we exit 0 immediately with
//!   nothing on stdout, and the session carries on untouched.
//! * Every step runs under a deadline enforced by the main thread, so a pipe that
//!   accepts the connection and then stops reading cannot wedge the session
//!   either: we abandon the worker and exit.
//! * Only `PermissionRequest` waits for an answer, because approving from the
//!   island is the whole point. No answer means empty stdout, and Claude Code
//!   asks in the terminal exactly as if Coucou were not installed. The same
//!   wait answers an `AskUserQuestion` from the island.
//! * `Stop` also listens, but only for as long as the fire-and-forget budget:
//!   Coucou answers at once, with the next prompt you queued on the island or
//!   with nothing, and nothing lets the turn end as usual.
//!
//! Usage: `coucou-hook <EventName>` (the name is also read from the JSON).
//!
//! `coucou-hook inject --pid <pid> --started <time>` is Coucou's, not Claude
//! Code's: it types the prompt on stdin into that session's terminal and presses
//! Enter. Coucou uses it to hand a prompt queued on the iPhone to a session that
//! has finished its turn and would otherwise never get another Stop.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Budget for getting a pipe connection. Beyond this Claude Code wins, always.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
/// Whole-run budget for an event nobody waits on: connect and write, no more.
const FIRE_AND_FORGET_BUDGET: Duration = Duration::from_secs(2);
/// How long a permission prompt may stay on screen before the terminal takes over.
const DECISION_BUDGET: Duration = Duration::from_secs(110);

/// `ERROR_PIPE_BUSY` — every instance is serving someone else right now. This is
/// the one error worth retrying: the server exists and a slot will free up.
const ERROR_PIPE_BUSY: i32 = 231;

/// Fields that are pointless to forward and can be enormous (a whole file read,
/// a full command output). The island never shows them.
const DROPPED_FIELDS: &[&str] = &["tool_response", "transcript_path"];
/// Longest string forwarded for any single field; the island truncates to far
/// less than this anyway.
const MAX_FIELD_LEN: usize = 2_000;

mod win;

/// `\\.\pipe\coucou-<sid>`. The SID keeps two accounts on the same machine from
/// ever meeting on the same pipe; the name falls back to the user name only if
/// the SID cannot be read at all, which should not happen.
fn pipe_path() -> String {
    let key = win::current_user_sid()
        .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    format!(r"\\.\pipe\coucou-{key}")
}

/// Opens the pipe. Retries only while the server is busy: any other error means
/// there is nothing to talk to, and waiting would only delay Claude Code.
fn connect() -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    let path = pipe_path();
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle());
                // Somebody else's server on our pipe name gets nothing from us.
                return win::pipe_server_is_same_user(handle).then_some(file);
            }
            Err(err) => {
                if err.raw_os_error() != Some(ERROR_PIPE_BUSY) || Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

/// Set by Coucou on the `claude -p` runs behind its own chats. Those are not the
/// user's sessions: they must not show up on the island, chime, or ask for
/// permission there.
const INTERNAL_ENV: &str = "COUCOU_INTERNAL";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("inject") {
        std::process::exit(inject());
    }
    if std::env::var_os(INTERNAL_ENV).is_some() {
        // Drain stdin so Claude Code never sees a broken pipe.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    }
    let Some((payload, event, ask_input)) = read_event() else { std::process::exit(0) };

    let waits_for_answer = event == "PermissionRequest" || event == "Stop";
    let budget = if event == "PermissionRequest" { DECISION_BUDGET } else { FIRE_AND_FORGET_BUDGET };

    // The worker owns every blocking call. If it overruns the budget we simply
    // stop listening and exit: the process dying takes the pipe handle with it.
    // (No catch_unwind here — the release profile is panic = "abort", so it would
    // be dead code. `talk` is written to have nothing to panic on instead.)
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let _ = tx.send(talk(&payload, waits_for_answer));
    });

    if let Ok(Some(decision)) = rx.recv_timeout(budget) {
        if let Some(json) = decision_json(&event, &decision, ask_input.as_ref()) {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{json}");
            let _ = out.flush();
        }
    }
    // Nothing printed: Claude Code asks in the terminal, as if we were not here.
    std::process::exit(0);
}

/// `inject --pid <pid> --started <time>`, prompt on stdin. Exit code 0 when the
/// prompt was typed, 1 otherwise, with the reason on stderr for Coucou's log.
fn inject() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let (Some(pid), Some(started)) = (value("--pid"), value("--started")) else {
        eprintln!("usage: coucou-hook inject --pid <pid> --started <time>");
        return 1;
    };
    let mut prompt = String::new();
    if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
        eprintln!("no prompt on stdin");
        return 1;
    }
    match win::inject(pid as u32, started, prompt.trim()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

/// The documented PermissionRequest or Stop output. Anything we do not
/// recognise prints nothing at all rather than guessing — silence is the safe answer.
/// See https://code.claude.com/docs/en/hooks
///
/// `answers {"<question>": "<label>"}` is the island answering an
/// AskUserQuestion; `ask_input` is that tool's input as Claude Code sent it.
/// `continue "<prompt>"` is the next queued prompt, and only ever answers a Stop.
fn decision_json(event: &str, decision: &str, ask_input: Option<&serde_json::Value>) -> Option<String> {
    let decision = decision.trim();
    if event == "Stop" {
        return continue_json(decision.strip_prefix("continue ")?);
    }
    if let Some(raw) = decision.strip_prefix("answers ") {
        return answer_json(raw, ask_input?);
    }
    let behavior = match decision {
        // "always" still answers a plain allow; remembering it is the island's
        // business, not Claude Code's.
        "allow" | "always" => r#"{"behavior":"allow"}"#.to_string(),
        "deny" => r#"{"behavior":"deny","message":"Denied from Coucou"}"#.to_string(),
        _ => return None,
    };
    Some(format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{behavior}}}}}"#
    ))
}

/// AskUserQuestion answered from the island: allow it, with the choices added
/// to its own input as `answers: {question text: label}` — where Claude Code
/// reads them. Every question must be answered, and only questions that are in
/// the input count; otherwise nothing is printed and the terminal asks.
fn answer_json(raw: &str, input: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let given: serde_json::Map<String, Value> = serde_json::from_str(raw).ok()?;
    let asked: Vec<&str> = input
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|q| q.get("question")?.as_str())
        .collect();
    let mut answers = serde_json::Map::new();
    for (question, answer) in given {
        let Some(answer) = answer.as_str().filter(|a| !a.is_empty()) else { continue };
        if asked.contains(&question.as_str()) {
            answers.insert(question, Value::String(answer.to_string()));
        }
    }
    if asked.is_empty() || answers.len() != asked.len() {
        return None;
    }
    let mut updated = input.clone();
    updated.as_object_mut()?.insert("answers".into(), Value::Object(answers));
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow", "updatedInput": updated },
            }
        })
        .to_string(),
    )
}

/// A prompt queued on the island: the documented Stop `decision: block` keeps
/// Claude working, and `reason` is what it reads next. The prompt arrives as a
/// JSON string so a multi-line one still travels on one line.
fn continue_json(raw: &str) -> Option<String> {
    let prompt: String = serde_json::from_str(raw).ok()?;
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return None;
    }
    Some(
        serde_json::json!({
            "decision": "block",
            "reason": format!("The user queued their next prompt while you were working. Treat it as their new message:\n\n{prompt}"),
        })
        .to_string(),
    )
}

/// Reads stdin and returns the payload to forward, the event name and, for an
/// AskUserQuestion permission request, the tool's input before any truncation
/// (the answer has to go back with the questions exactly as they came).
fn read_event() -> Option<(String, String, Option<serde_json::Value>)> {
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() || raw.is_empty() {
        return None;
    }
    // Some shells hand us a UTF-8 BOM; serde_json would choke on it.
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        raw.drain(..3);
    }

    let mut payload = serde_json::from_slice::<serde_json::Value>(&raw).ok()?;
    let map = payload.as_object_mut()?;

    // The event name is passed as argv[1] by the hook command; the JSON usually
    // carries it too. Trust argv when the JSON is missing it.
    let arg_event = std::env::args().nth(1).unwrap_or_default();
    let event = map
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or(arg_event);
    map.insert("hook_event_name".into(), serde_json::Value::String(event.clone()));

    for field in DROPPED_FIELDS {
        map.remove(*field);
    }

    let asks = event == "PermissionRequest"
        && map.get("tool_name").and_then(|v| v.as_str()) == Some("AskUserQuestion");
    let ask_input = if asks { map.get("tool_input").cloned() } else { None };

    let cwd_missing = map
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::is_empty)
        .unwrap_or(true);
    if cwd_missing {
        if let Ok(cwd) = std::env::current_dir() {
            map.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
        }
    }

    // Which terminal the session runs in. Unlike macOS, Coucou on Windows accepts
    // events from every terminal, so this is context only — never a filter.
    for (key, var) in [
        ("term_program", "TERM_PROGRAM"),
        ("wt_session", "WT_SESSION"),
        ("term_session_id", "TERM_SESSION_ID"),
        ("session_pid", "CLAUDE_CODE_SSE_PORT"),
        // Warp's deep link to this exact tab and pane: `warp://session/<id>`.
        ("warp_focus_url", "WARP_FOCUS_URL"),
    ] {
        if !map.contains_key(key) {
            let value = std::env::var(var).unwrap_or_default();
            map.insert(key.into(), serde_json::Value::String(value));
        }
    }

    // The Claude Code process behind this session, so Coucou can type a queued
    // prompt into its terminal once the turn is over (see `inject`).
    if let Some((pid, started)) = win::claude_process() {
        map.insert("coucou_claude_pid".into(), serde_json::json!(pid));
        map.insert("coucou_claude_started".into(), serde_json::json!(started));
    }

    // The repo (or worktree) the session works in, whatever subfolder it is in
    // right now: the island names the session after it.
    let root = map
        .get("cwd")
        .and_then(|v| v.as_str())
        .and_then(|cwd| repo_root_name(std::path::Path::new(cwd)));
    if let Some(root) = root {
        map.insert("coucou_root".into(), serde_json::Value::String(root));
    }

    // Counted before truncation: the island only ever sees cut-down strings, and
    // a long Write would otherwise report a fraction of its lines.
    if event == "PostToolUse" {
        if let Some((added, removed)) = edit_line_counts(map) {
            map.insert(
                "coucou_lines".into(),
                serde_json::json!({ "added": added, "removed": removed }),
            );
        }
    }

    truncate_strings(&mut payload);

    let mut line = payload.to_string();
    line.push('\n');
    Some((line, event, ask_input))
}

/// Caps every string in the payload. A single Write can carry a whole file.
/// Name of the nearest folder holding a `.git` — a directory in a repo, a file
/// in a worktree, so a worktree is named after itself. A handful of stats at most.
fn repo_root_name(cwd: &std::path::Path) -> Option<String> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
}

/// Lines an edit added and removed, for the island's end-of-turn summary.
/// None for anything that isn't a file edit.
fn edit_line_counts(map: &serde_json::Map<String, serde_json::Value>) -> Option<(usize, usize)> {
    use serde_json::Value;
    let tool = map.get("tool_name")?.as_str()?;
    let input = map.get("tool_input")?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let pairs: Vec<(String, String)> = match tool {
        "Write" => vec![(String::new(), text(input, "content"))],
        "Edit" => vec![(text(input, "old_string"), text(input, "new_string"))],
        "NotebookEdit" => vec![(String::new(), text(input, "new_source"))],
        "MultiEdit" => input
            .get("edits")?
            .as_array()?
            .iter()
            .map(|e| (text(e, "old_string"), text(e, "new_string")))
            .collect(),
        _ => return None,
    };
    Some(pairs.iter().fold((0, 0), |(a, r), (old, new)| {
        let (da, dr) = line_diff(old, new);
        (a + da, r + dr)
    }))
}

/// A multiset line diff: a line present on both sides cancels out, so the
/// context an Edit carries around its change isn't counted as churn.
fn line_diff(old: &str, new: &str) -> (usize, usize) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in old.lines() {
        *counts.entry(line).or_default() += 1;
    }
    let mut added = 0;
    for line in new.lines() {
        match counts.get_mut(line) {
            Some(c) if *c > 0 => *c -= 1,
            _ => added += 1,
        }
    }
    (added, counts.values().sum())
}

fn truncate_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > MAX_FIELD_LEN {
                // Cut on a char boundary; a lone byte index can split UTF-8.
                let mut end = MAX_FIELD_LEN;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push('…');
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        serde_json::Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

/// Connect, send, and — for a permission request or a Stop — wait for the island's word.
fn talk(payload: &str, waits_for_answer: bool) -> Option<String> {
    let mut pipe = connect()?;

    if pipe.write_all(payload.as_bytes()).is_err() {
        return None;
    }
    let _ = pipe.flush();

    if !waits_for_answer {
        return None;
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let answer = String::from_utf8_lossy(&buf).trim().to_string();
    (!answer.is_empty()).then_some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_json_matches_the_documented_shape() {
        assert_eq!(
            decision_json("PermissionRequest", "allow", None).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert_eq!(
            decision_json("PermissionRequest", "deny", None).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from Coucou"}}}"#
        );
        // "always" is an island concept; Claude Code just gets an allow.
        assert!(decision_json("PermissionRequest", "always", None).unwrap().contains(r#""behavior":"allow""#));
    }

    #[test]
    fn anything_unrecognised_prints_nothing() {
        assert!(decision_json("PermissionRequest", "", None).is_none());
        assert!(decision_json("PermissionRequest", "maybe", None).is_none());
        // The shape the app used to send must not be mistaken for a decision.
        assert!(decision_json("PermissionRequest", r#"{"permissionDecision":"allow"}"#, None).is_none());
        // Answers without the question they answer go nowhere.
        assert!(decision_json("PermissionRequest", r#"answers {"Color?":"Rojo"}"#, None).is_none());
    }

    #[test]
    fn answers_go_back_inside_the_tool_input() {
        let input = serde_json::json!({
            "questions": [
                { "question": "Color?", "header": "Color", "multiSelect": false,
                  "options": [{ "label": "Rojo" }, { "label": "Azul" }] },
                { "question": "Size?", "header": "Size", "multiSelect": true,
                  "options": [{ "label": "S" }, { "label": "M" }] }
            ]
        });
        let out = decision_json("PermissionRequest", r#"answers {"Color?":"Rojo","Size?":"S, M"}"#, Some(&input)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let d = &v["hookSpecificOutput"]["decision"];
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"]["questions"], input["questions"]);
        assert_eq!(d["updatedInput"]["answers"]["Color?"], "Rojo");
        assert_eq!(d["updatedInput"]["answers"]["Size?"], "S, M");
        // A question left unanswered hands the whole thing to the terminal.
        assert!(decision_json("PermissionRequest", r#"answers {"Color?":"Rojo"}"#, Some(&input)).is_none());
        // So does an answer to a question that was never asked.
        assert!(decision_json("PermissionRequest", r#"answers {"Color?":"Rojo","Other?":"x"}"#, Some(&input)).is_none());
    }

    #[test]
    fn a_queued_prompt_keeps_the_turn_going() {
        let out = decision_json("Stop", r#"continue "corre los tests\ny arréglalos""#, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["decision"], "block");
        assert!(v["reason"].as_str().unwrap().ends_with("corre los tests\ny arréglalos"));
        // Nothing queued, or something that isn't a prompt: the turn just ends.
        assert!(decision_json("Stop", "", None).is_none());
        assert!(decision_json("Stop", r#"continue "  ""#, None).is_none());
        assert!(decision_json("Stop", "allow", None).is_none());
        // A permission request never mistakes a prompt for an answer.
        assert!(decision_json("PermissionRequest", r#"continue "hola""#, None).is_none());
    }

    #[test]
    fn sessions_are_named_after_the_repo_root() {
        let tmp = std::env::temp_dir().join(format!("coucou-root-{}", std::process::id()));
        let sub = tmp.join("myrepo").join("windows").join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(tmp.join("myrepo").join(".git")).unwrap();
        assert_eq!(repo_root_name(&sub).as_deref(), Some("myrepo"));
        // A worktree's .git is a file, and the worktree wins over the main repo.
        let wt = tmp.join("myrepo").join(".claude").join("worktrees").join("cvj-ver-doc");
        std::fs::create_dir_all(wt.join("windows")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(repo_root_name(&wt.join("windows")).as_deref(), Some("cvj-ver-doc"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn edits_count_only_the_lines_that_changed() {
        let edit = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": { "old_string": "a
b
c", "new_string": "a
B
c
d" }
        });
        assert_eq!(edit_line_counts(edit.as_object().unwrap()), Some((2, 1)));
        let write = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "content": "x
y
" }
        });
        assert_eq!(edit_line_counts(write.as_object().unwrap()), Some((2, 0)));
        let bash = serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": "ls" } });
        assert_eq!(edit_line_counts(bash.as_object().unwrap()), None);
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let mut v = serde_json::json!({ "tool_input": { "content": "é".repeat(4000) } });
        truncate_strings(&mut v);
        let s = v["tool_input"]["content"].as_str().unwrap();
        assert!(s.len() <= MAX_FIELD_LEN + 4);
        assert!(s.ends_with('…'));
    }
}
