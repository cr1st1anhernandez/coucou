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

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

use crate::log;

pub const PORT: u16 = 47823;

/// Everything the server knows. Lives in Tauri's state.
#[derive(Default)]
pub struct PhoneHub {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Set while the server is listening; sending on it stops the server.
    server: Option<oneshot::Sender<()>>,
    /// Why the server couldn't start (port taken…), shown in the settings.
    error: Option<String>,
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

// ── Routes ────────────────────────────────────────────────────────────────────

fn router(app: AppHandle) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .fallback(static_file)
        .with_state(app)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
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
    fn caching_rules() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
        assert_eq!(cache_control("assets/index-abc.js"), "public, max-age=31536000, immutable");
        assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json");
    }
}
