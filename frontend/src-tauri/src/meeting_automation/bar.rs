//! The meeting bar: a small always-on-top window (`public/meeting-bar.*`) with the recording
//! timer, "Open transcript" and "Stop". It has its own taskbar entry, so it can always be found
//! again; it can be dragged and shrunk to a dot + timer, and remembers both.

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};

const LABEL: &str = "meeting-bar";
const SIZE: (f64, f64) = (620.0, 76.0);
const COMPACT_SIZE: (f64, f64) = (168.0, 52.0);
/// The "meeting ended" bar closes itself after this long.
const STOPPED_BAR_SECS: u64 = 12;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Recording,
    Stopped,
}

/// Where the user left the bar and whether they shrank it (`<app data>/meeting-bar.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Placement {
    x: Option<f64>,
    y: Option<f64>,
    compact: bool,
}

static PLACEMENT: Store<Placement> = Store::new("meeting-bar.json");

/// Bumped on every `show`, so a pending auto-close does not close a newer bar.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The bar window may only call the app's own commands.
pub fn register_capability<R: Runtime>(app: &AppHandle<R>) {
    let capability = tauri::ipc::CapabilityBuilder::new("fork-meeting-bar").window(LABEL).permission("core:default");
    if let Err(error) = app.add_capability(capability) {
        log::warn!("meeting-automation: cannot register the bar capability: {}", error);
    }
}

fn size(compact: bool) -> (f64, f64) {
    if compact { COMPACT_SIZE } else { SIZE }
}

/// True when the logical point lies on one of the connected monitors.
fn on_some_monitor<R: Runtime>(app: &AppHandle<R>, x: f64, y: f64) -> bool {
    app.available_monitors().unwrap_or_default().iter().any(|monitor| {
        let scale = monitor.scale_factor();
        let left = monitor.position().x as f64 / scale;
        let top = monitor.position().y as f64 / scale;
        let width = monitor.size().width as f64 / scale;
        let height = monitor.size().height as f64 / scale;
        x >= left - 20.0 && x < left + width - 40.0 && y >= top - 20.0 && y < top + height - 30.0
    })
}

/// Top centre of the primary screen, below the title bars of maximised windows.
fn default_position<R: Runtime>(app: &AppHandle<R>) -> (f64, f64) {
    app.primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let width = monitor.size().width as f64 / scale;
            let left = monitor.position().x as f64 / scale;
            let top = monitor.position().y as f64 / scale;
            (left + (width - SIZE.0) / 2.0, top + 100.0)
        })
        .unwrap_or((400.0, 100.0))
}

/// Shows the bar (or updates the open one). `app_name` is the detected meeting app, if any.
pub fn show<R: Runtime>(app: &AppHandle<R>, mode: Mode, app_name: &str) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let placement = PLACEMENT.get(app);
    let shortcut = super::settings(app).shortcut;
    let url = format!(
        "meeting-bar.html?mode={}&app={}&since={}&compact={}&key={}&after={}",
        if mode == Mode::Stopped { "stopped" } else { "recording" },
        app_name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>(),
        chrono::Utc::now().timestamp_millis(),
        u8::from(placement.compact),
        shortcut.trim().replace('+', "%2B"),
        u8::from(crate::post_meeting::is_active(app))
    );
    let (width, height) = size(placement.compact);
    let title = if mode == Mode::Stopped { "Meetily · Meeting ended" } else { "Meetily · Recording" };
    if mode == Mode::Stopped {
        close_later(app, generation);
    }

    // Reuse an open bar: closing is asynchronous, a new window with the same label would fail.
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.eval(&format!("location.replace({:?})", format!("/{}", url)));
        let _ = window.set_size(tauri::LogicalSize::new(width, height));
        let _ = window.set_title(title);
        let _ = window.unminimize();
        return;
    }
    let (x, y) = match (placement.x, placement.y) {
        (Some(x), Some(y)) if on_some_monitor(app, x, y) => (x, y),
        _ => default_position(app),
    };
    let built = tauri::WebviewWindowBuilder::new(app, LABEL, tauri::WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(width, height)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(false) // its own taskbar entry and Alt+Tab, so it is never lost
        .resizable(false)
        .focused(false)
        .build();
    match built {
        Ok(window) => {
            let handle = app.clone();
            let scale = window.scale_factor().unwrap_or(1.0);
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Moved(position) = event {
                    let mut placement = PLACEMENT.get(&handle);
                    placement.x = Some(position.x as f64 / scale);
                    placement.y = Some(position.y as f64 / scale);
                    let _ = PLACEMENT.set(&handle, placement);
                }
            });
        }
        Err(error) => log::warn!("meeting-automation: cannot show the bar: {}", error),
    }
}

fn close_later<R: Runtime>(app: &AppHandle<R>, generation: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STOPPED_BAR_SECS)).await;
        if GENERATION.load(Ordering::SeqCst) == generation {
            close(&app);
        }
    });
}

fn close<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.close();
    }
}

/// Bar: drag it around (mouse held on the bar).
#[tauri::command]
pub async fn meeting_bar_drag<R: Runtime>(app: AppHandle<R>) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.start_dragging();
    }
}

/// Bar: shrink to the dot + timer, or grow back. Remembered for the next meetings.
#[tauri::command]
pub async fn meeting_bar_compact<R: Runtime>(app: AppHandle<R>, compact: bool) {
    let mut placement = PLACEMENT.get(&app);
    placement.compact = compact;
    let _ = PLACEMENT.set(&app, placement);
    if let Some(window) = app.get_webview_window(LABEL) {
        let (width, height) = size(compact);
        let _ = window.set_size(tauri::LogicalSize::new(width, height));
    }
}

/// Bar: open the app on the live transcript.
#[tauri::command]
pub async fn meeting_bar_open<R: Runtime>(app: AppHandle<R>) {
    close(&app);
    crate::tray::show_live_transcript(&app).await;
}

/// Bar: stop the recording now.
#[tauri::command]
pub async fn meeting_bar_stop<R: Runtime>(app: AppHandle<R>) {
    close(&app);
    if crate::audio::recording_commands::is_recording().await {
        crate::tray::stop_recording_from_app(&app);
    }
}

/// Bar "x": while recording, minimise to the taskbar (it stays findable there); otherwise close.
#[tauri::command]
pub async fn meeting_bar_close<R: Runtime>(app: AppHandle<R>) {
    if crate::audio::recording_commands::is_recording().await {
        if let Some(window) = app.get_webview_window(LABEL) {
            let _ = window.minimize();
            return;
        }
    }
    close(&app);
}
