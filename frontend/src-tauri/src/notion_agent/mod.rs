//! Fork: post-meeting agent that writes the meeting recap and tasks into Notion.
//!
//! Meetily never talks to Notion directly. It runs Claude Code (`claude --print`, the same
//! installation the Claude Code summary provider uses) with the user's claude.ai Notion
//! connector enabled, and Claude does the writing following a prompt file.
//!
//! Reliability ("never lose a meeting"):
//! - every finished meeting becomes a JSON job in `<app data>/notion-agent/queue/`, so jobs survive
//!   crashes and restarts;
//! - a worker retries failed jobs forever with exponential backoff (1 min .. 1 h);
//! - a periodic scan enqueues any recorded meeting that is neither queued nor done, so a meeting
//!   is processed even if the summary step never ran or failed;
//! - the Notion side is idempotent: every page carries `{"meetily_id": ...}` and the prompt
//!   updates instead of duplicating.
//!
//! User-editable files in `<app data>/notion-agent/`:
//! - `config.json`  — enabled, model, proxy, test_mode, min_seconds (written with defaults);
//! - `meeting-to-notion.md` — the agent prompt (seeded from the bundled copy, never overwritten);
//! - `.env` — personal values (name, Notion ids, proxy). `{{KEY}}` in the prompt is replaced
//!   with `KEY` from this file; `PROXY` / `NO_PROXY` override config.json; `PROMPT_FILE` points to
//!   a personal prompt anywhere on disk. A commented template
//!   is written on first run.

use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;

const DEFAULT_PROMPT: &str = include_str!("prompt.md");
/// One agent run: reading context + writing a recap and a handful of tasks.
const AGENT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// How often the worker wakes up without being poked.
const TICK: Duration = Duration::from_secs(60);
/// A meeting row must be this old before the scan picks it up (recording fully saved).
const SCAN_SETTLE_SECS: i64 = 120;
/// The scan only looks back this far.
const SCAN_LOOKBACK_DAYS: i64 = 7;
/// Notify the user once a job has failed this many times (it keeps retrying anyway).
const NOTIFY_AFTER_FAILURES: u32 = 3;

const ALLOWED_TOOLS: &str = "mcp__claude_ai_Notion__notion-search,\
mcp__claude_ai_Notion__notion-fetch,\
mcp__claude_ai_Notion__notion-query-data-sources,\
mcp__claude_ai_Notion__notion-create-pages,\
mcp__claude_ai_Notion__notion-update-page";
/// Built-in tools are denied explicitly (`--tools ""` would also hide MCP connector tools).
const DISALLOWED_TOOLS: &str =
    "Bash,Read,Write,Edit,NotebookEdit,Glob,Grep,WebFetch,WebSearch,Task,Agent";
const SYSTEM_PROMPT: &str = "You are an unattended agent. Follow the instructions block in the message exactly. The last line of your answer must be the requested JSON.";

static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub model: String,
    /// HTTP proxy for Claude Code (e.g. a local bridge). Empty = inherit the environment.
    pub proxy: String,
    pub no_proxy: String,
    pub test_mode: bool,
    /// Meetings with less transcribed audio than this are skipped without calling Claude.
    pub min_seconds: f64,
    /// Meetings created before this instant are never processed (set on first run).
    pub since: Option<DateTime<Utc>>,
    /// Claude model for the transcript cleanup / merge after the re-transcription.
    pub clean_model: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "sonnet".into(),
            proxy: String::new(),
            no_proxy: "localhost,127.0.0.1".into(),
            test_mode: false,
            min_seconds: 120.0,
            since: None,
            clean_model: "haiku".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Job {
    meeting_id: String,
    attempts: u32,
    next_attempt: Option<DateTime<Utc>>,
    last_error: Option<String>,
    failure_notified: bool,
    /// Second pass done (or attempted): the transcript in the DB is the final one.
    retranscribed: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct AgentOutcome {
    status: String,
    summary_url: Option<String>,
    tasks_created: Option<i64>,
    tasks_existing: Option<i64>,
    unassigned: Option<i64>,
    note: Option<String>,
}

fn base_dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("notion-agent"))
}

fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn job_path(base: &Path, folder: &str, meeting_id: &str) -> PathBuf {
    base.join(folder).join(format!("{}.json", sanitize(meeting_id)))
}

/// Writes atomically (temp file + rename) so a crash never leaves a half-written job.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

fn load_config(base: &Path) -> AgentConfig {
    let path = base.join("config.json");
    let mut config = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<AgentConfig>(&bytes).ok())
        .unwrap_or_default();
    if config.since.is_none() {
        config.since = Some(Utc::now());
    }
    let env = load_env(base);
    let from_env = |key: &str| env.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (proxy, no_proxy) = (from_env("PROXY"), from_env("NO_PROXY"));
    // Rewrite so new fields with defaults become visible to the user.
    if let Err(error) = write_json(&path, &config) {
        log::warn!("notion-agent: cannot write config: {}", error);
    }
    // .env wins over config.json, and is not copied into it.
    if let Some(proxy) = proxy {
        config.proxy = proxy;
    }
    if let Some(no_proxy) = no_proxy {
        config.no_proxy = no_proxy;
    }
    config
}

