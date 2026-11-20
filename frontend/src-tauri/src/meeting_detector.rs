//! Fork: starts and stops recording automatically when a meeting app uses the microphone.
//!
//! Windows records which apps hold the microphone in the registry
//! (`HKCU\...\CapabilityAccessManager\ConsentStore\microphone`): while an app is using it,
//! `LastUsedTimeStop` is 0. We poll that every few seconds:
//! - a meeting app (allowlist) holds the mic for `start_debounce_secs` -> start recording and
//!   notify "Trascrizione automatica avviata";
//! - no meeting app holds the mic for `stop_grace_secs` -> stop recording (the meeting is saved
//!   and the Notion agent picks it up);
//! - if the user stops a recording by hand, we do not restart until the mic is released.
//! Meetily's own microphone use is ignored.
//!
//! Settings: `<app data>/auto-meeting.json` (written with defaults on first run).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, Runtime};

const POLL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorConfig {
    pub enabled: bool,
    pub start_debounce_secs: u64,
    pub stop_grace_secs: u64,
    /// Case-insensitive fragments of the registry key (package name or exe path).
    pub apps: Vec<String>,
    /// Global shortcut that starts/stops a recording from anywhere (e.g. "Ctrl+Alt+M").
    /// Empty = no shortcut. Read at app start.
    pub shortcut: String,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start_debounce_secs: 5,
            stop_grace_secs: 8,
            apps: [
                "msteams", "teams.exe", "zoom", "webex", "ciscocollabhost", "slack", "discord",
                "skype", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera",
                "whatsapp", "gotomeeting", "ringcentral",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            shortcut: "Ctrl+Alt+M".to_string(),
        }
    }
}

fn config_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("auto-meeting.json"))
}

fn load_config<R: Runtime>(app: &AppHandle<R>) -> DetectorConfig {
    let Some(path) = config_path(app) else { return DetectorConfig::default() };
    match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<DetectorConfig>(&b).ok()) {
        Some(config) => config,
        None => {
            let config = DetectorConfig::default();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap_or_default());
            config
        }
    }
}

/// Registry key names of the apps currently holding the microphone.
#[cfg(windows)]
fn apps_using_microphone() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const ROOT: &str =
        r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    fn in_use(key: &RegKey) -> bool {
        let start: u64 = key.get_value("LastUsedTimeStart").unwrap_or(0);
        let stop: u64 = key.get_value("LastUsedTimeStop").unwrap_or(1);
        start > 0 && stop == 0
    }
    let mut found = Vec::new();
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(ROOT) else { return found };
    for name in root.enum_keys().flatten() {
        if name == "NonPackaged" {
            if let Ok(non_packaged) = root.open_subkey(&name) {
                for exe in non_packaged.enum_keys().flatten() {
                    if non_packaged.open_subkey(&exe).is_ok_and(|k| in_use(&k)) {
                        found.push(exe);
                    }
                }
            }
        } else if root.open_subkey(&name).is_ok_and(|k| in_use(&k)) {
            found.push(name);
        }
    }
    found
}

#[cfg(not(windows))]
fn apps_using_microphone() -> Vec<String> {
    Vec::new()
}

/// A short readable name for a registry key ("C:#...#ms-teams.exe" -> "ms-teams").
fn display_name(key: &str) -> String {
    let last = key.rsplit('#').next().unwrap_or(key);
    let last = last.split('_').next().unwrap_or(last);
    last.trim_end_matches(".exe").trim_end_matches(".EXE").to_string()
}

fn meeting_app(apps_in_use: &[String], config: &DetectorConfig, own_exe: &str) -> Option<String> {
    apps_in_use
        .iter()
        .filter(|key| own_exe.is_empty() || !key.to_lowercase().ends_with(own_exe))
        .find(|key| {
            let lower = key.to_lowercase();
            config.apps.iter().any(|fragment| !fragment.is_empty() && lower.contains(&fragment.to_lowercase()))
        })
        .map(|key| display_name(key))
}

