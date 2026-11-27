//! Fork: the speaker embedding model of the voiceprints (`crate::voiceprint`): WeSpeaker
//! ResNet34 (VoxCeleb, ONNX, 25 MB, CC-BY-4.0, https://github.com/wenet-e2e/wespeaker). Downloaded on
//! demand from Hugging Face (pinned commit, verified digest) into
//! `<app data>/models/speaker-embedding/`; never part of the repository.

use super::download::download_files;
use super::hf::{self, Digest, RemoteFile};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, Runtime};
use tokio_util::sync::CancellationToken;

const REPO: &str = "Wespeaker/wespeaker-voxceleb-resnet34-LM";
const COMMIT: &str = "f0c48c298fd835726c27956a5d617bad7115627e";
pub const FILE: &str = "voxceleb_resnet34_LM.onnx";
const SIZE: u64 = 26_530_309;
const SHA256: &str = "7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068";

fn dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("models").join("speaker-embedding"))
}

/// The model file when installed.
pub fn model_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    dir(app).map(|d| d.join(FILE)).filter(|p| p.is_file())
}

/// Downloads the model unless it is installed.
pub async fn ensure_installed<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    if let Some(path) = model_path(app) {
        return Ok(path);
    }
    let root = dir(app).ok_or("app data directory unavailable")?;
    let staging = root.with_file_name(".speaker-embedding.partial");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let files = vec![RemoteFile { repo_path: FILE.into(), name: FILE.into(), size: SIZE, expected: Digest::Sha256(SHA256.into()), origin: None }];
    let client = hf::http_client()?;
    let cancel = CancellationToken::new();
    log::info!("voiceprint: downloading the speaker model from {}", REPO);
    if let Err(error) = download_files(&client, REPO, COMMIT, &files, &staging, None, &cancel, &|_, _, _| {}).await {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let _ = std::fs::remove_dir_all(&root);
    std::fs::rename(&staging, &root).map_err(|e| e.to_string())?;
    log::info!("voiceprint: speaker model installed");
    Ok(root.join(FILE))
}
