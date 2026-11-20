//! Filesystem safety for custom models: every path we create comes from validated
//! single components joined under the custom models root.

use super::{InstalledFile, ID_PREFIX};
use std::path::{Path, PathBuf};

pub(crate) const STAGING_DIR: &str = ".staging";

const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate one path component (a file or directory name) coming from outside the app
/// (Hugging Face listings, local file names). Rejects traversal, separators, drive or stream
/// syntax, control characters and names Windows treats specially.
pub(crate) fn safe_component(name: &str) -> Result<&str, String> {
    if name.is_empty() || name.len() > 200 {
        return Err(format!("invalid file name length: {:?}", name));
    }
    if name == "." || name == ".." {
        return Err(format!("path traversal is not allowed: {:?}", name));
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(format!("unsupported character in file name: {:?}", name));
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(format!("unsupported file name: {:?}", name));
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        return Err(format!("reserved file name: {:?}", name));
    }
    Ok(name)
}

/// Validate a `/`-separated repository path (e.g. `onnx/combined-v0.1.0-int8/vocab.txt`).
pub(crate) fn safe_repo_path(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty() || path.len() > 1024 {
        return Err(format!("invalid repository path: {:?}", path));
    }
    path.split('/').map(safe_component).collect()
}

/// Directory of an installed model from its stored `local_dir` (`<engine>/<id>`).
pub(crate) fn resolve_model_dir(root: &Path, local_dir: &str) -> Result<PathBuf, String> {
    let parts: Vec<&str> = local_dir.split('/').collect();
    match parts.as_slice() {
        [engine, id] if super::ModelEngine::parse(engine).is_some() && id.starts_with(ID_PREFIX) => {
            Ok(root.join(safe_component(engine)?).join(safe_component(id)?))
        }
        _ => Err(format!("unexpected model directory {:?}", local_dir)),
    }
}

/// Cheap integrity check used during discovery: every file exists with its exact size,
/// and nothing in the directory is a link that could point outside it.
pub(crate) fn check_files_on_disk(dir: &Path, files: &[InstalledFile]) -> (Result<(), String>, u64) {
    let mut total = 0u64;
    for file in files {
        let path = match safe_component(&file.path) {
            Ok(name) => dir.join(name),
            Err(error) => return (Err(error), total),
        };
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return (Err(format!("{} is a link", file.path)), total)
            }
            Ok(metadata) if !metadata.is_file() => {
                return (Err(format!("{} is not a regular file", file.path)), total)
            }
            Ok(metadata) => {
                total += metadata.len();
                if metadata.len() != file.size {
                    return (
                        Err(format!(
                            "{} has {} bytes, expected {}",
                            file.path,
                            metadata.len(),
                            file.size
                        )),
                        total,
                    );
                }
            }
            Err(error) => return (Err(format!("{}: {}", file.path, error)), total),
        }
    }
    (Ok(()), total)
}

/// Turn a display name into an id like `custom-orukeet-int8`.
pub(crate) fn slug_id(display_name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for c in display_name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "model" } else { slug };
    format!("{}{}", ID_PREFIX, slug)
}

/// Remove leftovers of interrupted installs. Staging only ever holds unregistered files.
pub(crate) fn clean_staging(root: &Path) {
    let staging = root.join(STAGING_DIR);
    if staging.exists() {
        if let Err(error) = std::fs::remove_dir_all(&staging) {
            log::warn!("Could not clean {}: {}", staging.display(), error);
        }
    }
}

/// Free space on the volume that holds `path`, when the platform can tell.
pub(crate) fn available_space(path: &Path) -> Option<u64> {
    fs2_free_space(path)
}

#[cfg(windows)]
fn fs2_free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free_to_caller: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path and the out pointer is valid for writes.
    let ok = unsafe {
        windows_free_space::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_to_caller)
}

#[cfg(windows)]
mod windows_free_space {
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }
}

#[cfg(not(windows))]
fn fs2_free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_special_names() {
        for bad in ["..", ".", "", "a/b", "a\\b", "C:x", "con", "CON.txt", "nul.onnx", "x.", " x", "a\u{0}b", "file:stream"] {
            assert!(safe_component(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["vocab.txt", "encoder-model.int8.onnx", "ggml-model-q5_0.bin", "console.bin"] {
            assert!(safe_component(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn repo_paths_are_validated_per_segment() {
        assert!(safe_repo_path("onnx/combined-v0.1.0-int8/vocab.txt").is_ok());
        assert!(safe_repo_path("onnx/../secret").is_err());
        assert!(safe_repo_path("/abs").is_err());
        assert!(safe_repo_path("a//b").is_err());
    }

    #[test]
    fn model_dir_must_be_engine_and_custom_id() {
        let root = Path::new("root");
        assert!(resolve_model_dir(root, "parakeet/custom-x").is_ok());
        assert!(resolve_model_dir(root, "summary/custom-x").is_ok());
        assert!(resolve_model_dir(root, "other/custom-x").is_err());
        assert!(resolve_model_dir(root, "parakeet/other").is_err());
        assert!(resolve_model_dir(root, "../custom-x").is_err());
        assert!(resolve_model_dir(root, "whisper/custom-x/extra").is_err());
    }

    #[test]
    fn slug_ids_are_stable_and_safe() {
        assert_eq!(slug_id("Orukeet v0.1 (int8)"), "custom-orukeet-v0-1-int8");
        assert_eq!(slug_id("  ///  "), "custom-model");
        assert!(slug_id(&"x".repeat(500)).len() <= ID_PREFIX.len() + 48);
    }

    #[test]
    fn disk_check_detects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"1234").unwrap();
        let ok = [InstalledFile { path: "a.bin".into(), size: 4, sha256: String::new() }];
        assert!(check_files_on_disk(dir.path(), &ok).0.is_ok());
        let wrong = [InstalledFile { path: "a.bin".into(), size: 5, sha256: String::new() }];
        assert!(check_files_on_disk(dir.path(), &wrong).0.is_err());
        let missing = [InstalledFile { path: "b.bin".into(), size: 1, sha256: String::new() }];
        assert!(check_files_on_disk(dir.path(), &missing).0.is_err());
    }
}
