// Preferences, stored as plain JSON in %APPDATA%\Coucou\settings.json.
// No secret ever lands here — API keys live in the Windows Credential Manager.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// `serde(default)` on the whole struct: a settings.json written by an older
/// build (one field fewer) must load with that field defaulted, not be thrown
/// away wholesale — that silently reset every preference after an update.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    /// Seconds before the open island closes; one of AUTO_CLOSE_CHOICES.
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    pub model: String,
    /// Minutes a Claude Code session may wait on the user before Mochi nags. 0 = off.
    pub waiting_alert_minutes: f64,
    /// Which kinds of sound play, by cue id (SOUND_CUES in sound.ts). A cue
    /// that isn't here uses its default, so new cues need no migration.
    pub sound_cues: HashMap<String, bool>,
    /// "claudeCode" (the user's Claude Code and its account) or "api" (API key).
    pub chat_engine: String,
    /// How the library's buttons hand a prompt to the terminal.
    pub paste_modes: PasteModes,
    /// "Acceso desde el iPhone": the phone server (phone.rs) runs only while on.
    pub phone_enabled: bool,
    /// Folders whose direct subfolders the iPhone may open a session in.
    pub phone_launch_roots: Vec<String>,
    /// "Permitir que el iPhone abra sesiones sin permisos": the phone may
    /// launch Claude Code with --dangerously-skip-permissions. Off by default.
    pub phone_skip_permissions: bool,
}

/// The library's three ways to take a prompt to the terminal. At least one is
/// always on: `normalized()` turns "copy" back on if all three were switched off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PasteModes {
    /// Copy to the clipboard.
    pub copy: bool,
    /// Copy and bring Warp to the front.
    pub warp: bool,
    /// Copy, bring Warp to the front and press Ctrl+V (never Enter).
    pub paste: bool,
}

impl Default for PasteModes {
    fn default() -> Self {
        Self { copy: true, warp: true, paste: false }
    }
}

/// The only auto-close delays the settings offer (AUTO_CLOSE_CHOICES in state.ts).
const AUTO_CLOSE_CHOICES: [f64; 4] = [2.0, 3.0, 5.0, 10.0];

impl Settings {
    pub fn normalized(mut self) -> Self {
        // Older builds offered 5–120 s: snap to the closest choice still offered.
        let v = self.auto_close_interval;
        if !AUTO_CLOSE_CHOICES.contains(&v) {
            self.auto_close_interval = AUTO_CLOSE_CHOICES
                .into_iter()
                .min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs()))
                .unwrap_or(5.0);
        }
        let m = &mut self.paste_modes;
        if !m.copy && !m.warp && !m.paste {
            m.copy = true;
        }
        if self.chat_engine != "api" {
            self.chat_engine = "claudeCode".into();
        }
        self
    }
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 5.0,
            absence_interval: 180.0,
            active_integrations: vec!["integration_github".into()],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
            waiting_alert_minutes: 2.0,
            sound_cues: HashMap::new(),
            chat_engine: "claudeCode".into(),
            paste_modes: PasteModes::default(),
            phone_enabled: false,
            phone_launch_roots: crate::launch::default_roots(),
            phone_skip_permissions: false,
        }
    }
}

/// %APPDATA%\Coucou
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

/// %LOCALAPPDATA%\Coucou — where coucou-hook.exe and the log live.
pub fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join("coucou-hook.exe")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// Pills this build still has; older settings may name removed ones.
const INTEGRATION_IDS: &[&str] = &["integration_github"];

pub fn load() -> Settings {
    let mut settings: Settings = match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    };
    settings
        .active_integrations
        .retain(|id| INTEGRATION_IDS.contains(&id.as_str()));
    settings.normalized()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Temp file + rename: a PC switched off mid-write leaves the old file intact
    // instead of a truncated one that would load as defaults.
    let path = settings_path();
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, &path)
}
