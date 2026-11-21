//! Deletes the recording audio of meetings older than the retention period (off by default).
//! Transcripts, summaries and every other file in the meeting folder are kept.

use std::path::Path;
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};

/// Audio files Meetily itself writes in a meeting folder: recordings (`audio.mp4`) and imports
/// (`audio.<ext>`). Anything else the user put there is never touched.
fn is_meetily_audio(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let name = name.to_ascii_lowercase();
    if name == "recording.mp4" {
        return true;
    }
    name.strip_prefix("audio.").is_some_and(|ext| {
        ["mp4", "m4a", "wav", "mp3", "flac", "ogg", "aac", "mkv", "webm", "wma"].contains(&ext)
    })
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        // Let the app finish starting first.
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            run(&app).await;
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

pub async fn run<R: Runtime>(app: &AppHandle<R>) {
    let days = super::settings(app).audio_retention_days;
    if days == 0 || crate::audio::recording_commands::is_recording().await {
        return;
    }
    let Some(state) = app.try_state::<crate::state::AppState>() else { return };
    let pool = state.db_manager.pool().clone();
    let rows: Vec<(String, Option<String>, String)> =
        match sqlx::query_as("SELECT id, folder_path, created_at FROM meetings WHERE folder_path IS NOT NULL")
            .fetch_all(&pool)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("audio-retention: cannot list meetings: {}", error);
                return;
            }
        };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
    let folders: Vec<String> = rows
        .into_iter()
        .filter(|(_, _, created)| chrono::DateTime::parse_from_rfc3339(created).is_ok_and(|created| created < cutoff))
        // A meeting still waiting for its re-transcription keeps its audio.
        .filter(|(id, _, _)| !crate::post_meeting::is_queued(app, id))
        .filter_map(|(_, folder, _)| folder.filter(|folder| !folder.trim().is_empty()))
        .collect();
    let deleted = tauri::async_runtime::spawn_blocking(move || delete_audio(&folders)).await;
    if let Ok((files, bytes)) = deleted {
        if files > 0 {
            log::info!("audio-retention: deleted {} audio files ({} MB) older than {} days", files, bytes / 1_048_576, days);
        }
    }
}

/// Deletes the Meetily audio files in these folders. Returns (files, bytes).
fn delete_audio(folders: &[String]) -> (u32, u64) {
    let (mut files, mut bytes) = (0u32, 0u64);
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_meetily_audio(&path) {
                continue;
            }
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    files += 1;
                    bytes += size;
                }
                Err(error) => log::warn!("audio-retention: cannot delete {}: {}", path.display(), error),
            }
        }
    }
    (files, bytes)
}

#[cfg(test)]
mod tests {
    use super::is_meetily_audio;
    use std::path::Path;

    #[test]
    fn only_meetily_audio_files() {
        assert!(is_meetily_audio(Path::new("C:/m/audio.mp4")));
        assert!(is_meetily_audio(Path::new("C:/m/Audio.WAV")));
        assert!(is_meetily_audio(Path::new("C:/m/recording.mp4")));
        assert!(!is_meetily_audio(Path::new("C:/m/interview.mp3")));
        assert!(!is_meetily_audio(Path::new("C:/m/transcripts.json")));
        assert!(!is_meetily_audio(Path::new("C:/m/audio.txt")));
    }
}