const ENV_TEMPLATE: &str = "# Meetily -> Notion: personal values used by the agent prompt ({{KEY}}).
# Keep this file private. Lines: KEY=value
USER_NAME=
# Other ways people call you in meetings, comma separated
USER_ALIASES=
# Notion data sources (collection://...) and the Work area page URL
NOTION_TASKS_DB=
NOTION_PROJECTS_DB=
NOTION_WORK_AREA=
TIMEZONE=Europe/Rome
# Value written in the Sync_Source property of the pages the agent creates
SYNC_SOURCE=meetily
# Optional: full path of your own prompt file (default: meeting-to-notion.md next to this file)
PROMPT_FILE=
# Example task title and name fixes, to make the agent match your projects
TASK_EXAMPLE=Inviare la stima effort a Mario
NAME_FIXES=\"copilota studio\" -> Copilot Studio
# Optional Windows scheduled task the health check starts when the proxy does not answer
REPAIR_TASK=
# Optional HTTP proxy for Claude Code (e.g. http://127.0.0.1:8080) and its exclusions
PROXY=
NO_PROXY=localhost,127.0.0.1
";

/// Reads `<base>/.env` (KEY=value lines; `#` comments; optional quotes). Writes the template
/// when the file does not exist.
fn load_env(base: &Path) -> std::collections::HashMap<String, String> {
    let path = base.join(".env");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            let _ = std::fs::create_dir_all(base);
            let _ = std::fs::write(&path, ENV_TEMPLATE);
            ENV_TEMPLATE.to_string()
        }
    };
    parse_env(&text)
}

fn parse_env(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            (key.trim().to_string(), value.replace("\\\"", "\""))
        })
        .collect()
}

/// Replaces `{{KEY}}` with values from `.env`; warns about keys left empty.
fn fill_placeholders(text: &str, env: &std::collections::HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in env {
        out = out.replace(&format!("{{{{{}}}}}", key), value);
    }
    if let Some(start) = out.find("{{") {
        let missing: String = out[start..].chars().take(40).collect();
        log::warn!("notion-agent: prompt placeholder without value in .env: {}", missing);
    }
    out
}

fn load_prompt(base: &Path) -> String {
    // A personal prompt elsewhere on disk wins, when .env points to it.
    if let Some(custom) = load_env(base).get("PROMPT_FILE").map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        match std::fs::read_to_string(&custom) {
            Ok(text) if !text.trim().is_empty() => return text,
            _ => log::warn!("notion-agent: PROMPT_FILE {} not readable, using the default prompt", custom),
        }
    }
    let path = base.join("meeting-to-notion.md");
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => {
            let _ = std::fs::create_dir_all(base);
            let _ = std::fs::write(&path, DEFAULT_PROMPT);
            DEFAULT_PROMPT.to_string()
        }
    }
}

/// Queues a meeting for the Notion agent (idempotent). Called when a summary completes; the
/// periodic scan covers every other path.
pub fn enqueue<R: Runtime>(app: &AppHandle<R>, meeting_id: &str) {
    let Some(base) = base_dir(app) else { return };
    if job_path(&base, "done", meeting_id).exists() || job_path(&base, "queue", meeting_id).exists() {
        return;
    }
    let job = Job { meeting_id: meeting_id.to_string(), ..Default::default() };
    match write_json(&job_path(&base, "queue", meeting_id), &job) {
        Ok(()) => {
            log::info!("notion-agent: queued {}", meeting_id);
            WAKE.notify_one();
        }
        Err(error) => log::error!("notion-agent: cannot queue {}: {}", meeting_id, error),
    }
}

/// Starts the background worker. Safe to call once at startup.
pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let Some(base) = base_dir(&app) else {
            log::error!("notion-agent: no app data dir, agent disabled");
            return;
        };
        let _ = load_config(&base); // seeds config.json and the `since` marker
        let _ = load_prompt(&base);
        let env = load_env(&base);
        log::info!("notion-agent: .env loaded ({} values)", env.values().filter(|v| !v.trim().is_empty()).count());
        log::info!("notion-agent: worker started in {:?}", base);
        loop {
            let config = load_config(&base);
            // The queue also drives the post-meeting re-transcription, which works without Notion.
            if config.enabled || crate::meeting_detector::second_pass_mode(&app) != "off" {
                if let Err(error) = scan_meetings(&app, &base, &config).await {
                    log::warn!("notion-agent: scan failed: {}", error);
                }
                run_due_jobs(&app, &base, &config).await;
            }
            let _ = tokio::time::timeout(TICK, WAKE.notified()).await;
        }
    });
}

fn pool<R: Runtime>(app: &AppHandle<R>) -> Option<sqlx::SqlitePool> {
    app.try_state::<crate::state::AppState>().map(|state| state.db_manager.pool().clone())
}

async fn scan_meetings<R: Runtime>(app: &AppHandle<R>, base: &Path, config: &AgentConfig) -> Result<(), String> {
    let Some(pool) = pool(app) else { return Ok(()) };
    let now = Utc::now();
    let lookback = now - ChronoDuration::days(SCAN_LOOKBACK_DAYS);
    let since = config.since.map_or(lookback, |since| since.max(lookback));
    let rows = sqlx::query("SELECT id, created_at FROM meetings")
        .fetch_all(&pool)
        .await
        .map_err(|e| e.to_string())?;
    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let created: String = row.try_get("created_at").unwrap_or_default();
        let Ok(created) = DateTime::parse_from_rfc3339(&created).map(|d| d.with_timezone(&Utc)) else {
            continue;
        };
        if created >= since && (now - created).num_seconds() >= SCAN_SETTLE_SECS {
            enqueue(app, &id);
        }
    }
    Ok(())
}

