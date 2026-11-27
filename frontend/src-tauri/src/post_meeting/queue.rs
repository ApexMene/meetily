//! Persistent job queue ("never lose a meeting"):
//! - every saved meeting becomes a JSON job in `<app data>/post-meeting/queue/`, so jobs survive
//!   crashes and restarts; finished ones move to `done/`;
//! - one worker runs the jobs in order, never while recording, and retries failures with
//!   exponential backoff (1 min .. 1 h);
//! - a periodic scan queues every recent meeting that is neither queued nor done, so a meeting is
//!   processed even when the summary step never ran.

use super::Retranscribe;
use crate::fork_store::write_json;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::Notify;

/// How often the worker wakes up without being poked.
const TICK: Duration = Duration::from_secs(60);
/// A meeting must be this old before the scan picks it up (recording fully saved).
const SCAN_SETTLE_SECS: i64 = 120;
/// The scan and the jobs window only look back this far.
pub const LOOKBACK_DAYS: i64 = 7;
/// The user is told once a job has failed this many times (it keeps retrying anyway).
const NOTIFY_AFTER_FAILURES: u32 = 3;

static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Meeting being processed right now.
static RUNNING: Mutex<Option<String>> = Mutex::new(None);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub meeting_id: String,
    pub attempts: u32,
    pub next_attempt: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    failure_notified: bool,
    /// Re-transcription and clean-up done (or not wanted): the transcript in the database is final.
    retranscribed: bool,
}

pub fn wake() {
    WAKE.notify_one();
}

pub fn running() -> Option<String> {
    RUNNING.lock().ok().and_then(|running| running.clone())
}

fn set_running(meeting_id: Option<&str>) {
    if let Ok(mut running) = RUNNING.lock() {
        *running = meeting_id.map(str::to_string);
    }
}

fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn job_path(dir: &Path, folder: &str, meeting_id: &str) -> PathBuf {
    dir.join(folder).join(format!("{}.json", sanitize(meeting_id)))
}

pub fn read_job(path: &Path) -> Option<Job> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn pool<R: Runtime>(app: &AppHandle<R>) -> Option<sqlx::SqlitePool> {
    app.try_state::<crate::state::AppState>().map(|state| state.db_manager.pool().clone())
}

/// Meetings saved before this instant are never processed: the earliest time an active step was
/// turned on, and never more than `LOOKBACK_DAYS` ago. None when every step is off.
pub fn since<R: Runtime>(app: &AppHandle<R>) -> Option<DateTime<Utc>> {
    let lookback = Utc::now() - ChronoDuration::days(LOOKBACK_DAYS);
    let settings = super::settings(app);
    let notion = crate::notion_agent::settings(app);
    let retranscribe = (settings.retranscribe != Retranscribe::Off).then(|| settings.since.unwrap_or(lookback));
    let notion = notion.enabled.then(|| notion.since.unwrap_or(lookback));
    retranscribe.into_iter().chain(notion).min().map(|since| since.max(lookback))
}

/// Queues a meeting (idempotent) when an after-meeting step is on. Called when a summary is
/// saved; the periodic scan covers every other path.
pub fn enqueue<R: Runtime>(app: &AppHandle<R>, meeting_id: &str) {
    if !super::is_active(app) {
        return;
    }
    let Some(dir) = super::data_dir(app) else { return };
    if job_path(&dir, "done", meeting_id).exists() || job_path(&dir, "queue", meeting_id).exists() {
        return;
    }
    queue(&dir, meeting_id, false);
}

/// True while a meeting waits in the queue (its recording is still needed).
pub fn is_queued<R: Runtime>(app: &AppHandle<R>, meeting_id: &str) -> bool {
    super::data_dir(app).is_some_and(|dir| job_path(&dir, "queue", meeting_id).exists())
}

/// Processes a meeting again from its last step: a transcript already re-transcribed (and maybe
/// cleaned by hand since) is not redone, only the recap.
pub fn requeue(dir: &Path, meeting_id: &str) {
    let retranscribed = [job_path(dir, "queue", meeting_id), job_path(dir, "done", meeting_id)]
        .iter()
        .filter_map(|path| read_job(path))
        .any(|job| job.retranscribed);
    let _ = std::fs::remove_file(job_path(dir, "done", meeting_id));
    queue(dir, meeting_id, retranscribed);
}

/// Writes a fresh job, replacing any previous one.
fn queue(dir: &Path, meeting_id: &str, retranscribed: bool) {
    let job = Job { meeting_id: meeting_id.to_string(), retranscribed, ..Default::default() };
    match write_json(&job_path(dir, "queue", meeting_id), &job) {
        Ok(()) => {
            log::info!("post-meeting: queued {}", meeting_id);
            wake();
        }
        Err(error) => log::error!("post-meeting: cannot queue {}: {}", meeting_id, error),
    }
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        loop {
            if let (Some(since), Some(dir)) = (since(&app), super::data_dir(&app)) {
                if let Err(error) = scan(&app, since).await {
                    log::warn!("post-meeting: scan failed: {}", error);
                }
                run_due_jobs(&app, &dir).await;
            }
            let _ = tokio::time::timeout(TICK, WAKE.notified()).await;
        }
    });
}

