// Integration pollers — the Rust side of GithubPoller, plus Google Calendar.
//
// Same endpoints, same first-run delays and intervals as the Swift pollers. Each
// one emits an `integration` event; the island owns the badge, the sound and the
// 60 s auto-clear, exactly as the Swift handlers do.
//
// Nothing is polled until its key exists in the Credential Manager, and no
// request goes anywhere the user has not configured.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::island::WINDOW_LABEL;
use crate::secrets;

const TIMEOUT: Duration = Duration::from_secs(10);

/// What the island receives. `event` is only set when something actually changed,
/// which is what drives the pill badge and the sound.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationUpdate {
    pub id: &'static str,
    pub data: Value,
    pub error: Option<String>,
    pub event: Option<IntegrationEvent>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationEvent {
    pub success: bool,
    pub label: String,
    pub detail: Option<String>,
}

fn emit(app: &AppHandle, update: IntegrationUpdate) {
    let _ = app.emit_to(WINDOW_LABEL, "integration", update);
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Set from the tray's Pause item. While it is on, nothing reaches the network:
/// pausing Coucou has to mean pausing Coucou, not just hiding the island.
pub static PAUSED: AtomicBool = AtomicBool::new(false);

pub fn set_paused(on: bool) {
    PAUSED.store(on, Ordering::Relaxed);
}

/// Spawns every poller with the macOS delays and intervals.
pub fn start(app: AppHandle) {
    // Every minute: a review or a CI run is news you want while it's fresh.
    spawn(app.clone(), "integration_github", 7, 60, poll_github);
    spawn(app, "integration_calendar", 10, 300, poll_calendar);
}

/// True when the user has this integration switched on in settings.
fn enabled(app: &AppHandle, id: &str) -> bool {
    app.try_state::<crate::Shared>()
        .map(|shared| {
            let settings = shared.settings.lock().unwrap();
            settings.active_integrations.iter().any(|x| x == id)
        })
        .unwrap_or(false)
}

fn spawn<F, Fut>(app: AppHandle, id: &'static str, delay_secs: u64, every_secs: u64, poll: F)
where
    F: Fn(AppHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(every_secs));
        loop {
            ticker.tick().await;
            // The ticker keeps its cadence; we just decline to do the work. An
            // integration the user switched off, or a paused app, must make no
            // network calls at all — CLAUDE.md allows talking only to services
            // the user configured, and a disabled one is not configured.
            if PAUSED.load(Ordering::Relaxed) || !enabled(&app, id) {
                continue;
            }
            poll(app.clone()).await;
        }
    });
}

/// One-shot refresh from the Refresh buttons in the island.
pub async fn poll_once(app: AppHandle, id: &str) {
    match id {
        "integration_github" => poll_github(app).await,
        "integration_calendar" => poll_calendar(app).await,
        _ => {}
    }
}

fn status_error(code: u16, unauthorised_hint: &str) -> String {
    match code {
        401 => "API key inválida (401)".into(),
        403 => unauthorised_hint.into(),
        _ => format!("Error de la API {code}"),
    }
}

// ── GitHub ────────────────────────────────────────────────────────────────────

/// Your open pull requests in one GraphQL call: their review decision, the
/// latest review from each reviewer, and the CI rollup of the head commit.
const PRS_QUERY: &str = "query { viewer { login pullRequests(first: 10, states: OPEN, \
orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { number title url updatedAt isDraft \
repository { nameWithOwner } reviewDecision \
latestReviews(first: 10) { nodes { id state author { login } } } \
commits(last: 1) { nodes { commit { oid statusCheckRollup { state } } } } } } } }";

/// Stars and repos change slowly; they're refreshed at most this often.
const STATS_EVERY: Duration = Duration::from_secs(300);

/// What the last poll knew about one PR, to tell a new review or a finished CI
/// run from one already announced.
#[derive(Default)]
struct PrSeen {
    reviews: std::collections::HashSet<String>,
    /// (head commit, CI rollup state) last time round.
    ci: Option<(String, String)>,
}

struct GithubState {
    /// Keyed by PR url. None until the first poll has filled it silently.
    prs: Option<std::collections::HashMap<String, PrSeen>>,
    stats: Option<(i64, i64, std::time::Instant)>,
}

static GITHUB: std::sync::LazyLock<Mutex<GithubState>> =
    std::sync::LazyLock::new(|| Mutex::new(GithubState { prs: None, stats: None }));

