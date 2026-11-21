//! "Start with Windows": a `HKCU\...\Run` entry that starts Meetily hidden in the tray
//! (`--autostart`). Since the window stays hidden, the tray icon is pinned next to the clock.

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "Meetily";
/// Command-line flag of a start by Windows: stay in the tray, no window.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// Adds or removes the Run entry.
pub fn apply(enabled: bool) {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        let Ok(run) = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) else {
            return;
        };
        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                let value = format!("\"{}\" {}", exe.display(), AUTOSTART_FLAG);
                match run.set_value(RUN_VALUE, &value) {
                    Ok(()) => log::info!("meeting-automation: start with Windows on"),
                    Err(error) => log::warn!("meeting-automation: cannot enable start with Windows: {}", error),
                }
            }
        } else if run.delete_value(RUN_VALUE).is_ok() {
            log::info!("meeting-automation: start with Windows off");
        }
    }
    #[cfg(not(windows))]
    let _ = enabled;
}

/// Pins the tray icon next to the clock, unless the user already chose its place in the Windows
/// settings (`IsPromoted` present). Windows registers the icon a few seconds after it appears.
pub fn pin_tray_icon_later() {
    #[cfg(windows)]
    tauri::async_runtime::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
        let Ok(exe) = std::env::current_exe() else { return };
        let exe = exe.to_string_lossy().to_lowercase();
        let Ok(root) = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Control Panel\NotifyIconSettings") else {
            return;
        };
        for name in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&name, KEY_READ | KEY_SET_VALUE) else { continue };
            let path: String = key.get_value("ExecutablePath").unwrap_or_default();
            if path.to_lowercase() != exe || key.get_value::<u32, _>("IsPromoted").is_ok() {
                continue;
            }
            if key.set_value("IsPromoted", &1u32).is_ok() {
                log::info!("meeting-automation: tray icon pinned next to the clock");
            }
        }
    });
}