fn list_jobs(base: &Path) -> Vec<(PathBuf, Job)> {
    let Ok(entries) = std::fs::read_dir(base.join("queue")) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let job = serde_json::from_slice::<Job>(&std::fs::read(&path).ok()?).ok()?;
            Some((path, job))
        })
        .collect()
}

static RUNNING: LazyLock<std::sync::Mutex<Option<String>>> = LazyLock::new(|| std::sync::Mutex::new(None));

fn set_running(meeting_id: Option<&str>) {
    if let Ok(mut running) = RUNNING.lock() {
        *running = meeting_id.map(str::to_string);
    }
}

async fn run_due_jobs<R: Runtime>(app: &AppHandle<R>, base: &Path, config: &AgentConfig) {
    // Never run the agent while a meeting is being recorded: it competes for CPU and network.
    if crate::audio::recording_commands::is_recording().await {
        return;
    }
    let now = Utc::now();
    let mut jobs = list_jobs(base);
    jobs.sort_by_key(|(_, job)| job.next_attempt);
    for (path, mut job) in jobs {
        if job.next_attempt.is_some_and(|at| at > now) {
            continue;
        }
        log::info!("notion-agent: processing {} (attempt {})", job.meeting_id, job.attempts + 1);
        set_running(Some(&job.meeting_id));
        if !job.retranscribed {
            let finished = second_pass(app, config, &job.meeting_id).await;
            if crate::audio::recording_commands::is_recording().await || !finished {
                set_running(None);
                return; // a new meeting started meanwhile: redo the pass and send the recap after it
            }
            job.retranscribed = true;
            let _ = write_json(&path, &job);
        }
        if !config.enabled {
            // Re-transcription only: no Notion recap.
            set_running(None);
            let record = serde_json::json!({ "meeting_id": job.meeting_id, "finished": Utc::now(), "outcome": { "status": "skipped", "note": "Notion disattivato, solo ritrascrizione" } });
            if write_json(&job_path(base, "done", &job.meeting_id), &record).is_ok() {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let result = process(app, base, config, &job.meeting_id).await;
        set_running(None);
        match result {
            Ok(outcome) => {
                let done = job_path(base, "done", &job.meeting_id);
                let record = serde_json::json!({ "meeting_id": job.meeting_id, "finished": Utc::now(), "outcome": outcome });
                if write_json(&done, &record).is_ok() {
                    let _ = std::fs::remove_file(&path);
                }
                notify_outcome(app, &outcome);
            }
            Err(error) => {
                job.attempts += 1;
                let backoff = 60u64.saturating_mul(1u64 << job.attempts.min(6)).min(3600);
                job.next_attempt = Some(Utc::now() + ChronoDuration::seconds(backoff as i64));
                log::warn!("notion-agent: {} failed ({}), retry in {}s", job.meeting_id, error, backoff);
                job.last_error = Some(error.clone());
                if job.attempts >= NOTIFY_AFTER_FAILURES && !job.failure_notified {
                    job.failure_notified = true;
                    notify(app, "Notion: resoconto in attesa", &format!("Non riesco a scrivere in Notion ({}). Riprovo da solo.", truncate(&error, 120)));
                }
                let _ = write_json(&path, &job);
            }
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        log::warn!("notion-agent: notification failed: {}", error);
    }
}

fn notify_outcome<R: Runtime>(app: &AppHandle<R>, outcome: &AgentOutcome) {
    if outcome.status == "skipped" {
        log::info!("notion-agent: skipped ({})", outcome.note.clone().unwrap_or_default());
        return;
    }
    let mut body = format!(
        "{} task nuove, {} da assegnare a un progetto.",
        outcome.tasks_created.unwrap_or(0),
        outcome.unassigned.unwrap_or(0)
    );
    if let Some(url) = &outcome.summary_url {
        body.push('\n');
        body.push_str(url);
    }
    notify(app, "Resoconto scritto in Notion", &body);
}

struct MeetingInput {
    meta: serde_json::Value,
    transcript: String,
    seconds: f64,
}

const WEEKDAYS: [&str; 7] = ["lunedi'", "martedi'", "mercoledi'", "giovedi'", "venerdi'", "sabato", "domenica"];

/// Nearest quarter hour (ties go up).
fn round_quarter(time: DateTime<Utc>) -> DateTime<Utc> {
    const QUARTER: i64 = 15 * 60;
    let secs = time.timestamp();
    let rounded = (secs + QUARTER / 2).div_euclid(QUARTER) * QUARTER;
    DateTime::from_timestamp(rounded, 0).unwrap_or(time)
}

/// Calendar slot for a meeting: both ends rounded to the quarter hour, at least 15 minutes long.
fn calendar_slot(start: DateTime<Utc>, end: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let slot_start = round_quarter(start);
    let slot_end = round_quarter(end).max(slot_start + ChronoDuration::minutes(15));
    (slot_start, slot_end)
}

async fn load_meeting(pool: &sqlx::SqlitePool, meeting_id: &str, test_mode: bool) -> Result<MeetingInput, String> {
    let meeting = sqlx::query("SELECT title, created_at FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "meeting not found".to_string())?;
    let title: String = meeting.try_get("title").unwrap_or_default();
    let created: String = meeting.try_get("created_at").unwrap_or_default();
    let saved = DateTime::parse_from_rfc3339(&created).map_err(|e| e.to_string())?.with_timezone(&Utc);

    let rows = sqlx::query(
        "SELECT transcript, timestamp, audio_start_time, audio_end_time FROM transcripts \
         WHERE meeting_id = ? ORDER BY COALESCE(audio_start_time, 0), timestamp",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let last_end = rows
        .iter()
        .filter_map(|row| row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten())
        .fold(0.0_f64, f64::max);
    // The meeting row is saved when the recording stops; audio times are offsets from the start.
    let start = saved - ChronoDuration::milliseconds((last_end * 1000.0) as i64);

    let mut transcript = String::new();
    for row in &rows {
        let text: String = row.try_get("transcript").unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let label = match row.try_get::<Option<f64>, _>("audio_start_time").ok().flatten() {
            Some(offset) => (start + ChronoDuration::milliseconds((offset * 1000.0) as i64))
                .with_timezone(&Local)
                .format("%H:%M:%S")
                .to_string(),
            None => row.try_get::<String, _>("timestamp").unwrap_or_default(),
        };
        transcript.push_str(&format!("[{}] {}\n", label, text.trim()));
    }

    let (calendar_start, calendar_end) = calendar_slot(start, saved);
    let local_start = start.with_timezone(&Local);
    let weekday = WEEKDAYS[local_start.format("%u").to_string().parse::<usize>().unwrap_or(1) - 1];
    let meta = serde_json::json!({
        "meetily_id": meeting_id,
        "title": title,
        "start_utc": start.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "end_utc": saved.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        // Real times rounded to the nearest quarter hour (15:07 -> 15:00, 15:27 -> 15:30): what
        // goes in the Notion Calendar property.
        "calendar_start_utc": calendar_start.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "calendar_end_utc": calendar_end.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "calendar_local": format!(
            "{} - {}",
            calendar_start.with_timezone(&Local).format("%H:%M"),
            calendar_end.with_timezone(&Local).format("%H:%M")
        ),
        "meeting_date_local": format!("{} ({})", local_start.format("%Y-%m-%d"), weekday),
        "timezone": "Europe/Rome",
        "test_mode": test_mode,
    });
    Ok(MeetingInput { meta, transcript, seconds: last_end })
}

async fn process<R: Runtime>(app: &AppHandle<R>, base: &Path, config: &AgentConfig, meeting_id: &str) -> Result<AgentOutcome, String> {
    let pool = pool(app).ok_or("database not ready")?;
    let input = load_meeting(&pool, meeting_id, config.test_mode).await?;
    if input.seconds < config.min_seconds || input.transcript.trim().is_empty() {
        return Ok(AgentOutcome {
            status: "skipped".into(),
            note: Some(format!("solo {:.0}s di audio trascritto", input.seconds)),
            ..Default::default()
        });
    }
    let prompt = fill_placeholders(&load_prompt(base), &load_env(base));
    let stdin = format!(
        "<instructions>\n{}\n</instructions>\n\nMETA\n{}\n\nTRASCRIZIONE\n{}",
        prompt, input.meta, input.transcript
    );
    let installation = crate::claude_code::agent_installation().await?;
    let output = run_claude(&installation, config, stdin).await?;
    parse_outcome(&output)
}

fn proxy_env(config: &AgentConfig) -> Vec<(String, String)> {
    if config.proxy.trim().is_empty() {
        return Vec::new();
    }
    let proxy = config.proxy.trim().to_string();
    let no_proxy = config.no_proxy.trim().to_string();
    // Both spellings: WSL's autoProxy injects lowercase variables that point to a proxy which
    // is unreachable from inside WSL, and curl-style clients prefer the lowercase ones.
    vec![
        ("https_proxy".into(), proxy.clone()),
        ("HTTPS_PROXY".into(), proxy.clone()),
        ("http_proxy".into(), proxy.clone()),
        ("HTTP_PROXY".into(), proxy),
        ("no_proxy".into(), no_proxy.clone()),
        ("NO_PROXY".into(), no_proxy),
    ]
}

async fn run_claude(installation: &crate::claude_code::Installation, config: &AgentConfig, stdin: String) -> Result<String, String> {
    let args: Vec<&str> = vec![
        "--print",
        "--output-format",
        "json",
        "--model",
        &config.model,
        "--system-prompt",
        SYSTEM_PROMPT,
        "--allowedTools",
        ALLOWED_TOOLS,
        "--disallowedTools",
        DISALLOWED_TOOLS,
        "--disable-slash-commands",
        "--setting-sources",
        "",
        "--no-session-persistence",
    ];
    run_claude_args(installation, config, &args, stdin).await
}

/// Claude Code without any tool (no MCP, no files): plain text in, plain text out.
async fn run_claude_text(installation: &crate::claude_code::Installation, config: &AgentConfig, model: &str, system: &str, stdin: String) -> Result<String, String> {
    let args: Vec<&str> = vec![
        "--print",
        "--output-format",
        "json",
        "--model",
        model,
        "--system-prompt",
        system,
        "--tools",
        "",
        "--disable-slash-commands",
        "--setting-sources",
        "",
        "--no-session-persistence",
    ];
    let stdout = run_claude_args(installation, config, &args, stdin).await?;
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default();
    let print: PrintResult = serde_json::from_str(line.trim()).map_err(|_| truncate(line, 200))?;
    if print.is_error {
        return Err(truncate(&print.result.unwrap_or_default(), 200));
    }
    Ok(print.result.unwrap_or_default())
}

async fn run_claude_args(installation: &crate::claude_code::Installation, config: &AgentConfig, args: &[&str], stdin: String) -> Result<String, String> {
    let env = proxy_env(config);
    let mut command = match &installation.runtime {
        crate::claude_code::Runtime::Native => {
            let mut command = Command::new(&installation.executable);
            command.env_remove("ANTHROPIC_API_KEY").current_dir(std::env::temp_dir());
            for (key, value) in &env {
                command.env(key, value);
            }
            command
        }
        crate::claude_code::Runtime::Wsl { distro } => {
            let mut command = Command::new("wsl.exe");
            command.args(["-d", distro, "--cd", "/tmp", "--exec", "env", "-u", "ANTHROPIC_API_KEY"]);
            for (key, value) in &env {
                command.arg(format!("{}={}", key, value));
            }
            command.arg(&installation.executable);
            command
        }
    };
    command.args(args.iter().copied()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|e| format!("cannot start Claude Code: {}", e))?;
    if let Some(mut pipe) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = pipe.write_all(stdin.as_bytes()).await;
        });
    }
    let output = tokio::time::timeout(AGENT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "Claude Code timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_string());
    }
    Ok(stdout)
}

#[derive(Deserialize)]
struct PrintResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
}

fn parse_outcome(stdout: &str) -> Result<AgentOutcome, String> {
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default();
    let print: PrintResult = serde_json::from_str(line.trim()).map_err(|_| truncate(line, 200))?;
    let text = print.result.unwrap_or_default();
    if print.is_error {
        return Err(truncate(&text, 200));
    }
    let outcome = text
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str::<AgentOutcome>(l).ok())
        .ok_or_else(|| format!("no outcome JSON in answer: {}", truncate(&text, 200)))?;
    match outcome.status.as_str() {
        "ok" | "skipped" => Ok(outcome),
        _ => Err(format!("agent error: {}", outcome.note.clone().unwrap_or_default())),
    }
}

