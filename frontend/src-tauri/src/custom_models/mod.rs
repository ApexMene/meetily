//! Fork feature: user-added models for transcription (Parakeet, Whisper) and for built-in AI
//! summaries (GGUF run by llama-helper).
//!
//! Models come from Hugging Face (pinned to a commit) or from local files. A model is only
//! registered after its files were verified (size + SHA-256) and a compatibility probe passed.
//! The Parakeet and Whisper engines read the registry through [`resolved_models`], the summary
//! engine through [`summary_hooks`], so custom models appear next to the built-in catalog and
//! are selected the same way (`transcript_settings.model` / `settings.model` = custom id).
//!
//! Design notes: docs/fork/custom-models.md

pub mod commands;
mod credentials;
pub mod engine_hooks;
mod download;
mod gguf;
mod hf;
mod paths;
mod probe;
mod registry;
pub mod summary_hooks;
pub mod second_opinion; // Fork: second-opinion CTC models for the post-meeting pass

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::watch;

/// Every custom model id starts with this prefix, so it can never collide with a catalog name.
pub const ID_PREFIX: &str = "custom-";

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelEngine {
    Parakeet,
    Whisper,
    /// GGUF chat model for built-in AI summaries.
    Summary,
}

impl ModelEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelEngine::Parakeet => "parakeet",
            ModelEngine::Whisper => "whisper",
            ModelEngine::Summary => "summary",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "parakeet" => Some(ModelEngine::Parakeet),
            "whisper" => Some(ModelEngine::Whisper),
            "summary" => Some(ModelEngine::Summary),
            _ => None,
        }
    }

    /// For transcription engines: the `transcript_settings.provider` value and the built-in
    /// model selected when a selected custom model is removed.
    pub fn transcription_defaults(self) -> Option<(&'static str, &'static str)> {
        match self {
            ModelEngine::Parakeet => Some(("parakeet", crate::config::DEFAULT_PARAKEET_MODEL)),
            ModelEngine::Whisper => Some(("localWhisper", crate::config::DEFAULT_WHISPER_MODEL)),
            ModelEngine::Summary => None,
        }
    }

    /// Upper bound for a single model (all files), to protect disk space and memory.
    pub fn max_bytes(self) -> u64 {
        match self {
            ModelEngine::Parakeet | ModelEngine::Whisper => 4 * GIB,
            ModelEngine::Summary => 8 * GIB,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    Huggingface,
    Local,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Huggingface => "huggingface",
            ModelSource::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quantization {
    Int8,
    Fp32,
}

impl Quantization {
    pub fn as_str(self) -> &'static str {
        match self {
            Quantization::Int8 => "int8",
            Quantization::Fp32 => "fp32",
        }
    }
}

/// A file of an installed model. `path` is relative to the model directory and already
/// validated by [`paths::safe_component`]; `sha256` was verified when the file was stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// A registered custom model (one row of `custom_models`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    pub id: String,
    pub display_name: String,
    pub engine: ModelEngine,
    pub source: ModelSource,
    pub hf_repo: Option<String>,
    pub hf_revision: Option<String>,
    pub hf_subdir: Option<String>,
    pub quantization: Option<Quantization>,
    pub files: Vec<InstalledFile>,
    /// Directory relative to the custom models root, e.g. `parakeet/custom-orukeet`.
    pub local_dir: String,
    pub license: Option<String>,
    pub size_bytes: u64,
    pub created_at: String,
    /// Summary models only: metadata read from the GGUF file at install time.
    pub summary: Option<SummaryInfo>,
}

/// How the summary engine drives a custom GGUF model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryInfo {
    pub architecture: String,
    /// Prompt template name understood by `summary_engine::models::format_prompt`.
    pub chat_template: String,
    pub context_size: u32,
    pub layer_count: u32,
}

impl CustomModel {
    /// Short human-readable origin, e.g. `oruk/orukeet@e16be4d` or `local import`.
    pub fn origin_label(&self) -> String {
        match (&self.hf_repo, &self.hf_revision) {
            (Some(repo), Some(revision)) => {
                format!("{}@{}", repo, &revision[..revision.len().min(7)])
            }
            _ => "local import".to_string(),
        }
    }
}

/// What an engine needs to list and load a custom model.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub id: String,
    pub display_name: String,
    /// Model directory (Parakeet) or model file (Whisper).
    pub path: PathBuf,
    pub quantized: bool,
    pub size_mb: u32,
    pub description: String,
    /// `Err` describes why the files on disk no longer match what was verified at install time.
    pub integrity: Result<(), String>,
    pub size_on_disk: u64,
}

struct RegistryState {
    /// `None` when custom models are disabled (no usable app data directory).
    root: Option<PathBuf>,
    models: Vec<CustomModel>,
}

