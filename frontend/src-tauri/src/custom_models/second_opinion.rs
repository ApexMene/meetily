//! Fork: "second opinion" models for the post-meeting re-transcription (NeMo CTC models, see
//! `parakeet_engine::ctc_model`). They live in `<app data>/models/second-opinion/<name>/`.
//! A small catalog can be downloaded from Hugging Face (pinned commit, verified digests); any
//! other compatible folder copied there by hand is listed too.

use super::download::download_files;
use super::hf::{self, Digest, RemoteFile};
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, Runtime};
use tokio_util::sync::CancellationToken;

struct CatalogEntry {
    name: &'static str,
    label: &'static str,
    repo: &'static str,
    commit: &'static str,
    /// (path, size, sha256 for LFS files or git blob sha1 for small ones, is_lfs)
    files: &'static [(&'static str, u64, &'static str, bool)],
}

const IT_REPO: &str = "OpenVoiceOS/stt_it_fastconformer_hybrid_large_pc_onnx";
const IT_COMMIT: &str = "e780cde1bcc30ce4ffffba310767cb90ea6ef62d";

const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        name: "fastconformer-it",
        label: "Italian FastConformer (457 MB)",
        repo: IT_REPO,
        commit: IT_COMMIT,
        files: &[
            ("model.onnx", 457_109_871, "4a6677e0d6efd832f91009f73ab68d6bbfe5f22b62234d9114f603782b1c5c74", true),
            ("vocab.txt", 4_365, "146c5b6f8f18a4ccda7ebff1e76a6761c6e25f40", false),
            ("config.json", 96, "b3d4808743ef6cddbbe19107f413a5b17b26cfad", false),
        ],
    },
    CatalogEntry {
        name: "fastconformer-it-int8",
        label: "Italian FastConformer int8 (131 MB, faster)",
        repo: IT_REPO,
        commit: IT_COMMIT,
        files: &[
            ("model.int8.onnx", 131_387_355, "099443e3b2c167199e0d450f8853bffff140d75359faef66fa7893deb61d48dc", true),
            ("vocab.txt", 4_365, "146c5b6f8f18a4ccda7ebff1e76a6761c6e25f40", false),
            ("config.json", 96, "b3d4808743ef6cddbbe19107f413a5b17b26cfad", false),
        ],
    },
];

pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && !name.starts_with('.')
}

fn root<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("models").join("second-opinion"))
}

/// Setting values for a Parakeet model (same list as the main model) start with this prefix.
pub const PARAKEET_PREFIX: &str = "parakeet:";

/// A valid stored setting: empty, a CTC folder name or `parakeet:<model name>`.
pub fn is_valid_setting(value: &str) -> bool {
    match value.strip_prefix(PARAKEET_PREFIX) {
        // A folder name: no separators, no "." or "..".
        Some(name) => !name.trim_matches(['.', ' ']).is_empty() && name.len() <= 200 && !name.contains(['/', '\\']),
        None => value.is_empty() || is_safe_name(value),
    }
}

/// The second-opinion model chosen in the settings, if usable.
pub fn alt_model<R: Runtime>(app: &AppHandle<R>, setting: &str, main_model: Option<&str>) -> Option<crate::audio::retranscription::AltModel> {
    use crate::audio::retranscription::AltModel;
    match setting.strip_prefix(PARAKEET_PREFIX) {
        Some(name) if Some(name) == main_model => {
            log::info!("second opinion = main model ({}): skipped", name);
            None
        }
        Some(name) if !name.trim().is_empty() => Some(AltModel::Parakeet(name.to_string())),
        Some(_) => None,
        None => model_dir(app, setting).map(AltModel::Ctc),
    }
}

/// Folder of an installed, usable second-opinion model.
pub fn model_dir<R: Runtime>(app: &AppHandle<R>, name: &str) -> Option<PathBuf> {
    if name.is_empty() || !is_safe_name(name) {
        return None;
    }
    let dir = root(app)?.join(name);
    match crate::parakeet_engine::ctc_model::check_dir(&dir) {
        None => Some(dir),
        Some(problem) => {
            log::warn!("second opinion model {} not usable: {}", name, problem);
            None
        }
    }
}

#[derive(Serialize)]
pub struct SecondOpinionModel {
    name: String,
    label: String,
    installed: bool,
    problem: Option<String>,
}

#[tauri::command]
pub async fn second_opinion_list<R: Runtime>(app: AppHandle<R>) -> Vec<SecondOpinionModel> {
    let Some(root) = root(&app) else { return Vec::new() };
    let mut out: Vec<SecondOpinionModel> = CATALOG
        .iter()
        .map(|e| {
            let dir = root.join(e.name);
            let problem = dir.exists().then(|| crate::parakeet_engine::ctc_model::check_dir(&dir)).flatten();
            SecondOpinionModel { name: e.name.into(), label: e.label.into(), installed: dir.exists() && problem.is_none(), problem }
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() || !is_safe_name(&name) || CATALOG.iter().any(|e| e.name == name) {
                continue;
            }
            let problem = crate::parakeet_engine::ctc_model::check_dir(&entry.path());
            out.push(SecondOpinionModel { label: name.clone(), installed: problem.is_none(), problem, name });
        }
    }
    out
}

/// Downloads a catalog model. Resolves when the download is complete (can take minutes).
#[tauri::command]
pub async fn second_opinion_install<R: Runtime>(app: AppHandle<R>, name: String) -> Result<(), String> {
    let entry = CATALOG.iter().find(|e| e.name == name).ok_or("unknown model")?;
    let root = root(&app).ok_or("no app data dir")?;
    let staging = root.join(format!(".{}.partial", entry.name));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let files: Vec<RemoteFile> = entry
        .files
        .iter()
        .map(|(path, size, digest, lfs)| RemoteFile {
            repo_path: path.to_string(),
            name: path.to_string(),
            size: *size,
            expected: if *lfs { Digest::Sha256(digest.to_string()) } else { Digest::GitSha1(digest.to_string()) },
            origin: None,
        })
        .collect();
    let client = hf::http_client()?;
    let cancel = CancellationToken::new();
    log::info!("second opinion: downloading {} from {}", entry.name, entry.repo);
    let result = download_files(&client, entry.repo, entry.commit, &files, &staging, None, &cancel, &|_, _, _| {}).await;
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let target = root.join(entry.name);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target).map_err(|e| e.to_string())?;
    if let Some(problem) = crate::parakeet_engine::ctc_model::check_dir(&target) {
        return Err(problem);
    }
    log::info!("second opinion: {} installed", entry.name);
    Ok(())
}
