// Chats that run on the user's own Claude Code, so they use the Claude account
// it is signed in with (Pro, Max…) instead of an API key — and get its MCP
// connectors along the way.
//
// Each turn is one `claude -p` run in stream-json mode. The text streams to the
// island as `code-chat` events; the conversation carries on with `--resume`.
//
// Two channels, two sets of rights:
// * `chat`, the island's chat: read-only. It may read files and search, never
//   write, run commands or send anything.
// * `library`, the library's chat: it may write, but only inside the library
//   folder (its working directory, under `acceptEdits`). No shell at all.
//
// The runs set COUCOU_INTERNAL so coucou-hook ignores them: they are not the
// user's sessions and must not show up on the island.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::island::WINDOW_LABEL;

/// A turn that takes longer than this is stopped: the island can't wait forever.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);

const CHAT_PROMPT: &str = "You are Mochi, a small assistant living at the top of the user's Windows screen, \
answering through the user's own Claude Code. Respond in the user's language. Be helpful and complete but concise: \
the answer is shown in a small chat bubble. No markdown formatting (no **, no ##, no bullet dashes, no tables). \
Use plain text with line breaks. You can read files the user attaches and search, but you cannot change anything.";

const LIBRARY_PROMPT: &str = "You manage the user's library, which is your working directory. \
Layout: one folder per project, named in lowercase-with-dashes. Each project folder has instrucciones, accesos and \
documentos subfolders and an optional proyecto.json with ruta (the repo folder on disk) and color (a hex colour). \
An instruction is a .md file: a front matter block between --- lines with titulo, tipo (prompt for how the user wants \
Claude to work, entorno for how to bring an environment up), etiquetas (comma separated) and orden (a number), then the \
text exactly as the user gave it. An access is a .md file with titulo and entorno (dev, qa...) in the front matter, then \
one clave: valor line per value (host, puerto, base, usuario, password, url...): development credentials the user wants \
stored as given, one database or service per file. Documents (.docx, .pdf, .xlsx) live as files in documentos; the user \
adds them by dropping them on the library, and you cannot copy them, so if one is attached tell the user to drop it there. \
When the user asks to save something, decide the project (ask briefly if it is really unclear), the kind (instruccion or \
acceso) and a short title, then write the file with a short dashed file name. When a text file is attached, read it and \
store its content the same way. Never write outside your working directory and never delete anything unless the user \
asks for it. Answer in the user's language in one or two short plain sentences, and when you saved something start with: \
Guardado en <proyecto> > <Instrucciones|Accesos>: <titulo>.";

/// Read-only tools the island's chat may use without asking.
const CHAT_TOOLS: &[&str] = &[
    "Read", "Glob", "Grep", "WebSearch", "WebFetch",
    "mcp__claude_ai_Slack__slack_read_channel",
    "mcp__claude_ai_Slack__slack_read_thread",
    "mcp__claude_ai_Slack__slack_read_canvas",
    "mcp__claude_ai_Slack__slack_read_file",
    "mcp__claude_ai_Slack__slack_read_user_profile",
    "mcp__claude_ai_Slack__slack_search_channels",
    "mcp__claude_ai_Slack__slack_search_public",
    "mcp__claude_ai_Slack__slack_search_public_and_private",
    "mcp__claude_ai_Slack__slack_search_users",
    "mcp__claude_ai_Slack__slack_list_user_channels",
    "mcp__claude_ai_Slack__slack_list_channel_members",
    "mcp__claude_ai_Slack__slack_get_reactions",
];

/// Never, in either channel.
const DENIED_TOOLS: &[&str] = &["Bash", "PowerShell", "KillShell", "BashOutput"];

const LIBRARY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Write", "Edit", "MultiEdit"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    Chat,
    Library,
}

impl Channel {
    fn key(self) -> &'static str {
        match self {
            Channel::Chat => "chat",
            Channel::Library => "library",
        }
    }
}

#[derive(Default)]
struct Run {
    /// Claude Code's session id, for `--resume` on the next turn.
    session: Option<String>,
    child: Option<Arc<Mutex<Child>>>,
    /// Bumped by reset(), so a turn that was cancelled doesn't store its session.
    generation: u64,
}

#[derive(Default)]
pub struct CodeChats {
    runs: Mutex<HashMap<Channel, Run>>,
}