/// Fork settings page: Claude models used after the meeting.
#[derive(Serialize, Deserialize)]
pub struct AgentModels {
    pub recap_model: String,
    pub clean_model: String,
}

const ALLOWED_MODELS: &[&str] = &["haiku", "sonnet", "opus"];

#[tauri::command]
pub async fn agent_models_get<R: Runtime>(app: AppHandle<R>) -> Result<AgentModels, String> {
    let base = base_dir(&app).ok_or("no app data dir")?;
    let config = load_config(&base);
    Ok(AgentModels { recap_model: config.model, clean_model: config.clean_model })
}

#[tauri::command]
pub async fn agent_models_set<R: Runtime>(app: AppHandle<R>, models: AgentModels) -> Result<(), String> {
    let base = base_dir(&app).ok_or("no app data dir")?;
    let path = base.join("config.json");
    // Raw file, so values coming from .env are not copied into it.
    let mut config: AgentConfig = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if ALLOWED_MODELS.contains(&models.recap_model.as_str()) {
        config.model = models.recap_model;
    }
    if ALLOWED_MODELS.contains(&models.clean_model.as_str()) {
        config.clean_model = models.clean_model;
    }
    write_json(&path, &config).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_outcome() {
        let stdout = r#"{"type":"result","is_error":false,"result":"Fatto.\n{\"status\":\"ok\",\"summary_url\":\"https://x\",\"tasks_created\":3,\"tasks_existing\":1,\"unassigned\":0,\"note\":\"\"}"}"#;
        let outcome = parse_outcome(stdout).unwrap();
        assert_eq!(outcome.status, "ok");
        assert_eq!(outcome.tasks_created, Some(3));
    }

    #[test]
    fn agent_error_is_retried() {
        let stdout = r#"{"is_error":false,"result":"{\"status\":\"error\",\"note\":\"notion down\"}"}"#;
        assert!(parse_outcome(stdout).is_err());
    }

    #[test]
    fn cli_error_is_retried() {
        assert!(parse_outcome(r#"{"is_error":true,"result":"API Error: ENETUNREACH"}"#).is_err());
        assert!(parse_outcome("not json").is_err());
    }

    #[test]
    fn calendar_rounds_to_quarters() {
        let at = |h: u32, m: u32| DateTime::parse_from_rfc3339(&format!("2026-11-20T{:02}:{:02}:00Z", h, m)).unwrap().with_timezone(&Utc);
        assert_eq!(round_quarter(at(14, 7)), at(14, 0));
        assert_eq!(round_quarter(at(14, 27)), at(14, 30));
        assert_eq!(round_quarter(at(14, 53)), at(15, 0));
        assert_eq!(calendar_slot(at(14, 2), at(14, 5)), (at(14, 0), at(14, 15)));
    }

    #[test]
    fn env_fills_prompt() {
        let env = parse_env("# c\nUSER_NAME=Mario Rossi\nNAME_FIXES=\"a\" -> B\nPROXY=\n");
        assert_eq!(env.get("USER_NAME").map(String::as_str), Some("Mario Rossi"));
        assert_eq!(fill_placeholders("Ciao {{USER_NAME}}", &env), "Ciao Mario Rossi");
        assert_eq!(env.get("PROXY").map(String::as_str), Some(""));
    }

    #[test]
    fn sanitizes_ids() {
        assert_eq!(sanitize("meeting-a/b:c"), "meeting-a_b_c");
    }
}


// ---------------------------------------------------------------------------------------------
// Fork: "Resoconti Notion" window — job states, retry.

/// One row of the jobs window.
#[derive(Debug, Serialize)]
pub struct JobView {
    meeting_id: String,
    title: String,
    /// Meeting save time (RFC3339 UTC).
    created_at: String,
    /// waiting | queued | running | retrying | ok | skipped
    state: String,
    attempts: u32,
    next_attempt: Option<DateTime<Utc>>,
    last_error: Option<String>,
    finished: Option<String>,
    summary_url: Option<String>,
    tasks_created: Option<i64>,
    note: Option<String>,
}

/// Every meeting the agent knows about (last 7 days, newest first).
#[tauri::command]
pub async fn notion_agent_jobs<R: Runtime>(app: AppHandle<R>) -> Result<Vec<JobView>, String> {
    let base = base_dir(&app).ok_or("no app data dir")?;
    let config = load_config(&base);
    let pool = pool(&app).ok_or("database not ready")?;
    let rows = sqlx::query("SELECT id, title, created_at FROM meetings")
        .fetch_all(&pool)
        .await
        .map_err(|e| e.to_string())?;
    let running = RUNNING.lock().ok().and_then(|r| r.clone());
    let recording = crate::audio::recording_commands::is_recording().await;
    let lookback = Utc::now() - ChronoDuration::days(SCAN_LOOKBACK_DAYS);
    let since = config.since.map_or(lookback, |since| since.max(lookback));
    let mut views = Vec::new();
    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let title: String = row.try_get("title").unwrap_or_default();
        let created_at: String = row.try_get("created_at").unwrap_or_default();
        let Ok(created) = DateTime::parse_from_rfc3339(&created_at).map(|d| d.with_timezone(&Utc)) else {
            continue;
        };
        let mut view = JobView {
            meeting_id: id.clone(),
            title,
            created_at,
            state: String::new(),
            attempts: 0,
            next_attempt: None,
            last_error: None,
            finished: None,
            summary_url: None,
            tasks_created: None,
            note: None,
        };
        let done = job_path(&base, "done", &id);
        let queued = job_path(&base, "queue", &id);
        if let Some(record) = std::fs::read(&done).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
            let outcome = record.get("outcome").cloned().unwrap_or_default();
            view.state = outcome.get("status").and_then(|v| v.as_str()).unwrap_or("ok").to_string();
            view.finished = record.get("finished").and_then(|v| v.as_str()).map(str::to_string);
            view.summary_url = outcome.get("summary_url").and_then(|v| v.as_str()).map(str::to_string);
            view.tasks_created = outcome.get("tasks_created").and_then(|v| v.as_i64());
            view.note = outcome.get("note").and_then(|v| v.as_str()).map(str::to_string);
        } else if let Some(job) = std::fs::read(&queued).ok().and_then(|b| serde_json::from_slice::<Job>(&b).ok()) {
            view.state = if running.as_deref() == Some(id.as_str()) {
                "running"
            } else if job.attempts > 0 {
                "retrying"
            } else {
                "queued"
            }
            .to_string();
            view.attempts = job.attempts;
            view.next_attempt = job.next_attempt;
            view.last_error = job.last_error;
            if recording && view.state != "running" {
                view.note = Some("parte a fine registrazione".into());
            }
        } else if created >= since {
            view.state = "waiting".into();
            view.note = Some("in coda tra poco".into());
        } else {
            continue; // older than the agent: never sent
        }
        views.push(view);
    }
    views.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(views)
}

