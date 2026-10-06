// Named-pipe server for coucou-hook.
//
// `\\.\pipe\coucou-<sid>` — one instance per connection. Every hook event is
// forwarded to the island as a `hook` event. `PermissionRequest` is the only one
// that keeps its connection open: it waits for the island's decision and writes
// it back on the same pipe, which is how approving from the island works.
//
// Claude Code is never blocked by us. Three things guarantee it:
//   * coucou-hook gives the connection 300 ms and exits cleanly if we are closed;
//   * we only wait for a human once the island has *confirmed* the card is on
//     screen, so a paused island or a webview that is not listening costs a few
//     hundred milliseconds, not two minutes;
//   * whatever happens we drop the connection after the decision timeout, and
//     the terminal takes over.
//
// The iPhone (phone.rs), when it is switched on, hears of every request too. A
// request the island won't show (paused, busy, not listening) normally goes to
// the terminal at once; it keeps waiting only if `phone::can_take` says a phone
// is really there to answer — anything else behaves exactly as before.
//
// `Stop` waits too, but never for a human: we answer on the spot with the next
// prompt queued for that session, `continue "<prompt>"`, or hang up with nothing
// and the turn ends as usual.
//
// What we write back is the bare word `allow` or `deny`, or for a question
// `answers {"<question>": "<label>"}` on one line. Turning that into the
// documented hookSpecificOutput JSON is coucou-hook's job, so the wire format
// Claude Code expects lives in exactly one place.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::island::WINDOW_LABEL;
use crate::log;
use crate::phone::{self, ClosedBy};

/// Slightly under coucou-hook's own 110 s wait, so we always answer first.
const DECISION_TIMEOUT: Duration = Duration::from_secs(108);
/// How long the island gets to say "the card is up". This is the whole of B4:
/// without it, an island that is paused, hidden behind a crashed webview or
/// simply not listening would leave Claude Code staring at a prompt nobody can
/// see for nearly two minutes.
const ACK_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_PAYLOAD: usize = 1 << 20;

/// What the island can say about a permission request.
pub enum Reply {
    /// The card is on screen and a human can act on it.
    Ack,
    /// A human clicked: `allow`, `deny`, or `answers {…}` for a question —
    /// on the island or on the phone.
    Decision(String, ClosedBy),
    /// Nobody can act on it — paused, or another request already holds the card.
    Decline,
}

/// Permission requests the island has been told about.
#[derive(Default)]
pub struct Pending(pub Mutex<HashMap<String, mpsc::Sender<Reply>>>);

/// Prompts queued on the island, per Claude Code session, sent one per Stop.
#[derive(Default)]
pub struct Queue(pub Mutex<HashMap<String, VecDeque<String>>>);

/// Replaces a session's queue with what the island shows. Empty forgets it.
pub fn set_queue(app: &AppHandle, session_id: &str, prompts: Vec<String>) {
    let queue = app.state::<Queue>();
    let mut map = queue.0.lock().unwrap();
    let prompts: VecDeque<String> = prompts.into_iter().filter(|p| !p.trim().is_empty()).collect();
    if prompts.is_empty() {
        map.remove(session_id);
    } else {
        map.insert(session_id.to_string(), prompts);
    }
}

/// The next prompt for a session whose turn just ended, if one is queued.
fn peek_queued(app: &AppHandle, session_id: &str) -> Option<String> {
    let queue = app.state::<Queue>();
    let map = queue.0.lock().unwrap();
    map.get(session_id)?.front().cloned()
}

