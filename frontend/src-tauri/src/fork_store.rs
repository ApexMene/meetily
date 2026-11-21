//! Fork: small JSON settings files in the app data directory, read once and kept in memory.
//!
//! - A missing file means defaults; it is written so the user can find and edit it.
//! - An unreadable file also means defaults, but it is left untouched (never overwrite a file the
//!   user is editing by hand).
//! - Writes are atomic (temporary file + rename).
//! Changes made by hand to a file are picked up at the next app start.

use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tauri::{AppHandle, Manager, Runtime};

pub struct Store<T> {
    /// Path relative to the app data directory.
    file: &'static str,
    cached: RwLock<Option<T>>,
}

impl<T: Clone + Default + Serialize + DeserializeOwned> Store<T> {
    pub const fn new(file: &'static str) -> Self {
        Self { file, cached: RwLock::new(None) }
    }

    pub fn path<R: Runtime>(&self, app: &AppHandle<R>) -> Option<PathBuf> {
        app.path().app_data_dir().ok().map(|dir| dir.join(self.file))
    }

    pub fn get<R: Runtime>(&self, app: &AppHandle<R>) -> T {
        if let Some(value) = self.cached.read().ok().and_then(|cached| cached.clone()) {
            return value;
        }
        let value = self.path(app).map(|path| self.load(&path)).unwrap_or_default();
        if let Ok(mut cached) = self.cached.write() {
            *cached = Some(value.clone());
        }
        value
    }

    pub fn set<R: Runtime>(&self, app: &AppHandle<R>, value: T) -> Result<(), String> {
        let path = self.path(app).ok_or("app data directory unavailable")?;
        write_json(&path, &value).map_err(|error| format!("cannot save {}: {}", self.file, error))?;
        if let Ok(mut cached) = self.cached.write() {
            *cached = Some(value);
        }
        Ok(())
    }

    fn load(&self, path: &Path) -> T {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                log::warn!("{} is not valid ({}), using defaults until it is fixed", path.display(), error);
                T::default()
            }),
            Err(_) => {
                let value = T::default();
                if let Err(error) = write_json(path, &value) {
                    log::warn!("cannot create {}: {}", path.display(), error);
                }
                value
            }
        }
    }
}

/// Writes atomically (temporary file + rename) so a crash never leaves a half-written file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
