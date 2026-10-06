// The iPhone server: a small HTTP + WebSocket server for the Coucou PWA.
//
// It only ever listens on 127.0.0.1:47823. The phone reaches it through
// `tailscale serve`, which publishes it with HTTPS inside the user's tailnet —
// nothing is open on the local network or the internet.
//
// It runs only while "Acceso desde el iPhone" is on in the settings; switched
// off, there is no socket and nothing runs, exactly as before it existed.
//
// The PWA itself is served from %LOCALAPPDATA%\CoucouPhone\web, where the PWA
// project deploys its build. That folder sits outside Coucou's install folder
// so reinstalling Coucou never wipes it.
//
// Pairing: the settings window shows a 6-digit code, valid 5 minutes and once;
// the phone trades it for a random token kept in an HttpOnly cookie. Only the
// token's SHA-256 is stored (devices.json), so that file holds no secret.
//
// Push notifications (webpush.rs) only go out while the user is away from the
// PC, and never to a phone that has the app open. They say what is waiting
// ("Permiso: Bash · coucou"), never the command itself.
//
// Sessions live in the island's front end, not here: the island publishes a
// snapshot (`phone_publish`) whenever one changes, and the server hands it on
// to every open WebSocket.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{broadcast, oneshot};

use crate::island::WINDOW_LABEL;
use crate::log;
use crate::secrets;
use crate::webpush::{self, Subscription, Vapid};

pub const PORT: u16 = 47823;
const COOKIE: &str = "coucou_phone";
const PAIR_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong guesses a pairing code survives; then it's burnt and a new one is needed.
const PAIR_ATTEMPTS: u32 = 5;
/// `lastSeen` reaches the disk at most this often per device.
const SEEN_SAVE_MS: i64 = 60_000;
/// The PWA pings every 20 s; three missed pings and the socket is dead.
const PING_TIMEOUT: Duration = Duration::from_secs(60);
/// Prompts a session can have queued (MAX_QUEUE in sessions.ts).
const MAX_QUEUE: usize = 4;
/// Longest prompt accepted from the phone.
const MAX_PROMPT: usize = 8_000;
const VAPID_KEY: &str = "phone-vapid-key";

/// Everything the server knows. Lives in Tauri's state.
pub struct PhoneHub {
    inner: Mutex<Inner>,
    /// Fan-out to every open WebSocket.
    tx: broadcast::Sender<Arc<Out>>,
}

#[derive(Default)]
struct Inner {
    /// Set while the server is listening; sending on it stops the server.
    server: Option<oneshot::Sender<()>>,
    /// Why the server couldn't start (port taken…), shown in the settings.
    error: Option<String>,
    devices: Vec<Device>,
    pairing: Option<Pairing>,
    /// The island's last snapshot, and whether it thinks the user is away.
    sessions: Vec<PhoneSession>,
    away: bool,
    /// Open WebSockets: which device, and whether its app is on screen.
    conns: HashMap<u64, Conn>,
    /// Permission requests and questions still waiting, oldest first.
    approvals: Vec<PhoneApproval>,
    questions: Vec<PhoneQuestion>,
    /// Loaded from the Credential Manager on first use.
    vapid: Option<Arc<Vapid>>,
}

/// Who settled a request, for `approval-closed` / `question-closed`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClosedBy {
    Phone,
    Island,
    /// Nobody could take it, or the island handed it over: Claude Code asks.
    Terminal,
    /// Nobody answered in time; Claude Code asks.
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhoneApproval {
    request_id: String,
    session_id: String,
    session_name: String,
    tool: String,
    summary: String,
    detail: String,
    created_at: i64,
    expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhoneQuestion {
    request_id: String,
    session_id: String,
    session_name: String,
    questions: Vec<AskedQuestion>,
    created_at: i64,
    expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AskedQuestion {
    question: String,
    header: String,
    multi_select: bool,
    options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Serialize)]
struct QuestionOption {
    label: String,
    description: String,
}

#[derive(Debug)]
enum Request {
    Approval(PhoneApproval),
    Question(PhoneQuestion),
}

const ASK_TOOL: &str = "AskUserQuestion";
const MAX_SUMMARY: usize = 300;
const MAX_DETAIL: usize = 4000;
/// What the approval says it authorises, most specific field first
/// (approvalTarget in hooks.ts).
const SUMMARY_FIELDS: &[&str] =
    &["command", "file_path", "path", "url", "query", "pattern", "description", "prompt"];

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn folder_name(cwd: &str) -> String {
    cwd.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or_default().to_string()
}

/// A PermissionRequest hook payload as the phone sees it. None for a question
/// with nothing answerable in it.
fn request_from_hook(
    request_id: &str,
    payload: &serde_json::Value,
    session_name: Option<&str>,
    created_at: i64,
    expires_at: i64,
) -> Option<Request> {
    let str_of = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let session_id = str_of(payload, "session_id");
    let session_name = session_name
        .map(str::to_string)
        .unwrap_or_else(|| folder_name(&str_of(payload, "cwd")));
    let tool = str_of(payload, "tool_name");
    let input = payload.get("tool_input").cloned().unwrap_or_else(|| json!({}));

    if tool == ASK_TOOL {
        let questions: Vec<AskedQuestion> = input
            .get("questions")
            .and_then(|q| q.as_array())
            .into_iter()
            .flatten()
            .map(|q| AskedQuestion {
                question: str_of(q, "question"),
                header: str_of(q, "header"),
                multi_select: q.get("multiSelect").and_then(|m| m.as_bool()).unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(|o| o.as_array())
                    .into_iter()
                    .flatten()
                    .map(|o| QuestionOption { label: str_of(o, "label"), description: str_of(o, "description") })
                    .filter(|o| !o.label.is_empty())
                    .collect(),
            })
            .filter(|q| !q.question.is_empty() && !q.options.is_empty())
            .collect();
        if questions.is_empty() {
            return None;
        }
        return Some(Request::Question(PhoneQuestion {
            request_id: request_id.to_string(),
            session_id,
            session_name,
            questions,
            created_at,
            expires_at,
        }));
    }

    let summary = SUMMARY_FIELDS
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()))
        .unwrap_or(&tool);
    Some(Request::Approval(PhoneApproval {
        request_id: request_id.to_string(),
        session_id,
        session_name,
        summary: clip(summary, MAX_SUMMARY),
        detail: clip(&serde_json::to_string_pretty(&input).unwrap_or_default(), MAX_DETAIL),
        tool,
        created_at,
        expires_at,
    }))
}

