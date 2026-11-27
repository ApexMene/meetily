//! Compares the live transcript (first reading, saved in `transcripts.live.json`) with the
//! re-transcription made after the meeting (second reading, with the most precise model) and marks
//! critical values the two disagree on. See `critical::reconcile`. Fails safe: any error leaves
//! the transcript as it is.

use super::critical::reconcile;
use serde::Deserialize;
use sqlx::Row;
use std::path::Path;

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct LiveSegment {
    pub text: String,
    pub start: Option<f64>,
    pub end: Option<f64>,
}

#[derive(Deserialize)]
struct Backup {
    segments: Vec<LiveSegment>,
}

pub fn load_live(folder: &Path) -> Vec<LiveSegment> {
    let Ok(bytes) = std::fs::read(folder.join("transcripts.live.json")) else { return Vec::new() };
    serde_json::from_slice::<Backup>(&bytes).map(|b| b.segments).unwrap_or_default()
}

/// The live texts that overlap the interval [start, end] (seconds), joined.
pub fn live_text_for(live: &[LiveSegment], start: f64, end: f64) -> String {
    live.iter()
        .filter(|segment| match (segment.start, segment.end) {
            (Some(s), Some(e)) => s < end && e > start,
            _ => false,
        })
        .map(|segment| segment.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// New text for each (id, text, start, end) whose critical values disagree with the live reading.
pub fn plan(live: &[LiveSegment], rows: &[(String, String, f64, f64)]) -> Vec<(String, String)> {
    let mut changes = Vec::new();
    for (id, text, start, end) in rows {
        let first = live_text_for(live, *start, *end);
        let result = reconcile(&first, text);
        if !result.uncertain.is_empty() {
            changes.push((id.clone(), result.text));
        }
    }
    changes
}

/// Marks the uncertain values in the stored transcript of a meeting. Returns how many segments changed.
pub async fn mark_uncertain(pool: &sqlx::SqlitePool, meeting_id: &str, folder: &Path) -> Result<usize, String> {
    let live = load_live(folder);
    if live.is_empty() {
        return Ok(0);
    }
    let rows = sqlx::query("SELECT id, transcript, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ?")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, f64, f64)> = rows
        .iter()
        .filter_map(|row| {
            Some((
                row.try_get::<String, _>("id").ok()?,
                row.try_get::<String, _>("transcript").ok()?,
                row.try_get::<Option<f64>, _>("audio_start_time").ok().flatten()?,
                row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten()?,
            ))
        })
        .collect();
    let changes = plan(&live, &rows);
    if changes.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (id, text) in &changes {
        sqlx::query("UPDATE transcripts SET transcript = ? WHERE id = ?")
            .bind(text)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(changes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(text: &str, s: f64, e: f64) -> LiveSegment {
        LiveSegment { text: text.into(), start: Some(s), end: Some(e) }
    }

    #[test]
    fn matches_overlapping_live_segments_only() {
        let l = vec![live("uno", 0.0, 5.0), live("due", 5.0, 10.0), live("tre", 10.0, 15.0), LiveSegment { text: "x".into(), start: None, end: None }];
        assert_eq!(live_text_for(&l, 4.0, 6.0), "uno due");
        assert_eq!(live_text_for(&l, 5.0, 10.0), "due");
        assert_eq!(live_text_for(&l, 100.0, 101.0), "");
    }

    #[test]
    fn plans_only_real_disagreements() {
        let l = vec![live("la versione 1.0.0 va in produzione", 0.0, 10.0), live("alle 10:30", 10.0, 14.0)];
        let rows = vec![
            ("a".to_string(), "La versione 1.0.0g va in produzione".to_string(), 0.0, 9.0),
            ("b".to_string(), "Alle 10.30".to_string(), 10.0, 14.0),
            ("c".to_string(), "Nessun numero".to_string(), 20.0, 22.0),
        ];
        let plan = plan(&l, &rows);
        assert_eq!(plan, vec![("a".to_string(), "La versione 1.0.0g (?) va in produzione".to_string())]);
    }

    #[test]
    fn missing_or_broken_backup_is_empty() {
        assert!(load_live(Path::new("/definitely/not/here")).is_empty());
        let dir = std::env::temp_dir().join(format!("gp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("transcripts.live.json"), b"{not json").unwrap();
        assert!(load_live(&dir).is_empty());
        std::fs::write(dir.join("transcripts.live.json"), br#"{"meeting_id":"m","segments":[{"text":"a 1","start":0.0,"end":1.0}]}"#).unwrap();
        assert_eq!(load_live(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
