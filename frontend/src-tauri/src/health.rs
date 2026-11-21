//! Fork: health check. Verifies, without spending Claude usage, that everything the automatic
//! meeting -> Notion flow needs is working, repairs what it can, and warns before a meeting
//! instead of after it.

use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Runtime};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HealthReport {
    checked_at: Option<String>,
    checks: Vec<Check>,
}

static REPORT: std::sync::Mutex<Option<HealthReport>> = std::sync::Mutex::new(None);
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn check(name: &str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check { name: name.into(), ok: true, detail },
        Err(detail) => Check { name: name.into(), ok: false, detail },
    }
}

fn microphone() -> Result<String, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_input_device().ok_or("nessun microfono predefinito in Windows")?;
    Ok(device.name().unwrap_or_else(|_| "microfono predefinito".into()))
}

fn ffmpeg() -> Result<String, String> {
    let dir = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = dir.parent().map(|d| d.join("ffmpeg.exe")).ok_or("cartella app sconosciuta")?;
    if path.is_file() { Ok("presente".into()) } else { Err(format!("manca {}", path.display())) }
}

async fn proxy_reachable(proxy: &str) -> Result<String, String> {
    let host = proxy.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/');
    if host.is_empty() {
        return Ok("nessun proxy configurato".into());
    }
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(host)).await {
        Ok(Ok(_)) => Ok(format!("{} raggiungibile", host)),
        Ok(Err(e)) => Err(format!("{} non risponde ({})", host, e)),
        Err(_) => Err(format!("{} non risponde (timeout)", host)),
    }
}

/// `claude mcp list` checks the connectors' login without using any model tokens.
async fn claude_and_notion(env: &[(String, String)]) -> (Result<String, String>, Result<String, String>) {
    let installation = match crate::claude_code::agent_installation().await {
        Ok(installation) => installation,
        Err(e) => return (Err(format!("Claude Code non trovato: {}", e)), Err("non verificabile senza Claude".into())),
    };
    let mut command = match &installation.runtime {
        crate::claude_code::Runtime::Native => {
            let mut command = Command::new(&installation.executable);
            command.env_remove("ANTHROPIC_API_KEY");
            for (key, value) in env {
                command.env(key, value);
            }
            command
        }
        crate::claude_code::Runtime::Wsl { distro } => {
            let mut command = Command::new("wsl.exe");
            command.args(["-d", distro, "--cd", "/tmp", "--exec", "env", "-u", "ANTHROPIC_API_KEY"]);
            for (key, value) in env {
                command.arg(format!("{}={}", key, value));
            }
            command.arg(&installation.executable);
            command
        }
    };
    command.args(["mcp", "list"]).kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = match tokio::time::timeout(Duration::from_secs(120), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return (Err(format!("Claude non parte: {}", e)), Err("non verificabile".into())),
        Err(_) => return (Err("Claude non risponde (timeout)".into()), Err("non verificabile".into())),
    };
    let text = String::from_utf8_lossy(&output.stdout).to_string() + &String::from_utf8_lossy(&output.stderr);
    let lower = text.to_lowercase();
    if lower.contains("not logged in") || lower.contains("please run /login") || lower.contains("invalid api key") {
        return (Err("Claude non e' collegato all'account: apri un terminale e fai login".into()), Err("non verificabile".into()));
    }
    let claude = Ok(installation.place());
    let notion = match text.lines().find(|l| l.contains("claude.ai Notion")) {
        Some(line) if line.contains("Connected") => Ok("collegato".into()),
        Some(_) => Err("collegamento Notion scaduto: riconnetti Notion in claude.ai > Impostazioni > Connettori".into()),
        None if output.status.success() => Err("connettore Notion non trovato in claude.ai".into()),
        None => Err(format!("controllo fallito: {}", text.lines().last().unwrap_or("").trim())),
    };
    (claude, notion)
}

/// Runs a Windows scheduled task named in `.env` (REPAIR_TASK), e.g. a proxy bridge.
async fn repair(task: &str) {
    if task.is_empty() {
        return;
    }
    log::info!("health: starting repair task {}", task);
    let mut command = Command::new("schtasks");
    command.args(["/Run", "/TN", task]);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    let _ = command.output().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
}

pub async fn run<R: Runtime>(app: &AppHandle<R>) -> HealthReport {
    let _guard = RUNNING.lock().await;
    let (env, repair_task) = crate::notion_agent::health_inputs(app);
    let proxy = env.iter().find(|(k, _)| k == "HTTPS_PROXY").map(|(_, v)| v.clone()).unwrap_or_default();

    let mut network = proxy_reachable(&proxy).await;
    if network.is_err() && !repair_task.is_empty() {
        repair(&repair_task).await;
        network = proxy_reachable(&proxy).await.map(|ok| format!("{} (riparato)", ok));
    }
    let (claude, notion) = claude_and_notion(&env).await;
    let mut checks = vec![
        check("Microfono", microphone()),
        check("Conversione audio (ffmpeg)", ffmpeg()),
        check("Rete / proxy", network),
        check("Claude", claude),
        check("Notion", notion),
    ];
    let stuck = crate::notion_agent::failing_jobs(app);
    checks.push(check(
        "Resoconti in errore",
        if stuck == 0 { Ok("nessuno".into()) } else { Err(format!("{} meeting in errore: vedi la lista sotto", stuck)) },
    ));

    let report = HealthReport { checked_at: Some(chrono::Utc::now().to_rfc3339()), checks };
    let previous = REPORT.lock().ok().and_then(|mut r| r.replace(report.clone()));
    let failed: Vec<&Check> = report.checks.iter().filter(|c| !c.ok).collect();
    let was_failed = |name: &str| previous.as_ref().is_some_and(|p| p.checks.iter().any(|c| c.name == name && !c.ok));
    let new_failures: Vec<&&Check> = failed.iter().filter(|c| !was_failed(&c.name)).collect();
    if !new_failures.is_empty() {
        use tauri_plugin_notification::NotificationExt;
        let body = new_failures.iter().map(|c| format!("{}: {}", c.name, c.detail)).collect::<Vec<_>>().join("\n");
        let _ = app.notification().builder().title("Meetily: qualcosa non funziona").body(body).show();
    }
    if failed.is_empty() && previous.as_ref().is_some_and(|p| p.checks.iter().any(|c| !c.ok)) {
        crate::notion_agent::wake(); // fixed: retry queued recaps now
    }
    log::info!(
        "health: {}",
        report.checks.iter().map(|c| format!("{}={}", c.name, if c.ok { "ok" } else { "KO" })).collect::<Vec<_>>().join(", ")
    );
    report
}

pub fn start<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            if !crate::audio::recording_commands::is_recording().await {
                run(&app).await;
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[tauri::command]
pub async fn health_status() -> HealthReport {
    REPORT.lock().ok().and_then(|r| r.clone()).unwrap_or_default()
}

#[tauri::command]
pub async fn health_run<R: Runtime>(app: AppHandle<R>) -> HealthReport {
    run(&app).await
}