fn github_get(http: &reqwest::Client, token: &str, url: &str) -> reqwest::RequestBuilder {
    http.get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Coucou")
}

/// (repos, stars), from the cache when it is fresh enough.
async fn github_stats(http: &reqwest::Client, token: &str) -> Result<(i64, i64), u16> {
    if let Some((repos, stars, at)) = GITHUB.lock().unwrap().stats {
        if at.elapsed() < STATS_EVERY {
            return Ok((repos, stars));
        }
    }
    let response = github_get(http, token, "https://api.github.com/user").send().await.map_err(|_| 0u16)?;
    if !response.status().is_success() {
        return Err(response.status().as_u16());
    }
    let json: Value = response.json().await.unwrap_or(json!({}));
    let public = json.get("public_repos").and_then(Value::as_i64).unwrap_or(0);
    let private = json
        .get("owned_private_repos")
        .or_else(|| json.get("total_private_repos"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let repos = github_get(http, token, "https://api.github.com/user/repos?per_page=100&affiliation=owner&sort=pushed")
        .send()
        .await;
    let stars: i64 = match repos {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|list| {
                list.iter()
                    .filter_map(|r| r.get("stargazers_count").and_then(Value::as_i64))
                    .sum()
            })
            .unwrap_or(0),
        _ => 0,
    };
    GITHUB.lock().unwrap().stats = Some((public + private, stars, std::time::Instant::now()));
    Ok((public + private, stars))
}

/// One open PR as the card shows it, worst news first: CI failing, changes
/// requested, CI running, approved, waiting for a review.
fn pr_status(review_decision: &str, ci: &str, draft: bool) -> &'static str {
    match (ci, review_decision) {
        ("FAILURE" | "ERROR", _) => "ci_failed",
        (_, "CHANGES_REQUESTED") => "changes",
        ("PENDING" | "EXPECTED", _) => "ci_running",
        (_, "APPROVED") => "approved",
        _ if draft => "draft",
        _ => "review",
    }
}

/// Something worth a badge, most important first.
fn pr_event_rank(kind: &str) -> u8 {
    match kind {
        "ci_failed" => 0,
        "CHANGES_REQUESTED" => 1,
        "APPROVED" => 2,
        "ci_passed" => 3,
        _ => 4,
    }
}

