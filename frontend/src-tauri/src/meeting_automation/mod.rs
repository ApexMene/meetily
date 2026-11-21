//! Fork: meeting automation (Windows). Everything is off until the user turns it on in
//! Settings › General › Automatic Meetings.
//!
//! - `detector`: starts/stops recording while a meeting app holds the microphone.
//! - `bar`: the always-on-top meeting bar (timer, open transcript, stop).
//! - `shortcut`: a global start/stop shortcut.
//! - `retention`: deletes old recording audio (transcripts are kept).
//! - `autostart`: starts Meetily hidden in the tray when Windows starts.
//!
//! Settings: `<app data>/meeting-automation.json`.

mod autostart;
pub mod bar;
mod detector;
mod retention;
mod shortcut;

pub use autostart::AUTOSTART_FLAG;

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Start/stop recording automatically when a meeting app uses the microphone.
    pub enabled: bool,
    pub start_debounce_secs: u64,
    pub stop_grace_secs: u64,
    /// Case-insensitive fragments of the Windows registry key of the app (package name or exe path).
    pub apps: Vec<String>,
    /// Global start/stop shortcut, e.g. "Ctrl+Alt+M". Empty = none.
    pub shortcut: String,
    /// Recording audio is deleted this many days after the meeting (transcripts are kept). 0 = never.
    pub audio_retention_days: u32,
    /// Start Meetily hidden in the tray when Windows starts.
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            start_debounce_secs: 5,
            stop_grace_secs: 8,
            apps: [
                "msteams", "teams.exe", "zoom", "webex", "ciscocollabhost", "slack", "discord", "skype",
                "whatsapp", "gotomeeting", "ringcentral",
                // Browser calls (Google Meet, Teams web…): any site using the microphone counts.
                "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera",
            ]
            .iter()
            .map(|app| app.to_string())
            .collect(),
            shortcut: String::new(),
            audio_retention_days: 0,
            start_with_windows: false,
        }
    }
}

// Not `auto-meeting.json`: the first version wrote it with everything turned on.
static SETTINGS: Store<Settings> = Store::new("meeting-automation.json");

pub fn settings<R: Runtime>(app: &AppHandle<R>) -> Settings {
    SETTINGS.get(app)
}

/// "   (Ctrl+Alt+M)" for menu labels; empty when no shortcut is active.
pub fn shortcut_hint<R: Runtime>(app: &AppHandle<R>) -> String {
    let shortcut = if cfg!(windows) { settings(app).shortcut } else { String::new() };
    match shortcut.trim() {
        "" => String::new(),
        key => format!("   ({})", key),
    }
}

/// Starts the enabled features. Windows only: other platforms are unaffected.
pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !cfg!(windows) {
        return;
    }
    bar::register_capability(&app);
    let settings = settings(&app);
    shortcut::register_at_startup(&app, &settings.shortcut);
    autostart::apply(settings.start_with_windows);
    if settings.start_with_windows {
        autostart::pin_tray_icon_later();
    }
    retention::start(app.clone());
    detector::start(app);
}

/// What the settings page shows and edits.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationSettings {
    /// False on platforms without automation (the card is hidden).
    #[serde(default)]
    pub supported: bool,
    pub enabled: bool,
    pub shortcut: String,
    pub audio_retention_days: u32,
    pub start_with_windows: bool,
}

#[tauri::command]
pub async fn automation_settings_get<R: Runtime>(app: AppHandle<R>) -> AutomationSettings {
    let settings = settings(&app);
    AutomationSettings {
        supported: cfg!(windows),
        enabled: settings.enabled,
        shortcut: settings.shortcut,
        audio_retention_days: settings.audio_retention_days,
        start_with_windows: settings.start_with_windows,
    }
}

#[tauri::command]
pub async fn automation_settings_set<R: Runtime>(app: AppHandle<R>, settings: AutomationSettings) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Automatic meetings are only available on Windows".into());
    }
    let mut next = self::settings(&app);
    let shortcut = settings.shortcut.trim().to_string();
    if shortcut != next.shortcut {
        shortcut::replace(&app, &next.shortcut, &shortcut)?;
        next.shortcut = shortcut;
    }
    if settings.start_with_windows != next.start_with_windows {
        autostart::apply(settings.start_with_windows);
        if settings.start_with_windows {
            autostart::pin_tray_icon_later();
        }
    }
    next.enabled = settings.enabled;
    next.start_with_windows = settings.start_with_windows;
    next.audio_retention_days = settings.audio_retention_days.min(3650);
    SETTINGS.set(&app, next)?;
    crate::tray::update_tray_menu(&app);
    log::info!(
        "meeting-automation: settings saved (detection {}, audio kept {} days)",
        settings.enabled,
        settings.audio_retention_days
    );
    retention::run(&app).await;
    Ok(())
}