struct Conn {
    device_id: String,
    visible: bool,
}

/// What goes out to the WebSockets.
enum Out {
    /// A message for every phone.
    Json(String),
    /// This device was removed: its sockets close.
    Revoke(String),
    /// The server is stopping.
    Shutdown,
}

/// One Claude Code session, as the island publishes it (PhoneSession in the
/// API contract). Rust only reads `id`, `name` and `status`; the rest travels on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneSession {
    id: String,
    name: String,
    cwd: String,
    status: String,
    steps: Vec<String>,
    step_index: i64,
    last_event_at: i64,
    waiting_since: Option<i64>,
    finished_at: Option<i64>,
    final_message: Option<String>,
    rate_reset_at: Option<i64>,
    todo: Option<serde_json::Value>,
    subagents: i64,
    summary: serde_json::Value,
    queue: Vec<String>,
}

static CONN_IDS: AtomicU64 = AtomicU64::new(1);

/// A paired phone, as stored in devices.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    device_id: String,
    /// The name the phone gave itself ("iPhone de Cristian").
    device: String,
    /// SHA-256 of the cookie token, hex. The token itself is never stored.
    token_hash: String,
    created_at: i64,
    last_seen: i64,
    /// Where to send its notifications, once it allowed them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    push: Option<Subscription>,
}

impl PhoneHub {
    pub fn load() -> Self {
        let devices = std::fs::read(devices_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(Inner { devices, ..Default::default() }),
            tx: broadcast::channel(64).0,
        }
    }

    fn send(&self, out: Out) {
        // No socket open is not an error.
        let _ = self.tx.send(Arc::new(out));
    }

    fn broadcast(&self, message: serde_json::Value) {
        self.send(Out::Json(message.to_string()));
    }
}

/// `{ t: "snapshot", sessions, approvals, questions }`.
fn snapshot(inner: &Inner) -> serde_json::Value {
    json!({ "sessions": inner.sessions, "approvals": inner.approvals, "questions": inner.questions })
}

/// A pairing code on screen in the settings window.
struct Pairing {
    code: String,
    expires: Instant,
    failures: u32,
}

#[derive(Debug, PartialEq)]
enum PairCheck {
    Ok,
    Wrong,
    TooMany,
}

impl Pairing {
    fn check(&mut self, code: &str, now: Instant) -> PairCheck {
        if self.failures >= PAIR_ATTEMPTS {
            return PairCheck::TooMany;
        }
        if now >= self.expires {
            return PairCheck::Wrong;
        }
        if ct_eq(code.trim().as_bytes(), self.code.as_bytes()) {
            return PairCheck::Ok;
        }
        self.failures += 1;
        if self.failures >= PAIR_ATTEMPTS { PairCheck::TooMany } else { PairCheck::Wrong }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    // The OS generator failing leaves nothing sensible to fall back on.
    getrandom::getrandom(&mut buf).expect("OS random generator");
    buf
}

/// Six digits, uniformly drawn.
fn random_code() -> String {
    loop {
        let n = u32::from_le_bytes(random_bytes::<4>());
        // Below the largest multiple of a million, so `% 1e6` has no bias.
        if n < 4_294_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Compares without stopping at the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// %LOCALAPPDATA%\CoucouPhone
fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("CoucouPhone")
}

fn web_dir() -> PathBuf {
    data_dir().join("web")
}

fn devices_path() -> PathBuf {
    data_dir().join("devices.json")
}

/// Temp file + rename, like settings.json: never a half-written file.
fn save_devices(devices: &[Device]) {
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let json = serde_json::to_vec_pretty(devices)?;
        let path = devices_path();
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, &path)
    })();
    if let Err(err) = result {
        log::line(format!("phone: cannot save devices.json: {err}"));
    }
}

// ── Start / stop ──────────────────────────────────────────────────────────────

/// Follows the "Acceso desde el iPhone" switch.
pub fn set_enabled(app: &AppHandle, on: bool) {
    if on {
        start(app);
    } else {
        stop(app);
    }
}

fn start(app: &AppHandle) {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    if inner.server.is_some() {
        return;
    }
    // Bound right here so a taken port is known before this returns.
    let listener = match std::net::TcpListener::bind(("127.0.0.1", PORT))
        .and_then(|l| l.set_nonblocking(true).map(|_| l))
    {
        Ok(l) => l,
        Err(err) => {
            log::line(format!("phone server: cannot listen on 127.0.0.1:{PORT}: {err}"));
            inner.error = Some(format!("No se pudo abrir el puerto {PORT}: {err}"));
            drop(inner);
            let _ = app.emit("phone-changed", ());
            return;
        }
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    inner.server = Some(stop_tx);
    inner.error = None;
    drop(inner);

    let router = router(app.clone());
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => l,
            Err(err) => {
                log::line(format!("phone server: {err}"));
                return;
            }
        };
        log::line(format!("phone server listening on 127.0.0.1:{PORT}"));
        let served = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await;
        if let Err(err) = served {
            log::line(format!("phone server stopped: {err}"));
            let hub = handle.state::<PhoneHub>();
            let mut inner = hub.inner.lock().unwrap();
            inner.server = None;
            inner.error = Some(err.to_string());
        } else {
            log::line("phone server stopped");
        }
        let _ = handle.emit("phone-changed", ());
    });
    let _ = app.emit("phone-changed", ());
}

