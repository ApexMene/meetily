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
            Error::Cancelled => "Cancelled".to_string(),
            Error::TimedOut => "Claude Code did not answer in time".to_string(),
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

pub fn is_auth_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["authenticate", "not logged in", "/login", "oauth", "credential"]
        .iter()
        .any(|marker| message.contains(marker))
}

/// What a print-mode run may use besides plain text.
#[derive(Clone, Copy, Debug)]
pub enum Tools<'a> {
    /// No tools and no MCP servers (summaries, transcript clean-up).
    None,
    /// Only the listed tools (comma-separated) of the account's claude.ai connectors; built-in
    /// tools stay denied. `--strict-mcp-config` is not passed because it would hide the connectors.
    Connectors(&'a str),
}

/// Built-in tools denied when connector tools are allowed (`--tools ""` would hide those too).
const BUILTIN_TOOLS: &str = "Bash,Read,Write,Edit,NotebookEdit,Glob,Grep,WebFetch,WebSearch,Task,Agent";

/// Arguments of a print-mode run: no skills, no user/project settings (their hooks would run too)
/// and no saved session; tools as requested.
fn print_args<'a>(model: &'a str, system_prompt: &'a str, tools: Tools<'a>) -> Vec<&'a str> {
    let mut args = vec!["--print", "--output-format", "json", "--model", model, "--system-prompt", system_prompt];
    match tools {
        Tools::None => args.extend(["--tools", "", "--strict-mcp-config"]),
        Tools::Connectors(allowed) => args.extend(["--allowedTools", allowed, "--disallowedTools", BUILTIN_TOOLS]),
    }
    args.extend(["--disable-slash-commands", "--setting-sources", "", "--no-session-persistence"]);
    args
}

fn stdin_prompt(system_prompt: &str, user_prompt: &str) -> String {
    format!("<instructions>\n{}\n</instructions>\n\n{}", system_prompt, user_prompt)
}

/// The command that runs `claude <args>` for an installation. `ANTHROPIC_API_KEY` is removed so
/// the CLI always uses the account login, never a key that happens to be in the environment.
pub fn command<'a>(installation: &Installation, args: impl IntoIterator<Item = &'a str>) -> Command {
    command_with_env(installation, &[], args)
}

/// Like [`command`], with extra environment variables (e.g. a proxy). In WSL they are passed to
/// `env`, so values must be plain words (no spaces or quotes).
pub fn command_with_env<'a>(
    installation: &Installation,
    env: &[(String, String)],
    args: impl IntoIterator<Item = &'a str>,
) -> Command {
    let mut command = match &installation.runtime {
        Runtime::Native => {
            let mut command = Command::new(&installation.executable);
            command.env_remove("ANTHROPIC_API_KEY").envs(env.iter().map(|(key, value)| (key, value)));
            // A neutral directory: no project CLAUDE.md or settings are picked up.
            command.current_dir(std::env::temp_dir());
            command
        }
        Runtime::Wsl { distro } => {
            let mut command = wsl_command(distro);
            command
                .args(["--cd", "/tmp", "--exec", "env", "-u", "ANTHROPIC_API_KEY"])
                .args(env.iter().map(|(key, value)| format!("{}={}", key, value)))
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
    let request = Request {
        model,
        system_prompt: SYSTEM_PROMPT,
        instructions: system_prompt,
        content: user_prompt,
        tools: Tools::None,
        env: &[],
        timeout: GENERATION_TIMEOUT,
    };
    print(installation, &request, cancellation_token).await
}

/// A print-mode run. `instructions` and `content` travel through stdin; the rest are arguments.
pub struct Request<'a> {
    pub model: &'a str,
    /// Short fixed sentence (a command-line argument, so plain words only).
    pub system_prompt: &'a str,
    pub instructions: &'a str,
    pub content: &'a str,
    pub tools: Tools<'a>,
    pub env: &'a [(String, String)],
    pub timeout: Duration,
}

pub async fn print(
    installation: &Installation,
    request: &Request<'_>,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, Error> {
    let command = command_with_env(
        installation,
        request.env,
        print_args(request.model, request.system_prompt, request.tools),
    );
    let stdin = stdin_prompt(request.instructions, request.content);
    let output = run(command, Some(stdin), request.timeout, cancellation_token).await?;
    parse_print_result(&output)
}

/// `claude mcp list`: the configured MCP servers and claude.ai connectors with their connection
/// state. Uses no model tokens.
pub async fn mcp_list(installation: &Installation, env: &[(String, String)]) -> Result<String, Error> {
    // Checking every connector can take a while, longer than `--version`.
    let command = command_with_env(installation, env, ["mcp", "list"]);
    let output = run(command, None, 2 * QUERY_TIMEOUT, None).await?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() || is_auth_error(&text) {
        Ok(text)
    } else {
        Err(Error::Failed(
            last_line(&output.stderr)
                .or_else(|| last_line(&output.stdout))
                .unwrap_or_else(|| format!("exited with {}", output.status)),
        ))
    }
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
        let native = argv(&command(&installation(Runtime::Native), print_args("sonnet", SYSTEM_PROMPT, Tools::None)));
        let wsl = argv(&command(&installation(Runtime::Wsl { distro: "Ubuntu".into() }), print_args("sonnet", SYSTEM_PROMPT, Tools::None)));

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
        let connectors = Tools::Connectors("mcp__claude_ai_Notion__notion-search,mcp__claude_ai_Notion__notion-fetch");
        for tools in [Tools::None, connectors] {
            for arg in print_args("sonnet", SYSTEM_PROMPT, tools) {
                assert!(
                    arg.chars().all(|c| c.is_ascii_alphanumeric() || " -.,_".contains(c)),
                    "unsafe argument: {:?}",
                    arg
                );
            }
        }
    }

    #[test]
    fn connector_runs_deny_builtin_tools_and_stay_isolated() {
        let args = print_args("sonnet", SYSTEM_PROMPT, Tools::Connectors("mcp__x__y"));
        let after = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
        assert_eq!(after("--allowedTools"), Some("mcp__x__y"));
        assert_eq!(after("--disallowedTools"), Some(BUILTIN_TOOLS));
        assert_eq!(after("--setting-sources"), Some(""));
        // `--tools ""` would hide the connector tools as well.
        assert!(!args.contains(&"--tools"));
    }

    #[test]
    fn environment_reaches_both_launchers() {
        let env = vec![("HTTPS_PROXY".to_string(), "http://127.0.0.1:8080".to_string())];
        let wsl = argv(&command_with_env(&installation(Runtime::Wsl { distro: "Ubuntu".into() }), &env, ["--version"]));
        assert_eq!(wsl[9..], ["HTTPS_PROXY=http://127.0.0.1:8080", "/home/u/.local/bin/claude", "--version"]);
        let native = command_with_env(&installation(Runtime::Native), &env, ["--version"]);
        let value = native.as_std().get_envs().find(|(key, _)| *key == "HTTPS_PROXY").and_then(|(_, value)| value);
        assert_eq!(value.map(|v| v.to_string_lossy().into_owned()), Some("http://127.0.0.1:8080".to_string()));
    }

    #[test]
    fn prompt_goes_through_stdin() {
        let prompt = stdin_prompt("Fill the template.", "<transcript>hello</transcript>");
        assert_eq!(prompt, "<instructions>\nFill the template.\n</instructions>\n\n<transcript>hello</transcript>");
        assert!(!print_args("sonnet", SYSTEM_PROMPT, Tools::None).iter().any(|arg| arg.contains("transcript")));
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
