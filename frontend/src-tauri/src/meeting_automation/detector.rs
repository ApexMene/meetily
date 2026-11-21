//! Starts and stops recording while a meeting app holds the microphone.
//!
//! Windows records which apps use the microphone in the registry
//! (`HKCU\...\CapabilityAccessManager\ConsentStore\microphone`): while an app is using it,
//! `LastUsedTimeStop` is 0. The detector polls that every few seconds:
//! - a meeting app holds the mic for `start_debounce_secs` -> start recording;
//! - no meeting app holds it for `stop_grace_secs` -> stop recording (the meeting is saved);
//! - a recording the user started, or one the user stopped, is left alone until the mic is free.
//! Meetily's own microphone use is ignored.

use std::time::{Duration, Instant};
use tauri::{AppHandle, Runtime};

const POLL: Duration = Duration::from_secs(3);
/// After asking the frontend to start, wait this long before concluding the start failed.
const START_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq)]
enum Phase {
    /// Waiting for a meeting app to take the microphone.
    Idle,
    /// Recording started by the detector.
    AutoRecording,
    /// A recording the detector does not own, or one the user stopped: wait for the mic to be free.
    HandsOff,
}

/// Registry key names of the apps currently holding the microphone.
#[cfg(windows)]
fn apps_using_microphone() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    fn in_use(key: &RegKey) -> bool {
        let start: u64 = key.get_value("LastUsedTimeStart").unwrap_or(0);
        let stop: u64 = key.get_value("LastUsedTimeStop").unwrap_or(1);
        start > 0 && stop == 0
    }
    let mut found = Vec::new();
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(ROOT) else { return found };
    for name in root.enum_keys().flatten() {
        if name == "NonPackaged" {
            // Desktop apps, one key per executable path.
            if let Ok(non_packaged) = root.open_subkey(&name) {
                for exe in non_packaged.enum_keys().flatten() {
                    if non_packaged.open_subkey(&exe).is_ok_and(|key| in_use(&key)) {
                        found.push(exe);
                    }
                }
            }
        } else if root.open_subkey(&name).is_ok_and(|key| in_use(&key)) {
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
    let lower = last.to_ascii_lowercase();
    match lower.strip_suffix(".exe") {
        Some(_) => last[..last.len() - 4].to_string(),
        None => last.to_string(),
    }
}

/// The first meeting app (from `apps`) among those using the microphone, Meetily excluded.
fn meeting_app(apps_in_use: &[String], apps: &[String], own_exe: &str) -> Option<String> {
    apps_in_use
        .iter()
        .filter(|key| own_exe.is_empty() || !key.to_lowercase().ends_with(own_exe))
        .find(|key| {
            let lower = key.to_lowercase();
            apps.iter().any(|fragment| !fragment.is_empty() && lower.contains(&fragment.to_lowercase()))
        })
        .map(|key| display_name(key))
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let own_exe = std::env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        let mut phase = Phase::Idle;
        let mut seen_since: Option<Instant> = None;
        let mut gone_since: Option<Instant> = None;
        let mut start_requested: Option<Instant> = None;

        loop {
            tokio::time::sleep(POLL).await;
            let settings = super::settings(&app);
            if !settings.enabled {
                phase = Phase::Idle;
                seen_since = None;
                continue;
            }
            let apps = tokio::task::spawn_blocking(apps_using_microphone).await.unwrap_or_default();
            let meeting = meeting_app(&apps, &settings.apps, &own_exe);
            let recording = crate::audio::recording_commands::is_recording().await;
            let now = Instant::now();

            match phase {
                Phase::Idle => {
                    if recording {
                        phase = Phase::HandsOff; // started by hand: not ours
                        continue;
                    }
                    match &meeting {
                        Some(name) => {
                            let since = *seen_since.get_or_insert(now);
                            if now.duration_since(since) >= Duration::from_secs(settings.start_debounce_secs) {
                                log::info!("meeting-automation: {} holds the microphone, starting recording", name);
                                crate::tray::start_recording_from_app(&app);
                                super::bar::show(&app, super::bar::Mode::Recording, name);
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
                        if start_requested.is_some_and(|at| now.duration_since(at) < START_TIMEOUT) {
                            continue; // the frontend is still starting the recording
                        }
                        // Stopped by the user (or the start failed): do not restart during this call.
                        log::info!("meeting-automation: recording ended outside the detector");
                        super::bar::show(&app, super::bar::Mode::Stopped, "");
                        phase = Phase::HandsOff;
                        continue;
                    }
                    start_requested = None;
                    match meeting {
                        Some(_) => gone_since = None,
                        None => {
                            let since = *gone_since.get_or_insert(now);
                            if now.duration_since(since) >= Duration::from_secs(settings.stop_grace_secs) {
                                log::info!("meeting-automation: microphone released, stopping recording");
                                crate::tray::stop_recording_from_app(&app);
                                super::bar::show(&app, super::bar::Mode::Stopped, "");
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

    fn apps() -> Vec<String> {
        super::super::Settings::default().apps
    }

    #[test]
    fn ignores_own_exe_and_unknown_apps() {
        let in_use = vec![
            r"C:#Users#me#AppData#Local#meetily#meetily.exe".to_string(),
            "Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe".to_string(),
        ];
        assert_eq!(meeting_app(&in_use, &apps(), "meetily.exe"), None);
    }

    #[test]
    fn finds_teams() {
        let in_use = vec!["MSTeams_8wekyb3d8bbwe".to_string()];
        assert_eq!(meeting_app(&in_use, &apps(), "meetily.exe"), Some("MSTeams".to_string()));
    }

    #[test]
    fn readable_names() {
        assert_eq!(display_name(r"C:#Program Files#Google#Chrome#Application#chrome.exe"), "chrome");
        assert_eq!(display_name(r"C:#Apps#Zoom.EXE"), "Zoom");
    }

    #[test]
    fn everything_is_off_by_default() {
        let settings = super::super::Settings::default();
        assert!(!settings.enabled && !settings.start_with_windows);
        assert_eq!(settings.audio_retention_days, 0);
        assert!(settings.shortcut.is_empty());
    }
}
