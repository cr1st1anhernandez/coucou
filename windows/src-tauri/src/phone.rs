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

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

use crate::log;

pub const PORT: u16 = 47823;
const COOKIE: &str = "coucou_phone";
const PAIR_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong guesses a pairing code survives; then it's burnt and a new one is needed.
const PAIR_ATTEMPTS: u32 = 5;
/// `lastSeen` reaches the disk at most this often per device.
const SEEN_SAVE_MS: i64 = 60_000;

/// Everything the server knows. Lives in Tauri's state.
pub struct PhoneHub {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Set while the server is listening; sending on it stops the server.
    server: Option<oneshot::Sender<()>>,
    /// Why the server couldn't start (port taken…), shown in the settings.
    error: Option<String>,
    devices: Vec<Device>,
    pairing: Option<Pairing>,
}

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
}

impl PhoneHub {
    pub fn load() -> Self {
        let devices = std::fs::read(devices_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { inner: Mutex::new(Inner { devices, ..Default::default() }) }
    }
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
    fn caching_rules() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
        assert_eq!(cache_control("assets/index-abc.js"), "public, max-age=31536000, immutable");
        assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json");
    }
}
