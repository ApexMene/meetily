//! Merges two or more meetings into a new one. The transcripts follow each other in
//! chronological order, each block opened by a line with the source title. The transcript view
//! orders segments by `audio_start_time`, so the copies are placed on one continuous timeline:
//! every source keeps its own times, shifted after the previous source. The sources are left
//! untouched; the new meeting has no audio folder (and no summary yet).

use crate::state::AppState;
use chrono::Utc;
use sqlx::Row;

/// Room for the title line between two merged meetings (seconds).
const GAP: f64 = 1.0;
/// Length given to segments without times (meetings saved before audio sync).
const UNTIMED: f64 = 1.0;
const MAX_TITLE: usize = 200;

/// Start and end of every segment of one meeting on the merged timeline, the meeting starting at
/// `offset`, plus the end of the meeting. Segments come in their own order.
fn place(times: &[(Option<f64>, Option<f64>)], offset: f64) -> (Vec<(f64, f64)>, f64) {
    let mut cursor = offset;
    let placed = times
        .iter()
        .map(|&(start, end)| {
            let (start, end) = match start {
                Some(start) => (offset + start, offset + end.unwrap_or(start).max(start)),
                None => (cursor, cursor + UNTIMED),
            };
            cursor = cursor.max(end);
            (start, end)
        })
        .collect();
    (placed, cursor)
}

fn merged_title(titles: &[&str]) -> String {
    let title = format!("Merged: {}", titles.join(" + "));
    title.chars().take(MAX_TITLE).collect()
}

/// Creates the merged meeting and returns its id.
#[tauri::command]
pub async fn meeting_edit_merge(state: tauri::State<'_, AppState>, meeting_ids: Vec<String>) -> Result<String, String> {
    let mut ids: Vec<String> = Vec::new();
    for id in meeting_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() < 2 {
        return Err("Select at least two meetings".into());
    }
    let pool = state.db_manager.pool();
    let mut tx = pool.begin().await.map_err(|error| error.to_string())?;

    let mut sources: Vec<(String, String, String)> = Vec::new(); // id, title, created_at
    for id in &ids {
        let row: Option<(String, String, String)> = sqlx::query_as("SELECT id, title, created_at FROM meetings WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
        sources.push(row.ok_or("One of the selected meetings no longer exists")?);
    }
    sources.sort_by(|a, b| a.2.cmp(&b.2));

    let new_id = format!("meeting-{}", uuid::Uuid::new_v4());
    let titles: Vec<&str> = sources.iter().map(|(_, title, _)| title.as_str()).collect();
    sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, NULL)")
        .bind(&new_id)
        .bind(merged_title(&titles))
        .bind(&sources[0].2)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;

    let insert = "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, audio_start_time, audio_end_time, duration) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
    let mut offset = 0.0;
    for (source_id, title, created_at) in &sources {
        sqlx::query(insert)
            .bind(format!("transcript-{}", uuid::Uuid::new_v4()))
            .bind(&new_id)
            .bind(format!("--- {} ---", title))
            .bind(created_at)
            .bind(None::<String>)
            .bind(offset)
            .bind(offset)
            .bind(0.0)
            .execute(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
        let rows = sqlx::query(
            "SELECT transcript, timestamp, speaker, audio_start_time, audio_end_time FROM transcripts \
             WHERE meeting_id = ? ORDER BY audio_start_time, timestamp",
        )
        .bind(source_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        let times: Vec<(Option<f64>, Option<f64>)> = rows
            .iter()
            .map(|row| {
                let time = |column| row.try_get::<Option<f64>, _>(column).ok().flatten();
                (time("audio_start_time"), time("audio_end_time"))
            })
            .collect();
        let (placed, end) = place(&times, offset + GAP);
        for (row, (start, stop)) in rows.iter().zip(placed) {
            sqlx::query(insert)
                .bind(format!("transcript-{}", uuid::Uuid::new_v4()))
                .bind(&new_id)
                .bind(row.try_get::<String, _>("transcript").unwrap_or_default())
                .bind(row.try_get::<String, _>("timestamp").unwrap_or_default())
                .bind(row.try_get::<Option<String>, _>("speaker").ok().flatten())
                .bind(start)
                .bind(stop)
                .bind(stop - start)
                .execute(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        }
        offset = end + GAP;
    }
    tx.commit().await.map_err(|error| error.to_string())?;
    log::info!("meeting-edit: merged {} meetings into {}", sources.len(), new_id);
    Ok(new_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_segments_on_one_timeline() {
        let (placed, end) = place(&[(Some(0.5), Some(2.0)), (Some(3.0), None)], 10.0);
        assert_eq!(placed, vec![(10.5, 12.0), (13.0, 13.0)]);
        assert_eq!(end, 13.0);
        // Meetings without times keep their order, one slot per segment.
        let (placed, end) = place(&[(None, None), (None, None)], 5.0);
        assert_eq!(placed, vec![(5.0, 6.0), (6.0, 7.0)]);
        assert_eq!(end, 7.0);
        assert_eq!(place(&[], 4.0), (vec![], 4.0));
    }

    #[test]
    fn builds_a_bounded_title() {
        assert_eq!(merged_title(&["Kickoff", "Follow-up"]), "Merged: Kickoff + Follow-up");
        assert_eq!(merged_title(&["è".repeat(300).as_str(), "x"]).chars().count(), MAX_TITLE);
    }
}
