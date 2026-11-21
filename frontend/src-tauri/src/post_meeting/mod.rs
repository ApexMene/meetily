//! Fork: background work after a meeting is saved. Every step is optional and off by default:
//!
//! 1. `retranscribe`: the whole recording is transcribed again with the best local model,
//!    optionally with a second model whose reading Claude can merge;
//! 2. `cleanup`: Claude Code cleans the new transcript (noise, obvious mistakes);
//! 3. the Notion agent (`crate::notion_agent`) writes the recap and the tasks in Notion.
//!
//! Each meeting becomes a job in a persistent queue (`queue`), processed one at a time and never
//! while recording. Settings: `<app data>/post-meeting/settings.json`; the status window is
//! `jobs`.
//!
//! The summary shown in Meetily is generated from the live transcript when the meeting ends,
//! before this work runs; regenerating it afterwards uses the improved transcript.

mod cleanup;
pub mod jobs;
mod queue;
mod retranscribe;

pub use jobs::open_jobs_window;
pub use queue::{enqueue, failing_jobs, is_queued, wake};

use crate::fork_store::Store;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, Runtime};

/// Data directory: queue, finished jobs, the editable clean-up prompt, settings.
const DIR: &str = "post-meeting";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retranscribe {
    #[default]
    Off,
    /// Local model(s) only.
    Model,
    /// Local model(s), then Claude cleans the text (and merges the second model's reading).
    Full,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub retranscribe: Retranscribe,
    /// Parakeet model name; empty = automatic (an installed "ultra" custom model, otherwise the
    /// live model).
    pub model: String,
    /// Second model: empty, a CTC folder name, or `parakeet:<model name>`
    /// (see `custom_models::second_opinion`).
    pub second_opinion: String,
    /// Claude model for the clean-up.
    pub clean_model: String,
    /// When the re-transcription was turned on: older meetings are never re-transcribed.
    pub since: Option<DateTime<Utc>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            retranscribe: Retranscribe::Off,
            model: String::new(),
            second_opinion: String::new(),
            clean_model: "haiku".into(),
            since: None,
        }
    }
}

static SETTINGS: Store<Settings> = Store::new("post-meeting/settings.json");

pub fn settings<R: Runtime>(app: &AppHandle<R>) -> Settings {
    SETTINGS.get(app)
}

/// True when any after-meeting step is on.
pub fn is_active<R: Runtime>(app: &AppHandle<R>) -> bool {
    settings(app).retranscribe != Retranscribe::Off || crate::notion_agent::is_enabled(app)
}

fn data_dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join(DIR))
}

/// Moves the queue and the clean-up prompt of the first version (in `notion-agent/`) here. Its
/// settings are not carried over: it turned every step on by default.
fn migrate_data<R: Runtime>(app: &AppHandle<R>) {
    let (Ok(app_dir), Some(dir)) = (app.path().app_data_dir(), data_dir(app)) else { return };
    let old = app_dir.join("notion-agent");
    for name in ["queue", "done", cleanup::PROMPT_FILE] {
        let (from, to) = (old.join(name), dir.join(name));
        if from.exists() && !to.exists() {
            let _ = std::fs::create_dir_all(&dir);
            match std::fs::rename(&from, &to) {
                Ok(()) => log::info!("post-meeting: moved {} from notion-agent/", name),
                Err(error) => log::warn!("post-meeting: cannot move {}: {}", from.display(), error),
            }
        }
    }
}

/// Call once the database is ready.
pub fn start<R: Runtime>(app: AppHandle<R>) {
    migrate_data(&app);
    let _ = settings(&app);
    jobs::register_capability(&app);
    queue::start(app);
}

/// What the settings page shows and edits.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostMeetingSettings {
    pub retranscribe: Retranscribe,
    pub model: String,
    pub second_opinion: String,
    pub clean_model: String,
    pub notion_enabled: bool,
    pub recap_model: String,
}

#[tauri::command]
pub async fn post_meeting_settings_get<R: Runtime>(app: AppHandle<R>) -> PostMeetingSettings {
    let settings = settings(&app);
    let notion = crate::notion_agent::settings(&app);
    PostMeetingSettings {
        retranscribe: settings.retranscribe,
        model: settings.model,
        second_opinion: settings.second_opinion,
        clean_model: settings.clean_model,
        notion_enabled: notion.enabled,
        recap_model: notion.model,
    }
}

#[tauri::command]
pub async fn post_meeting_settings_set<R: Runtime>(app: AppHandle<R>, settings: PostMeetingSettings) -> Result<(), String> {
    for model in [&settings.clean_model, &settings.recap_model] {
        if !crate::claude_code::is_model_id(model.trim()) {
            return Err(format!("Invalid Claude model \"{}\"", model));
        }
    }
    let second_opinion = settings.second_opinion.trim();
    if !crate::custom_models::second_opinion::is_valid_setting(second_opinion) {
        return Err(format!("Invalid second model \"{}\"", second_opinion));
    }
    let mut next = self::settings(&app);
    if next.retranscribe == Retranscribe::Off && settings.retranscribe != Retranscribe::Off {
        next.since = Some(Utc::now()); // only meetings recorded from now on
    }
    next.retranscribe = settings.retranscribe;
    next.model = settings.model.trim().chars().take(200).collect();
    next.second_opinion = second_opinion.to_string();
    next.clean_model = settings.clean_model.trim().to_string();
    SETTINGS.set(&app, next)?;
    crate::notion_agent::configure(&app, settings.notion_enabled, settings.recap_model.trim())?;
    crate::tray::update_tray_menu(&app);
    queue::wake();
    Ok(())
}

/// Opens the folder holding an editable Claude prompt: "cleanup" or "notion".
#[tauri::command]
pub async fn post_meeting_open_folder<R: Runtime>(app: AppHandle<R>, which: String) -> Result<(), String> {
    let dir = match which.as_str() {
        "cleanup" => {
            let dir = data_dir(&app).ok_or("app data directory unavailable")?;
            cleanup::ensure_prompt(&dir);
            dir
        }
        "notion" => crate::notion_agent::prepare_folder(&app)?,
        _ => return Err("unknown folder".into()),
    };
    open::that_detached(&dir).map_err(|error| error.to_string())
}