fn stop(app: &AppHandle) {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    inner.error = None;
    if let Some(stop) = inner.server.take() {
        let _ = stop.send(());
        // A graceful shutdown waits for open connections: close the sockets.
        hub.send(Out::Shutdown);
        inner.conns.clear();
    }
}

fn is_running(app: &AppHandle) -> bool {
    app.state::<PhoneHub>().inner.lock().unwrap().server.is_some()
}

// ── Sessions (island → phones) ────────────────────────────────────────────────

/// The island's sessions changed. Stored for `/api/snapshot`, sent to every
/// open socket.
pub fn publish(app: &AppHandle, sessions: Vec<PhoneSession>, away: bool) {
    if !is_running(app) {
        return;
    }
    let (news, targets) = {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        let news = session_news(&inner.sessions, &sessions);
        inner.sessions = sessions;
        inner.away = away;
        hub.broadcast(json!({ "t": "sessions", "sessions": inner.sessions }));
        (news, push_targets(&inner))
    };
    if news.is_empty() || targets.is_empty() || !user_away(app, away) {
        return;
    }
    for message in news {
        send_push(app, targets.clone(), message, "normal");
    }
}

/// Sessions that just finished, failed or hit their limit, as push messages.
/// A session seen for the first time doesn't count: after a restart the whole
/// list arrives at once, and none of it is news.
fn session_news(before: &[PhoneSession], after: &[PhoneSession]) -> Vec<serde_json::Value> {
    after
        .iter()
        .filter_map(|s| {
            let old = before.iter().find(|o| o.id == s.id)?;
            if old.status == s.status {
                return None;
            }
            let (title, body) = match s.status.as_str() {
                "finished" => (format!("Terminó {}", s.name), s.final_message.as_deref().map(|m| clip(m, 120)).unwrap_or_default()),
                "error" => (format!("Error en {}", s.name), "La sesión se detuvo con un error.".to_string()),
                "ratelimit" => (format!("Límite de uso en {}", s.name), "Coucou te avisa cuando se libere.".to_string()),
                _ => return None,
            };
            Some(json!({
                "title": title,
                "body": body,
                "tag": format!("session:{}", s.id),
                "url": format!("/#/session/{}", s.id),
            }))
        })
        .collect()
}

// ── Permission requests (pipe.rs → phones) ────────────────────────────────────

/// Whether the island thinks the user is away, or Windows has seen no input
/// for the absence interval — Rust checks for itself so a quiet island (no
/// hook event lately) can't hide that the user left.
fn user_away(app: &AppHandle, island_says: bool) -> bool {
    if island_says {
        return true;
    }
    let absence = app.state::<crate::Shared>().settings.lock().unwrap().absence_interval;
    crate::win_ui::idle_seconds() as f64 >= absence
}

/// Can a phone answer a request the island won't show? Only if someone will
/// actually see it there: (a) the app is open on screen, or (b) the user is
/// away and a phone gets notifications. Otherwise the request goes to the
/// terminal exactly as it did before the phone existed — no extra wait for
/// someone sitting at the PC with the island paused.
pub fn can_take(app: &AppHandle) -> bool {
    let (running, visible, push, island_away) = {
        let hub = app.state::<PhoneHub>();
        let inner = hub.inner.lock().unwrap();
        (
            inner.server.is_some(),
            inner.conns.values().any(|c| c.visible),
            inner.devices.iter().any(|d| d.push.is_some()),
            inner.away,
        )
    };
    running && (visible || (push && user_away(app, island_away)))
}

/// A PermissionRequest reached the relay: the phones hear of it too.
pub fn open_request(app: &AppHandle, request_id: &str, payload: &serde_json::Value, created_at: i64, expires_at: i64) {
    let (message, targets, island_away) = {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        if inner.server.is_none() {
            return;
        }
        let session_id = payload.get("session_id").and_then(|v| v.as_str()).unwrap_or_default();
        let name = inner.sessions.iter().find(|s| s.id == session_id).map(|s| s.name.clone());
        // What is waiting, never the command itself: that stays in the app.
        let message = match request_from_hook(request_id, payload, name.as_deref(), created_at, expires_at) {
            Some(Request::Approval(a)) => {
                hub.broadcast(json!({ "t": "approval", "approval": a }));
                let message = json!({
                    "title": format!("Permiso: {} · {}", a.tool, a.session_name),
                    "body": "Ábrelo para aprobar o negar.",
                    "tag": format!("approval:{request_id}"),
                    "url": format!("/#/approval/{request_id}"),
                });
                inner.approvals.push(a);
                message
            }
            Some(Request::Question(q)) => {
                hub.broadcast(json!({ "t": "question", "question": q }));
                let message = json!({
                    "title": format!("Pregunta · {}", q.session_name),
                    "body": "Claude espera tu respuesta.",
                    "tag": format!("question:{request_id}"),
                    "url": format!("/#/question/{request_id}"),
                });
                inner.questions.push(q);
                message
            }
            None => return,
        };
        (message, push_targets(&inner), inner.away)
    };
    if !targets.is_empty() && user_away(app, island_away) {
        send_push(app, targets, message, "high");
    }
}

// ── Push ──────────────────────────────────────────────────────────────────────

/// Phones that get notifications and don't have the app on screen right now.
fn push_targets(inner: &Inner) -> Vec<(String, Subscription)> {
    inner
        .devices
        .iter()
        .filter(|d| !inner.conns.values().any(|c| c.visible && c.device_id == d.device_id))
        .filter_map(|d| Some((d.device_id.clone(), d.push.clone()?)))
        .collect()
}

