//! Global start/stop shortcut (e.g. Ctrl+Alt+M), usable from any app.

use tauri::{AppHandle, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Registers the saved shortcut; tells the user when another app already owns it.
pub fn register_at_startup<R: Runtime>(app: &AppHandle<R>, shortcut: &str) {
    if let Err(error) = replace(app, "", shortcut) {
        log::warn!("meeting-automation: {}", error);
        use tauri_plugin_notification::NotificationExt;
        let _ = app
            .notification()
            .builder()
            .title("Meetily: shortcut not available")
            .body(format!("{} Choose another one in Settings › General.", error))
            .show();
    }
}

/// Replaces `old` with `new` (either may be empty). On failure `old` stays registered.
pub fn replace<R: Runtime>(app: &AppHandle<R>, old: &str, new: &str) -> Result<(), String> {
    let new_shortcut = match new.trim() {
        "" => None,
        text => Some(text.parse::<Shortcut>().map_err(|error| format!("Invalid shortcut \"{}\": {}", text, error))?),
    };
    let old_shortcut = old.trim().parse::<Shortcut>().ok();
    if let Some(old_shortcut) = old_shortcut {
        let _ = app.global_shortcut().unregister(old_shortcut);
    }
    let Some(new_shortcut) = new_shortcut else { return Ok(()) };
    match app.global_shortcut().on_shortcut(new_shortcut, |app, _, event| {
        if event.state == ShortcutState::Pressed {
            toggle_recording(app.clone());
        }
    }) {
        Ok(()) => {
            log::info!("meeting-automation: shortcut {} registered", new.trim());
            Ok(())
        }
        Err(error) => {
            if let Some(old_shortcut) = old_shortcut {
                let _ = app.global_shortcut().on_shortcut(old_shortcut, |app, _, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_recording(app.clone());
                    }
                });
            }
            Err(format!("{} is already used by another app ({}).", new.trim(), error))
        }
    }
}

fn toggle_recording<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        if crate::audio::recording_commands::is_recording().await {
            log::info!("meeting-automation: shortcut -> stop recording");
            crate::tray::stop_recording_from_app(&app);
            super::bar::show(&app, super::bar::Mode::Stopped, "");
        } else {
            log::info!("meeting-automation: shortcut -> start recording");
            crate::tray::start_recording_from_app(&app);
            super::bar::show(&app, super::bar::Mode::Recording, "");
        }
    });
}