async fn scan<R: Runtime>(app: &AppHandle<R>, since: DateTime<Utc>) -> Result<(), String> {
    let Some(pool) = pool(app) else { return Ok(()) };
    let now = Utc::now();
    let rows = sqlx::query("SELECT id, created_at FROM meetings")
        .fetch_all(&pool)
        .await
        .map_err(|error| error.to_string())?;
    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let created: String = row.try_get("created_at").unwrap_or_default();
        let Ok(created) = DateTime::parse_from_rfc3339(&created).map(|date| date.with_timezone(&Utc)) else { continue };
        if created >= since && (now - created).num_seconds() >= SCAN_SETTLE_SECS {
            enqueue(app, &id);
        }
    }
    Ok(())
}

fn list_jobs(dir: &Path) -> Vec<(PathBuf, Job)> {
    let Ok(entries) = std::fs::read_dir(dir.join("queue")) else { return Vec::new() };
    let mut jobs: Vec<(PathBuf, Job)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| read_job(&path).map(|job| (path, job)))
        .collect();
    jobs.sort_by_key(|(_, job)| job.next_attempt);
    jobs
}

/// Queued jobs that already failed at least once (for the health check).
pub fn failing_jobs<R: Runtime>(app: &AppHandle<R>) -> usize {
    super::data_dir(app).map_or(0, |dir| list_jobs(&dir).iter().filter(|(_, job)| job.attempts > 0).count())
}

async fn run_due_jobs<R: Runtime>(app: &AppHandle<R>, dir: &Path) {
    let Some(pool) = pool(app) else { return };
    for (path, mut job) in list_jobs(dir) {
        // Never while recording: the work competes for CPU and network.
        if crate::audio::recording_commands::is_recording().await {
            return;
        }
        if job.next_attempt.is_some_and(|at| at > Utc::now()) {
            continue;
        }
        log::info!("post-meeting: processing {} (attempt {})", job.meeting_id, job.attempts + 1);
        set_running(Some(&job.meeting_id));
        let result = run_job(app, &pool, &path, &mut job).await;
        set_running(None);
        match result {
            Ok(Some(outcome)) => {
                let record = serde_json::json!({
                    "meeting_id": job.meeting_id,
                    "finished": Utc::now(),
                    "outcome": outcome,
                    "retranscribed": job.retranscribed,
                });
                if write_json(&job_path(dir, "done", &job.meeting_id), &record).is_ok() {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Ok(None) => return, // interrupted by a new recording: redo it afterwards
            Err(error) => retry_later(app, &path, &mut job, error),
        }
    }
}

/// Runs the steps of a job. `Ok(None)` = interrupted, to be redone.
async fn run_job<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, path: &Path, job: &mut Job) -> Result<Option<serde_json::Value>, String> {
    if !job.retranscribed {
        if !super::retranscribe::run(app, pool, &job.meeting_id).await {
            return Ok(None);
        }
        job.retranscribed = true;
        let _ = write_json(path, &*job);
    }
    crate::voiceprint::run(app, pool, &job.meeting_id).await; // optional, never fails the job
    let outcome = if crate::notion_agent::is_enabled(app) {
        crate::notion_agent::write_recap(app, pool, &job.meeting_id).await?
    } else {
        crate::notion_agent::Outcome { status: "done".into(), ..Default::default() }
    };
    serde_json::to_value(outcome).map(Some).map_err(|error| error.to_string())
}

fn retry_later<R: Runtime>(app: &AppHandle<R>, path: &Path, job: &mut Job, error: String) {
    job.attempts += 1;
    let backoff = 60u64.saturating_mul(1 << job.attempts.min(6)).min(3600);
    job.next_attempt = Some(Utc::now() + ChronoDuration::seconds(backoff as i64));
    log::warn!("post-meeting: {} failed ({}), retry in {}s", job.meeting_id, error, backoff);
    if job.attempts >= NOTIFY_AFTER_FAILURES && !job.failure_notified {
        job.failure_notified = true;
        use tauri_plugin_notification::NotificationExt;
        let body = format!("Cannot write the meeting recap ({}). Meetily keeps retrying.", crate::notion_agent::truncate(&error, 120));
        let _ = app.notification().builder().title("Meetily: recap waiting").body(body).show();
    }
    job.last_error = Some(error);
    let _ = write_json(path, &*job);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ids() {
        assert_eq!(sanitize("meeting-a/b:c"), "meeting-a_b_c");
        assert!(job_path(Path::new("x"), "queue", "../evil").ends_with("___evil.json"));
    }
}
