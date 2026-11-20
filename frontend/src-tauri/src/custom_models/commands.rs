//! Tauri commands for custom models (transcription and built-in AI summaries).
//!
//! Install flow (Hugging Face or local): files go to `<root>/.staging/<id>`, are verified and
//! probed there, then moved to `<root>/<engine>/<id>` and registered. Nothing half-installed
//! ever becomes selectable; staging is wiped at startup.

use super::download::{self, CANCELLED};
use super::hf::{self, Variant};
use super::paths::{self, safe_component, STAGING_DIR};
use super::{credentials, gguf, probe, registry};
use super::{CustomModel, InstalledFile, ModelEngine, ModelSource, Quantization, SummaryInfo};
use crate::database::repositories::setting::SettingsRepository;
use crate::state::AppState;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Runtime, State};
use tokio_util::sync::CancellationToken;

const EVENT_PROGRESS: &str = "custom-model-install-progress";
const EVENT_COMPLETE: &str = "custom-model-install-complete";
const EVENT_ERROR: &str = "custom-model-install-error";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Active installs by job id (= model id), so they can be cancelled and never run twice.
static JOBS: LazyLock<Mutex<HashMap<String, CancellationToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelView {
    pub id: String,
    pub display_name: String,
    pub engine: ModelEngine,
    pub source: ModelSource,
    pub origin: String,
    pub hf_repo: Option<String>,
    pub hf_revision: Option<String>,
    pub hf_subdir: Option<String>,
    pub quantization: Option<Quantization>,
    pub size_bytes: u64,
    pub license: Option<String>,
    pub created_at: String,
    /// GGUF metadata, for summary models only.
    pub summary: Option<SummaryInfo>,
    /// `None` when the files on disk still match what was verified at install time.
    pub problem: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectResult {
    pub repo: String,
    pub commit: String,
    pub license: Option<String>,
    pub gated: bool,
    pub used_token: bool,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent<'a> {
    job_id: &'a str,
    phase: &'a str,
    downloaded_bytes: u64,
    total_bytes: u64,
    percent: u8,
    current_file: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteEvent<'a> {
    job_id: &'a str,
    model_id: &'a str,
    engine: ModelEngine,
    display_name: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEvent<'a> {
    job_id: &'a str,
    error: &'a str,
    cancelled: bool,
}

fn emit_progress<R: Runtime>(app: &AppHandle<R>, job_id: &str, phase: &str, done: u64, total: u64, file: &str) {
    let percent = if total > 0 { ((done as f64 / total as f64) * 100.0).min(100.0) as u8 } else { 0 };
    let _ = app.emit(
        EVENT_PROGRESS,
        ProgressEvent { job_id, phase, downloaded_bytes: done, total_bytes: total, percent, current_file: file },
    );
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

#[tauri::command]
pub async fn custom_models_list() -> Result<Vec<CustomModelView>, String> {
    let root = super::models_root().await.ok_or("Custom models are not available")?;
    Ok(super::registered_models()
        .await
        .into_iter()
        .map(|model| {
            let problem = match paths::resolve_model_dir(&root, &model.local_dir) {
                Ok(dir) => paths::check_files_on_disk(&dir, &model.files).0.err(),
                Err(error) => Some(error),
            };
            CustomModelView {
                origin: model.origin_label(),
                id: model.id,
                display_name: model.display_name,
                engine: model.engine,
                source: model.source,
                hf_repo: model.hf_repo,
                hf_revision: model.hf_revision,
                hf_subdir: model.hf_subdir,
                quantization: model.quantization,
                size_bytes: model.size_bytes,
                license: model.license,
                created_at: model.created_at,
                summary: model.summary,
                problem,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Hugging Face
// ---------------------------------------------------------------------------------------------

/// Resolve a repository reference to a commit and list the variants our engines can run.
#[tauri::command]
pub async fn custom_models_hf_inspect(reference: String) -> Result<InspectResult, String> {
    let reference = hf::parse_reference(&reference)?;
    let token = credentials::get_token().unwrap_or_else(|error| {
        log::warn!("{}", error);
        None
    });
    let client = hf::http_client()?;
    let revision =
        hf::resolve_revision(&client, &reference.repo, reference.revision.as_deref(), token.as_deref()).await?;
    let files = hf::list_files(&client, &reference.repo, &revision.commit, token.as_deref()).await?;
    let variants = hf::detect_variants(&files, reference.subpath.as_deref());
    log::info!(
        "Inspected {}@{}: {} files, {} compatible variants",
        reference.repo,
        &revision.commit[..7],
        files.len(),
        variants.len()
    );
    Ok(InspectResult {
        repo: reference.repo,
        commit: revision.commit,
        license: revision.license,
        gated: revision.gated,
        used_token: token.is_some(),
        variants,
    })
}

/// Download, verify, probe and register one variant. Returns the job id; progress and the
/// outcome arrive as events.
#[tauri::command]
pub async fn custom_models_hf_install<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    repo: String,
    commit: String,
    variant_key: String,
    display_name: String,
) -> Result<String, String> {
    let reference = hf::parse_reference(&repo)?;
    if reference.repo != repo || !hf::is_commit_sha(&commit) {
        return Err("Invalid repository or commit".into());
    }
    let display_name = validate_display_name(&display_name)?;
    let token = credentials::get_token().unwrap_or(None);

    // Re-read the listing for the pinned commit: the frontend only names the variant, it never
    // supplies URLs, file names or digests.
    let client = hf::http_client()?;
    let license = hf::resolve_revision(&client, &repo, Some(&commit), token.as_deref())
        .await?
        .license;
    let files = hf::list_files(&client, &repo, &commit, token.as_deref()).await?;
    let variant = hf::detect_variants(&files, None)
        .into_iter()
        .find(|v| v.key == variant_key)
        .ok_or("This model variant is no longer available at that commit")?;
    if let Some(reason) = &variant.blocked_reason {
        return Err(reason.clone());
    }
    if variant.engine == ModelEngine::Summary {
        // Fail fast from the header (a few MB) instead of after a multi-GB download.
        let file = variant.files.first().ok_or("GGUF file missing from the variant")?;
        gguf::read_remote_metadata(&client, &repo, &commit, file, token.as_deref())
            .await
            .map_err(|e| format!("Model is not compatible: {}", e))?;
    }

    let pool = state.db_manager.pool().clone();
    let id = unique_id(&pool, &display_name).await?;
    let root = super::models_root().await.ok_or("Custom models are not available")?;
    ensure_space(&root, variant.total_bytes)?;
    let cancel = start_job(&id)?;

    let job = InstallJob {
        id: id.clone(),
        display_name,
        engine: variant.engine,
        source: ModelSource::Huggingface,
        hf_repo: Some(repo.clone()),
        hf_revision: Some(commit.clone()),
        hf_subdir: Some(variant.subdir.clone()),
        quantization: variant.quantization,
        license,
    };
    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        let staging = root.join(STAGING_DIR).join(&job.id);
        let result = async {
            create_fresh_dir(&staging).await?;
            let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
            let progress_app = app_for_task.clone();
            let job_id = job.id.clone();
            let on_progress = move |done: u64, total: u64, file: &str| {
                // Throttled: the UI only needs a few updates per second.
                if done == total || last_emit_elapsed(&mut last_emit) {
                    emit_progress(&progress_app, &job_id, "downloading", done, total, file);
                }
            };
            let on_progress = std::sync::Mutex::new(on_progress);
            let installed = download::download_files(
                &client,
                &repo,
                &commit,
                &variant.files,
                &staging,
                token.as_deref(),
                &cancel,
                &|done, total, file| {
                    if let Ok(mut callback) = on_progress.lock() {
                        (callback)(done, total, file)
                    }
                },
            )
            .await?;
            finish_install(&app_for_task, &pool, &root, &staging, &job, installed, &cancel).await
        }
        .await;
        report_outcome(&app_for_task, &job, result, &staging).await;
    });
    Ok(id)
}

fn last_emit_elapsed(last: &mut Instant) -> bool {
    if last.elapsed() >= PROGRESS_INTERVAL {
        *last = Instant::now();
        true
    } else {
        false
    }
}

#[tauri::command]
pub async fn custom_models_cancel(job_id: String) -> Result<bool, String> {
    let token = JOBS.lock().map_err(|_| "job registry poisoned")?.get(&job_id).cloned();
    Ok(match token {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    })
}

// ---------------------------------------------------------------------------------------------
// Local import
// ---------------------------------------------------------------------------------------------

/// Let the user choose a Parakeet model folder, a Whisper `.bin` or a summary `.gguf` file.
#[tauri::command]
pub async fn custom_models_pick_local<R: Runtime>(app: AppHandle<R>, engine: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let engine = ModelEngine::parse(&engine).ok_or("Unknown engine")?;
    let picked = tokio::task::spawn_blocking(move || {
        let dialog = app.dialog().file();
        match engine {
            ModelEngine::Parakeet => dialog.set_title("Select the Parakeet ONNX model folder").blocking_pick_folder(),
            ModelEngine::Whisper => dialog
                .set_title("Select a whisper.cpp ggml model")
                .add_filter("whisper.cpp model", &["bin"])
                .blocking_pick_file(),
            ModelEngine::Summary => dialog
                .set_title("Select a GGUF instruct model")
                .add_filter("GGUF model", &["gguf"])
                .blocking_pick_file(),
        }
    })
    .await
    .map_err(|e| format!("File dialog failed: {}", e))?;
    match picked {
        Some(path) => Ok(Some(path.into_path().map_err(|e| e.to_string())?.to_string_lossy().into_owned())),
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn custom_models_import_local<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    engine: String,
    path: String,
    display_name: String,
) -> Result<String, String> {
    let engine = ModelEngine::parse(&engine).ok_or("Unknown engine")?;
    let display_name = validate_display_name(&display_name)?;
    let source_path = PathBuf::from(&path);
    let plan = plan_local_import(engine, &source_path)?;
    if engine == ModelEngine::Summary {
        let header_path = source_path.clone();
        tokio::task::spawn_blocking(move || gguf::read_file_metadata(&header_path))
            .await
            .map_err(|e| format!("Cannot read the model: {}", e))?
            .map_err(|e| format!("Model is not compatible: {}", e))?;
    }

    let pool = state.db_manager.pool().clone();
    let id = unique_id(&pool, &display_name).await?;
    let root = super::models_root().await.ok_or("Custom models are not available")?;
    ensure_space(&root, plan.total_bytes + plan.preprocessor.as_ref().map_or(0, |p| p.size))?;
    let cancel = start_job(&id)?;

    let job = InstallJob {
        id: id.clone(),
        display_name,
        engine,
        source: ModelSource::Local,
        hf_repo: None,
        hf_revision: None,
        hf_subdir: None,
        quantization: plan.quantization,
        license: None,
    };
    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        let staging = root.join(STAGING_DIR).join(&job.id);
        let result = async {
            create_fresh_dir(&staging).await?;
            emit_progress(&app_for_task, &job.id, "copying", 0, plan.total_bytes, "");
            let sources = plan.sources.clone();
            let copy_dir = staging.clone();
            let copy_cancel = cancel.clone();
            let max_bytes = job.engine.max_bytes();
            let mut installed = tokio::task::spawn_blocking(move || {
                download::copy_local(&sources, &copy_dir, max_bytes, &copy_cancel)
            })
            .await
            .map_err(|e| format!("copy failed: {}", e))??;
            if let Some(preprocessor) = &plan.preprocessor {
                // Folder without nemo128.onnx: fetch the pinned standard preprocessor.
                let client = hf::http_client()?;
                installed.extend(
                    download::download_files(
                        &client,
                        "istupakov/parakeet-tdt-0.6b-v3-onnx",
                        "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce",
                        std::slice::from_ref(preprocessor),
                        &staging,
                        None,
                        &cancel,
                        &|_, _, _| {},
                    )
                    .await?,
                );
            }
            emit_progress(&app_for_task, &job.id, "copying", plan.total_bytes, plan.total_bytes, "");
            finish_install(&app_for_task, &pool, &root, &staging, &job, installed, &cancel).await
        }
        .await;
        report_outcome(&app_for_task, &job, result, &staging).await;
    });
    Ok(id)
}

struct LocalPlan {
    sources: Vec<(PathBuf, String)>,
    quantization: Option<Quantization>,
    total_bytes: u64,
    preprocessor: Option<hf::RemoteFile>,
}

fn plan_local_import(engine: ModelEngine, source: &Path) -> Result<LocalPlan, String> {
    let file_size = |path: &Path| -> Result<u64, String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        Ok(metadata.len())
    };
    let mut sources = Vec::new();
    let mut quantization = None;
    let mut preprocessor = None;

    match engine {
        ModelEngine::Whisper | ModelEngine::Summary => {
            let name = source
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid file name")?;
            if engine == ModelEngine::Summary && !name.to_ascii_lowercase().ends_with(".gguf") {
                return Err("Select a .gguf file".into());
            }
            sources.push((source.to_path_buf(), safe_component(name)?.to_string()));
        }
        ModelEngine::Parakeet => {
            if !source.is_dir() {
                return Err("Select the folder that contains the Parakeet ONNX files".into());
            }
            let has = |name: &str| source.join(name).is_file();
            let q = if has("encoder-model.int8.onnx") && has("decoder_joint-model.int8.onnx") {
                Quantization::Int8
            } else if has("encoder-model.onnx") && has("decoder_joint-model.onnx") {
                Quantization::Fp32
            } else {
                return Err(
                    "Folder must contain encoder-model[.int8].onnx and decoder_joint-model[.int8].onnx".into(),
                );
            };
            quantization = Some(q);
            let mut names: Vec<String> = probe::parakeet_graph_names(q == Quantization::Int8)
                .iter()
                .map(|s| s.to_string())
                .collect();
            if !has("vocab.txt") {
                return Err("vocab.txt is missing from the folder".into());
            }
            names.push("vocab.txt".into());
            for optional in ["config.json", "nemo128.onnx"] {
                if has(optional) {
                    names.push(optional.into());
                }
            }
            if !has("nemo128.onnx") {
                preprocessor = Some(hf::standard_preprocessor());
            }
            if q == Quantization::Fp32 {
                let entries = std::fs::read_dir(source).map_err(|e| e.to_string())?;
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if (name.starts_with("encoder-model.onnx.") || name.starts_with("decoder_joint-model.onnx."))
                        && !name.ends_with(".zst")
                    {
                        names.push(name);
                    }
                }
            }
            for name in names {
                safe_component(&name)?;
                sources.push((source.join(&name), name));
            }
        }
    }

    let mut total_bytes = 0u64;
    for (path, _) in &sources {
        total_bytes += file_size(path)?;
    }
    let max_bytes = engine.max_bytes();
    if total_bytes > max_bytes {
        return Err(format!(
            "Model is {:.1} GB, above the {} GB limit",
            total_bytes as f64 / 1e9,
            max_bytes / super::GIB
        ));
    }
    Ok(LocalPlan { sources, quantization, total_bytes, preprocessor })
}

// ---------------------------------------------------------------------------------------------
// Shared install steps
// ---------------------------------------------------------------------------------------------

struct InstallJob {
    id: String,
    display_name: String,
    engine: ModelEngine,
    source: ModelSource,
    hf_repo: Option<String>,
    hf_revision: Option<String>,
    hf_subdir: Option<String>,
    quantization: Option<Quantization>,
    license: Option<String>,
}

async fn finish_install<R: Runtime>(
    app: &AppHandle<R>,
    pool: &sqlx::SqlitePool,
    root: &Path,
    staging: &Path,
    job: &InstallJob,
    installed: Vec<InstalledFile>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    let total: u64 = installed.iter().map(|f| f.size).sum();
    emit_progress(app, &job.id, "testing", total, total, "");
    let model_file = match job.engine {
        ModelEngine::Whisper | ModelEngine::Summary => installed.first().map(|f| f.path.as_str()),
        ModelEngine::Parakeet => None,
    };
    let summary = probe::probe(job.engine, staging, job.quantization, model_file)
        .await
        .map_err(|e| format!("Model is not compatible: {}", e))?;
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }

    let local_dir = format!("{}/{}", job.engine.as_str(), job.id);
    let final_dir = paths::resolve_model_dir(root, &local_dir)?;
    if let Some(parent) = final_dir.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    if final_dir.exists() {
        return Err(format!("{} already exists", final_dir.display()));
    }
    tokio::fs::rename(staging, &final_dir)
        .await
        .map_err(|e| format!("Cannot move the model into place: {}", e))?;

    let model = CustomModel {
        id: job.id.clone(),
        display_name: job.display_name.clone(),
        engine: job.engine,
        source: job.source,
        hf_repo: job.hf_repo.clone(),
        hf_revision: job.hf_revision.clone(),
        hf_subdir: job.hf_subdir.clone(),
        quantization: job.quantization,
        files: installed,
        local_dir,
        license: job.license.clone(),
        size_bytes: total,
        created_at: chrono::Utc::now().to_rfc3339(),
        summary,
    };
    if let Err(error) = registry::insert(pool, &model).await {
        let _ = tokio::fs::remove_dir_all(&final_dir).await;
        return Err(format!("Cannot register the model: {}", error));
    }
    super::reload(pool).await
}