/// Coucou's VAPID keys: from the Credential Manager, made the first time.
fn vapid(app: &AppHandle) -> Result<Arc<Vapid>, String> {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    if let Some(v) = &inner.vapid {
        return Ok(v.clone());
    }
    let key = match secrets::get_internal(VAPID_KEY).and_then(|k| Vapid::from_b64(&k)) {
        Some(k) => k,
        None => {
            let k = Vapid::generate()?;
            secrets::set_internal(VAPID_KEY, &k.private_b64())?;
            log::line("phone: VAPID keys created");
            k
        }
    };
    let key = Arc::new(key);
    inner.vapid = Some(key.clone());
    Ok(key)
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

fn send_push(app: &AppHandle, targets: Vec<(String, Subscription)>, message: serde_json::Value, urgency: &'static str) {
    let vapid = match vapid(app) {
        Ok(v) => v,
        Err(err) => {
            log::line(format!("phone: no VAPID key: {err}"));
            return;
        }
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let body = message.to_string();
        let now_s = (now_ms() / 1000) as u64;
        for (device_id, sub) in targets {
            match webpush::send(http_client(), &vapid, &sub, body.as_bytes(), urgency, now_s).await {
                webpush::Sent::Delivered => {}
                webpush::Sent::Gone => {
                    log::line(format!("phone: push subscription of {device_id} is gone"));
                    forget_subscription(&app, &device_id, &sub.endpoint);
                }
                webpush::Sent::Failed(err) => log::line(format!("phone: push to {device_id} failed: {err}")),
            }
        }
    });
}

fn forget_subscription(app: &AppHandle, device_id: &str, endpoint: &str) {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    let Some(d) = inner.devices.iter_mut().find(|d| d.device_id == device_id) else { return };
    if d.push.as_ref().is_some_and(|p| p.endpoint == endpoint) {
        d.push = None;
        save_devices(&inner.devices);
    }
}

/// The request is settled, one way or another: the phones drop it.
pub fn close_request(app: &AppHandle, request_id: &str, by: ClosedBy) {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    if let Some(i) = inner.approvals.iter().position(|a| a.request_id == request_id) {
        inner.approvals.remove(i);
        hub.broadcast(json!({ "t": "approval-closed", "requestId": request_id, "by": by }));
    } else if let Some(i) = inner.questions.iter().position(|q| q.request_id == request_id) {
        inner.questions.remove(i);
        hub.broadcast(json!({ "t": "question-closed", "requestId": request_id, "by": by }));
    }
}

// ── Settings window ───────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneStatus {
    running: bool,
    error: Option<String>,
    port: u16,
    /// `https://<pc>.<tailnet>.ts.net` when Tailscale is installed and logged in.
    url: Option<String>,
    tailscale: bool,
    web_dir: String,
    web_installed: bool,
}

pub fn status(app: &AppHandle) -> PhoneStatus {
    let (running, error) = {
        let hub = app.state::<PhoneHub>();
        let inner = hub.inner.lock().unwrap();
        (inner.server.is_some(), inner.error.clone())
    };
    let (tailscale, url) = tailscale_url();
    PhoneStatus {
        running,
        error,
        port: PORT,
        url,
        tailscale,
        web_dir: web_dir().to_string_lossy().to_string(),
        web_installed: web_dir().join("index.html").is_file(),
    }
}

fn tailscale_exe() -> Option<PathBuf> {
    crate::find_on_path("tailscale").or_else(|| {
        let p = PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe");
        p.is_file().then_some(p)
    })
}

/// (installed, `https://` + Self.DNSName from `tailscale status --json`).
fn tailscale_url() -> (bool, Option<String>) {
    use std::os::windows::process::CommandExt;
    let Some(exe) = tailscale_exe() else { return (false, None) };
    let out = std::process::Command::new(exe)
        .args(["status", "--json"])
        .creation_flags(crate::CREATE_NO_WINDOW)
        .output();
    let Ok(out) = out else { return (true, None) };
    let url = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v["Self"]["DNSName"].as_str().map(|s| s.trim_end_matches('.').to_string()))
        .filter(|s| !s.is_empty())
        .map(|host| format!("https://{host}"));
    (true, url)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCode {
    code: String,
    expires_at: i64,
}

/// "Emparejar iPhone": a fresh code replaces any earlier one.
pub fn new_pair_code(app: &AppHandle) -> PairCode {
    let code = random_code();
    let hub = app.state::<PhoneHub>();
    hub.inner.lock().unwrap().pairing =
        Some(Pairing { code: code.clone(), expires: Instant::now() + PAIR_TTL, failures: 0 });
    log::line("phone: pairing code issued");
    PairCode { code, expires_at: now_ms() + PAIR_TTL.as_millis() as i64 }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    device_id: String,
    device: String,
    created_at: i64,
    last_seen: i64,
    /// It allowed notifications.
    push: bool,
}

pub fn devices(app: &AppHandle) -> Vec<DeviceInfo> {
    let hub = app.state::<PhoneHub>();
    let inner = hub.inner.lock().unwrap();
    inner
        .devices
        .iter()
        .map(|d| DeviceInfo {
            device_id: d.device_id.clone(),
            device: d.device.clone(),
            created_at: d.created_at,
            last_seen: d.last_seen,
            push: d.push.is_some(),
        })
        .collect()
}

/// "Quitar" in the settings, or the phone unpairing itself.
pub fn revoke(app: &AppHandle, device_id: &str) {
    {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        let before = inner.devices.len();
        inner.devices.retain(|d| d.device_id != device_id);
        if inner.devices.len() == before {
            return;
        }
        save_devices(&inner.devices);
        inner.conns.retain(|_, c| c.device_id != device_id);
        hub.send(Out::Revoke(device_id.to_string()));
    }
    log::line(format!("phone: device {device_id} removed"));
    let _ = app.emit("phone-changed", ());
}

// ── Routes ────────────────────────────────────────────────────────────────────

