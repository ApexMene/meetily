//! "After-meeting status" window (`public/post-meeting-jobs.html`): health check and the state of
//! every recent meeting's job, with retry.

use super::queue::{self, job_path, read_job};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use tauri::{AppHandle, Manager, Runtime};

const LABEL: &str = "post-meeting-jobs";

pub fn register_capability<R: Runtime>(app: &AppHandle<R>) {
    let capability = tauri::ipc::CapabilityBuilder::new("fork-post-meeting-jobs").window(LABEL).permission("core:default");
    if let Err(error) = app.add_capability(capability) {
        log::warn!("post-meeting: cannot register the jobs window capability: {}", error);
    }
}

/// Shows the window (tray menu).
pub fn open_jobs_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(app, LABEL, tauri::WebviewUrl::App("post-meeting-jobs.html".into()))
        .title("Meetily · After the meeting")
        .inner_size(760.0, 640.0)
        .min_inner_size(520.0, 320.0)
        .center()
        .build();
    if let Err(error) = built {
        log::warn!("post-meeting: cannot open the jobs window: {}", error);
    }
}

#[tauri::command]
pub async fn post_meeting_open_jobs<R: Runtime>(app: AppHandle<R>) {
    open_jobs_window(&app);
}

/// One row of the window.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    meeting_id: String,
    title: String,
    /// Meeting save time (RFC 3339, UTC).
    created_at: String,
    /// waiting | queued | running | retrying | done | ok | skipped
    state: String,
    attempts: u32,
    next_attempt: Option<DateTime<Utc>>,
    last_error: Option<String>,
    finished: Option<String>,
    summary_url: Option<String>,
    tasks_created: Option<i64>,
    note: Option<String>,
}

/// Every meeting since the steps were turned on (at most `LOOKBACK_DAYS`), newest first.
#[tauri::command]
pub async fn post_meeting_jobs<R: Runtime>(app: AppHandle<R>) -> Result<Vec<JobView>, String> {
    let (Some(dir), Some(pool)) = (super::data_dir(&app), queue::pool(&app)) else {
        return Err("Meetily is still starting".into());
    };
    let since = queue::since(&app);
    let rows = sqlx::query("SELECT id, title, created_at FROM meetings")
        .fetch_all(&pool)
        .await
        .map_err(|error| error.to_string())?;
    let running = queue::running();
    let recording = crate::audio::recording_commands::is_recording().await;
    let mut views = Vec::new();
    for row in rows {
        let mut view = JobView {
            meeting_id: row.try_get("id").unwrap_or_default(),
            title: row.try_get("title").unwrap_or_default(),
            created_at: row.try_get("created_at").unwrap_or_default(),
            ..Default::default()
        };
        let Ok(created) = DateTime::parse_from_rfc3339(&view.created_at).map(|date| date.with_timezone(&Utc)) else { continue };
        let done = std::fs::read(job_path(&dir, "done", &view.meeting_id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        if let Some(record) = done {
            let outcome = &record["outcome"];
            let text = |value: &serde_json::Value| value.as_str().map(str::to_string);
            view.state = text(&outcome["status"]).unwrap_or_else(|| "done".into());
            view.finished = text(&record["finished"]);
            view.summary_url = text(&outcome["summary_url"]);
            view.tasks_created = outcome["tasks_created"].as_i64();
            view.note = text(&outcome["note"]);
        } else if let Some(job) = read_job(&job_path(&dir, "queue", &view.meeting_id)) {
            view.state = if running.as_deref() == Some(view.meeting_id.as_str()) {
                "running"
            } else if job.attempts > 0 {
                "retrying"
            } else {
                "queued"
            }
            .into();
            view.attempts = job.attempts;
            view.next_attempt = job.next_attempt;
            view.last_error = job.last_error;
            if recording && view.state != "running" {
                view.note = Some("starts when the recording ends".into());
            }
        } else if since.is_some_and(|since| created >= since) {
            view.state = "waiting".into();
            view.note = Some("queued shortly".into());
        } else {
            continue;
        }
        views.push(view);
    }
    views.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(views)
}

/// Processes a meeting again now, clearing a finished result or a retry wait.
#[tauri::command]
pub async fn post_meeting_retry<R: Runtime>(app: AppHandle<R>, meeting_id: String) -> Result<(), String> {
    if queue::running().as_deref() == Some(meeting_id.as_str()) {
        return Err("Already running".into());
    }
    let dir = super::data_dir(&app).ok_or("app data directory unavailable")?;
    log::info!("post-meeting: retry requested for {}", meeting_id);
    queue::requeue(&dir, &meeting_id);
    Ok(())
}

/// Opens a Notion page in the browser or the Notion app. Only Notion links: the URL comes from
/// Claude's answer.
#[tauri::command]
pub async fn post_meeting_open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid link".to_string())?;
    if !is_notion_link(&parsed) {
        return Err("Not a Notion link".into());
    }
    open::that_detached(parsed.as_str()).map_err(|error| error.to_string())
}

fn is_notion_link(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && ["notion.so", "notion.site"]
            .iter()
            .any(|domain| host == *domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_notion_links() {
        let check = |url: &str| is_notion_link(&url::Url::parse(url).unwrap());
        assert!(check("https://www.notion.so/page-123"));
        assert!(check("https://team.notion.site/page"));
        assert!(!check("http://www.notion.so/page"));
        assert!(!check("https://evilnotion.so/page"));
        assert!(!check("https://notion.so.evil.com/page"));
    }
}