async fn report_outcome<R: Runtime>(app: &AppHandle<R>, job: &InstallJob, result: Result<(), String>, staging: &Path) {
    finish_job(&job.id);
    match result {
        Ok(()) => {
            log::info!("Custom model {} installed ({})", job.id, job.engine.as_str());
            let _ = app.emit(
                EVENT_COMPLETE,
                CompleteEvent { job_id: &job.id, model_id: &job.id, engine: job.engine, display_name: &job.display_name },
            );
        }
        Err(error) => {
            if staging.exists() {
                let _ = tokio::fs::remove_dir_all(staging).await;
            }
            let cancelled = error == CANCELLED;
            if cancelled {
                log::info!("Custom model install {} cancelled", job.id);
            } else {
                log::warn!("Custom model install {} failed: {}", job.id, error);
            }
            let _ = app.emit(EVENT_ERROR, ErrorEvent { job_id: &job.id, error: &error, cancelled });
        }
    }
}

fn start_job(id: &str) -> Result<CancellationToken, String> {
    let mut jobs = JOBS.lock().map_err(|_| "job registry poisoned")?;
    if jobs.contains_key(id) {
        return Err("An install with this name is already running".into());
    }
    let token = CancellationToken::new();
    jobs.insert(id.to_string(), token.clone());
    Ok(token)
}