fn router(app: AppHandle) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/pair", post(pair))
        .route("/api/me", get(me))
        .route("/api/unpair", post(unpair))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/ws", get(ws))
        .route("/api/approvals/{id}", post(decide_approval))
        .route("/api/questions/{id}", post(answer_question))
        .route("/api/sessions/{id}/queue", put(set_queue))
        .route("/api/push/key", get(push_key))
        .route("/api/push/subscribe", post(push_subscribe).delete(push_unsubscribe))
        .fallback(static_file)
        .with_state(app)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// Who is asking: the device behind the `coucou_phone` cookie, if any.
struct Caller {
    device_id: String,
    device: String,
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then_some(v)
        })
}

fn authenticate(app: &AppHandle, headers: &HeaderMap) -> Option<Caller> {
    let hash = hash_token(cookie(headers, COOKIE)?);
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    // Every hash is compared, so the time taken says nothing about which matched.
    let mut found = None;
    for (i, d) in inner.devices.iter().enumerate() {
        if ct_eq(d.token_hash.as_bytes(), hash.as_bytes()) {
            found = Some(i);
        }
    }
    let d = &mut inner.devices[found?];
    let now = now_ms();
    let stale = now - d.last_seen > SEEN_SAVE_MS;
    d.last_seen = now;
    let caller = Caller { device_id: d.device_id.clone(), device: d.device.clone() };
    if stale {
        save_devices(&inner.devices);
    }
    Some(caller)
}

/// The host part of `https://host[:port]/…`.
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    Some(rest.split('/').next().unwrap_or(rest))
}

/// Writes come from our own page: they carry `X-Coucou-Phone: 1` (a header a
/// cross-site form can't set) and an Origin naming the host they were sent to.
/// Behind `tailscale serve` that host may arrive as X-Forwarded-Host.
fn same_origin(headers: &HeaderMap) -> bool {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let Some(origin) = get("origin").and_then(origin_host) else { return false };
    [get("host"), get("x-forwarded-host")]
        .into_iter()
        .flatten()
        .any(|host| host.eq_ignore_ascii_case(origin))
}

fn write_allowed(headers: &HeaderMap) -> bool {
    headers.get("x-coucou-phone").and_then(|v| v.to_str().ok()) == Some("1") && same_origin(headers)
}

fn status_only(code: StatusCode) -> Response {
    code.into_response()
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    device: String,
}

