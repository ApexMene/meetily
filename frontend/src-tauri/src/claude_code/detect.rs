//! Finds a `claude` executable for the configured environment.
//!
//! Detection never depends on a terminal: GUI apps do not inherit the PATH of the user's shell
//! (macOS, WSL), so well-known install locations are checked first and the user's login shell
//! is only asked as a fallback.

use super::{cli, config, ClaudeCodeConfig, Environment, Installation, Runtime};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A stopped distribution boots on first use, which takes a few seconds.
const WSL_TIMEOUT: Duration = Duration::from_secs(45);

/// WSL helper distributions that never contain a user's tools.
const WSL_SYSTEM_DISTROS: [&str; 2] = ["docker-desktop", "docker-desktop-data"];

/// Locates `claude` inside a distribution. It runs as `sh -s` (script on stdin, so nothing needs
/// quoting): first the installers' default locations, then the user's login shell, which also
/// sees PATH changes from rc files (npm prefixes, nvm, custom installs).
const WSL_FIND_SCRIPT: &str = r#"for p in "$HOME/.local/bin/claude" "$HOME/.claude/local/claude" /usr/local/bin/claude /usr/bin/claude; do
  if [ -x "$p" ]; then echo "$p"; exit 0; fi
done
"${SHELL:-/bin/sh}" -lic 'command -v claude' </dev/null 2>/dev/null
"#;

pub async fn resolve(config: &ClaudeCodeConfig) -> Result<Installation, String> {
    match &config.environment {
        Environment::Native => native(config.executable.as_deref()),
        Environment::Wsl { distro } => wsl(distro, config.executable.as_deref()).await,
        Environment::Auto => {
            if let Ok(found) = native(None) {
                return Ok(found);
            }
            for distro in wsl_distros().await {
                if let Ok(found) = wsl(&distro, None).await {
                    return Ok(found);
                }
            }
            Err(not_found_message())
        }
    }
}

fn not_found_message() -> String {
    if cfg!(windows) {
        "Claude Code was not found on this computer or in WSL. Install it (https://claude.com/claude-code), or choose where it is installed in the Claude Code settings.".to_string()
    } else {
        "Claude Code was not found on this computer. Install it (https://claude.com/claude-code), or set its path in the Claude Code settings.".to_string()
    }
}

fn native(executable: Option<&str>) -> Result<Installation, String> {
    let path = match executable {
        Some(path) if Path::new(path).is_file() => PathBuf::from(path),
        Some(path) => return Err(format!("Claude Code was not found at {}", path)),
        None => which::which("claude")
            .ok()
            .or_else(|| native_candidates().into_iter().find(|path| path.is_file()))
            .ok_or_else(not_found_message)?,
    };
    Ok(Installation { runtime: Runtime::Native, executable: path.to_string_lossy().into_owned() })
}

/// Default install locations, for when the app's PATH is not the user's shell PATH.
fn native_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local").join("bin").join(name));
        candidates.push(home.join(".claude").join("local").join(name));
    }
    if !cfg!(windows) {
        candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
        candidates.push(PathBuf::from("/usr/local/bin/claude"));
    }
    candidates
}

/// Installed WSL distributions (empty when WSL is unavailable or on other systems).
pub async fn wsl_distros() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut command = tokio::process::Command::new("wsl.exe");
    command.args(["--list", "--quiet"]);
    match cli::run(command, None, WSL_TIMEOUT, None).await {
        Ok(output) if output.status.success() => parse_distro_list(&output.stdout),
        _ => Vec::new(),
    }
}

/// `wsl.exe --list` prints UTF-16LE; names are filtered to the ones `wsl.exe -d` can take.
fn parse_distro_list(bytes: &[u8]) -> Vec<String> {
    let text = if bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).any(|&b| b == 0) {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').to_string())
        .filter(|name| config::is_valid_distro_name(name) && !WSL_SYSTEM_DISTROS.contains(&name.as_str()))
        .collect()
}