const PILL_LABEL: &str = "meeting-pill";
const PILL_WIDTH: f64 = 520.0;
const PILL_HEIGHT: f64 = 76.0;
/// Like Notion's bar: the recording bar stays for the whole meeting; the "stopped" bar hides
/// itself after a few seconds.
const PILL_STOPPED_SECS: u64 = 12;

const PILL_COMPACT_WIDTH: f64 = 168.0;
const PILL_COMPACT_HEIGHT: f64 = 52.0;

/// Where the user left the bar and whether they shrank it (`<app data>/meeting-pill.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct PillPrefs {
    x: Option<f64>,
    y: Option<f64>,
    compact: bool,
}

fn pill_prefs_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("meeting-pill.json"))
}

fn load_pill_prefs<R: Runtime>(app: &AppHandle<R>) -> PillPrefs {
    pill_prefs_path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_pill_prefs<R: Runtime>(app: &AppHandle<R>, prefs: &PillPrefs) {
    if let Some(path) = pill_prefs_path(app) {
        let _ = std::fs::write(path, serde_json::to_vec_pretty(prefs).unwrap_or_default());
    }
}

fn pill_size(compact: bool) -> (f64, f64) {
    if compact { (PILL_COMPACT_WIDTH, PILL_COMPACT_HEIGHT) } else { (PILL_WIDTH, PILL_HEIGHT) }
}

/// True when the logical point lies on one of the connected monitors.
fn on_some_monitor<R: Runtime>(app: &AppHandle<R>, x: f64, y: f64) -> bool {
    app.available_monitors().unwrap_or_default().iter().any(|m| {
        let scale = m.scale_factor();
        let left = m.position().x as f64 / scale;
        let top = m.position().y as f64 / scale;
        let width = m.size().width as f64 / scale;
        let height = m.size().height as f64 / scale;
        x >= left - 20.0 && x < left + width - 40.0 && y >= top - 20.0 && y < top + height - 30.0
    })
}

/// Shows the Notion-style bar: where the user last left it, otherwise at the top centre of
/// the primary screen. `mode` is "recording" or "stopped".
pub fn show_pill<R: Runtime>(app: &AppHandle<R>, mode: &str, app_name: &str) {
    let generation = PILL_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let prefs = load_pill_prefs(app);
    let safe_name: String = app_name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let since = chrono::Utc::now().timestamp_millis();
    let url = format!(
        "pill.html?mode={}&app={}&since={}&compact={}&key={}",
        mode, safe_name, since, if prefs.compact { 1 } else { 0 },
        load_config(app).shortcut.trim().replace('+', "%2B")
    );
    let (width, height) = pill_size(prefs.compact);
    if mode == "stopped" {
        schedule_pill_close(app, generation);
    }
    // Reuse the open bar (closing is asynchronous: a new window with the same label would fail).
    if let Some(window) = app.get_webview_window(PILL_LABEL) {
        let _ = window.eval(&format!("location.replace({:?})", format!("/{}", url)));
        let _ = window.set_size(tauri::LogicalSize::new(width, height));
        return;
    }
    let default_pos = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let w = monitor.size().width as f64 / scale;
            let left = monitor.position().x as f64 / scale;
            let top = monitor.position().y as f64 / scale;
            (left + (w - PILL_WIDTH) / 2.0, top + 100.0) // below the meeting app title bar and other apps' bars
        })
        .unwrap_or((400.0, 100.0));
    let (x, y) = match (prefs.x, prefs.y) {
        (Some(x), Some(y)) if on_some_monitor(app, x, y) => (x, y),
        _ => default_pos,
    };
    let built = tauri::WebviewWindowBuilder::new(app, PILL_LABEL, tauri::WebviewUrl::App(url.into()))
        .title("Meetily")
        .inner_size(width, height)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .build();
    match built {
        Ok(window) => {
            // Remember where the user drags the bar.
            let handle = app.clone();
            let scale = window.scale_factor().unwrap_or(1.0);
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Moved(pos) = event {
                    let mut prefs = load_pill_prefs(&handle);
                    prefs.x = Some(pos.x as f64 / scale);
                    prefs.y = Some(pos.y as f64 / scale);
                    save_pill_prefs(&handle, &prefs);
                }
            });
        }
        Err(error) => log::warn!("meeting-detector: cannot show bar: {}", error),
    }
}

