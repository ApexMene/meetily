//! Tauri commands behind the Claude Code settings card.

use super::{cli, config, detect, ClaudeCodeConfig, Environment, Installation, MODELS};
use crate::state::AppState;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeSettings {
    pub config: ClaudeCodeConfig,
    pub models: Vec<&'static str>,
    /// Environments the user can pick on this machine (always Auto and Native, plus WSL distributions).
    pub environments: Vec<Environment>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeStatus {
    pub installation: Installation,
    pub version: String,
    pub logged_in: bool,
    pub auth_method: Option<String>,
}

#[tauri::command]
pub async fn claude_code_get_settings() -> Result<ClaudeCodeSettings, String> {
    let mut environments = vec![Environment::Auto, Environment::Native];
    environments.extend(detect::wsl_distros().await.into_iter().map(|distro| Environment::Wsl { distro }));
    Ok(ClaudeCodeSettings {
        config: super::current_config().await,
        models: MODELS.to_vec(),
        environments,
    })
}

#[tauri::command]
pub async fn claude_code_save_config(
    state: tauri::State<'_, AppState>,
    config: ClaudeCodeConfig,
) -> Result<ClaudeCodeConfig, String> {
    let config = config::validate(config)?;
    config::save(state.db_manager.pool(), &config).await?;
    super::set_config(config.clone()).await;
    Ok(config)
}

/// Checks the configuration shown in the card (saved or not): finds `claude`, reads its version
/// and login state. Does not send any prompt, so it costs no usage.
#[tauri::command]
pub async fn claude_code_check(config: ClaudeCodeConfig) -> Result<ClaudeCodeStatus, String> {
    let config = config::validate(config)?;
    let installation = detect::resolve(&config).await?;
    let describe = |error: cli::Error| error.user_message(&installation);
    let version = cli::version(&installation).await.map_err(describe)?;
    let auth = cli::auth_status(&installation).await.map_err(describe)?;
    Ok(ClaudeCodeStatus {
        installation,
        version,
        logged_in: auth.logged_in,
        auth_method: auth.auth_method,
    })
}
