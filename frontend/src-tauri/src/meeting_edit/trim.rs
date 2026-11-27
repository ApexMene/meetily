//! Removes transcript segments from a meeting and cuts the same time ranges out of its audio, so
//! the remaining segments (shifted back) stay in sync with the recording, the speaker timeline
//! (`speakers.json`) included. The first trim keeps the untouched recording as
//! `audio.original.<ext>`, deleted together with the audio by the retention setting.
//! A meeting without audio (merged, or audio already deleted) only loses the segments.

use crate::speakers::{self, Run};
use crate::state::AppState;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tauri::{AppHandle, Runtime};

pub const BACKUP_PREFIX: &str = "audio.original.";

type Range = (f64, f64);

/// Sorted, non-overlapping ranges.
fn merge_ranges(mut ranges: Vec<Range>) -> Vec<Range> {
    ranges.retain(|(start, end)| end > start);
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<Range> = Vec::new();
    for (start, end) in ranges {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Seconds removed before time `t`.
fn removed_before(ranges: &[Range], t: f64) -> f64 {
    ranges.iter().map(|&(start, end)| (t.min(end) - start).max(0.0)).sum()
}

/// Where time `t` of the old audio ends up in the cut one.
fn shift(ranges: &[Range], t: f64) -> f64 {
    t - removed_before(ranges, t)
}

/// The speaker timeline of the cut audio: removed time is dropped, the rest moves back.
fn cut_runs(runs: &[Run], ranges: &[Range]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for run in runs {
        // Parts of the run outside every removed range.
        let mut start = run.start;
        let mut parts = Vec::new();
        for &(cut_start, cut_end) in ranges.iter().filter(|(a, b)| *b > run.start && *a < run.end) {
            if cut_start > start {
                parts.push((start, cut_start));
            }
            start = start.max(cut_end);
        }
        if run.end > start {
            parts.push((start, run.end));
        }
        for (part_start, part_end) in parts {
            let (part_start, part_end) = (shift(ranges, part_start), shift(ranges, part_end));
            match out.last_mut() {
                Some(last) if last.source == run.source && (last.end - part_start).abs() < 1e-6 => last.end = part_end,
                _ => out.push(Run { start: part_start, end: part_end, source: run.source }),
            }
        }
    }
    out
}

/// Encoder for the cut file: the recordings are AAC in MP4; other containers use FFmpeg's default.
fn encoder_args(extension: &str) -> &'static [&'static str] {
    match extension {
        "mp4" | "m4a" | "aac" => &["-c:a", "aac", "-b:a", "192k"],
        _ => &[],
    }
}

/// The recording of the meeting folder, never the backup of a previous trim.
fn audio_file(folder: &Path) -> Option<PathBuf> {
    let audio = crate::audio::retranscription::find_audio_file(folder).ok()?;
    let name = audio.file_name()?.to_string_lossy().to_ascii_lowercase();
    (!name.starts_with(BACKUP_PREFIX)).then_some(audio)
}

/// Writes the cut audio next to the original and returns its path. Keeps the backup first.
fn cut_audio(audio: &Path, ranges: &[Range]) -> Result<PathBuf, String> {
    let ffmpeg = crate::audio::ffmpeg::find_ffmpeg_path().ok_or("FFmpeg not found")?;
    let extension = audio.extension().and_then(|ext| ext.to_str()).unwrap_or("mp4").to_ascii_lowercase();
    let backup = audio.with_file_name(format!("{}{}", BACKUP_PREFIX, extension));
    if !backup.exists() {
        std::fs::copy(audio, &backup).map_err(|error| format!("Cannot keep a copy of the audio: {}", error))?;
    }
    let removed = ranges.iter().map(|(start, end)| format!("between(t,{:.3},{:.3})", start, end)).collect::<Vec<_>>().join("+");
    let filter = format!("aselect='not({})',asetpts=N/SR/TB", removed);
    let output = audio.with_file_name(format!("audio.trim-tmp.{}", extension));
    let mut command = Command::new(ffmpeg);
    command
        .args(["-y", "-i"])
        .arg(audio)
        .args(["-vn", "-af", &filter])
        .args(encoder_args(&extension))
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let result = command.output().map_err(|error| format!("FFmpeg failed to start: {}", error))?;
    if !result.status.success() {
        let _ = std::fs::remove_file(&output);
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!("FFmpeg error: {}", stderr.lines().last().unwrap_or("unknown")));
    }
    Ok(output)
}