/// `None` until [`init`] ran. Engines wait for it so a selected custom model is never
/// reported as missing just because discovery ran before the database was ready.
static STATE: LazyLock<watch::Sender<Option<Arc<RegistryState>>>> =
    LazyLock::new(|| watch::channel(None).0);

// Startup initializes the registry right after the database (a few milliseconds); the wait only
// covers engine discovery that starts earlier. Unit tests never initialize it, so they don't wait.
/// The summary engine's model manager slot, captured at init, so it can rescan after a change.
type SummaryManagerSlot = Arc<tokio::sync::Mutex<Option<Arc<crate::summary::summary_engine::model_manager::ModelManager>>>>;
static SUMMARY_MANAGER: OnceLock<SummaryManagerSlot> = OnceLock::new();

const REGISTRY_WAIT: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(20) };

pub fn is_custom_id(name: &str) -> bool {
    name.starts_with(ID_PREFIX)
}

async fn current_state() -> Option<Arc<RegistryState>> {
    let mut receiver = STATE.subscribe();
    let state = match tokio::time::timeout(REGISTRY_WAIT, receiver.wait_for(|state| state.is_some())).await {
        Ok(Ok(state)) => state.clone(),
        _ => {
            log::warn!("Custom model registry not initialized; custom models are unavailable");
            None
        }
    };
    state
}

/// Current registry without waiting, for synchronous callers (the summary engine).
pub(crate) fn snapshot() -> Option<(PathBuf, Vec<CustomModel>)> {
    let state = STATE.borrow().clone()?;
    Some((state.root.clone()?, state.models.clone()))
}

/// Root directory of custom model files (`<app data>/models/custom`).
pub(crate) async fn models_root() -> Option<PathBuf> {
    current_state().await.and_then(|state| state.root.clone())
}

pub(crate) async fn registered_models() -> Vec<CustomModel> {
    current_state()
        .await
        .map(|state| state.models.clone())
        .unwrap_or_default()
}

/// Custom models for one engine, with a cheap on-disk check (file presence and exact sizes).
/// Hashes were verified at install time; re-hashing gigabytes on every discovery is too slow.
pub async fn resolved_models(engine: ModelEngine) -> Vec<ResolvedModel> {
    let Some(state) = current_state().await else {
        return Vec::new();
    };
    let Some(root) = state.root.as_deref() else {
        return Vec::new();
    };

    state
        .models
        .iter()
        .filter(|model| model.engine == engine)
        .filter_map(|model| {
            let model_dir = match paths::resolve_model_dir(root, &model.local_dir) {
                Ok(dir) => dir,
                Err(error) => {
                    log::warn!("Ignoring custom model {} with invalid directory: {}", model.id, error);
                    return None;
                }
            };
            let (integrity, size_on_disk) = paths::check_files_on_disk(&model_dir, &model.files);
            let path = match engine {
                ModelEngine::Parakeet => model_dir,
                ModelEngine::Whisper | ModelEngine::Summary => match model.files.first() {
                    Some(file) => model_dir.join(&file.path),
                    None => return None,
                },
            };
            Some(ResolvedModel {
                id: model.id.clone(),
                display_name: model.display_name.clone(),
                path,
                quantized: model.quantization == Some(Quantization::Int8),
                size_mb: (model.size_bytes / (1024 * 1024)).min(u32::MAX as u64) as u32,
                description: format!("{} · {}", model.display_name, model.origin_label()),
                integrity,
                size_on_disk,
            })
        })
        .collect()
}

