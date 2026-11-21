//! Fork: "Latin script only" switch for Parakeet (Settings › Transcription). The multilingual
//! model sometimes drifts into Cyrillic or Greek on short or noisy chunks; with the switch on the
//! decoder only emits Latin-script tokens (see `parakeet_engine::model::LATIN_ONLY`). Off by
//! default, since it breaks meetings held in other scripts.

use crate::fork_store::Store;
use crate::parakeet_engine::model::LATIN_ONLY;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Runtime};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Options {
    latin_only: bool,
}

static OPTIONS: Store<Options> = Store::new("transcription-fork.json");

/// Applies the saved choice; call once at startup.
pub fn apply<R: Runtime>(app: &AppHandle<R>) {
    LATIN_ONLY.store(OPTIONS.get(app).latin_only, Ordering::Relaxed);
}

#[tauri::command]
pub async fn latin_filter_get<R: Runtime>(app: AppHandle<R>) -> bool {
    OPTIONS.get(&app).latin_only
}

#[tauri::command]
pub async fn latin_filter_set<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    OPTIONS.set(&app, Options { latin_only: enabled })?;
    LATIN_ONLY.store(enabled, Ordering::Relaxed);
    log::info!("Parakeet Latin-only output {}", if enabled { "on" } else { "off" });
    Ok(())
}
