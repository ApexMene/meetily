//! Fork: writes the meeting recap and the tasks in Notion (off by default; turned on in
//! Settings › Transcription › After the meeting). It is the last step of `crate::post_meeting`.
//!
//! Meetily never talks to Notion directly: it runs Claude Code in print mode with only the
//! Notion tools of the user's claude.ai connector allowed, and Claude follows a prompt file.
//! Notion pages carry `{"meetily_id": ...}` and the prompt updates them instead of duplicating,
//! so a retried job never writes twice.
//!
//! Files in `<app data>/notion-agent/` (created when the agent is turned on):
//! - `settings.json`: enabled, model, proxy, no_proxy, test_mode, min_seconds, since. Read at
//!   start; hand edits apply after a restart. (Not `config.json`: the first version wrote it with
//!   the agent turned on.);
//! - `meeting-to-notion.md`: the prompt (seeded from the bundled example, never overwritten);
//! - `.env`: personal values. `{{KEY}}` in the prompt becomes `KEY`; `PROXY` / `NO_PROXY`
//!   override settings.json; `PROMPT_FILE` points to a prompt anywhere on disk; `TIMEZONE` names
//!   the user's time zone; `REPAIR_TASK` is used by the health check.

use crate::claude_code::{Request, Tools};
use crate::fork_store::Store;
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;

const DIR: &str = "notion-agent";
const PROMPT_FILE: &str = "meeting-to-notion.md";
const DEFAULT_PROMPT: &str = include_str!("prompt.md");
/// One agent run: reading the context, then writing a recap and a handful of tasks.
const AGENT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const ALLOWED_TOOLS: &str = "mcp__claude_ai_Notion__notion-search,\
mcp__claude_ai_Notion__notion-fetch,\
mcp__claude_ai_Notion__notion-query-data-sources,\
mcp__claude_ai_Notion__notion-create-pages,\
mcp__claude_ai_Notion__notion-update-page";
const SYSTEM_PROMPT: &str =
    "You are an unattended agent. Follow the instructions block exactly. The last line of your answer must be the requested JSON.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    /// Claude model that writes the recap.
    pub model: String,
    /// HTTP proxy for Claude Code (e.g. a local bridge). Empty = inherit the environment.
    pub proxy: String,
    pub no_proxy: String,
    /// Pages get a `[TEST]` prefix and existing calendar cards are not touched.
    pub test_mode: bool,
    /// Meetings with less transcribed audio than this are skipped without calling Claude.
    pub min_seconds: f64,
    /// When the agent was turned on: older meetings are never sent.
    pub since: Option<DateTime<Utc>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "sonnet".into(),
            proxy: String::new(),
            no_proxy: "localhost,127.0.0.1".into(),
            test_mode: false,
            min_seconds: 120.0,
            since: None,
        }
    }
}

static CONFIG: Store<Config> = Store::new("notion-agent/settings.json");

pub fn settings<R: Runtime>(app: &AppHandle<R>) -> Config {
    CONFIG.get(app)
}

pub fn is_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    settings(app).enabled
}

/// Turns the agent on or off and sets the recap model. Turning it on prepares the folder (prompt
/// and `.env` template) and sends only meetings recorded from now on.
pub fn configure<R: Runtime>(app: &AppHandle<R>, enabled: bool, model: &str) -> Result<(), String> {
    let mut config = settings(app);
    if enabled && !config.enabled {
        prepare_folder(app)?;
        config.since = Some(Utc::now());
    }
    config.enabled = enabled;
    config.model = model.to_string();
    CONFIG.set(app, config)
}

fn base_dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join(DIR))
}

/// Creates the folder with the example prompt and the `.env` template (existing files are kept).
pub fn prepare_folder<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let base = base_dir(app).ok_or("app data directory unavailable")?;
    std::fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    for (name, text) in [(PROMPT_FILE, DEFAULT_PROMPT), (".env", ENV_TEMPLATE)] {
        let path = base.join(name);
        if !path.exists() {
            std::fs::write(&path, text).map_err(|error| error.to_string())?;
        }
    }
    Ok(base)
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
# Your time zone, e.g. Europe/Rome (default: the computer's UTC offset)
TIMEZONE=
# Value written in the Sync_Source property of the pages the agent creates
SYNC_SOURCE=meetily
# Optional: full path of your own prompt file (default: meeting-to-notion.md next to this file)
PROMPT_FILE=
# Example task title and name fixes, to make the agent match your projects
TASK_EXAMPLE=Send the effort estimate to Mario
NAME_FIXES=\"copilot studio\" -> Copilot Studio
# Optional Windows scheduled task the health check starts when the proxy does not answer
REPAIR_TASK=
# Optional HTTP proxy for Claude Code (e.g. http://127.0.0.1:8080) and its exclusions
PROXY=
NO_PROXY=localhost,127.0.0.1
";

