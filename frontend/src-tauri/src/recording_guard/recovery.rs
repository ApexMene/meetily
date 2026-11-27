//! Recovers recordings left half-written by a crash or a power loss.
//!
//! A recording that ended normally has no `.checkpoints` folder (it is deleted after the merge).
//! A folder that still has complete chunks and no `audio.mp4` therefore belongs to an interrupted
//! recording. A few seconds after start-up its chunks are merged into `audio.mp4` (the chunks are
//! kept) and the user is told. Everything is best effort: a failure is logged and nothing else
//! is affected.

use std::path::{Path, PathBuf};

use crate::audio::checkpoint_files::valid_checkpoints;

/// Meeting folders directly under `base` that hold complete chunks but no non-empty `audio.mp4`.
pub fn find_orphans(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|folder| is_orphan(folder))
        .collect();
    found.sort();
    found
}

fn is_orphan(folder: &Path) -> bool {
    let checkpoints = folder.join(".checkpoints");
    if !checkpoints.is_dir() {
        return false;
    }
    let has_audio = std::fs::metadata(folder.join("audio.mp4")).map(|m| m.len() > 0).unwrap_or(false);
    if has_audio {
        return false;
    }
    valid_checkpoints(&checkpoints).map(|files| !files.is_empty()).unwrap_or(false)
}

/// Seconds of audio in a folder's chunks (30 s each).
pub fn estimated_seconds(folder: &Path) -> u64 {
    valid_checkpoints(&folder.join(".checkpoints")).map(|f| f.len() as u64 * 30).unwrap_or(0)
}

/// Starts the one-off check a little after launch.
pub fn start<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    use tauri::Emitter;
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        let base = crate::audio::recording_preferences::get_default_recordings_folder();
        let orphans = tokio::task::spawn_blocking(move || find_orphans(&base)).await.unwrap_or_default();
        for folder in orphans {
            // Never touch a folder while a recording is running.
            if crate::audio::recording_commands::is_recording().await {
                log::info!("recording-guard: a recording is running, recovery postponed");
                return;
            }
            let target = folder.clone();
            let outcome = tokio::task::spawn_blocking(move || crate::audio::incremental_saver::merge_checkpoint_dir(&target)).await;
            match outcome {
                Ok(Ok(chunks)) => {
                    let minutes = estimated_seconds(&folder) / 60;
                    log::warn!("recording-guard: recovered the audio of an interrupted recording in {} ({} chunks)", folder.display(), chunks);
                    let audio = folder.join("audio.mp4").to_string_lossy().into_owned();
                    let _ = app.emit("interrupted-recording-recovered", serde_json::json!({ "folder": folder.to_string_lossy(), "audio": audio, "minutes": minutes }));
                    use tauri_plugin_notification::NotificationExt;
                    let body = format!("Meetily e' stato chiuso durante una registrazione. Recuperati circa {} minuti di audio: {}. Per trascriverli usa Importa audio.", minutes, audio);
                    if let Err(error) = app.notification().builder().title("Registrazione interrotta recuperata").body(body).show() {
                        log::warn!("recording-guard: cannot show the notification: {}", error);
                    }
                }
                Ok(Err(error)) => log::warn!("recording-guard: cannot recover {}: {}", folder.display(), error),
                Err(error) => log::warn!("recording-guard: recovery task failed for {}: {}", folder.display(), error),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting(base: &Path, name: &str, chunks: &[(&str, usize)], audio: Option<usize>) -> PathBuf {
        let folder = base.join(name);
        std::fs::create_dir_all(folder.join(".checkpoints")).unwrap();
        for (file, size) in chunks {
            std::fs::write(folder.join(".checkpoints").join(file), vec![1u8; *size]).unwrap();
        }
        if let Some(size) = audio {
            std::fs::write(folder.join("audio.mp4"), vec![1u8; size]).unwrap();
        }
        folder
    }

    #[test]
    fn finds_only_interrupted_recordings() {
        let tmp = tempfile::tempdir().unwrap();
        let crashed = meeting(tmp.path(), "crashed", &[("audio_chunk_000.mp4", 9), ("audio_chunk_001.mp4", 9)], None);
        meeting(tmp.path(), "already_merged", &[("audio_chunk_000.mp4", 9)], Some(100));
        let empty_audio = meeting(tmp.path(), "empty_audio_file", &[("audio_chunk_000.mp4", 9)], Some(0));
        meeting(tmp.path(), "only_junk", &[("audio_chunk_000.mp4", 0), ("audio_chunk_001.mp4.part", 9)], None);
        meeting(tmp.path(), "no_chunks", &[], None);
        std::fs::create_dir_all(tmp.path().join("finished")).unwrap(); // normal recording: no .checkpoints
        std::fs::write(tmp.path().join("stray_file.txt"), b"x").unwrap();
        assert_eq!(find_orphans(tmp.path()), vec![crashed.clone(), empty_audio]);
        assert_eq!(estimated_seconds(&crashed), 60);
    }

    #[test]
    fn missing_base_folder_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_orphans(&tmp.path().join("nope")).is_empty());
        assert_eq!(estimated_seconds(&tmp.path().join("nope")), 0);
    }
}
