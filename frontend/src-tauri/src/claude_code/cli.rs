//! Builds and runs Claude Code CLI commands.
//!
//! One command line serves every environment: only the launcher differs (`claude …` natively,
//! `wsl.exe -d <distro> --exec env -u ANTHROPIC_API_KEY <claude> …` in WSL).
//!
//! The prompt (instructions + transcript) always goes through stdin and the arguments are short
//! fixed words. That keeps transcripts out of process listings, avoids command-line length limits,
//! and avoids quoting problems: on Windows an npm install is a `claude.cmd` batch file, which goes
//! through cmd.exe, and arguments cross the `wsl.exe` boundary.

use super::{Installation, Runtime};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Long meetings with the largest model can take several minutes.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(600);
/// `--version` / `auth status`; generous because a stopped WSL distribution has to boot first.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Replaces Claude Code's coding-agent system prompt. The real instructions travel in stdin.
/// Plain words only: this is a command-line argument (see the module docs).
const SYSTEM_PROMPT: &str = "You write meeting summaries for the Meetily app. Follow the instructions block in the message exactly and reply with only the requested content.";

#[derive(Debug, PartialEq)]
pub enum Error {
    /// The executable (or `wsl.exe`) does not exist.
    NotFound,
    Cancelled,
    TimedOut,
    /// Claude Code reported an error (authentication, usage limit, invalid model…).
    Claude(String),
    /// The process failed without a usable answer.
    Failed(String),
}

impl Error {
    pub fn user_message(&self, installation: &Installation) -> String {
        match self {
            Error::NotFound => format!(
                "Claude Code was not found at {} in {}. Check the Claude Code settings.",
                installation.executable,
                installation.place()
            ),
            Error::Cancelled => "Summary generation was cancelled".to_string(),
            Error::TimedOut => format!(
                "Claude Code did not answer within {} minutes",
                GENERATION_TIMEOUT.as_secs() / 60
            ),
            Error::Claude(message) if is_auth_error(message) => format!(
                "Claude Code is not logged in to a Claude account in {}. Open a terminal there, run `claude` and use /login. ({})",
                installation.place(),
                message
            ),
            Error::Claude(message) => format!("Claude Code: {}", message),
            Error::Failed(message) => format!("Claude Code failed: {}", message),
        }
    }
}

fn is_auth_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["authenticate", "not logged in", "/login", "oauth", "credential"]
        .iter()
        .any(|marker| message.contains(marker))
}

/// Arguments of a print-mode completion: no tools, no MCP servers, no skills, no user/project
/// settings (their hooks would run too) and no saved session.
fn print_args(model: &str) -> Vec<&str> {
    vec![
        "--print",
        "--output-format",
        "json",
        "--model",
        model,
        "--system-prompt",
        SYSTEM_PROMPT,
        "--tools",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--setting-sources",
        "",
        "--no-session-persistence",
    ]
}

fn stdin_prompt(system_prompt: &str, user_prompt: &str) -> String {
    format!("<instructions>\n{}\n</instructions>\n\n{}", system_prompt, user_prompt)
}

/// The command that runs `claude <args>` for an installation. `ANTHROPIC_API_KEY` is removed so
/// the CLI always uses the account login, never a key that happens to be in the environment.
pub fn command<'a>(installation: &Installation, args: impl IntoIterator<Item = &'a str>) -> Command {
    let mut command = match &installation.runtime {
        Runtime::Native => {
            let mut command = Command::new(&installation.executable);
            command.env_remove("ANTHROPIC_API_KEY");
            // A neutral directory: no project CLAUDE.md or settings are picked up.
            command.current_dir(std::env::temp_dir());
            command
        }
        Runtime::Wsl { distro } => {
            let mut command = wsl_command(distro);
            command
                .args(["--cd", "/tmp", "--exec", "env", "-u", "ANTHROPIC_API_KEY"])
                .arg(&installation.executable);
            command
        }
    };
    command.args(args);
    command
}

/// `wsl.exe -d <distro>`; the caller adds `--exec …`.
pub fn wsl_command(distro: &str) -> Command {
    let mut command = Command::new("wsl.exe");
    command.args(["-d", distro]);
    command
}

/// Runs a command to completion, with optional stdin, a timeout and cancellation.
/// The child is killed if the wait is abandoned (`kill_on_drop`).
pub async fn run(
    mut command: Command,
    stdin: Option<String>,
    timeout: Duration,
    cancellation_token: Option<&CancellationToken>,
) -> Result<std::process::Output, Error> {
    command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound,
        _ => Error::Failed(error.to_string()),
    })?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Written concurrently with reading stdout so a large prompt cannot deadlock the pipes.
        tokio::spawn(async move {
            let _ = pipe.write_all(text.as_bytes()).await;
            // Dropping the pipe closes stdin, which ends the prompt.
        });
    }

    let output = child.wait_with_output();
    let cancelled = async {
        match cancellation_token {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        output = tokio::time::timeout(timeout, output) => match output {
            Ok(output) => output.map_err(|error| Error::Failed(error.to_string())),
            Err(_) => Err(Error::TimedOut),
        },
        _ = cancelled => Err(Error::Cancelled),
    }
}

/// One completion: instructions and user content in, answer text out.
pub async fn complete(
    installation: &Installation,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, Error> {
    let command = command(installation, print_args(model));
    let output = run(
        command,
        Some(stdin_prompt(system_prompt, user_prompt)),
        GENERATION_TIMEOUT,
        cancellation_token,
    )
    .await?;
    parse_print_result(&output)
}

/// The final message of `--output-format json`.
#[derive(Deserialize)]
struct PrintResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
}