async fn pair(State(app): State<AppHandle>, headers: HeaderMap, body: Bytes) -> Response {
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let Ok(req) = serde_json::from_slice::<PairBody>(&body) else {
        return status_only(StatusCode::BAD_REQUEST);
    };
    let token = B64.encode(random_bytes::<32>());
    let device = {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        let now = Instant::now();
        let check = match inner.pairing.as_mut() {
            Some(p) => p.check(&req.code, now),
            None => PairCheck::Wrong,
        };
        match check {
            PairCheck::Wrong => {
                if inner.pairing.as_ref().is_some_and(|p| now >= p.expires) {
                    inner.pairing = None;
                }
                log::line("phone: wrong pairing code");
                return status_only(StatusCode::UNAUTHORIZED);
            }
            PairCheck::TooMany => {
                log::line("phone: pairing code burnt after too many attempts");
                return status_only(StatusCode::TOO_MANY_REQUESTS);
            }
            PairCheck::Ok => {}
        }
        // One use only.
        inner.pairing = None;
        let name: String = req.device.trim().chars().take(60).collect();
        let now = now_ms();
        let device = Device {
            device_id: B64.encode(random_bytes::<12>()),
            device: if name.is_empty() { "iPhone".into() } else { name },
            token_hash: hash_token(&token),
            created_at: now,
            last_seen: now,
            push: None,
        };
        inner.devices.push(device.clone());
        save_devices(&inner.devices);
        device
    };
    log::line(format!("phone: paired {} ({})", device.device_id, device.device));
    let _ = app.emit("phone-changed", ());
    let mut res = Json(json!({ "deviceId": device.device_id })).into_response();
    let cookie = format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=31536000");
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

async fn me(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    match authenticate(&app, &headers) {
        Some(c) => Json(json!({ "deviceId": c.device_id, "device": c.device })).into_response(),
        None => status_only(StatusCode::UNAUTHORIZED),
    }
}

async fn unpair(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    let Some(caller) = authenticate(&app, &headers) else {
        return status_only(StatusCode::UNAUTHORIZED);
    };
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    revoke(&app, &caller.device_id);
    let mut res = status_only(StatusCode::NO_CONTENT);
    let cleared = format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0");
    if let Ok(v) = HeaderValue::from_str(&cleared) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

async fn get_snapshot(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if authenticate(&app, &headers).is_none() {
        return status_only(StatusCode::UNAUTHORIZED);
    }
    let hub = app.state::<PhoneHub>();
    let inner = hub.inner.lock().unwrap();
    Json(snapshot(&inner)).into_response()
}

#[derive(Deserialize)]
struct DecisionBody {
    decision: String,
}

/// Allow / Deny from the phone — always after the user's slide there. The
/// first decision wins, wherever it came from; any later one is a 404.
async fn decide_approval(
    State(app): State<AppHandle>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if authenticate(&app, &headers).is_none() {
        return status_only(StatusCode::UNAUTHORIZED);
    }
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let decision = match serde_json::from_slice::<DecisionBody>(&body) {
        Ok(b) if b.decision == "allow" || b.decision == "deny" => b.decision,
        _ => return status_only(StatusCode::BAD_REQUEST),
    };
    let open = {
        let hub = app.state::<PhoneHub>();
        let inner = hub.inner.lock().unwrap();
        inner.approvals.iter().any(|a| a.request_id == id)
    };
    if !open || !crate::pipe::answer(&app, &id, &decision, ClosedBy::Phone) {
        return status_only(StatusCode::NOT_FOUND);
    }
    // The island may have the same card up: it closes too.
    let _ = app.emit_to(WINDOW_LABEL, "phone-decision", json!({ "requestId": id }));
    status_only(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AnswersBody {
    answers: HashMap<String, String>,
}

async fn answer_question(
    State(app): State<AppHandle>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if authenticate(&app, &headers).is_none() {
        return status_only(StatusCode::UNAUTHORIZED);
    }
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let Ok(AnswersBody { answers }) = serde_json::from_slice::<AnswersBody>(&body) else {
        return status_only(StatusCode::BAD_REQUEST);
    };
    let open = {
        let hub = app.state::<PhoneHub>();
        let inner = hub.inner.lock().unwrap();
        inner.questions.iter().any(|q| q.request_id == id)
    };
    if answers.is_empty() {
        return status_only(StatusCode::BAD_REQUEST);
    }
    if !open || !crate::pipe::answer_question(&app, &id, &answers, ClosedBy::Phone) {
        return status_only(StatusCode::NOT_FOUND);
    }
    let _ = app.emit_to(WINDOW_LABEL, "phone-decision", json!({ "requestId": id }));
    status_only(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct QueueBody {
    prompts: Vec<String>,
}

/// Replaces a session's queue. The island owns the queue (it shows it and
/// hands it to the relay), so this only asks the island to apply it; the next
/// snapshot shows the result.
async fn set_queue(
    State(app): State<AppHandle>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if authenticate(&app, &headers).is_none() {
        return status_only(StatusCode::UNAUTHORIZED);
    }
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let prompts: Vec<String> = match serde_json::from_slice::<QueueBody>(&body) {
        Ok(b) => b.prompts.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
        Err(_) => return status_only(StatusCode::BAD_REQUEST),
    };
    if prompts.len() > MAX_QUEUE || prompts.iter().any(|p| p.chars().count() > MAX_PROMPT) {
        return status_only(StatusCode::BAD_REQUEST);
    }
    let known = {
        let hub = app.state::<PhoneHub>();
        let inner = hub.inner.lock().unwrap();
        inner.sessions.iter().any(|s| s.id == id)
    };
    if !known {
        return status_only(StatusCode::NOT_FOUND);
    }
    log::line(format!("phone: queue for {id} ({} prompt(s))", prompts.len()));
    let _ = app.emit_to(WINDOW_LABEL, "phone-queue", json!({ "sessionId": id, "prompts": prompts }));
    status_only(StatusCode::NO_CONTENT)
}

async fn push_key(State(app): State<AppHandle>, headers: HeaderMap) -> Response {
    if authenticate(&app, &headers).is_none() {
        return status_only(StatusCode::UNAUTHORIZED);
    }
    match vapid(&app) {
        Ok(v) => Json(json!({ "publicKey": v.public_b64() })).into_response(),
        Err(err) => {
            log::line(format!("phone: no VAPID key: {err}"));
            status_only(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// A subscription worth keeping: an https endpoint and keys of the right size.
fn valid_subscription(sub: &Subscription) -> bool {
    let len = |s: &str| B64.decode(s.trim_end_matches('=')).map(|b| b.len()).unwrap_or(0);
    sub.endpoint.starts_with("https://")
        && reqwest::Url::parse(&sub.endpoint).is_ok()
        && len(&sub.keys.p256dh) == 65
        && len(&sub.keys.auth) == 16
}

async fn push_subscribe(State(app): State<AppHandle>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(caller) = authenticate(&app, &headers) else {
        return status_only(StatusCode::UNAUTHORIZED);
    };
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let sub = match serde_json::from_slice::<Subscription>(&body) {
        Ok(s) if valid_subscription(&s) => s,
        _ => return status_only(StatusCode::BAD_REQUEST),
    };
    {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        if let Some(d) = inner.devices.iter_mut().find(|d| d.device_id == caller.device_id) {
            d.push = Some(sub);
            save_devices(&inner.devices);
        }
    }
    log::line(format!("phone: {} subscribed to notifications", caller.device_id));
    let _ = app.emit("phone-changed", ());
    status_only(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EndpointBody {
    endpoint: String,
}

async fn push_unsubscribe(State(app): State<AppHandle>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(caller) = authenticate(&app, &headers) else {
        return status_only(StatusCode::UNAUTHORIZED);
    };
    if !write_allowed(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    let Ok(EndpointBody { endpoint }) = serde_json::from_slice::<EndpointBody>(&body) else {
        return status_only(StatusCode::BAD_REQUEST);
    };
    forget_subscription(&app, &caller.device_id, &endpoint);
    let _ = app.emit("phone-changed", ());
    status_only(StatusCode::NO_CONTENT)
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

async fn ws(State(app): State<AppHandle>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let Some(caller) = authenticate(&app, &headers) else {
        return status_only(StatusCode::UNAUTHORIZED);
    };
    // Browsers always send Origin on a WebSocket; another site's page can't
    // pretend to be ours.
    if !same_origin(&headers) {
        return status_only(StatusCode::FORBIDDEN);
    }
    upgrade.on_upgrade(move |socket| socket_loop(app, caller.device_id, socket))
}

#[derive(Deserialize)]
struct FromPhone {
    t: String,
    visible: Option<bool>,
}

async fn socket_loop(app: AppHandle, device_id: String, mut socket: WebSocket) {
    let conn_id = CONN_IDS.fetch_add(1, Ordering::Relaxed);
    let (mut rx, first) = {
        let hub = app.state::<PhoneHub>();
        let mut inner = hub.inner.lock().unwrap();
        // Visible until it says otherwise: it just opened, so it's on screen.
        inner.conns.insert(conn_id, Conn { device_id: device_id.clone(), visible: true });
        let mut first = snapshot(&inner);
        first["t"] = json!("snapshot");
        (hub.tx.subscribe(), first.to_string())
    };
    log::line(format!("phone: socket {conn_id} open ({device_id})"));

    let mut alive = socket.send(Message::Text(first.into())).await.is_ok();
    let mut deadline = tokio::time::Instant::now() + PING_TIMEOUT;
    while alive {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    deadline = tokio::time::Instant::now() + PING_TIMEOUT;
                    let Ok(msg) = serde_json::from_str::<FromPhone>(text.as_str()) else { continue };
                    match msg.t.as_str() {
                        "ping" => alive = socket.send(Message::Text(r#"{"t":"pong"}"#.into())).await.is_ok(),
                        "visible" => set_visible(&app, conn_id, msg.visible.unwrap_or(false)),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => alive = false,
                Some(Ok(_)) => {}
            },
            out = rx.recv() => match out {
                Ok(out) => match &*out {
                    Out::Json(text) => alive = socket.send(Message::Text(text.clone().into())).await.is_ok(),
                    Out::Revoke(id) if *id == device_id => alive = false,
                    Out::Revoke(_) => {}
                    Out::Shutdown => alive = false,
                },
                // Fell behind: start over from the full picture.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let fresh = {
                        let hub = app.state::<PhoneHub>();
                        let inner = hub.inner.lock().unwrap();
                        let mut s = snapshot(&inner);
                        s["t"] = json!("snapshot");
                        s.to_string()
                    };
                    alive = socket.send(Message::Text(fresh.into())).await.is_ok();
                }
                Err(broadcast::error::RecvError::Closed) => alive = false,
            },
            _ = tokio::time::sleep_until(deadline) => alive = false,
        }
    }
    let _ = socket.send(Message::Close(None)).await;
    app.state::<PhoneHub>().inner.lock().unwrap().conns.remove(&conn_id);
    log::line(format!("phone: socket {conn_id} closed"));
}

fn set_visible(app: &AppHandle, conn_id: u64, visible: bool) {
    let hub = app.state::<PhoneHub>();
    let mut inner = hub.inner.lock().unwrap();
    if let Some(c) = inner.conns.get_mut(&conn_id) {
        c.visible = visible;
    }
}

// ── Static files ──────────────────────────────────────────────────────────────

/// The PWA's own files. Unknown paths fall back to index.html (it routes with
/// the hash, but a reload on a deep link must still land somewhere).
async fn static_file(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") || path == "/api" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(rel) = safe_relative(path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let root = web_dir();
    let rel = if rel.is_empty() { "index.html".to_string() } else { rel };
    if let Ok(bytes) = tokio::fs::read(root.join(&rel)).await {
        return file_response(&rel, bytes);
    }
    // A hashed asset that isn't there is a 404, not the app shell.
    if rel.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(root.join("index.html")).await {
        Ok(bytes) => file_response("index.html", bytes),
        Err(_) => not_installed(),
    }
}

/// `/a/b.js` → `a/b.js`, percent-decoded. None for anything that could step
/// outside the web folder: `..`, `.`, empty segments, backslashes, drive
/// letters, NUL.
fn safe_relative(path: &str) -> Option<String> {
    let decoded = percent_decode(path)?;
    let trimmed = decoded.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let trimmed = trimmed.strip_suffix('/').map(|t| format!("{t}/index.html")).unwrap_or_else(|| trimmed.to_string());
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains(['\\', ':', '\0']) {
            return None;
        }
    }
    Some(trimmed)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(rel: &str) -> &'static str {
    let ext = Path::new(rel).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Hashed assets never change; everything else must be revalidated, or an
/// installed PWA would keep running an old service worker forever.
fn cache_control(rel: &str) -> &'static str {
    if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn file_response(rel: &str, bytes: Vec<u8>) -> Response {
    let mut res = Response::new(Body::from(bytes));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(rel)));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control(rel)));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

fn not_installed() -> Response {
    let page = "<!doctype html><html lang=\"es\"><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
        <title>Coucou</title>\
        <body style=\"font:16px system-ui;background:#0b0c0e;color:#f5f6f8;padding:32px\">\
        <h1 style=\"font-size:20px\">La PWA no está instalada</h1>\
        <p style=\"color:#9398a1\">Coucou está corriendo, pero todavía no hay nada en \
        %LOCALAPPDATA%\\CoucouPhone\\web. Despliega la PWA con <code>npm run deploy</code>.</p>";
    let mut res = Response::new(Body::from(page));
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_paths_stay_inside() {
        assert_eq!(safe_relative("/").as_deref(), Some(""));
        assert_eq!(safe_relative("/assets/app-1a2b.js").as_deref(), Some("assets/app-1a2b.js"));
        assert_eq!(safe_relative("/sub/").as_deref(), Some("sub/index.html"));
        assert_eq!(safe_relative("/caf%C3%A9.png").as_deref(), Some("café.png"));
        for bad in [
            "/../secret", "/assets/../../x", "/%2e%2e/x", "/%2E%2E%2Fx", "/a//b", "/./a",
            "/a\\..\\b", "/%5c..%5cb", "/C:/Windows", "/a%00b", "/%zz",
        ] {
            assert_eq!(safe_relative(bad), None, "{bad}");
        }
    }

    #[test]
    fn token_hash_and_compare() {
        let token = B64.encode(random_bytes::<32>());
        assert_eq!(token.len(), 43);
        let hash = hash_token(&token);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_token(&token));
        assert_ne!(hash, hash_token(&format!("{token}x")));
        assert_eq!(hash_token("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(ct_eq(b"123456", b"123456"));
        assert!(!ct_eq(b"123456", b"123457"));
        assert!(!ct_eq(b"12345", b"123456"));
    }

    #[test]
    fn pairing_code_burns_after_five_misses() {
        let now = Instant::now();
        let mut p = Pairing { code: "042137".into(), expires: now + PAIR_TTL, failures: 0 };
        for _ in 0..4 {
            assert_eq!(p.check("000000", now), PairCheck::Wrong);
        }
        assert_eq!(p.check("000000", now), PairCheck::TooMany);
        // Burnt: even the right code is refused now.
        assert_eq!(p.check("042137", now), PairCheck::TooMany);

        let mut p = Pairing { code: "042137".into(), expires: now + PAIR_TTL, failures: 0 };
        assert_eq!(p.check("042137", now + PAIR_TTL), PairCheck::Wrong, "expired");
        assert_eq!(p.check(" 042137 ", now), PairCheck::Ok);
        assert_eq!(random_code().len(), 6);
    }

    #[test]
    fn writes_need_header_and_same_origin() {
        let mut h = HeaderMap::new();
        h.insert("host", "pc.tail1234.ts.net".parse().unwrap());
        h.insert("origin", "https://pc.tail1234.ts.net".parse().unwrap());
        assert!(!write_allowed(&h));
        h.insert("x-coucou-phone", "1".parse().unwrap());
        assert!(write_allowed(&h));
        h.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!write_allowed(&h));
        h.insert("host", "127.0.0.1:47823".parse().unwrap());
        h.insert("x-forwarded-host", "pc.tail1234.ts.net".parse().unwrap());
        h.insert("origin", "https://pc.tail1234.ts.net".parse().unwrap());
        assert!(write_allowed(&h));
        h.remove("origin");
        assert!(!write_allowed(&h));
    }

    #[test]
    fn reads_the_cookie() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; coucou_phone=tok_en-1; b=2".parse().unwrap());
        assert_eq!(cookie(&h, COOKIE), Some("tok_en-1"));
        assert_eq!(cookie(&h, "c"), None);
    }

    #[test]
    fn hook_to_approval() {
        let payload = json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s1",
            "cwd": "C:\\Users\\dev\\Projects\\coucou\\",
            "tool_name": "Bash",
            "tool_input": { "command": format!("  npm test {}", "x".repeat(400)), "description": "Run tests" },
        });
        let Some(Request::Approval(a)) = request_from_hook("7-1", &payload, None, 1_000, 109_000) else {
            panic!("not an approval")
        };
        assert_eq!(a.request_id, "7-1");
        assert_eq!(a.session_id, "s1");
        assert_eq!(a.session_name, "coucou", "falls back to the folder name");
        assert_eq!(a.tool, "Bash");
        assert!(a.summary.starts_with("npm test x"));
        assert_eq!(a.summary.chars().count(), MAX_SUMMARY);
        assert!(a.summary.ends_with('…'));
        assert!(a.detail.contains("\"description\": \"Run tests\""));
        assert_eq!((a.created_at, a.expires_at), (1_000, 109_000));
        let wire = serde_json::to_value(&a).unwrap();
        assert!(wire.get("sessionName").is_some() && wire.get("expiresAt").is_some());

        // A known session name wins; an unknown tool falls back to its own name.
        let payload = json!({ "session_id": "s1", "tool_name": "mcp__x__y", "tool_input": { "n": 1 } });
        let Some(Request::Approval(a)) = request_from_hook("7-2", &payload, Some("mi-sesión"), 0, 0) else {
            panic!("not an approval")
        };
        assert_eq!(a.session_name, "mi-sesión");
        assert_eq!(a.summary, "mcp__x__y");
    }

    #[test]
    fn hook_to_question() {
        let payload = json!({
            "session_id": "s2",
            "cwd": "/home/x/proj",
            "tool_name": "AskUserQuestion",
            "tool_input": { "questions": [
                { "question": "¿Qué base?", "header": "Base", "multiSelect": true,
                  "options": [{ "label": "Postgres", "description": "SQL" }, { "label": "" }] },
                { "question": "Sin opciones", "header": "X", "options": [] },
            ]},
        });
        let Some(Request::Question(q)) = request_from_hook("7-3", &payload, None, 5, 6) else {
            panic!("not a question")
        };
        assert_eq!(q.session_name, "proj");
        assert_eq!(q.questions.len(), 1);
        assert!(q.questions[0].multi_select);
        assert_eq!(q.questions[0].options.len(), 1);
        let wire = serde_json::to_value(&q).unwrap();
        assert_eq!(wire["questions"][0]["multiSelect"], true);
        assert_eq!(wire["questions"][0]["options"][0]["label"], "Postgres");

        let empty = json!({ "tool_name": "AskUserQuestion", "tool_input": {} });
        assert!(request_from_hook("7-4", &empty, None, 0, 0).is_none());
        assert_eq!(serde_json::to_value(ClosedBy::Timeout).unwrap(), "timeout");
    }

    fn session(id: &str, status: &str) -> PhoneSession {
        serde_json::from_value(json!({
            "id": id, "name": format!("n-{id}"), "cwd": "", "status": status, "steps": [], "stepIndex": 0,
            "lastEventAt": 0, "waitingSince": null, "finishedAt": null, "finalMessage": "Listo, quedó.",
            "rateResetAt": null, "todo": null, "subagents": 0,
            "summary": { "files": [], "added": 0, "removed": 0, "tests": "none" }, "queue": [],
        }))
        .unwrap()
    }

    #[test]
    fn session_news_only_on_transitions() {
        let before = vec![session("a", "working"), session("b", "finished"), session("c", "thinking")];
        let after = vec![
            session("a", "finished"),
            session("b", "finished"),
            session("c", "ratelimit"),
            session("d", "error"),
        ];
        let news = session_news(&before, &after);
        assert_eq!(news.len(), 2, "{news:?}");
        assert_eq!(news[0]["title"], "Terminó n-a");
        assert_eq!(news[0]["body"], "Listo, quedó.");
        assert_eq!(news[0]["tag"], "session:a");
        assert_eq!(news[0]["url"], "/#/session/a");
        assert_eq!(news[1]["title"], "Límite de uso en n-c");
    }

    #[test]
    fn caching_rules() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
        assert_eq!(cache_control("assets/index-abc.js"), "public, max-age=31536000, immutable");
        assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json");
    }
}