/// Bar: move it around (mouse held on the bar).
#[tauri::command]
pub async fn meeting_pill_drag<R: Runtime>(app: AppHandle<R>) {
    if let Some(window) = app.get_webview_window(PILL_LABEL) {
        let _ = window.start_dragging();
    }
}

/// Bar: shrink to the small dot + timer, or grow back. Remembered for the next meetings.
#[tauri::command]
pub async fn meeting_pill_compact<R: Runtime>(app: AppHandle<R>, compact: bool) {
    let mut prefs = load_pill_prefs(&app);
    prefs.compact = compact;
    save_pill_prefs(&app, &prefs);
    if let Some(window) = app.get_webview_window(PILL_LABEL) {
        let (width, height) = pill_size(compact);
        let _ = window.set_size(tauri::LogicalSize::new(width, height));
    }
}

static PILL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hides the "stopped" bar after a few seconds, unless a newer bar replaced it meanwhile.
fn schedule_pill_close<R: Runtime>(app: &AppHandle<R>, generation: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(PILL_STOPPED_SECS)).await;
        if PILL_GENERATION.load(std::sync::atomic::Ordering::SeqCst) == generation {
            close_pill(&app);
        }
    });
}

pub fn close_pill<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(PILL_LABEL) {
        let _ = window.close();
    }
}

/// Bar button: open the app on the live transcript.
#[tauri::command]
pub async fn meeting_pill_open<R: Runtime>(app: AppHandle<R>) {
    close_pill(&app);
    crate::tray::focus_main_window(&app);
    if crate::audio::recording_commands::is_recording().await {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.eval("if (window.location.pathname !== '/') window.location.assign('/')");
        }
    }
}

/// Bar button: stop the recording now.
#[tauri::command]
pub async fn meeting_pill_stop<R: Runtime>(app: AppHandle<R>) {
    close_pill(&app);
    if crate::audio::recording_commands::is_recording().await {
        crate::tray::stop_recording_from_app(&app);
    }
}

#[tauri::command]
pub async fn meeting_pill_close<R: Runtime>(app: AppHandle<R>) {
    close_pill(&app);
}


#[derive(Debug, PartialEq)]
enum Phase {
    /// Waiting for a meeting app to take the mic.
    Idle,
    /// We started this recording.
    AutoRecording,
    /// Recording that we did not start, or the user stopped ours: wait for the mic to be released.
    HandsOff,
}

/// "   (Ctrl+Alt+M)" for menu labels, empty when no shortcut is set.
pub fn shortcut_hint<R: Runtime>(app: &AppHandle<R>) -> String {
    let key = load_config(app).shortcut.trim().to_string();
    if key.is_empty() { String::new() } else { format!("   ({})", key) }
}