fn parse_print_result(output: &std::process::Output) -> Result<String, Error> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str::<PrintResult>(stdout.trim()) {
        Ok(PrintResult { is_error: false, result: Some(text) }) if !text.trim().is_empty() => Ok(text),
        Ok(PrintResult { is_error: false, .. }) => Err(Error::Claude("empty answer".to_string())),
        Ok(PrintResult { result, .. }) => Err(Error::Claude(result.unwrap_or_else(|| "unknown error".to_string()))),
        // No JSON: the CLI failed before answering (bad option, crash, missing WSL distribution…).
        Err(_) => Err(Error::Failed(last_line(&output.stderr).or_else(|| last_line(&output.stdout)).unwrap_or_else(
            || format!("exited with {}", output.status),
        ))),
    }
}

fn last_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines().map(str::trim).filter(|line| !line.is_empty()).last().map(str::to_string)
}

/// `claude --version`, e.g. "2.1.270 (Claude Code)".
pub async fn version(installation: &Installation) -> Result<String, Error> {
    let output = run(command(installation, ["--version"]), None, QUERY_TIMEOUT, None).await?;
    match last_line(&output.stdout) {
        Some(version) if output.status.success() => Ok(version),
        _ => Err(Error::Failed(last_line(&output.stderr).unwrap_or_else(|| "no version output".to_string()))),
    }
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub logged_in: bool,
    #[serde(default)]
    pub auth_method: Option<String>,
}

/// `claude auth status` (JSON). It exits non-zero when logged out, so only stdout is trusted.
pub async fn auth_status(installation: &Installation) -> Result<AuthStatus, Error> {
    let output = run(command(installation, ["auth", "status"]), None, QUERY_TIMEOUT, None).await?;
    serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::Failed(last_line(&output.stderr).unwrap_or_else(|| "unreadable auth status".to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int;

    fn installation(runtime: Runtime) -> Installation {
        Installation { runtime, executable: "/home/u/.local/bin/claude".into() }
    }

    fn argv(command: &Command) -> Vec<String> {
        let std = command.as_std();
        std::iter::once(std.get_program())
            .chain(std.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    fn exit(code: c_int) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    fn exit(code: c_int) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn output(code: c_int, stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output { status: exit(code), stdout: stdout.into(), stderr: stderr.into() }
    }

    #[test]
    fn same_cli_call_for_every_environment() {
        let native = argv(&command(&installation(Runtime::Native), print_args("sonnet")));
        let wsl = argv(&command(&installation(Runtime::Wsl { distro: "Ubuntu".into() }), print_args("sonnet")));

        assert_eq!(native[0], "/home/u/.local/bin/claude");
        assert_eq!(
            wsl[..9],
            ["wsl.exe", "-d", "Ubuntu", "--cd", "/tmp", "--exec", "env", "-u", "ANTHROPIC_API_KEY"]
        );
        assert_eq!(wsl[9], "/home/u/.local/bin/claude");
        assert_eq!(native[1..], wsl[10..], "the claude arguments must not depend on the environment");
    }

    #[test]
    fn arguments_are_plain_words() {
        // Arguments may pass through cmd.exe (npm's claude.cmd) or wsl.exe: keep them simple.
        for arg in print_args("sonnet") {
            assert!(
                arg.chars().all(|c| c.is_ascii_alphanumeric() || " -.,".contains(c)),
                "unsafe argument: {:?}",
                arg
            );
        }
    }

    #[test]
    fn prompt_goes_through_stdin() {
        let prompt = stdin_prompt("Fill the template.", "<transcript>hello</transcript>");
        assert_eq!(prompt, "<instructions>\nFill the template.\n</instructions>\n\n<transcript>hello</transcript>");
        assert!(!print_args("sonnet").iter().any(|arg| arg.contains("transcript")));
    }

    #[test]
    fn parses_success() {
        let ok = output(0, r###"{"type":"result","is_error":false,"result":"## Summary\nDone."}"###, "");
        assert_eq!(parse_print_result(&ok).unwrap(), "## Summary\nDone.");
    }

    #[test]
    fn parses_errors() {
        // Real output of an expired login (trimmed).
        let expired = output(
            1,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate: OAuth session expired and could not be refreshed"}"#,
            "",
        );
        let error = parse_print_result(&expired).unwrap_err();
        assert!(matches!(&error, Error::Claude(message) if is_auth_error(message)));
        let message = error.user_message(&installation(Runtime::Wsl { distro: "archlinux".into() }));
        assert!(message.contains("not logged in") && message.contains("WSL (archlinux)"), "{}", message);

        let empty = output(0, r#"{"is_error":false,"result":"  "}"#, "");
        assert_eq!(parse_print_result(&empty).unwrap_err(), Error::Claude("empty answer".into()));

        let crash = output(1, "", "warning\nerror: unknown option '--foo'\n");
        assert_eq!(parse_print_result(&crash).unwrap_err(), Error::Failed("error: unknown option '--foo'".into()));

        let limit = output(1, r#"{"is_error":true,"result":"Usage limit reached"}"#, "");
        assert!(!is_auth_error("Usage limit reached"));
        assert_eq!(parse_print_result(&limit).unwrap_err(), Error::Claude("Usage limit reached".into()));
    }

    #[test]
    fn parses_auth_status() {
        let status: AuthStatus =
            serde_json::from_str(r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#).unwrap();
        assert_eq!(status, AuthStatus { logged_in: false, auth_method: Some("none".into()) });
    }

    #[tokio::test]
    async fn missing_executable_is_not_found() {
        let missing = Installation { runtime: Runtime::Native, executable: "/no/such/claude-binary".into() };
        let result = run(command(&missing, ["--version"]), None, QUERY_TIMEOUT, None).await;
        assert_eq!(result.unwrap_err(), Error::NotFound);
    }
}
