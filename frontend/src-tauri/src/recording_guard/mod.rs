//! Fork: recording guard. Warns when a channel of a recording goes silent, marks audio gaps in the
//! transcript and recovers recordings interrupted by a crash.
//!
//! Everything here is a side watcher: it never touches the recording itself, and any failure is
//! logged and ignored so recording and transcription carry on unaffected.
//!
//! - `silence`: pure silent-channel logic.
//! - this file: settings, the per-chunk level hook used by the audio pipeline, the watcher task,
//!   the gap marker text.
//! - `recovery`: finds recordings left half-written by a crash and rebuilds their audio.
//!
//! Settings: `<app data>/recording-guard.json`.

pub mod recovery;
pub mod silence;

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Runtime};

use silence::{SilenceTracker, Verdict};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GuardSettings {
    /// Warn when a channel of the recording stays silent.
    pub silence_alert_enabled: bool,
    /// Seconds of silence on the meeting audio ("Others") before warning. The microphone gets twice as long:
    /// listening without speaking is normal.
    pub silence_seconds: u64,
    /// Level (dBFS) under which a channel counts as silent.
    pub silence_threshold_db: f32,
}

impl Default for GuardSettings {
    fn default() -> Self {
        Self { silence_alert_enabled: true, silence_seconds: 90, silence_threshold_db: -70.0 }
    }
}

impl GuardSettings {
    fn sanitized(mut self) -> Self {
        self.silence_seconds = self.silence_seconds.clamp(10, 3600);
        self.silence_threshold_db = if self.silence_threshold_db.is_finite() { self.silence_threshold_db.clamp(-100.0, -20.0) } else { -70.0 };
        self
    }
}

static SETTINGS: Store<GuardSettings> = Store::new("recording-guard.json");

pub fn settings<R: Runtime>(app: &AppHandle<R>) -> GuardSettings {
    SETTINGS.get(app).sanitized()
}

#[tauri::command]
pub async fn recording_guard_settings_get<R: Runtime>(app: AppHandle<R>) -> GuardSettings {
    settings(&app)
}

#[tauri::command]
pub async fn recording_guard_settings_set<R: Runtime>(app: AppHandle<R>, settings: GuardSettings) -> Result<(), String> {
    SETTINGS.set(&app, settings.sanitized())
}

// ---- level hook (called by the audio pipeline for every raw chunk) ---------------------------

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Last time (ms) each channel was above the threshold: [microphone, system].
static LAST_LOUD_MS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static LEVEL_TRACKING: AtomicBool = AtomicBool::new(false);
/// f32 bits of the linear RMS threshold.
static THRESHOLD_BITS: AtomicU32 = AtomicU32::new(0);

/// Cheap: one RMS over the chunk and one atomic store, and nothing at all when the alert is off.
/// Never panics, never blocks.
pub fn note_chunk(is_microphone: bool, samples: &[f32]) {
    if !LEVEL_TRACKING.load(Ordering::Relaxed) {
        return;
    }
    let threshold = f32::from_bits(THRESHOLD_BITS.load(Ordering::Relaxed));
    if silence::rms(samples) >= threshold {
        LAST_LOUD_MS[if is_microphone { 0 } else { 1 }].store(now_ms(), Ordering::Relaxed);
    }
}

fn begin_tracking(settings: &GuardSettings) {
    let now = now_ms();
    LAST_LOUD_MS[0].store(now, Ordering::Relaxed);
    LAST_LOUD_MS[1].store(now, Ordering::Relaxed);
    THRESHOLD_BITS.store(silence::db_to_rms(settings.silence_threshold_db).to_bits(), Ordering::Relaxed);
    LEVEL_TRACKING.store(settings.silence_alert_enabled, Ordering::Relaxed);
}

// ---- watcher ----------------------------------------------------------------------------------

const TICK: Duration = Duration::from_secs(3);
/// The microphone may stay silent this many times longer than the meeting audio before warning.
const MIC_FACTOR: u64 = 2;