/// That prompt reached the relay: it leaves the queue. Only if it is still the
/// first one — the island may have removed it in the meantime.
fn drop_queued(app: &AppHandle, session_id: &str, prompt: &str) {
    let queue = app.state::<Queue>();
    let mut map = queue.0.lock().unwrap();
    let Some(prompts) = map.get_mut(session_id) else { return };
    if prompts.front().map(String::as_str) == Some(prompt) {
        prompts.pop_front();
    }
    if prompts.is_empty() {
        map.remove(session_id);
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// `\\.\pipe\coucou-<sid>` — must match coucou-hook's `pipe_path()` exactly.
pub fn pipe_name() -> String {
    let key = crate::win_user::current_user_sid()
        .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    format!(r"\\.\pipe\coucou-{key}")
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let name = pipe_name();
        // first_pipe_instance also means we refuse to join a pipe somebody else
        // already owns under our name, rather than serving on top of it.
        let mut server = match ServerOptions::new().first_pipe_instance(true).create(&name) {
            Ok(s) => s,
            Err(err) => {
                log::line(format!("cannot open the relay pipe: {err}"));
                return;
            }
        };
        loop {
            if server.connect().await.is_err() {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            // Hand the connected instance to a task and listen on a fresh one.
            let next = match ServerOptions::new().create(&name) {
                Ok(s) => s,
                Err(err) => {
                    log::line(format!("cannot reopen the relay pipe: {err}"));
                    return;
                }
            };
            let connected = std::mem::replace(&mut server, next);
            let app = app.clone();
            tauri::async_runtime::spawn(async move { handle(app, connected).await });
        }
    });
}

async fn handle(app: AppHandle, mut pipe: NamedPipeServer) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() > MAX_PAYLOAD {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let line = match buf.iter().position(|b| *b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(line) else { return };
    if !payload.is_object() {
        return;
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if event == "Stop" {
        // Answered before the island hears of it, so the island learns in the
        // same event that the turn goes on with this prompt.
        // A relay from before the queue has already hung up: the write fails and
        // the prompt stays queued for the next Stop instead of vanishing.
        if let Some(prompt) = peek_queued(&app, &session_id) {
            let line = format!("continue {}\n", Value::String(prompt.clone()));
            let sent = pipe.write_all(line.as_bytes()).await.is_ok() && pipe.flush().await.is_ok();
            if sent {
                drop_queued(&app, &session_id, &prompt);
                payload["coucou_queued_prompt"] = json!(prompt);
            }
            log::line(format!("hook Stop — queued prompt {}", if sent { "sent" } else { "kept, relay gone" }));
        } else {
            log::line("hook Stop");
        }
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
        let _ = pipe.disconnect();
        return;
    }

    if event == "SessionEnd" {
        set_queue(&app, &session_id, Vec::new());
    }

    if event != "PermissionRequest" {
        log::line(format!("hook {event}"));
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
        let _ = pipe.disconnect();
        return;
    }

    let id = format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::channel::<Reply>(4);
    {
        let pending = app.state::<Pending>();
        pending.0.lock().unwrap().insert(id.clone(), tx);
    }
    payload["request_id"] = json!(id);
    log::line(format!("hook PermissionRequest id={id}"));
    let created = phone::now_ms();
    phone::open_request(&app, &id, &payload, created, created + DECISION_TIMEOUT.as_millis() as i64);
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);

    let (decision, by) = wait_for_decision(&app, &id, &mut rx).await;
    app.state::<Pending>().0.lock().unwrap().remove(&id);
    // Every way out of here passes this line, so the phones never keep a
    // request nobody can answer any more.
    phone::close_request(&app, &id, by);

    // No decision: say nothing at all. coucou-hook then writes nothing to stdout
    // and Claude Code asks in the terminal, exactly as if Coucou were closed.
    if let Some(d) = decision {
        let _ = pipe.write_all(format!("{d}\n").as_bytes()).await;
        let _ = pipe.flush().await;
    }
    let _ = pipe.disconnect();
}

/// Two waits: a short one for "the card is up", then the long one for a human.
/// Also says who settled it, for the phones.
async fn wait_for_decision(
    app: &AppHandle,
    id: &str,
    rx: &mut mpsc::Receiver<Reply>,
) -> (Option<String>, ClosedBy) {
    // The phone's deadline counts from the request's arrival, as `expiresAt` does.
    let deadline = tokio::time::Instant::now() + DECISION_TIMEOUT;
    let not_shown = match tokio::time::timeout(ACK_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Ack)) => None,
        // A click that beats the ack is still a click.
        Ok(Some(Reply::Decision(d, by))) => {
            log::line(format!("hook id={id} answered {}", verb(&d)));
            return (Some(d), by);
        }
        Ok(Some(Reply::Decline)) => Some("not shown"),
        Ok(None) => return (None, ClosedBy::Terminal),
        Err(_) => Some("island never acknowledged"),
    };
    if let Some(why) = not_shown {
        if !phone::can_take(app) {
            log::line(format!("hook id={id} {why} — terminal takes over"));
            return (None, ClosedBy::Terminal);
        }
        log::line(format!("hook id={id} {why} — waiting for the phone"));
        return wait_for_phone(id, rx, deadline).await;
    }

    match tokio::time::timeout(DECISION_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Decision(d, by))) => {
            log::line(format!("hook id={id} answered {}", verb(&d)));
            (Some(d), by)
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={id} released without a decision"));
            (None, ClosedBy::Terminal)
        }
        _ => {
            log::line(format!("hook id={id} timed out — terminal takes over"));
            (None, ClosedBy::Timeout)
        }
    }
}

