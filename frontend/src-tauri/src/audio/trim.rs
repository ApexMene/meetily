//! Fork: remove parts of a meeting (greetings, off-topic talk).
//! Deletes the selected transcript segments and cuts the same time ranges out of the audio.
//! The first trim keeps a copy of the untouched audio as `audio.original.<ext>`.

use crate::audio::ffmpeg::find_ffmpeg_path;
use crate::audio::retranscription::find_audio_file;
use crate::state::AppState;
use log::info;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn merge_ranges(mut r: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    r.retain(|(a, b)| b > a);
    r.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in r {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Seconds removed before time `t`.
fn removed_before(ranges: &[(f64, f64)], t: f64) -> f64 {
    ranges.iter().map(|(a, b)| if t >= *b { b - a } else if t > *a { t - a } else { 0.0 }).sum()
}

fn cut_audio(audio: &PathBuf, ranges: &[(f64, f64)]) -> Result<(), String> {
    let ffmpeg = find_ffmpeg_path().ok_or("FFmpeg not found")?;
    let ext = audio.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_string();
    let backup = audio.with_file_name(format!("audio.original.{}", ext));
    if !backup.exists() {
        std::fs::copy(audio, &backup).map_err(|e| format!("Backup failed: {}", e))?;
    }
    let cond = ranges.iter().map(|(a, b)| format!("between(t,{:.3},{:.3})", a, b)).collect::<Vec<_>>().join("+");
    let filter = format!("aselect='not({})',asetpts=N/SR/TB", cond);
    let tmp = audio.with_file_name(format!("audio.trim.tmp.{}", ext));
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-i"]).arg(audio).args(["-vn", "-af", &filter, "-c:a", "aac", "-b:a", "192k"]).arg(&tmp)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let out = cmd.output().map_err(|e| format!("FFmpeg failed to start: {}", e))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("FFmpeg error: {}", err.lines().last().unwrap_or("")));
    }
    std::fs::rename(&tmp, audio).map_err(|e| format!("Replace audio failed: {}", e))?;
    Ok(())
}

/// Remove the given transcript segments from the meeting and the same ranges from its audio.
/// Returns the number of seconds removed from the audio (0 when the meeting has no audio).
#[tauri::command]
pub async fn api_trim_meeting(
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    transcript_ids: Vec<String>,
) -> Result<f64, String> {
    if transcript_ids.is_empty() {
        return Ok(0.0);
    }
    let pool = state.db_manager.pool();
    let folder: Option<String> = sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(&meeting_id).fetch_optional(pool).await.map_err(|e| e.to_string())?.flatten();

    let rows: Vec<(String, Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT id, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ?")
            .bind(&meeting_id).fetch_all(pool).await.map_err(|e| e.to_string())?;
    let ranges = merge_ranges(rows.iter()
        .filter(|(id, _, _)| transcript_ids.contains(id))
        .filter_map(|(_, a, b)| Some(((*a)?, (*b)?)))
        .collect());

    let audio = folder.map(PathBuf::from).filter(|p| p.is_dir()).and_then(|p| find_audio_file(&p).ok());
    let mut removed = 0.0;
    if let (Some(audio), false) = (&audio, ranges.is_empty()) {
        let a = audio.clone();
        let r = ranges.clone();
        tokio::task::spawn_blocking(move || cut_audio(&a, &r)).await.map_err(|e| e.to_string())??;
        removed = ranges.iter().map(|(a, b)| b - a).sum();
    }

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for id in &transcript_ids {
        sqlx::query("DELETE FROM transcripts WHERE id = ? AND meeting_id = ?")
            .bind(id).bind(&meeting_id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    if audio.is_some() && !ranges.is_empty() {
        for (id, a, b) in rows.iter().filter(|(id, _, _)| !transcript_ids.contains(id)) {
            let (Some(a), Some(b)) = (a, b) else { continue };
            let (na, nb) = (a - removed_before(&ranges, *a), b - removed_before(&ranges, *b));
            sqlx::query("UPDATE transcripts SET audio_start_time = ?, audio_end_time = ?, duration = ? WHERE id = ?")
                .bind(na).bind(nb).bind(nb - na).bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
        }
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    info!("Trimmed meeting {}: {} segments, {:.1}s of audio", meeting_id, transcript_ids.len(), removed);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merges_and_shifts() {
        let r = merge_ranges(vec![(5.0, 8.0), (0.0, 2.0), (7.0, 10.0)]);
        assert_eq!(r, vec![(0.0, 2.0), (5.0, 10.0)]);
        assert_eq!(removed_before(&r, 12.0), 7.0);
        assert_eq!(removed_before(&r, 6.0), 3.0);
    }
}