/// `<base>/.env` (KEY=value lines, `#` comments, optional quotes); empty when missing.
fn load_env(base: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(base.join(".env")).map(|text| parse_env(&text)).unwrap_or_default()
}

fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            (key.trim().to_string(), value.replace("\\\"", "\""))
        })
        .collect()
}

fn env_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// Replaces `{{KEY}}` with values from `.env`; warns about placeholders left without a value.
fn fill_placeholders(text: &str, env: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in env {
        out = out.replace(&format!("{{{{{}}}}}", key), value);
    }
    if let Some(start) = out.find("{{") {
        let missing: String = out[start..].chars().take(40).collect();
        log::warn!("notion-agent: prompt placeholder without a value in .env: {}", missing);
    }
    out
}

fn load_prompt(base: &Path, env: &HashMap<String, String>) -> String {
    if let Some(custom) = env_value(env, "PROMPT_FILE") {
        match std::fs::read_to_string(&custom) {
            Ok(text) if !text.trim().is_empty() => return text,
            _ => log::warn!("notion-agent: PROMPT_FILE is not readable, using {}", PROMPT_FILE),
        }
    }
    match std::fs::read_to_string(base.join(PROMPT_FILE)) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => DEFAULT_PROMPT.to_string(),
    }
}

/// A proxy value passed to Claude Code as an environment variable: one word, no quotes.
fn is_plain_value(value: &str) -> bool {
    value.len() <= 500 && value.chars().all(|c| c.is_ascii_graphic() && !"\"'`$\\;|&<>".contains(c))
}

/// Proxy environment for Claude Code (settings.json, overridden by `.env`). Used by every
/// post-meeting Claude run, also when the Notion agent is off.
pub fn claude_env<R: Runtime>(app: &AppHandle<R>) -> Vec<(String, String)> {
    let config = settings(app);
    let env = base_dir(app).map(|base| load_env(&base)).unwrap_or_default();
    let proxy = env_value(&env, "PROXY").unwrap_or(config.proxy.trim().to_string());
    let no_proxy = env_value(&env, "NO_PROXY").unwrap_or(config.no_proxy.trim().to_string());
    proxy_env(&proxy, &no_proxy)
}

fn proxy_env(proxy: &str, no_proxy: &str) -> Vec<(String, String)> {
    if proxy.is_empty() {
        return Vec::new();
    }
    if !is_plain_value(proxy) || !is_plain_value(no_proxy) {
        log::warn!("notion-agent: proxy settings ignored (they must be single words without quotes)");
        return Vec::new();
    }
    // Both spellings: WSL's autoProxy injects lowercase variables that can point to a proxy
    // unreachable from inside WSL, and curl-style clients prefer the lowercase ones.
    ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"]
        .iter()
        .map(|key| (key.to_string(), proxy.to_string()))
        .chain(["no_proxy", "NO_PROXY"].iter().map(|key| (key.to_string(), no_proxy.to_string())))
        .collect()
}

/// For the health check: the proxy Claude runs with and the optional repair task.
pub fn health_inputs<R: Runtime>(app: &AppHandle<R>) -> (Vec<(String, String)>, String) {
    let env = base_dir(app).map(|base| load_env(&base)).unwrap_or_default();
    (claude_env(app), env_value(&env, "REPAIR_TASK").unwrap_or_default())
}

/// The agent's final JSON line.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Outcome {
    pub status: String,
    pub summary_url: Option<String>,
    pub tasks_created: Option<i64>,
    pub tasks_existing: Option<i64>,
    pub unassigned: Option<i64>,
    pub note: Option<String>,
}