impl CodeChats {
    /// Forgets the conversation and stops a turn in flight.
    pub fn reset(&self, channel: Channel) {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.entry(channel).or_default();
        run.session = None;
        run.generation += 1;
        if let Some(child) = run.child.take() {
            let _ = child.lock().unwrap().kill();
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StreamEvent {
    channel: &'static str,
    /// "delta" (more reply text) or "status" (what Claude is doing, e.g. a tool).
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeReply {
    pub text: String,
}

/// Where the Claude Code CLI lives: the native installer's spot first, then %PATH%.
pub fn find_claude() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let native = PathBuf::from(home).join(r".local\bin\claude.exe");
        if native.is_file() {
            return Some(native);
        }
    }
    crate::find_on_path("claude")
}

/// One turn. `attachments` are file paths Claude Code should read; `cwd` is the
/// run's working directory (the library folder for the library channel).
pub fn run_turn(
    app: &AppHandle,
    chats: &CodeChats,
    channel: Channel,
    prompt: &str,
    attachments: &[String],
    cwd: &Path,
) -> Result<CodeReply, String> {
    let exe = find_claude().ok_or_else(|| {
        "No encontré Claude Code. Instálalo (claude.ai/code) o cambia el motor del chat a API key en Ajustes."
            .to_string()
    })?;

    let (session, generation) = {
        let mut runs = chats.runs.lock().unwrap();
        let run = runs.entry(channel).or_default();
        (run.session.clone(), run.generation)
    };

    let mut cmd = Command::new(&exe);
    cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
    if let Some(id) = &session {
        cmd.args(["--resume", id]);
    }
    let (tools, system) = match channel {
        Channel::Chat => (CHAT_TOOLS, CHAT_PROMPT),
        Channel::Library => (LIBRARY_TOOLS, LIBRARY_PROMPT),
    };
    cmd.arg("--allowedTools").args(tools);
    cmd.arg("--disallowedTools").args(DENIED_TOOLS);
    if channel == Channel::Library {
        // Edits are accepted inside the working directory (the library) only.
        cmd.args(["--permission-mode", "acceptEdits"]);
    }
    // Attached files live in Coucou's inbox; let Claude Code read there too.
    let mut dirs: Vec<PathBuf> = attachments
        .iter()
        .filter_map(|a| Path::new(a).parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    for dir in &dirs {
        cmd.arg("--add-dir").arg(dir);
    }
    cmd.arg("--append-system-prompt").arg(system);
    cmd.current_dir(cwd)
        .env("COUCOU_INTERNAL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(crate::CREATE_NO_WINDOW);

    let mut child = cmd.spawn().map_err(|e| format!("No se pudo abrir Claude Code: {e}"))?;

    // The prompt goes in on stdin: no quoting rules to get wrong on the way.
    let mut text = prompt.trim().to_string();
    for path in attachments {
        text.push_str(&format!("\n\nArchivo adjunto: @\"{path}\""));
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let stdout = child.stdout.take().ok_or("Claude Code no respondió.")?;
    let stderr = child.stderr.take();

    let child = Arc::new(Mutex::new(child));
    chats.runs.lock().unwrap().entry(channel).or_default().child = Some(child.clone());

    // The watchdog: a turn that hangs is killed rather than left running.
    let watchdog = {
        let child = child.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if rx.recv_timeout(TURN_TIMEOUT).is_err() {
                let _ = child.lock().unwrap().kill();
            }
        });
        tx
    };
    let errors = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut out);
        }
        out
    });

    let mut streamed = String::new();
    let mut result: Option<(String, bool)> = None;
    let mut new_session: Option<String> = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(id) = event.get("session_id").and_then(Value::as_str) {
            new_session = Some(id.to_string());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let e = &event["event"];
                if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                    if let Some(t) = e["delta"]["text"].as_str() {
                        streamed.push_str(t);
                        emit(app, channel, "delta", t);
                    }
                } else if e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use" {
                    let name = e["content_block"]["name"].as_str().unwrap_or("");
                    emit(app, channel, "status", &tool_status(name));
                }
            }
            Some("result") => {
                let is_error = event["is_error"].as_bool().unwrap_or(false);
                let text = event["result"].as_str().unwrap_or("").to_string();
                result = Some((text, is_error));
            }
            _ => {}
        }
    }

    let _ = watchdog.send(());
    let status = child.lock().unwrap().wait();
    let stderr_text = errors.join().unwrap_or_default();

    {
        let mut runs = chats.runs.lock().unwrap();
        let run = runs.entry(channel).or_default();
        run.child = None;
        if run.generation != generation {
            return Err("Conversación reiniciada.".into());
        }
        if new_session.is_some() {
            run.session = new_session;
        }
    }

    match result {
        Some((text, false)) => {
            let text = if text.trim().is_empty() { streamed } else { text };
            Ok(CodeReply { text: text.trim().to_string() })
        }
        Some((text, true)) => Err(friendly_error(&text)),
        None => {
            let detail = stderr_text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            if status.map(|s| s.success()).unwrap_or(false) && !streamed.trim().is_empty() {
                Ok(CodeReply { text: streamed.trim().to_string() })
            } else if detail.is_empty() {
                Err("Claude Code se detuvo sin responder.".into())
            } else {
                Err(friendly_error(detail))
            }
        }
    }
}

fn emit(app: &AppHandle, channel: Channel, kind: &'static str, text: &str) {
    let _ = app.emit_to(
        WINDOW_LABEL,
        "code-chat",
        StreamEvent { channel: channel.key(), kind, text: text.to_string() },
    );
}

/// What the bubble says while a tool runs.
fn tool_status(name: &str) -> String {
    let label = match name {
        "Read" => "Leyendo el archivo…",
        "Glob" | "Grep" => "Buscando…",
        "Write" | "Edit" | "MultiEdit" => "Guardando…",
        "WebSearch" => "Buscando en la web…",
        "WebFetch" => "Leyendo la página…",
        n if n.contains("Slack") => "Revisando Slack…",
        n if n.starts_with("mcp__") => "Consultando un conector…",
        _ => "Trabajando…",
    };
    label.to_string()
}

fn friendly_error(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("not logged in") || lower.contains("/login") || lower.contains("authentication") {
        return "Claude Code no tiene sesión iniciada. Abre una terminal y corre: claude".into();
    }
    if lower.contains("limit") {
        return format!("Llegaste al límite de uso de tu plan. {text}");
    }
    let short: String = text.chars().take(240).collect();
    format!("Claude Code: {short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_read_only_in_the_chat() {
        for t in CHAT_TOOLS {
            assert!(!["Write", "Edit", "Bash", "PowerShell"].contains(t));
            assert!(!t.contains("send") && !t.contains("create") && !t.contains("update"));
        }
        assert!(!LIBRARY_TOOLS.contains(&"Bash"));
    }
}