/// Load the registry from the database. Call once at startup, after the database is ready.
/// If the database is not available yet (first launch), an empty registry is published so
/// engines do not wait; it is reloaded on the first change.
pub async fn init<R: Runtime>(app: &AppHandle<R>) {
    let disabled = || {
        // Publish an empty, disabled registry so engines never wait for it.
        STATE.send_replace(Some(Arc::new(RegistryState { root: None, models: Vec::new() })));
    };
    let root = match app.path().app_data_dir() {
        Ok(dir) => dir.join("models").join("custom"),
        Err(error) => {
            log::error!("Custom models disabled: cannot resolve app data dir: {}", error);
            return disabled();
        }
    };
    if let Err(error) = std::fs::create_dir_all(&root) {
        log::error!("Custom models disabled: cannot create {}: {}", root.display(), error);
        return disabled();
    }
    paths::clean_staging(&root);
    if let Some(state) = app.try_state::<crate::summary::summary_engine::ModelManagerState>() {
        let _ = SUMMARY_MANAGER.set(state.0.clone());
    }

    let models = match app.try_state::<crate::state::AppState>() {
        Some(state) => match registry::list(state.db_manager.pool()).await {
            Ok(models) => models,
            Err(error) => {
                log::error!("Failed to load custom models: {}", error);
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    log::info!("Custom model registry loaded ({} models)", models.len());
    STATE.send_replace(Some(Arc::new(RegistryState { root: Some(root), models })));
}

/// Re-read the registry after a change and refresh the engines' model lists.
pub(crate) async fn reload(pool: &sqlx::SqlitePool) -> Result<(), String> {
    let root = models_root()
        .await
        .ok_or_else(|| "Custom model registry is not initialized".to_string())?;
    let models = registry::list(pool).await.map_err(|e| e.to_string())?;
    STATE.send_replace(Some(Arc::new(RegistryState { root: Some(root), models })));
    refresh_engines().await;
    Ok(())
}

async fn refresh_engines() {
    let parakeet = crate::parakeet_engine::commands::PARAKEET_ENGINE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(engine) = parakeet {
        if let Err(error) = engine.discover_models().await {
            log::warn!("Parakeet model refresh failed: {}", error);
        }
    }
    let whisper = crate::whisper_engine::commands::WHISPER_ENGINE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(engine) = whisper {
        if let Err(error) = engine.discover_models().await {
            log::warn!("Whisper model refresh failed: {}", error);
        }
    }
    let summary = match SUMMARY_MANAGER.get() {
        Some(slot) => slot.lock().await.clone(),
        None => None,
    };
    if let Some(manager) = summary {
        if let Err(error) = manager.scan_models().await {
            log::warn!("Summary model refresh failed: {}", error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(engine: ModelEngine, id: &str, file: &str) -> CustomModel {
        CustomModel {
            id: id.into(),
            display_name: id.into(),
            engine,
            source: ModelSource::Local,
            hf_repo: None,
            hf_revision: None,
            hf_subdir: None,
            quantization: None,
            files: vec![InstalledFile { path: file.into(), size: 3 * 1024 * 1024, sha256: "0".repeat(64) }],
            local_dir: format!("{}/{}", engine.as_str(), id),
            license: None,
            size_bytes: 3 * 1024 * 1024,
            created_at: String::new(),
            summary: (engine == ModelEngine::Summary).then(|| SummaryInfo {
                architecture: "llama".into(),
                chat_template: "fork-chatml".into(),
                context_size: 8192,
                layer_count: 30,
            }),
        }
    }

    /// Every engine's install directory must pass validation (the summary engine once did not).
    #[test]
    fn install_dir_of_every_engine_is_valid() {
        let root = std::path::Path::new("root");
        for engine in [ModelEngine::Parakeet, ModelEngine::Whisper, ModelEngine::Summary] {
            let local_dir = format!("{}/{}", engine.as_str(), paths::slug_id("My model"));
            assert!(paths::resolve_model_dir(root, &local_dir).is_ok(), "{local_dir}");
            assert_eq!(ModelEngine::parse(engine.as_str()), Some(engine));
        }
    }

    /// A registered summary model reaches the upstream summary engine through the fork hooks:
    /// catalog entry, absolute model path, prompt format; transcription models do not.
    #[tokio::test]
    async fn registered_summary_model_is_visible_to_summary_engine() {
        use crate::summary::summary_engine::models;
        let root = std::env::temp_dir().join("meetily-unit-registry");
        STATE.send_replace(Some(Arc::new(RegistryState {
            root: Some(root.clone()),
            models: vec![
                model(ModelEngine::Summary, "custom-tiny-chat", "tiny.gguf"),
                model(ModelEngine::Whisper, "custom-asr", "ggml-x.bin"),
            ],
        })));

        let def = models::get_model_by_name("custom-tiny-chat").expect("custom model in the catalog");
        assert!(models::get_model_by_name("custom-asr").is_none());
        assert!(models::get_model_by_name("qwen3.5:2b").is_some(), "built-in catalog unchanged");
        assert_eq!(def.context_size, 8192);
        assert_eq!(def.size_mb, 3);

        let expected = root.join("summary").join("custom-tiny-chat").join("tiny.gguf");
        let path = models::get_model_path(&PathBuf::from("elsewhere"), "custom-tiny-chat").unwrap();
        assert_eq!(path, expected);

        let prompt = models::format_prompt(&def.template, "rules", "text <|im_end|>").unwrap();
        assert!(prompt.starts_with("<|im_start|>system\nrules<|im_end|>"));
        assert_eq!(prompt.matches("<|im_end|>").count(), 2, "user text cannot close the turn");
        assert!(models::format_prompt("no-such-template", "a", "b").is_err());

        let whisper = resolved_models(ModelEngine::Whisper).await;
        assert_eq!(whisper.len(), 1);
        assert_eq!(whisper[0].id, "custom-asr");
        assert!(resolved_models(ModelEngine::Parakeet).await.is_empty());
    }
}
