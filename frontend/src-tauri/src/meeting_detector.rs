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
use tauri_plugin_notification::NotificationExt;

const POLL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorConfig {
    pub enabled: bool,
    pub start_debounce_secs: u64,
    pub stop_grace_secs: u64,
    /// Case-insensitive fragments of the registry key (package name or exe path).
    pub apps: Vec<String>,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start_debounce_secs: 20,
            stop_grace_secs: 90,
            apps: [
                "msteams", "teams.exe", "zoom", "webex", "ciscocollabhost", "slack", "discord",
                "skype", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera",
                "whatsapp", "gotomeeting", "ringcentral",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
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

fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        log::warn!("meeting-detector: notification failed: {}", error);
    }
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

pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !cfg!(windows) {
        return;
    }
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
                                notify(
                                    &app,
                                    "Trascrizione automatica avviata",
                                    &format!("Meeting rilevato ({}). Clicca per vedere la trascrizione in tempo reale.", name),
                                );
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
                                notify(
                                    &app,
                                    "Meeting terminato",
                                    "Trascrizione salvata. Il resoconto andra' in Notion appena pronto.",
                                );
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
