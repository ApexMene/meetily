//! Re-transcription. The live transcript is cut into short chunks, on which Parakeet sometimes
//! drifts into other languages. After the meeting the whole recording (speech-detected segments)
//! is transcribed again with the best local model, optionally with a second model, and in "full"
//! mode Claude cleans the result (`cleanup`).
//!
//! The live transcript is first saved to `transcripts.live.json` in the meeting folder; on any
//! failure the current transcript is kept.

use super::{cleanup, Retranscribe};
use chrono::{DateTime, Utc};
use sqlx::Row;
use std::path::Path;
use std::time::Instant;
use tauri::{AppHandle, Runtime};

/// Returns false only when a new recording interrupted the work, which must then be redone.
pub async fn run<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, meeting_id: &str) -> bool {
    let settings = super::settings(app);
    if settings.retranscribe == Retranscribe::Off {
        return true;
    }
    let meeting = sqlx::query("SELECT created_at, folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let Some(meeting) = meeting else { return true };
    let created = meeting
        .try_get::<String, _>("created_at")
        .ok()
        .and_then(|created| DateTime::parse_from_rfc3339(&created).ok())
        .map(|created| created.with_timezone(&Utc));
    if let (Some(since), Some(created)) = (settings.since, created) {
        if created < since {
            return true; // recorded before the re-transcription was turned on
        }
    }
    let folder = meeting.try_get::<Option<String>, _>("folder_path").ok().flatten();
    let Some(folder) = folder.filter(|folder| !folder.trim().is_empty()) else {
        log::info!("post-meeting: no recording folder for {}, transcript kept", meeting_id);
        return true;
    };
    backup_live_transcript(pool, meeting_id, Path::new(&folder)).await;

    let model = chosen_model(&settings.model).await;
    // The second model is only useful when Claude merges the two readings.
    let alt_model = match settings.retranscribe {
        Retranscribe::Full => crate::custom_models::second_opinion::alt_model(app, &settings.second_opinion, model.as_deref()),
        _ => None,
    };
    let started = Instant::now();
    log::info!("post-meeting: re-transcribing {} ({:?})", meeting_id, model);
    let result = crate::audio::retranscription::start_retranscription_with_alt(
        app.clone(),
        meeting_id.to_string(),
        folder.clone(),
        None,
        model,
        Some("parakeet".to_string()),
        alt_model,
    )
    .await;
    match result {
        Ok(result) => log::info!(
            "post-meeting: {} re-transcribed, {} segments in {:.0}s",
            meeting_id,
            result.segments_count,
            started.elapsed().as_secs_f64()
        ),
        Err(error) => {
            let error = error.to_string();
            // A recording cancels the work, a manual re-transcription blocks it: redo it later.
            if crate::audio::recording_commands::is_recording().await || error.contains("already in progress") {
                log::info!("post-meeting: re-transcription of {} postponed ({}), will retry", meeting_id, error);
                return false;
            }
            // Cancelled by the user, or failed: the live transcript stays.
            log::warn!("post-meeting: re-transcription of {} not done, live transcript kept: {}", meeting_id, error);
            return true;
        }
    }
    if settings.retranscribe == Retranscribe::Full {
        let started = Instant::now();
        match cleanup::run(app, pool, &settings.clean_model, meeting_id, Path::new(&folder)).await {
            Ok((kept, dropped)) => log::info!(
                "post-meeting: transcript of {} cleaned, {} segments kept, {} noise dropped in {:.0}s",
                meeting_id,
                kept,
                dropped,
                started.elapsed().as_secs_f64()
            ),
            Err(error) => log::warn!("post-meeting: clean-up of {} skipped: {}", meeting_id, error),
        }
    }
    true
}

/// The model chosen in the settings; when automatic, an installed custom Parakeet model with
/// "ultra" in its name (a fine-tuned v3, clearly better at the same speed), otherwise None (the
/// live model).
async fn chosen_model(chosen: &str) -> Option<String> {
    if !chosen.is_empty() {
        return Some(chosen.to_string());
    }
    use crate::parakeet_engine::parakeet_engine::ModelStatus;
    crate::custom_models::engine_hooks::parakeet_models()
        .await
        .into_iter()
        .filter(|model| matches!(model.status, ModelStatus::Available))
        .find(|model| model.name.to_lowercase().contains("ultra"))
        .map(|model| model.name)
}

async fn backup_live_transcript(pool: &sqlx::SqlitePool, meeting_id: &str, folder: &Path) {
    let target = folder.join("transcripts.live.json");
    if target.exists() {
        return; // keep the first (really live) copy when the work is retried
    }
    let rows = sqlx::query("SELECT transcript, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let segments: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "text": row.try_get::<String, _>("transcript").unwrap_or_default(),
                "start": row.try_get::<Option<f64>, _>("audio_start_time").ok().flatten(),
                "end": row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten(),
            })
        })
        .collect();
    let backup = serde_json::json!({ "meeting_id": meeting_id, "segments": segments });
    if let Err(error) = crate::fork_store::write_json(&target, &backup) {
        log::warn!("post-meeting: cannot save the live transcript backup: {}", error);
    }
}
