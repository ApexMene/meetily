//! Fork: health check of the meeting -> Notion flow. Runs hourly only while the Notion agent is
//! on, uses no Claude tokens (`claude mcp list`), repairs what it can and warns before a meeting
//! instead of after it. Shown in the "After the meeting" window.

use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Runtime};

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    checked_at: Option<String>,
    checks: Vec<Check>,
}

static REPORT: std::sync::Mutex<Option<HealthReport>> = std::sync::Mutex::new(None);
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn check(name: &str, result: Result<String, String>) -> Check {
    let (ok, detail) = match result {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    Check { name: name.into(), ok, detail }
}

/// Device enumeration can block for a while (audio drivers): off the async runtime.
async fn microphone() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let device = cpal::default_host().default_input_device().ok_or("no default microphone")?;
        Ok(device.name().unwrap_or_else(|_| "default microphone".into()))
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Host and port of a proxy URL such as `http://127.0.0.1:8080/`.
fn proxy_address(proxy: &str) -> &str {
    let proxy = proxy.trim();
    let proxy = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
    proxy.split('/').next().unwrap_or_default()
}

async fn proxy_reachable(proxy: &str) -> Result<String, String> {
    let address = proxy_address(proxy);
    if address.is_empty() {
        return Ok("no proxy configured".into());
    }
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(address)).await {
        Ok(Ok(_)) => Ok(format!("{} reachable", address)),
        Ok(Err(error)) => Err(format!("{} does not answer ({})", address, error)),
        Err(_) => Err(format!("{} does not answer (timeout)", address)),
    }
}

/// The state of the claude.ai Notion connector in a `claude mcp list` listing.
fn notion_state(listing: &str) -> Result<String, String> {
    match listing.lines().find(|line| line.contains("claude.ai Notion")) {
        Some(line) if line.contains("Connected") => Ok("connected".into()),
        Some(_) => Err("Notion connection expired: reconnect Notion in claude.ai › Settings › Connectors".into()),
        None => Err("Notion connector not found in claude.ai".into()),
    }
}

async fn claude_and_notion(env: &[(String, String)]) -> (Result<String, String>, Result<String, String>) {
    match crate::claude_code::connectors(env).await {
        Ok((place, listing)) => (Ok(place), notion_state(&listing)),
        Err(error) => (Err(error), Err("cannot be checked without Claude Code".into())),
    }
}

/// Starts the Windows scheduled task named in `.env` (`REPAIR_TASK`), e.g. a proxy bridge.
#[cfg(windows)]
async fn repair(task: &str) {
    log::info!("health: starting the repair task");
    let _ = tokio::process::Command::new("schtasks")
        .args(["/Run", "/TN", task])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .await;
    tokio::time::sleep(Duration::from_secs(10)).await;
}

#[cfg(not(windows))]
async fn repair(_task: &str) {}

pub async fn run<R: Runtime>(app: &AppHandle<R>) -> HealthReport {
    let _guard = RUNNING.lock().await;
    let (env, repair_task) = crate::notion_agent::health_inputs(app);
    let proxy = env.iter().find(|(key, _)| key == "HTTPS_PROXY").map(|(_, value)| value.as_str()).unwrap_or_default();

    let mut network = proxy_reachable(proxy).await;
    if network.is_err() && !repair_task.is_empty() {
        repair(&repair_task).await;
        network = proxy_reachable(proxy).await.map(|ok| format!("{} (repaired)", ok));
    }
    let (claude, notion) = claude_and_notion(&env).await;
    let failing = crate::post_meeting::failing_jobs(app);
    let checks = vec![
        check("Microphone", microphone().await),
        check("Network / proxy", network),
        check("Claude Code", claude),
        check("Notion", notion),
        check(
            "Failing meetings",
            if failing == 0 { Ok("none".into()) } else { Err(format!("{} meetings are being retried: see the list below", failing)) },
        ),
    ];
    let report = HealthReport { checked_at: Some(chrono::Utc::now().to_rfc3339()), checks };
    let previous = REPORT.lock().ok().and_then(|mut stored| stored.replace(report.clone()));
    notify_changes(app, previous.as_ref(), &report);
    log::info!(
        "health: {}",
        report.checks.iter().map(|c| format!("{}={}", c.name, if c.ok { "ok" } else { "failed" })).collect::<Vec<_>>().join(", ")
    );
    report
}

/// Warns about newly failed checks; when everything works again, retries the queue now.
fn notify_changes<R: Runtime>(app: &AppHandle<R>, previous: Option<&HealthReport>, report: &HealthReport) {
    let failed_before = |name: &str| previous.is_some_and(|p| p.checks.iter().any(|c| c.name == name && !c.ok));
    let new_failures: Vec<String> = report
        .checks
        .iter()
        .filter(|c| !c.ok && !failed_before(&c.name))
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    if !new_failures.is_empty() {
        use tauri_plugin_notification::NotificationExt;
        let _ = app.notification().builder().title("Meetily: something is not working").body(new_failures.join("\n")).show();
    }
    let all_ok = report.checks.iter().all(|c| c.ok);
    if all_ok && previous.is_some_and(|p| p.checks.iter().any(|c| !c.ok)) {
        crate::post_meeting::wake();
    }
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            if crate::notion_agent::is_enabled(&app) && !crate::audio::recording_commands::is_recording().await {
                run(&app).await;
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[tauri::command]
pub async fn health_status() -> HealthReport {
    REPORT.lock().ok().and_then(|report| report.clone()).unwrap_or_default()
}

#[tauri::command]
pub async fn health_run<R: Runtime>(app: AppHandle<R>) -> HealthReport {
    run(&app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proxy_address() {
        assert_eq!(proxy_address("http://127.0.0.1:8080/"), "127.0.0.1:8080");
        assert_eq!(proxy_address("proxy:3128"), "proxy:3128");
        assert_eq!(proxy_address(""), "");
    }

    #[test]
    fn reads_notion_state() {
        assert!(notion_state("claude.ai Notion: https://mcp.notion.com - ✔ Connected").is_ok());
        assert!(notion_state("claude.ai Notion: https://mcp.notion.com - ! Needs authentication").is_err());
        assert!(notion_state("github: ok").is_err());
    }
}