async fn wsl(distro: &str, executable: Option<&str>) -> Result<Installation, String> {
    let installation = |path: String| Installation {
        runtime: Runtime::Wsl { distro: distro.to_string() },
        executable: path,
    };
    let in_distro = format!("in WSL ({})", distro);

    if let Some(path) = executable {
        let mut command = cli::wsl_command(distro);
        command.args(["--exec", "test", "-x", path]);
        return match cli::run(command, None, WSL_TIMEOUT, None).await {
            Ok(output) if output.status.success() => Ok(installation(path.to_string())),
            Ok(_) => Err(format!("Claude Code was not found at {} {}", path, in_distro)),
            Err(error) => Err(format!("Cannot start WSL ({}): {:?}", distro, error)),
        };
    }

    let mut command = cli::wsl_command(distro);
    command.args(["--exec", "sh", "-s"]);
    let output = cli::run(command, Some(WSL_FIND_SCRIPT.to_string()), WSL_TIMEOUT, None)
        .await
        .map_err(|error| format!("Cannot start WSL ({}): {:?}", distro, error))?;
    pick_path(&String::from_utf8_lossy(&output.stdout))
        .map(installation)
        .ok_or_else(|| format!("Claude Code was not found {}", in_distro))
}

/// The executable path from the find script's output. Interactive shells may print banners
/// first, and `command -v` prints `alias claude='/path'` for an alias, so the last absolute
/// path wins.
fn pick_path(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .map(|line| line.trim().trim_start_matches("alias claude=").trim_matches(|c| c == '\'' || c == '"'))
        .find(|line| line.starts_with('/') && !line.contains(char::is_whitespace))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn parses_wsl_distro_list() {
        let listed = utf16("Ubuntu-24.04\r\narchlinux\r\ndocker-desktop\r\n\r\n");
        assert_eq!(parse_distro_list(&listed), ["Ubuntu-24.04", "archlinux"]);
        // Some WSL versions honor WSL_UTF8=1 and print UTF-8.
        assert_eq!(parse_distro_list(b"Debian\n"), ["Debian"]);
        assert!(parse_distro_list(&utf16("Windows Subsystem for Linux has no installed distributions.\r\n")).is_empty());
    }

    #[test]
    fn picks_claude_path_from_shell_output() {
        assert_eq!(pick_path("/home/u/.local/bin/claude\n").as_deref(), Some("/home/u/.local/bin/claude"));
        assert_eq!(
            pick_path("Welcome to Ubuntu\n[oh-my-zsh] update available\nalias claude='/home/u/.claude/local/claude'\n").as_deref(),
            Some("/home/u/.claude/local/claude")
        );
        assert_eq!(pick_path("claude not found\n"), None);
        assert_eq!(pick_path(""), None);
    }

    #[test]
    fn explicit_native_path_must_exist() {
        assert!(native(Some("/no/such/claude")).is_err());
        let exe = std::env::current_exe().unwrap();
        let found = native(Some(exe.to_str().unwrap())).unwrap();
        assert_eq!(found.runtime, Runtime::Native);
    }
}

/// Real WSL checks (Windows with WSL only): `cargo test -p meetily --lib claude_code::detect::live -- --ignored --nocapture`.
#[cfg(all(test, windows))]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn arguments_survive_the_wsl_boundary() {
        let Some(distro) = wsl_distros().await.into_iter().next() else {
            panic!("no WSL distribution installed");
        };
        // The same arguments a completion uses, echoed back one per line.
        let mut command = cli::wsl_command(&distro);
        command.args(["--exec", "printf", "[%s]\\n", "--tools", "", "plain words, with dots."]);
        let output = cli::run(command, None, WSL_TIMEOUT, None).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "[--tools]\n[]\n[plain words, with dots.]\n");
    }

    #[tokio::test]
    #[ignore]
    async fn detects_claude_in_wsl() {
        for distro in wsl_distros().await {
            match wsl(&distro, None).await {
                Ok(found) => {
                    let version = cli::version(&found).await.unwrap();
                    let auth = cli::auth_status(&found).await.unwrap();
                    println!("{}: {} ({}) {:?}", distro, found.executable, version, auth);
                }
                Err(error) => println!("{}: {}", distro, error),
            }
        }
    }
}