/// Registers the global start/stop shortcut from `auto-meeting.json`.
fn register_shortcut<R: Runtime>(app: &AppHandle<R>, config: &DetectorConfig) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
    let text = config.shortcut.trim();
    if text.is_empty() {
        return;
    }
    let shortcut = match text.parse::<Shortcut>() {
        Ok(shortcut) => shortcut,
        Err(error) => {
            log::warn!("meeting-detector: invalid shortcut {:?}: {}", text, error);
            return;
        }
    };
    let result = app.global_shortcut().on_shortcut(shortcut, |app, _shortcut, event| {
        if event.state != ShortcutState::Pressed {
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if crate::audio::recording_commands::is_recording().await {
                log::info!("meeting-detector: shortcut -> stop recording");
                crate::tray::stop_recording_from_app(&app);
                show_pill(&app, "stopped", "");
            } else {
                log::info!("meeting-detector: shortcut -> start recording");
                crate::tray::start_recording_from_app(&app);
                show_pill(&app, "recording", "");
            }
        });
    });
    match result {
        Ok(()) => log::info!("meeting-detector: shortcut {} registered", text),
        Err(error) => {
            // Usually another app already owns it.
            log::warn!("meeting-detector: cannot register shortcut {}: {}", text, error);
            use tauri_plugin_notification::NotificationExt;
            let _ = app
                .notification()
                .builder()
                .title("Meetily: scorciatoia non disponibile")
                .body(format!("{} e' gia' usata da un'altra app. Cambiala in auto-meeting.json.", text))
                .show();
        }
    }
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !cfg!(windows) {
        return;
    }
    register_shortcut(&app, &load_config(&app));
    tauri::async_runtime::spawn(async move {
        let own_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        let mut phase = Phase::Idle;
        let mut seen_since: Option<Instant> = None;
        let mut gone_since: Option<Instant> = None;
        let mut start_requested: Option<Instant> = None;
        log::info!("meeting-detector: started (own exe: {})", own_exe);

        loop {
            tokio::time::sleep(POLL).await;
            let config = load_config(&app);
            if !config.enabled {
                continue;
            }
            let apps = tokio::task::spawn_blocking(apps_using_microphone).await.unwrap_or_default();
            let meeting = meeting_app(&apps, &config, &own_exe);
            let recording = crate::audio::recording_commands::is_recording().await;
            let now = Instant::now();

            match phase {
                Phase::Idle => {
                    if recording {
                        // The user started a recording by hand: leave it alone.
                        phase = Phase::HandsOff;
                        continue;
                    }
                    match &meeting {
                        Some(name) => {
                            let since = *seen_since.get_or_insert(now);
                            if now.duration_since(since) >= Duration::from_secs(config.start_debounce_secs) {
                                log::info!("meeting-detector: {} holds the mic, starting recording", name);
                                crate::tray::start_recording_from_app(&app);
                                show_pill(&app, "recording", name);
                                phase = Phase::AutoRecording;
                                start_requested = Some(now);
                                gone_since = None;
                            }
                        }
                        None => seen_since = None,
                    }
                }
                Phase::AutoRecording => {
                    if !recording {
                        // Give the frontend time to actually start before judging.
                        if start_requested.is_some_and(|at| now.duration_since(at) < Duration::from_secs(30)) {
                            continue;
                        }
                        // Stopped by the user (or failed): don't restart during this call.
                        log::info!("meeting-detector: recording ended outside the detector");
                        show_pill(&app, "stopped", "");
                        phase = Phase::HandsOff;
                        continue;
                    }
                    start_requested = None;
                    match meeting {
                        Some(_) => gone_since = None,
                        None => {
                            let since = *gone_since.get_or_insert(now);
                            if now.duration_since(since) >= Duration::from_secs(config.stop_grace_secs) {
                                log::info!("meeting-detector: mic released, stopping recording");
                                crate::tray::stop_recording_from_app(&app);
                                show_pill(&app, "stopped", "");
                                phase = Phase::HandsOff;
                            }
                        }
                    }
                }
                Phase::HandsOff => {
                    if !recording && meeting.is_none() {
                        phase = Phase::Idle;
                        seen_since = None;
                        gone_since = None;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_own_exe_and_unknown_apps() {
        let config = DetectorConfig::default();
        let apps = vec![
            r"C:#Users#me#AppData#Local#meetily#meetily.exe".to_string(),
            "Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe".to_string(),
        ];
        assert_eq!(meeting_app(&apps, &config, "meetily.exe"), None);
    }

    #[test]
    fn finds_teams() {
        let config = DetectorConfig::default();
        let apps = vec!["MSTeams_8wekyb3d8bbwe".to_string()];
        assert_eq!(meeting_app(&apps, &config, "meetily.exe"), Some("MSTeams".to_string()));
    }

    #[test]
    fn readable_names() {
        assert_eq!(display_name(r"C:#Program Files#Google#Chrome#Application#chrome.exe"), "chrome");
    }
}