/// Runs a meeting again now: clears a finished result or a retry wait.
#[tauri::command]
pub async fn notion_agent_retry<R: Runtime>(app: AppHandle<R>, meeting_id: String) -> Result<(), String> {
    let base = base_dir(&app).ok_or("no app data dir")?;
    if RUNNING.lock().ok().and_then(|r| r.clone()).as_deref() == Some(meeting_id.as_str()) {
        return Err("gia' in corso".into());
    }
    let _ = std::fs::remove_file(job_path(&base, "done", &meeting_id));
    let job = Job { meeting_id: meeting_id.clone(), ..Default::default() };
    write_json(&job_path(&base, "queue", &meeting_id), &job).map_err(|e| e.to_string())?;
    log::info!("notion-agent: retry requested for {}", meeting_id);
    WAKE.notify_one();
    Ok(())
}

/// Opens a Notion page in the default browser / Notion app.
#[tauri::command]
pub async fn notion_agent_open_url(url: String) -> Result<(), String> {
    let ok = url.starts_with("https://") && !url.contains(['"', ' ', '\n', '&', '|', '^', '<', '>']);
    if !ok {
        return Err("url non valido".into());
    }
    #[cfg(windows)]
    {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", &url])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub const JOBS_LABEL: &str = "notion-jobs";

/// Shows the "Resoconti Notion" window (tray menu, meeting bar).
pub fn open_jobs_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(JOBS_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(app, JOBS_LABEL, tauri::WebviewUrl::App("notion-jobs.html".into()))
        .title("Meetily · Resoconti Notion")
        .inner_size(760.0, 640.0)
        .min_inner_size(520.0, 320.0)
        .center()
        .build();
    if let Err(error) = built {
        log::warn!("notion-agent: cannot open jobs window: {}", error);
    }
}

#[tauri::command]
pub async fn notion_agent_open_jobs<R: Runtime>(app: AppHandle<R>) {
    open_jobs_window(&app);
}


// Fork: inputs for the health check (crate::health).

/// Proxy environment Claude runs with, and the optional repair task from `.env` (REPAIR_TASK).
pub fn health_inputs<R: Runtime>(app: &AppHandle<R>) -> (Vec<(String, String)>, String) {
    let Some(base) = base_dir(app) else { return (Vec::new(), String::new()) };
    let env = load_env(&base);
    (proxy_env(&load_config(&base)), env.get("REPAIR_TASK").cloned().unwrap_or_default())
}

/// Queued recaps that have already failed at least once.
pub fn failing_jobs<R: Runtime>(app: &AppHandle<R>) -> usize {
    let Some(base) = base_dir(app) else { return 0 };
    let Ok(entries) = std::fs::read_dir(base.join("queue")) else { return 0 };
    entries
        .flatten()
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Job>(&b).ok())
        .filter(|job| job.attempts > 0)
        .count()
}

pub fn wake() {
    WAKE.notify_one();
}


/// Fork: second pass. The live transcript is cut into short chunks and Parakeet sometimes drifts
/// into other languages on them. After the meeting:
/// 1. the whole recording is re-transcribed (speech-detected segments) with the best local model
///    (Parakeet "ultra" when installed, otherwise the configured one);
/// 2. optionally Claude Haiku cleans it: drops the noise the model hallucinates on silence and
///    fixes obvious mistakes, without inventing anything.
/// The live transcript is saved to `transcripts.live.json` in the meeting folder first; on any
/// failure the current transcript is kept. Returns false only when the pass was interrupted by a
/// new recording and must be redone later.
async fn second_pass<R: Runtime>(app: &AppHandle<R>, config: &AgentConfig, meeting_id: &str) -> bool {
    let mode = crate::meeting_detector::second_pass_mode(app);
    if mode == "off" {
        return true;
    }
    let Some(pool) = pool(app) else { return true };
    if let Some(since) = crate::meeting_detector::second_pass_since(app) {
        let created: Option<String> = sqlx::query_scalar("SELECT created_at FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
        let created = created.and_then(|c| DateTime::parse_from_rfc3339(&c).ok()).map(|d| d.with_timezone(&Utc));
        if created.is_some_and(|c| c < since) {
            return true; // recorded before the re-transcription was turned on
        }
    }
    let folder: Option<String> = sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .flatten();
    let Some(folder) = folder.filter(|f| !f.trim().is_empty()) else {
        log::info!("notion-agent: second pass skipped for {} (no recording folder)", meeting_id);
        return true;
    };
    backup_live_transcript(&pool, meeting_id, Path::new(&folder)).await;

    let model = second_pass_model(app).await;
    let alt_dir = crate::second_opinion::model_dir(app, &crate::meeting_detector::second_opinion(app));
    let started = std::time::Instant::now();
    log::info!("notion-agent: second pass ({:?}, {}) for {}", model, mode, meeting_id);
    match crate::audio::retranscription::start_retranscription_with_alt(app.clone(), meeting_id.to_string(), folder.clone(), None, model, Some("parakeet".to_string()), alt_dir).await {
        Ok(result) => log::info!(
            "notion-agent: second pass done for {}: {} segments in {:.0}s",
            meeting_id,
            result.segments_count,
            started.elapsed().as_secs_f64()
        ),
        Err(error) => {
            if error.to_string().contains("cancelled") || crate::audio::recording_commands::is_recording().await {
                log::info!("notion-agent: second pass for {} interrupted by a new recording, will retry", meeting_id);
                return false;
            }
            log::warn!("notion-agent: second pass failed for {} (keeping live transcript): {}", meeting_id, error);
            return true;
        }
    }
    if mode == "full" {
        let started = std::time::Instant::now();
        match clean_transcript(app, &pool, config, meeting_id, Path::new(&folder)).await {
            Ok((kept, dropped)) => log::info!(
                "notion-agent: transcript cleaned for {}: {} segments kept, {} noise dropped in {:.0}s",
                meeting_id,
                kept,
                dropped,
                started.elapsed().as_secs_f64()
            ),
            Err(error) => log::warn!("notion-agent: transcript cleaning skipped for {}: {}", meeting_id, error),
        }
    }
    true
}

/// The model chosen in the settings; on "automatic", an installed custom Parakeet model with
/// "ultra" in its id (fine-tuned v3, clearly better in benchmarks at the same speed), otherwise
/// None = the live model.
async fn second_pass_model<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let chosen = crate::meeting_detector::second_pass_model(app);
    if !chosen.is_empty() {
        return Some(chosen);
    }
    let models = crate::custom_models::engine_hooks::parakeet_models().await;
    models
        .into_iter()
        .filter(|m| matches!(m.status, crate::parakeet_engine::parakeet_engine::ModelStatus::Available))
        .find(|m| m.name.to_lowercase().contains("ultra"))
        .map(|m| m.name)
}

async fn backup_live_transcript(pool: &sqlx::SqlitePool, meeting_id: &str, folder: &Path) {
    let target = folder.join("transcripts.live.json");
    if target.exists() {
        return; // keep the first (real live) copy when the pass is retried
    }
    let rows = sqlx::query("SELECT transcript, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let segments: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "text": r.try_get::<String, _>("transcript").unwrap_or_default(),
                "start": r.try_get::<Option<f64>, _>("audio_start_time").ok().flatten(),
                "end": r.try_get::<Option<f64>, _>("audio_end_time").ok().flatten(),
            })
        })
        .collect();
    if let Err(error) = write_json(&target, &serde_json::json!({ "meeting_id": meeting_id, "segments": segments })) {
        log::warn!("notion-agent: cannot save live transcript backup: {}", error);
    }
}