impl Outcome {
    pub fn skipped(note: impl Into<String>) -> Self {
        Self { status: "skipped".into(), note: Some(note.into()), ..Default::default() }
    }
}

/// Writes the recap of a meeting in Notion. An error means "retry later".
pub async fn write_recap<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, meeting_id: &str) -> Result<Outcome, String> {
    let config = settings(app);
    let base = base_dir(app).ok_or("app data directory unavailable")?;
    let env = load_env(&base);
    let input = load_meeting(pool, meeting_id, config.test_mode, env_value(&env, "TIMEZONE")).await?;
    if config.since.is_some_and(|since| input.saved < since) {
        return Ok(Outcome::skipped("recorded before the Notion agent was turned on"));
    }
    if input.seconds < config.min_seconds || input.transcript.trim().is_empty() {
        return Ok(Outcome::skipped(format!("only {:.0}s of transcribed audio", input.seconds)));
    }
    let instructions = fill_placeholders(&load_prompt(&base, &env), &env);
    let content = format!("META\n{}\n\nTRANSCRIPT\n{}", input.meta, input.transcript);
    // Fork: uncertain values (marked "(?)" by the transcript check) must stay marked
    let instructions = format!("{}\n\n{}", instructions, crate::glossary::critical::PROMPT_NOTE);
    let claude_env = claude_env(app);
    let request = Request {
        model: config.model.trim(),
        system_prompt: SYSTEM_PROMPT,
        instructions: &instructions,
        content: &content,
        tools: Tools::Connectors(ALLOWED_TOOLS),
        env: &claude_env,
        timeout: AGENT_TIMEOUT,
    };
    let answer = crate::claude_code::run(&request, None).await?;
    let outcome = parse_outcome(&answer)?;
    notify_outcome(app, &outcome);
    Ok(outcome)
}