/// Removes the given segments; returns the seconds cut from the audio (0 without audio).
#[tauri::command]
pub async fn meeting_edit_trim<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    transcript_ids: Vec<String>,
) -> Result<f64, String> {
    if transcript_ids.is_empty() {
        return Ok(0.0);
    }
    if crate::post_meeting::is_queued(&app, &meeting_id) {
        return Err("The meeting is still being processed after the recording: try again when it is done.".into());
    }
    let selected: HashSet<String> = transcript_ids.into_iter().collect();
    let pool = state.db_manager.pool();
    let folder: Option<String> = sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(&meeting_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| error.to_string())?
        .flatten();
    let rows: Vec<(String, Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT id, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ?")
            .bind(&meeting_id)
            .fetch_all(pool)
            .await
            .map_err(|error| error.to_string())?;
    let ranges = merge_ranges(
        rows.iter()
            .filter(|(id, _, _)| selected.contains(id))
            .filter_map(|&(_, start, end)| start.zip(end))
            .collect(),
    );

    let folder = folder.map(PathBuf::from).filter(|folder| folder.is_dir());
    let audio = folder.as_deref().and_then(audio_file).filter(|_| !ranges.is_empty());
    let cut = match &audio {
        Some(audio) => {
            let (audio, ranges) = (audio.clone(), ranges.clone());
            Some(tokio::task::spawn_blocking(move || cut_audio(&audio, &ranges)).await.map_err(|error| error.to_string())??)
        }
        None => None,
    };

    let result = async {
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        for id in &selected {
            sqlx::query("DELETE FROM transcripts WHERE id = ? AND meeting_id = ?")
                .bind(id)
                .bind(&meeting_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let (Some(audio), Some(cut)) = (&audio, &cut) {
            for (id, start, end) in rows.iter().filter(|(id, _, _)| !selected.contains(id)) {
                let (Some(start), Some(end)) = (start, end) else { continue };
                let (start, end) = (shift(&ranges, *start), shift(&ranges, *end));
                sqlx::query("UPDATE transcripts SET audio_start_time = ?, audio_end_time = ?, duration = ? WHERE id = ?")
                    .bind(start)
                    .bind(end)
                    .bind(end - start)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            // Replace the audio last, so a failure leaves both audio and transcript as they were.
            std::fs::rename(cut, audio).map_err(|error| format!("Cannot replace the audio: {}", error))?;
        }
        tx.commit().await.map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = result {
        if let Some(cut) = &cut {
            let _ = std::fs::remove_file(cut);
        }
        return Err(error);
    }

    let removed = if cut.is_some() { ranges.iter().map(|(start, end)| end - start).sum() } else { 0.0 };
    if let (Some(folder), true) = (&folder, removed > 0.0) {
        let runs = speakers::load(folder);
        if !runs.is_empty() {
            speakers::write(folder, &cut_runs(&runs, &ranges));
        }
    }
    log::info!("meeting-edit: removed {} segments and {:.1}s of audio from {}", selected.len(), removed, meeting_id);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speakers::Source;

    #[test]
    fn merges_and_shifts_ranges() {
        let ranges = merge_ranges(vec![(5.0, 8.0), (0.0, 2.0), (7.0, 10.0), (3.0, 3.0)]);
        assert_eq!(ranges, vec![(0.0, 2.0), (5.0, 10.0)]);
        assert_eq!(removed_before(&ranges, 12.0), 7.0);
        assert_eq!(removed_before(&ranges, 6.0), 3.0);
        assert_eq!(shift(&ranges, 4.0), 2.0);
        assert_eq!(shift(&ranges, 7.0), 3.0); // inside a cut: lands where the cut was (5 - 2)
    }

    #[test]
    fn cuts_the_speaker_timeline() {
        let run = |start, end, source| Run { start, end, source };
        let runs = vec![run(0.0, 4.0, Source::Mic), run(4.0, 6.0, Source::System), run(8.0, 10.0, Source::Mic)];
        // Removing 3..5 and 6..8: the timeline closes up and keeps its order.
        let cut = cut_runs(&runs, &[(3.0, 5.0), (6.0, 8.0)]);
        assert_eq!(cut, vec![run(0.0, 3.0, Source::Mic), run(3.0, 4.0, Source::System), run(4.0, 6.0, Source::Mic)]);
        // A run entirely removed disappears.
        assert_eq!(cut_runs(&[run(1.0, 2.0, Source::Mic)], &[(0.0, 3.0)]), vec![]);
    }

    #[test]
    fn picks_the_encoder_by_container() {
        assert_eq!(encoder_args("mp4"), &["-c:a", "aac", "-b:a", "192k"]);
        assert!(encoder_args("wav").is_empty());
    }
}