const CLEAN_SYSTEM: &str = "You clean automatic speech transcripts. Answer only with the requested lines.";
const CLEAN_PROMPT: &str = "# Transcript cleanup (edit freely; the segments are appended below)

This is the automatic transcript of a work meeting, split into numbered segments.
It may contain English technical terms on purpose (deploy, backlog, pull request, feedback...).
Speech models sometimes mishear words, switch to the wrong language by mistake, or invent
phrases on noise and silence (fragments in unrelated languages or scripts, nonsense words).

When a segment has two versions, A is the main model (multilingual, good with technical terms)
and B a second model specialised in the meeting language (good with ordinary words, weak with
foreign terms). Combine them: take the words of B where A is garbled, keep the technical terms
and names of A. If B is nonsense, ignore it.

For EACH segment write the most likely version of what was actually said:
- keep the language the meeting is held in; fix only obvious errors (garbled words, wrong language);
- keep technical terms, names and numbers; do not invent, summarize or add content;
- if the segment is noise or meaningless write exactly: [noise]
Output: one line per segment, same order, format \"n. text\". Nothing else.
";
const CLEAN_PROMPT_FILE: &str = "transcript-cleanup.md";

fn load_clean_prompt(base: &Path) -> String {
    let path = base.join(CLEAN_PROMPT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => {
            let _ = std::fs::create_dir_all(base);
            let _ = std::fs::write(&path, CLEAN_PROMPT);
            CLEAN_PROMPT.to_string()
        }
    }
}