/// Starts the watcher for one recording session. It ends by itself when the recording stops.
pub fn start<R: Runtime>(app: AppHandle<R>, session: Arc<crate::audio::RecordingState>) {
    let initial = settings(&app);
    if !initial.silence_alert_enabled {
        return;
    }
    begin_tracking(&initial);
    tauri::async_runtime::spawn(async move {
        log::info!("recording-guard: silence watcher started");
        let (mut mic, mut system) = (SilenceTracker::new(), SilenceTracker::new());
        let started = Instant::now();
        let mut seen_recording = false;
        loop {
            tokio::time::sleep(TICK).await;
            if session.is_recording() {
                seen_recording = true;
            } else if seen_recording || started.elapsed() > Duration::from_secs(60) {
                break;
            } else {
                continue;
            }
            let settings = settings(&app);
            if !settings.silence_alert_enabled {
                LEVEL_TRACKING.store(false, Ordering::Relaxed);
                continue;
            }
            begin_tracking_threshold(&settings);
            let (now, paused) = (now_ms(), session.is_paused());
            let limit = settings.silence_seconds * 1000;
            let (last_mic, last_system) = (LAST_LOUD_MS[0].load(Ordering::Relaxed), LAST_LOUD_MS[1].load(Ordering::Relaxed));

            let local_voice = now.saturating_sub(last_mic) < limit;
            let meeting_on = !paused && session.get_system_device().is_some() && !local_voice && meeting_app_active(&app).await;
            let watch_system = !paused && session.get_system_device().is_some() && (local_voice || meeting_on);
            let watch_mic = !paused && session.get_microphone_device().is_some() && now.saturating_sub(last_system) < limit;

            report(&app, "system", system.check(now, last_system, limit, watch_system));
            report(&app, "microphone", mic.check(now, last_mic, limit * MIC_FACTOR, watch_mic));
        }
        LEVEL_TRACKING.store(false, Ordering::Relaxed);
        log::info!("recording-guard: silence watcher stopped");
    });
}

fn begin_tracking_threshold(settings: &GuardSettings) {
    THRESHOLD_BITS.store(silence::db_to_rms(settings.silence_threshold_db).to_bits(), Ordering::Relaxed);
    LEVEL_TRACKING.store(true, Ordering::Relaxed);
}

/// A meeting app (Teams...) is using the microphone. Windows only; false elsewhere or on any error.
async fn meeting_app_active<R: Runtime>(app: &AppHandle<R>) -> bool {
    let app = app.clone();
    tokio::task::spawn_blocking(move || crate::meeting_automation::meeting_app_active(&app)).await.unwrap_or(false)
}

fn report<R: Runtime>(app: &AppHandle<R>, channel: &str, verdict: Verdict) {
    match verdict {
        Verdict::Ok => {}
        Verdict::Alert { silent_ms } => {
            let seconds = silent_ms / 1000;
            log::warn!("recording-guard: the {} channel has been silent for {} s", channel, seconds);
            let (title, body) = if channel == "system" {
                ("Audio della riunione non arriva", format!("Meetily non sente l'audio degli altri partecipanti da {} secondi. Controlla l'uscita audio (cuffie, altoparlanti) e che il volume della riunione non sia a zero.", seconds))
            } else {
                ("Il microfono non arriva", format!("Meetily non sente il microfono da {} secondi. Controlla che non sia disattivato o scollegato.", seconds))
            };
            if let Err(error) = app.emit("recording-guard-alert", serde_json::json!({ "channel": channel, "silentSeconds": seconds })) {
                log::warn!("recording-guard: cannot emit the alert event: {}", error);
            }
            use tauri_plugin_notification::NotificationExt;
            if let Err(error) = app.notification().builder().title(title).body(body).show() {
                log::warn!("recording-guard: cannot show the notification: {}", error);
            }
        }
        Verdict::Recovered => log::info!("recording-guard: the {} channel is back", channel),
    }
}

pub use silence::gap_marker_text;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_sanitized() {
        let s = GuardSettings { silence_alert_enabled: true, silence_seconds: 0, silence_threshold_db: f32::NAN }.sanitized();
        assert_eq!((s.silence_seconds, s.silence_threshold_db), (10, -70.0));
        let s = GuardSettings { silence_alert_enabled: false, silence_seconds: 99999, silence_threshold_db: 5.0 }.sanitized();
        assert_eq!((s.silence_seconds, s.silence_threshold_db), (3600, -20.0));
        // an old or partial settings file still loads
        let s: GuardSettings = serde_json::from_str(r#"{"silenceSeconds": 120}"#).unwrap();
        assert!(s.silence_alert_enabled);
        assert_eq!(s.silence_seconds, 120);
    }
}