async fn poll_github(app: AppHandle) {
    let Some(token) = secrets::get("github-token") else { return };
    let http = client();

    let (repos, stars) = match github_stats(&http, &token).await {
        Ok(v) => v,
        Err(0) => return,
        Err(code) => {
            emit(&app, IntegrationUpdate {
                id: "integration_github",
                data: json!({}),
                error: Some(status_error(code, "Al token le faltan permisos")),
                event: None,
            });
            return;
        }
    };

    // A token that can't read pull requests still gets the stats card.
    let graph = http
        .post("https://api.github.com/graphql")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "Coucou")
        .json(&json!({ "query": PRS_QUERY }))
        .send()
        .await;
    let viewer = match graph {
        Ok(r) if r.status().is_success() => {
            r.json::<Value>().await.ok().and_then(|v| v.pointer("/data/viewer").cloned())
        }
        _ => None,
    };

    let mut prs: Vec<Value> = Vec::new();
    // (rank, label, detail, success)
    let mut events: Vec<(u8, String, String, bool)> = Vec::new();
    if let Some(viewer) = viewer {
        let me = viewer.get("login").and_then(Value::as_str).unwrap_or("").to_string();
        let nodes = viewer
            .pointer("/pullRequests/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut state = GITHUB.lock().unwrap();
        let first_poll = state.prs.is_none();
        let seen = state.prs.get_or_insert_with(Default::default);
        let mut still_open = std::collections::HashSet::new();

        for pr in &nodes {
            let str_at = |ptr: &str| pr.pointer(ptr).and_then(Value::as_str).unwrap_or("").to_string();
            let url = str_at("/url");
            if url.is_empty() {
                continue;
            }
            still_open.insert(url.clone());
            let title = str_at("/title");
            let repo = str_at("/repository/nameWithOwner");
            let number = pr.get("number").and_then(Value::as_i64).unwrap_or(0);
            let decision = str_at("/reviewDecision");
            let commit = pr.pointer("/commits/nodes/0/commit");
            let oid = commit.and_then(|c| c.get("oid")).and_then(Value::as_str).unwrap_or("").to_string();
            let ci = commit
                .and_then(|c| c.pointer("/statusCheckRollup/state"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let draft = pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
            let where_ = format!("{repo}#{number}");

            let known = seen.contains_key(&url);
            let entry = seen.entry(url.clone()).or_default();
            let reviews = pr
                .pointer("/latestReviews/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for review in &reviews {
                let Some(id) = review.get("id").and_then(Value::as_str) else { continue };
                let fresh = entry.reviews.insert(id.to_string());
                let author = review.pointer("/author/login").and_then(Value::as_str).unwrap_or("");
                let kind = review.get("state").and_then(Value::as_str).unwrap_or("");
                // Only someone else's review is news, and never on the first look.
                if !fresh || !known || first_poll || author == me {
                    continue;
                }
                let label = match kind {
                    "APPROVED" => format!("Aprobado · {title}"),
                    "CHANGES_REQUESTED" => format!("Cambios pedidos · {title}"),
                    "COMMENTED" => format!("Comentario · {title}"),
                    _ => continue,
                };
                events.push((pr_event_rank(kind), label, format!("{where_} · por {author}"), kind != "CHANGES_REQUESTED"));
            }

            let finished = matches!(ci.as_str(), "SUCCESS" | "FAILURE" | "ERROR");
            let now = (oid.clone(), ci.clone());
            if finished && known && !first_poll && entry.ci.as_ref() != Some(&now) {
                if ci == "SUCCESS" {
                    events.push((pr_event_rank("ci_passed"), format!("CI pasó · {title}"), where_.clone(), true));
                } else {
                    events.push((pr_event_rank("ci_failed"), format!("CI falló · {title}"), where_.clone(), false));
                }
            }
            entry.ci = Some(now);

            prs.push(json!({
                "title": title,
                "repo": repo,
                "number": number,
                "url": url,
                "status": pr_status(&decision, &ci, draft),
                "updatedAt": str_at("/updatedAt"),
            }));
        }
        // Merged or closed PRs are forgotten, so the map can't grow forever.
        seen.retain(|url, _| still_open.contains(url));
    }

    events.sort_by_key(|e| e.0);
    let event = events.into_iter().next().map(|(_, label, detail, success)| IntegrationEvent {
        success,
        label,
        detail: Some(detail),
    });

    emit(&app, IntegrationUpdate {
        id: "integration_github",
        data: json!({ "totalRepos": repos, "totalStars": stars, "prs": prs }),
        error: None,
        event,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_pr_news_wins() {
        assert_eq!(pr_status("APPROVED", "FAILURE", false), "ci_failed");
        assert_eq!(pr_status("CHANGES_REQUESTED", "PENDING", false), "changes");
        assert_eq!(pr_status("APPROVED", "PENDING", false), "ci_running");
        assert_eq!(pr_status("APPROVED", "SUCCESS", false), "approved");
        assert_eq!(pr_status("REVIEW_REQUIRED", "SUCCESS", true), "draft");
        assert_eq!(pr_status("", "", false), "review");
        assert!(pr_event_rank("ci_failed") < pr_event_rank("APPROVED"));
    }
}

// ── Google Calendar (secret iCal address) ─────────────────────────────────────

/// The feed of a whole calendar is rarely more than a few MB; past this, stop.
const MAX_ICAL_BYTES: usize = 8 << 20;

async fn poll_calendar(app: AppHandle) {
    let Some(url) = secrets::get("calendar-ical-url") else { return };
    let url = url.trim().replacen("webcal://", "https://", 1);
    let fail = |error: String| {
        emit(&app, IntegrationUpdate {
            id: "integration_calendar",
            data: json!({}),
            error: Some(error),
            event: None,
        })
    };
    if !url.starts_with("https://") {
        fail("La dirección debe empezar con https://".into());
        return;
    }
    let response = match client().get(&url).header("User-Agent", "Coucou").send().await {
        Ok(r) => r,
        Err(e) => {
            fail(format!("Sin conexión: {e}"));
            return;
        }
    };
    if !response.status().is_success() {
        fail(status_error(response.status().as_u16(), "Google no aceptó la dirección"));
        return;
    }
    let Ok(bytes) = response.bytes().await else { return };
    if bytes.len() > MAX_ICAL_BYTES {
        fail("El calendario es demasiado grande".into());
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    if !text.contains("BEGIN:VCALENDAR") {
        fail("Eso no parece una dirección iCal".into());
        return;
    }
    emit(&app, IntegrationUpdate {
        id: "integration_calendar",
        data: json!({ "events": crate::calendar::parse(&text, None) }),
        error: None,
        event: None,
    });
}