/// The island isn't showing it, but a phone can answer: wait for a decision
/// until the deadline. A late ack or decline from the island changes nothing.
async fn wait_for_phone(
    id: &str,
    rx: &mut mpsc::Receiver<Reply>,
    deadline: tokio::time::Instant,
) -> (Option<String>, ClosedBy) {
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Reply::Decision(d, by))) => {
                log::line(format!("hook id={id} answered {}", verb(&d)));
                return (Some(d), by);
            }
            Ok(Some(_)) => continue,
            Ok(None) => return (None, ClosedBy::Terminal),
            Err(_) => {
                log::line(format!("hook id={id} timed out — terminal takes over"));
                return (None, ClosedBy::Timeout);
            }
        }
    }
}

/// `allow`, `deny` or `answers` — never what the answers say, which stays out of the log.
fn verb(decision: &str) -> &str {
    decision.split_whitespace().next().unwrap_or_default()
}

/// False when the request is already settled (or never existed).
fn send(app: &AppHandle, request_id: &str, reply: Reply, keep: bool) -> bool {
    let sender = {
        let pending = app.state::<Pending>();
        let mut map = pending.0.lock().unwrap();
        if keep { map.get(request_id).cloned() } else { map.remove(request_id) }
    };
    match sender {
        Some(tx) => tx.try_send(reply).is_ok(),
        None => {
            log::line(format!("reply for id={request_id} — no pending request"));
            false
        }
    }
}

/// The island has the card on screen; the long wait may begin.
pub fn acknowledge(app: &AppHandle, request_id: &str) {
    send(app, request_id, Reply::Ack, true);
}

/// Nobody can act on this one — paused, or another card already holds the view.
/// The request stays answerable: a phone may still take it (see wait_for_decision).
pub fn decline(app: &AppHandle, request_id: &str) {
    log::line(format!("decline id={request_id}"));
    send(app, request_id, Reply::Decline, true);
}

/// Called by the island's Allow / Deny buttons and the phone's slide. Only ever
/// a bare word: turning it into Claude Code's JSON is coucou-hook's job.
/// False if the request was already settled — the first decision wins.
pub fn answer(app: &AppHandle, request_id: &str, decision: &str, by: ClosedBy) -> bool {
    let word = match decision {
        "allow" | "always" => "allow",
        _ => "deny",
    };
    log::line(format!("decision id={request_id} {word} ({by:?})"));
    send(app, request_id, Reply::Decision(word.to_string(), by), false)
}

/// An AskUserQuestion answered on the island or the phone: question text →
/// chosen label(s). serde_json escapes any newline, so it always travels as one line.
pub fn answer_question(app: &AppHandle, request_id: &str, answers: &HashMap<String, String>, by: ClosedBy) -> bool {
    let Ok(json) = serde_json::to_string(answers) else { return false };
    log::line(format!("decision id={request_id} answers ({} question(s), {by:?})", answers.len()));
    send(app, request_id, Reply::Decision(format!("answers {json}"), by), false)
}
