//! Fork: user glossary and numeric-value safety for transcripts.
//!
//! - The glossary (Settings > Transcription) feeds Whisper's initial prompt and, for every engine,
//!   a prudent fuzzy correction of the transcribed text (`fuzzy`).
//! - After a meeting the precise re-transcription is compared with the live one; critical values
//!   (numbers, versions, IPs, dates, times) that the two readings disagree on are marked "(?)"
//!   (`critical`, `second_pass`) and Claude is told to carry the mark into recap and tasks.
//!
//! Everything fails safe: a bad settings file or an error leaves the text as it was.

pub mod critical;
pub mod fuzzy;
pub mod second_pass;
pub mod terms;

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use tauri::{AppHandle, Runtime};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub terms: Vec<String>,
    /// Fuzzy post-correction of the transcribed text (needs at least one term).
    pub correct: bool,
    /// Minimum similarity (0.7..1.0) for a spelling correction. High = prudent.
    pub min_similarity: f64,
    /// Time budget of the precise re-transcription after a meeting; when it runs out the live
    /// transcript is kept.
    pub second_pass_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { terms: Vec::new(), correct: true, min_similarity: 0.85, second_pass_minutes: 45 }
    }
}

impl Settings {
    fn sanitized(mut self) -> Self {
        self.terms = terms::clean_terms(self.terms);
        self.min_similarity = if self.min_similarity.is_finite() { self.min_similarity.clamp(0.7, 1.0) } else { 0.85 };
        self.second_pass_minutes = self.second_pass_minutes.clamp(5, 480);
        self
    }
}

static STORE: Store<Settings> = Store::new("glossary.json");
/// Copy read by the transcription threads (no app handle there).
static CURRENT: RwLock<Option<Settings>> = RwLock::new(None);

fn publish(settings: &Settings) {
    if let Ok(mut current) = CURRENT.write() {
        *current = Some(settings.clone());
    }
}

fn current() -> Option<Settings> {
    CURRENT.read().ok().and_then(|current| current.clone())
}

/// Loads the saved glossary; call once at startup.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    publish(&STORE.get(app).sanitized());
}

/// Whisper initial prompt built from the glossary, if there is one.
pub fn initial_prompt() -> Option<String> {
    terms::initial_prompt(&current()?.terms)
}

/// Fuzzy-corrects a transcribed text with the glossary; the text itself when there is nothing to do.
pub fn apply(text: &str) -> String {
    match current() {
        Some(settings) if settings.correct && !settings.terms.is_empty() => fuzzy::correct(text, &settings.terms, settings.min_similarity),
        _ => text.to_string(),
    }
}

pub fn second_pass_budget() -> std::time::Duration {
    let minutes = current().map(|s| s.second_pass_minutes).unwrap_or(45).clamp(5, 480);
    std::time::Duration::from_secs(minutes * 60)
}

#[tauri::command]
pub async fn glossary_settings_get<R: Runtime>(app: AppHandle<R>) -> Settings {
    STORE.get(&app).sanitized()
}

#[tauri::command]
pub async fn glossary_settings_set<R: Runtime>(app: AppHandle<R>, settings: Settings) -> Result<(), String> {
    let settings = settings.sanitized();
    STORE.set(&app, settings.clone())?;
    publish(&settings);
    log::info!("glossary: {} terms, correction {}", settings.terms.len(), if settings.correct { "on" } else { "off" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_sanitized_and_partial_json_works() {
        let s: Settings = serde_json::from_str(r#"{"terms":[" A ","a",""],"minSimilarity":5.0,"secondPassMinutes":0}"#).unwrap();
        let s = s.sanitized();
        assert_eq!(s.terms, vec!["A"]);
        assert_eq!(s.min_similarity, 1.0);
        assert_eq!(s.second_pass_minutes, 5);
        assert!(s.correct);
        let s = Settings { min_similarity: f64::NAN, ..Settings::default() }.sanitized();
        assert_eq!(s.min_similarity, 0.85);
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.terms.is_empty());
    }
}