/// Second-opinion texts from `transcripts.alt.json`, keyed by segment start (milliseconds).
fn load_alt_texts(folder: &Path) -> std::collections::HashMap<i64, String> {
    let Ok(bytes) = std::fs::read(folder.join("transcripts.alt.json")) else { return Default::default() };
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let start = r.get("start")?.as_f64()?;
            let b = r.get("b")?.as_str()?.trim().to_string();
            (!b.is_empty()).then(|| ((start * 1000.0).round() as i64, b))
        })
        .collect()
}
const CLEAN_CHUNK: usize = 60;

/// Claude pass over the re-transcribed segments (merging the second opinion when present).
/// Returns (kept, dropped).
async fn clean_transcript<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, config: &AgentConfig, meeting_id: &str, folder: &Path) -> Result<(usize, usize), String> {
    let base = base_dir(app).ok_or("no app data dir")?;
    let prompt = load_clean_prompt(&base);
    let alt = load_alt_texts(folder);
    let rows = sqlx::query("SELECT id, transcript, audio_start_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    let segments: Vec<(String, String, Option<String>)> = rows
        .iter()
        .map(|r| {
            let start = r.try_get::<Option<f64>, _>("audio_start_time").ok().flatten();
            let b = start.and_then(|s| alt.get(&((s * 1000.0).round() as i64)).cloned());
            (r.try_get::<String, _>("id").unwrap_or_default(), r.try_get::<String, _>("transcript").unwrap_or_default(), b)
        })
        .collect();
    if segments.is_empty() {
        return Ok((0, 0));
    }
    let installation = crate::claude_code::agent_installation().await?;
    let mut updates: Vec<(String, Option<String>)> = Vec::new(); // None = drop
    for chunk in segments.chunks(CLEAN_CHUNK) {
        let mut stdin = format!("{}\n\nSEGMENTS\n", prompt);
        for (n, (_, text, b)) in chunk.iter().enumerate() {
            match b {
                Some(b) if b != text => stdin.push_str(&format!("{}. A: {} | B: {}\n", n + 1, text.replace('\n', " "), b.replace('\n', " "))),
                _ => stdin.push_str(&format!("{}. {}\n", n + 1, text.replace('\n', " "))),
            }
        }
        let model = if config.clean_model.trim().is_empty() { "haiku" } else { config.clean_model.trim() };
        let answer = match run_claude_text(&installation, config, model, CLEAN_SYSTEM, stdin).await {
            Ok(answer) => answer,
            Err(error) => {
                log::warn!("notion-agent: cleaning chunk failed, keeping it as is: {}", error);
                continue;
            }
        };
        match parse_clean_answer(&answer, chunk.len()) {
            Some(lines) => {
                for ((id, original, _), line) in chunk.iter().zip(lines) {
                    match line {
                        None => updates.push((id.clone(), None)),
                        Some(text) if text != *original => updates.push((id.clone(), Some(text))),
                        Some(_) => {}
                    }
                }
            }
            None => log::warn!("notion-agent: cleaning answer malformed for a chunk, keeping it as is"),
        }
    }
    let dropped = updates.iter().filter(|(_, t)| t.is_none()).count();
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (id, text) in &updates {
        let query = match text {
            Some(text) => sqlx::query("UPDATE transcripts SET transcript = ? WHERE id = ?").bind(text).bind(id),
            None => sqlx::query("DELETE FROM transcripts WHERE id = ?").bind(id),
        };
        query.execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok((segments.len() - dropped, dropped))
}


/// "n. testo" lines -> one entry per segment (None = noise). None when anything is missing.
fn parse_clean_answer(answer: &str, expected: usize) -> Option<Vec<Option<String>>> {
    let mut out: Vec<Option<Option<String>>> = vec![None; expected];
    for line in answer.lines() {
        let line = line.trim();
        let Some((num, text)) = line.split_once(". ").or_else(|| line.split_once('.')) else { continue };
        let Ok(n) = num.trim().parse::<usize>() else { continue };
        if n == 0 || n > expected {
            continue;
        }
        let text = text.trim();
        out[n - 1] = Some(if text.is_empty() || text.eq_ignore_ascii_case("[noise]") { None } else { Some(text.to_string()) });
    }
    out.into_iter().collect()
}