fn finish_job(id: &str) {
    if let Ok(mut jobs) = JOBS.lock() {
        jobs.remove(id);
    }
}

async fn create_fresh_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        tokio::fs::remove_dir_all(dir).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Cannot create {}: {}", dir.display(), e))
}

fn validate_display_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err("Name must be 1 to 80 characters".into());
    }
    Ok(name.to_string())
}

async fn unique_id(pool: &sqlx::SqlitePool, display_name: &str) -> Result<String, String> {
    let base = paths::slug_id(display_name);
    let running: Vec<String> = JOBS.lock().map(|jobs| jobs.keys().cloned().collect()).unwrap_or_default();
    for suffix in 1..1000 {
        let candidate = if suffix == 1 { base.clone() } else { format!("{}-{}", base, suffix) };
        let taken = running.contains(&candidate)
            || registry::id_exists(pool, &candidate).await.map_err(|e| e.to_string())?;
        if !taken {
            return Ok(candidate);
        }
    }
    Err("Too many models with this name".into())
}

fn ensure_space(root: &Path, needed: u64) -> Result<(), String> {
    if let Some(available) = paths::available_space(root) {
        // Keep 1 GB of headroom for recordings and the database.
        let headroom = 1024 * 1024 * 1024;
        if available < needed.saturating_add(headroom) {
            return Err(format!(
                "Not enough disk space: {:.1} GB needed, {:.1} GB free",
                needed as f64 / 1e9,
                available as f64 / 1e9
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------------------------

/// Remove a custom model. If it is the selected model, the selection falls back to the
/// built-in default (transcription engine default, or the recommended built-in summary model).
#[tauri::command]
pub async fn custom_models_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if !super::is_custom_id(&id) {
        return Err("Not a custom model".into());
    }
    let pool = state.db_manager.pool().clone();
    let model = super::registered_models()
        .await
        .into_iter()
        .find(|m| m.id == id)
        .ok_or("Model not found")?;
    let root = super::models_root().await.ok_or("Custom models are not available")?;

    unload_if_loaded(model.engine, &id).await;

    // Files first: if they are locked the model stays registered and the user can retry,
    // instead of leaving an orphan folder behind.
    let dir = paths::resolve_model_dir(&root, &model.local_dir)?;
    remove_dir_with_retry(&dir).await?;

    reset_selection_if_needed(&pool, &model).await?;
    registry::delete(&pool, &id).await.map_err(|e| e.to_string())?;
    super::reload(&pool).await
}

async fn reset_selection_if_needed(pool: &sqlx::SqlitePool, model: &CustomModel) -> Result<(), String> {
    let id = model.id.as_str();
    if let Some((provider, default_model)) = model.engine.transcription_defaults() {
        if let Ok(Some(config)) = SettingsRepository::get_transcript_config(pool).await {
            if config.model == id {
                SettingsRepository::save_transcript_config(pool, provider, default_model)
                    .await
                    .map_err(|e| format!("Cannot reset the selected model: {}", e))?;
                log::info!("Selected model {} removed; switched to {}", id, default_model);
            }
        }
        return Ok(());
    }
    if let Ok(Some(config)) = SettingsRepository::get_model_config(pool).await {
        if config.provider == "builtin-ai" && config.model == id {
            let fallback =
                crate::summary::summary_engine::commands::get_recommended_summary_model_for_current_system()?;
            SettingsRepository::save_model_config(
                pool,
                "builtin-ai",
                fallback,
                &config.whisper_model,
                config.ollama_endpoint.as_deref(),
            )
            .await
            .map_err(|e| format!("Cannot reset the selected model: {}", e))?;
            log::info!("Selected summary model {} removed; switched to {}", id, fallback);
        }
    }
    Ok(())
}

/// Windows may keep a file locked for a moment after the process that mapped it exits.
async fn remove_dir_with_retry(dir: &Path) -> Result<(), String> {
    let mut last_error = None;
    for _ in 0..20 {
        if !dir.exists() {
            return Ok(());
        }
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "Cannot remove the model files, they may be in use: {}",
        last_error.map(|e| e.to_string()).unwrap_or_default()
    ))
}

async fn unload_if_loaded(engine: ModelEngine, id: &str) {
    match engine {
        ModelEngine::Parakeet => {
            let engine = crate::parakeet_engine::commands::PARAKEET_ENGINE
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().cloned());
            if let Some(engine) = engine {
                if engine.get_current_model().await.as_deref() == Some(id) {
                    engine.unload_model().await;
                }
            }
        }
        ModelEngine::Whisper => {
            let engine = crate::whisper_engine::commands::WHISPER_ENGINE
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().cloned());
            if let Some(engine) = engine {
                if engine.get_current_model().await.as_deref() == Some(id) {
                    engine.unload_model().await;
                }
            }
        }
        ModelEngine::Summary => {
            // The sidecar maps the GGUF while it runs: stop it so the file can be deleted.
            // It restarts on the next summary.
            if let Err(error) = crate::summary::summary_engine::shutdown_sidecar_gracefully().await {
                log::warn!("Could not stop the summary engine: {}", error);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Hugging Face token
// ---------------------------------------------------------------------------------------------

#[tauri::command]
pub async fn custom_models_token_status() -> Result<bool, String> {
    Ok(credentials::get_token()?.is_some())
}

#[tauri::command]
pub async fn custom_models_set_token(token: String) -> Result<(), String> {
    credentials::set_token(&token)
}

#[tauri::command]
pub async fn custom_models_clear_token() -> Result<(), String> {
    credentials::clear_token()
}

/// Folder that holds custom models, for "show in folder". The path comes from our own state.
#[tauri::command]
pub async fn custom_models_open_folder() -> Result<(), String> {
    let root = super::models_root().await.ok_or("Custom models are not available")?;
    open::that_detached(&root).map_err(|e| format!("Cannot open {}: {}", root.display(), e))
}