fn parse_outcome(answer: &str) -> Result<Outcome, String> {
    let outcome = answer
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .find_map(|line| serde_json::from_str::<Outcome>(line).ok())
        .ok_or_else(|| format!("no outcome JSON in the answer: {}", truncate(answer, 200)))?;
    match outcome.status.as_str() {
        "ok" | "skipped" => Ok(outcome),
        _ => Err(format!("agent error: {}", outcome.note.unwrap_or_default())),
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn notify_outcome<R: Runtime>(app: &AppHandle<R>, outcome: &Outcome) {
    if outcome.status == "skipped" {
        log::info!("notion-agent: skipped ({})", outcome.note.as_deref().unwrap_or_default());
        return;
    }
    let mut body = format!(
        "{} new tasks, {} to assign to a project.",
        outcome.tasks_created.unwrap_or(0),
        outcome.unassigned.unwrap_or(0)
    );
    if let Some(url) = &outcome.summary_url {
        body.push('\n');
        body.push_str(url);
    }
    if let Err(error) = app.notification().builder().title("Recap written in Notion").body(body).show() {
        log::warn!("notion-agent: notification failed: {}", error);
    }
}

struct MeetingInput {
    meta: serde_json::Value,
    transcript: String,
    /// When the meeting was saved (end of the recording).
    saved: DateTime<Utc>,
    seconds: f64,
}

/// Nearest quarter hour (ties go up).
fn round_quarter(time: DateTime<Utc>) -> DateTime<Utc> {
    const QUARTER: i64 = 15 * 60;
    let rounded = (time.timestamp() + QUARTER / 2).div_euclid(QUARTER) * QUARTER;
    DateTime::from_timestamp(rounded, 0).unwrap_or(time)
}

/// Calendar slot for a meeting: both ends rounded to the quarter hour, at least 15 minutes long.
fn calendar_slot(start: DateTime<Utc>, end: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let slot_start = round_quarter(start);
    (slot_start, round_quarter(end).max(slot_start + ChronoDuration::minutes(15)))
}

fn utc(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

async fn load_meeting(pool: &sqlx::SqlitePool, meeting_id: &str, test_mode: bool, timezone: Option<String>) -> Result<MeetingInput, String> {
    let meeting = sqlx::query("SELECT title, created_at, folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("meeting not found")?;
    let title: String = meeting.try_get("title").unwrap_or_default();
    let created: String = meeting.try_get("created_at").unwrap_or_default();
    let saved = DateTime::parse_from_rfc3339(&created).map_err(|error| error.to_string())?.with_timezone(&Utc);
    let folder: Option<String> = meeting.try_get("folder_path").ok().flatten();
    let runs = folder.map(|folder| crate::speakers::load(Path::new(&folder))).unwrap_or_default();

    let rows = sqlx::query(
        "SELECT transcript, timestamp, audio_start_time, audio_end_time FROM transcripts \
         WHERE meeting_id = ? ORDER BY COALESCE(audio_start_time, 0), timestamp",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;

    let seconds = rows
        .iter()
        .filter_map(|row| row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten())
        .fold(0.0_f64, f64::max);
    // The meeting row is saved when the recording stops; audio times are offsets from the start.
    let start = saved - ChronoDuration::milliseconds((seconds * 1000.0) as i64);

    let mut transcript = String::new();
    for row in &rows {
        let text: String = row.try_get("transcript").unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let offset = row.try_get::<Option<f64>, _>("audio_start_time").ok().flatten();
        let end = row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten();
        let tag = offset
            .zip(end)
            .and_then(|(offset, end)| crate::speakers::dominant(&runs, offset, end))
            .map(|source| format!("{} ", source.tag()))
            .unwrap_or_default();
        let label = match offset {
            Some(offset) => (start + ChronoDuration::milliseconds((offset * 1000.0) as i64))
                .with_timezone(&Local)
                .format("%H:%M:%S")
                .to_string(),
            None => row.try_get::<String, _>("timestamp").unwrap_or_default(),
        };
        transcript.push_str(&format!("[{}] {}{}\n", label, tag, text.trim()));
    }

    let (calendar_start, calendar_end) = calendar_slot(start, saved);
    let local_start = start.with_timezone(&Local);
    let mut meta = serde_json::json!({
        "meetily_id": meeting_id,
        "title": title,
        "start_utc": utc(start),
        "end_utc": utc(saved),
        // Real times rounded to the nearest quarter hour (15:07 -> 15:00, 15:27 -> 15:30): what
        // goes in the Notion calendar property.
        "calendar_start_utc": utc(calendar_start),
        "calendar_end_utc": utc(calendar_end),
        "calendar_local": format!(
            "{} - {}",
            calendar_start.with_timezone(&Local).format("%H:%M"),
            calendar_end.with_timezone(&Local).format("%H:%M")
        ),
        "meeting_date_local": local_start.format("%Y-%m-%d (%A)").to_string(),
        "timezone": timezone.unwrap_or_else(|| local_start.format("UTC%:z").to_string()),
        "test_mode": test_mode,
    });
    if !runs.is_empty() {
        meta["speaker_tags"] = crate::speakers::TAGS_HELP.into();
    }
    Ok(MeetingInput { meta, transcript, saved, seconds })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_outcome() {
        let answer = "Done.\n{\"status\":\"ok\",\"summary_url\":\"https://x\",\"tasks_created\":3,\"tasks_existing\":1,\"unassigned\":0,\"note\":\"\"}";
        let outcome = parse_outcome(answer).unwrap();
        assert_eq!(outcome.status, "ok");
        assert_eq!(outcome.tasks_created, Some(3));
    }

    #[test]
    fn agent_error_is_retried() {
        assert!(parse_outcome("{\"status\":\"error\",\"note\":\"notion down\"}").is_err());
        assert!(parse_outcome("no json here").is_err());
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
        let env = parse_env("# c\nUSER_NAME=Jane Doe\nNAME_FIXES=\"a\" -> B\nPROXY=\n");
        assert_eq!(env.get("USER_NAME").map(String::as_str), Some("Jane Doe"));
        assert_eq!(fill_placeholders("Hi {{USER_NAME}}", &env), "Hi Jane Doe");
        assert_eq!(env_value(&env, "PROXY"), None);
    }

    #[test]
    fn proxy_values_are_checked() {
        assert_eq!(proxy_env("http://127.0.0.1:8080", "localhost").len(), 6);
        assert!(proxy_env("", "localhost").is_empty());
        assert!(proxy_env("http://x:1 --flag", "localhost").is_empty());
        assert!(proxy_env("http://x:1", "a;b").is_empty());
    }
}
