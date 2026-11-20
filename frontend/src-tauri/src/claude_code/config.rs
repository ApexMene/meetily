//! Persistence of the Claude Code settings in `settings.claudeCodeConfig` (JSON).
//! Fork migration: `20261120000000_add_claude_code_config.sql`.

use super::{ClaudeCodeConfig, Environment};
use sqlx::{Row, SqlitePool};
use std::path::Path;

pub async fn load(pool: &SqlitePool) -> Result<ClaudeCodeConfig, String> {
    let row = sqlx::query("SELECT claudeCodeConfig FROM settings WHERE id = '1' LIMIT 1")
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    let json: Option<String> = row.and_then(|row| row.get("claudeCodeConfig"));
    match json {
        Some(json) => serde_json::from_str(&json).map_err(|e| format!("Invalid Claude Code settings: {}", e)),
        None => Ok(ClaudeCodeConfig::default()),
    }
}

pub async fn save(pool: &SqlitePool, config: &ClaudeCodeConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    // Same upsert shape as the other settings columns; the defaults only matter on a fresh database.
    sqlx::query(
        r#"
        INSERT INTO settings (id, provider, model, whisperModel, claudeCodeConfig)
        VALUES ('1', 'builtin-ai', '', 'large-v3', $1)
        ON CONFLICT(id) DO UPDATE SET claudeCodeConfig = excluded.claudeCodeConfig
        "#,
    )
    .bind(json)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Normalizes user input and rejects values that could not name a real environment or file.
pub fn validate(mut config: ClaudeCodeConfig) -> Result<ClaudeCodeConfig, String> {
    if let Environment::Wsl { distro } = &config.environment {
        if !is_valid_distro_name(distro) {
            return Err(format!("Invalid WSL distribution name: \"{}\"", distro));
        }
    }
    config.executable = config.executable.map(|path| path.trim().to_string()).filter(|path| !path.is_empty());
    if let Some(path) = &config.executable {
        match config.environment {
            // Detection picks the environment, so a path would be ambiguous.
            Environment::Auto => config.executable = None,
            Environment::Native if !Path::new(path).is_absolute() => {
                return Err("The Claude Code path must be absolute.".to_string());
            }
            Environment::Wsl { .. } if !path.starts_with('/') => {
                return Err("The Claude Code path inside WSL must be absolute (start with /).".to_string());
            }
            _ => {}
        }
    }
    Ok(config)
}

/// WSL distribution names are letters, digits, `.`, `_` and `-`, never starting with `-`
/// (they are passed to `wsl.exe -d`, so a leading dash would read as an option).
pub fn is_valid_distro_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl(distro: &str, executable: Option<&str>) -> ClaudeCodeConfig {
        ClaudeCodeConfig {
            environment: Environment::Wsl { distro: distro.into() },
            executable: executable.map(Into::into),
        }
    }

    #[test]
    fn json_shape_is_stable() {
        let json = serde_json::to_string(&wsl("Ubuntu-24.04", Some("/home/u/.local/bin/claude"))).unwrap();
        assert_eq!(
            json,
            r#"{"environment":{"kind":"wsl","distro":"Ubuntu-24.04"},"executable":"/home/u/.local/bin/claude"}"#
        );
        let empty: ClaudeCodeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ClaudeCodeConfig::default());
    }

    #[test]
    fn validation() {
        assert!(validate(wsl("Ubuntu-24.04", None)).is_ok());
        assert!(validate(wsl("-d", None)).is_err());
        assert!(validate(wsl("arch linux", None)).is_err());
        assert!(validate(wsl("Ubuntu", Some("claude"))).is_err());

        let trimmed = validate(wsl("Ubuntu", Some("  /usr/bin/claude "))).unwrap();
        assert_eq!(trimmed.executable.as_deref(), Some("/usr/bin/claude"));
        let blank = validate(wsl("Ubuntu", Some("   "))).unwrap();
        assert_eq!(blank.executable, None);

        let auto = ClaudeCodeConfig { environment: Environment::Auto, executable: Some("/x/claude".into()) };
        assert_eq!(validate(auto).unwrap().executable, None);
    }
}
