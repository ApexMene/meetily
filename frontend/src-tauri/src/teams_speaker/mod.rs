//! Fork: Teams active speaker, optional and OFF by default. While a recording runs, a separate
//! reader (see `reader`) watches which participant Teams shows as speaking; the result is saved
//! as `active_speakers.json` next to the recording: `[{"start": s, "end": s, "name": "..."}]`,
//! seconds from the start of the recording. It is only data for after-meeting steps (naming the
//! voices of "the others"); recording and transcription never wait for it.
//!
//! Fail safe: the reader runs on its own thread and process; every error or timeout feeds a
//! circuit breaker that switches it off for the recording after 3 failures in a row; a panic is
//! caught; the stop path never blocks.

mod logic;
mod reader;

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Runtime};

const FILE: &str = "active_speakers.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub enabled: bool,
}

static SETTINGS: Store<Settings> = Store::new("teams-speaker/settings.json");
static ENABLED: AtomicBool = AtomicBool::new(false);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

struct Session {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Option<logic::Outcome>>,
}

/// Loads the setting at startup.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    ENABLED.store(SETTINGS.get(app).enabled, Ordering::Relaxed);
}

/// A recording starts. Returns immediately.
pub fn start() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut session) = SESSION.lock() else { return };
    if let Some(old) = session.take() {
        old.stop.store(true, Ordering::Relaxed);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let spawned = std::thread::Builder::new().name("teams-speaker".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut reader = match reader::open() {
                Ok(reader) => reader,
                Err(error) => {
                    log::warn!("teams speaker: not started: {}", error);
                    return None;
                }
            };
            let began = Instant::now();
            let now = move || began.elapsed().as_secs_f64();
            Some(logic::run(reader.as_mut(), &flag, &now))
        }));
        outcome.unwrap_or_else(|_| {
            log::warn!("teams speaker: reader panicked, ignored");
            None
        })
    });
    match spawned {
        Ok(handle) => *session = Some(Session { stop, handle }),
        Err(error) => log::warn!("teams speaker: cannot start the reader thread: {}", error),
    }
}

/// The recording stopped: saves what was read next to it. Never blocks the caller.
pub fn stop_and_save(folder: &Path) {
    let Some(session) = SESSION.lock().ok().and_then(|mut s| s.take()) else { return };
    session.stop.store(true, Ordering::Relaxed);
    let folder = folder.to_path_buf();
    let _ = std::thread::Builder::new().name("teams-speaker-save".into()).spawn(move || {
        let Ok(Some(outcome)) = session.handle.join() else { return };
        if let Some(reason) = &outcome.disabled {
            log::warn!("teams speaker: switched off for this recording ({})", reason);
        }
        if outcome.spans.is_empty() {
            return;
        }
        #[derive(Serialize)]
        struct Entry<'a> {
            start: f64,
            end: f64,
            name: &'a str,
        }
        let entries: Vec<Entry> = outcome.spans.iter().map(|s| Entry { start: s.start, end: s.end, name: &s.name }).collect();
        if let Err(error) = crate::fork_store::write_json(&folder.join(FILE), &entries) {
            log::warn!("teams speaker: cannot save {}: {}", FILE, error);
        }
    });
}

#[tauri::command]
pub async fn teams_speaker_get<R: Runtime>(app: AppHandle<R>) -> Settings {
    SETTINGS.get(&app)
}

/// Takes effect with the next recording.
#[tauri::command]
pub async fn teams_speaker_set_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    SETTINGS.set(&app, Settings { enabled })?;
    ENABLED.store(enabled, Ordering::Relaxed);
    Ok(())
}
