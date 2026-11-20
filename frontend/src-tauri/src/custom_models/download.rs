//! Verified transfers: every byte is hashed while it is written, and a file only counts as
//! installed when its size and digest match what Hugging Face published (or, for local
//! imports, what we computed while copying).

use super::hf::{self, Digest, RemoteFile};
use super::paths::safe_component;
use super::InstalledFile;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_RANGE, RANGE};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: u32 = 3;

pub(crate) const CANCELLED: &str = "Download cancelled";

/// Incremental verifier for one file.
struct Hashers {
    sha256: Sha256,
    git_sha1: Option<Sha1>,
}

impl Hashers {
    fn new(expected: &Digest, size: u64) -> Self {
        let git_sha1 = match expected {
            Digest::GitSha1(_) => {
                let mut sha1 = Sha1::new();
                sha1.update(format!("blob {}\0", size).as_bytes());
                Some(sha1)
            }
            Digest::Sha256(_) => None,
        };
        Self { sha256: Sha256::new(), git_sha1 }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.sha256.update(bytes);
        if let Some(sha1) = self.git_sha1.as_mut() {
            sha1.update(bytes);
        }
    }

    /// Check the expected digest and return the content SHA-256 (stored for later audits).
    fn finish(self, expected: &Digest, name: &str) -> Result<String, String> {
        let sha256 = hex(&self.sha256.finalize());
        let (actual, wanted) = match expected {
            Digest::Sha256(wanted) => (sha256.clone(), wanted),
            Digest::GitSha1(wanted) => (hex(&self.git_sha1.expect("sha1 hasher").finalize()), wanted),
        };
        if !actual.eq_ignore_ascii_case(wanted) {
            return Err(format!("{} failed integrity check (expected {}, got {})", name, wanted, actual));
        }
        Ok(sha256)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Download `files` from a pinned commit into `dest_dir` (a fresh staging directory).
/// `on_progress(downloaded_bytes, total_bytes, current_file)` is called while data arrives.
pub(crate) async fn download_files(
    client: &reqwest::Client,
    repo: &str,
    commit: &str,
    files: &[RemoteFile],
    dest_dir: &Path,
    token: Option<&str>,
    cancel: &CancellationToken,
    on_progress: &(dyn Fn(u64, u64, &str) + Send + Sync),
) -> Result<Vec<InstalledFile>, String> {
    if !hf::is_commit_sha(commit) {
        return Err("Downloads must be pinned to a commit".into());
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut completed = 0u64;
    let mut installed = Vec::with_capacity(files.len());

    for file in files {
        let name = safe_component(&file.name)?;
        // Files borrowed from another pinned repository never receive the user's token.
        let (url, token) = match &file.origin {
            Some(origin) if hf::is_commit_sha(&origin.commit) => {
                (hf::file_url(&origin.repo, &origin.commit, &file.repo_path)?, None)
            }
            Some(_) => return Err("Invalid pinned origin".into()),
            None => (hf::file_url(repo, commit, &file.repo_path)?, token),
        };
        let target = dest_dir.join(name);
        let mut last_error = String::new();
        let mut done = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match download_one(client, &url, &target, file, token, cancel, &|bytes| {
                on_progress(completed + bytes, total, name)
            })
            .await
            {
                Ok(sha256) => {
                    done = Some(sha256);
                    break;
                }
                Err(DownloadError::Fatal(error)) => return Err(error),
                Err(DownloadError::Retryable(error)) => {
                    log::warn!("Download of {} failed (attempt {}/{}): {}", name, attempt, MAX_ATTEMPTS, error);
                    last_error = error;
                    if attempt < MAX_ATTEMPTS {
                        tokio::select! {
                            _ = cancel.cancelled() => return Err(CANCELLED.into()),
                            _ = tokio::time::sleep(Duration::from_secs(2 * attempt as u64)) => {}
                        }
                    }
                }
            }
        }
        let sha256 = done.ok_or_else(|| format!("Failed to download {}: {}", name, last_error))?;
        completed += file.size;
        on_progress(completed, total, name);
        installed.push(InstalledFile { path: name.to_string(), size: file.size, sha256 });
    }
    Ok(installed)
}

enum DownloadError {
    Retryable(String),
    Fatal(String),
}

/// Download one file to `target`, resuming a previous partial attempt when the server
/// supports ranges. Returns the verified SHA-256.
async fn download_one(
    client: &reqwest::Client,
    url: &url::Url,
    target: &Path,
    file: &RemoteFile,
    token: Option<&str>,
    cancel: &CancellationToken,
    on_bytes: &(dyn Fn(u64) + Send + Sync),
) -> Result<String, DownloadError> {
    use DownloadError::{Fatal, Retryable};

    let part = part_path(target);
    let existing = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    let existing = if existing >= file.size { 0 } else { existing };

    let mut request = client.get(url.clone());
    if let Some(token) = token {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", token))
            .map_err(|_| Fatal("Invalid Hugging Face token".into()))?;
        value.set_sensitive(true);
        request = request.header(AUTHORIZATION, value);
    }
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={}-", existing));
    }
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(Fatal(CANCELLED.into())),
        response = request.send() => response.map_err(|e| Retryable(format!("network error: {}", e)))?,
    };

    let status = response.status();
    let resume_from = if status == reqwest::StatusCode::PARTIAL_CONTENT && existing > 0 {
        let range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let expected_prefix = format!("bytes {}-", existing);
        if !range.starts_with(&expected_prefix) || !range.ends_with(&format!("/{}", file.size)) {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(Retryable(format!("unexpected Content-Range {:?}", range)));
        }
        existing
    } else if status.is_success() {
        0
    } else if matches!(status.as_u16(), 401 | 403 | 404) {
        return Err(Fatal(format!("{}: HTTP {}", file.repo_path, status.as_u16())));
    } else {
        return Err(Retryable(format!("HTTP {}", status.as_u16())));
    };

    // Hash what is already on disk so the final digest covers the whole file.
    let mut hashers = Hashers::new(&file.expected, file.size);
    if resume_from > 0 {
        let mut reader = tokio::fs::File::open(&part).await.map_err(|e| Retryable(e.to_string()))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut remaining = resume_from;
        while remaining > 0 {
            use tokio::io::AsyncReadExt;
            let read = reader.read(&mut buffer).await.map_err(|e| Retryable(e.to_string()))?;
            if read == 0 {
                return Err(Retryable("partial file shrank during resume".into()));
            }
            let read = read.min(remaining as usize);
            hashers.update(&buffer[..read]);
            remaining -= read as u64;
        }
    }

    let output = if resume_from > 0 {
        tokio::fs::OpenOptions::new().append(true).open(&part).await
    } else {
        tokio::fs::File::create(&part).await
    }
    .map_err(|e| Fatal(format!("cannot write {}: {}", part.display(), e)))?;
    let mut output = tokio::io::BufWriter::with_capacity(4 * 1024 * 1024, output);

    let mut written = resume_from;
    on_bytes(written);
    let mut response = response;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(Fatal(CANCELLED.into())),
            chunk = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk()) => chunk,
        };
        let chunk = match chunk {
            Err(_) => return Err(Retryable("download stalled".into())),
            Ok(Err(error)) => return Err(Retryable(format!("network error: {}", error))),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        if written + chunk.len() as u64 > file.size {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(Fatal(format!("{} is larger than published ({} bytes)", file.repo_path, file.size)));
        }
        hashers.update(&chunk);
        output.write_all(&chunk).await.map_err(|e| Fatal(format!("write failed: {}", e)))?;
        written += chunk.len() as u64;
        on_bytes(written);
    }
    output.flush().await.map_err(|e| Fatal(format!("write failed: {}", e)))?;
    output.into_inner().sync_all().await.map_err(|e| Fatal(format!("write failed: {}", e)))?;

    if written != file.size {
        return Err(Retryable(format!("incomplete download ({} of {} bytes)", written, file.size)));
    }
    let sha256 = match hashers.finish(&file.expected, &file.repo_path) {
        Ok(sha256) => sha256,
        Err(error) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(Fatal(error));
        }
    };
    tokio::fs::rename(&part, target)
        .await
        .map_err(|e| Fatal(format!("cannot finalize {}: {}", target.display(), e)))?;
    Ok(sha256)
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// Copy local files into `dest_dir`, hashing while copying. `sources` pairs a source path
/// with the (validated) name it gets in the model directory; the copy stops past `max_bytes`.
/// Blocking; run on a blocking thread.
pub(crate) fn copy_local(
    sources: &[(PathBuf, String)],
    dest_dir: &Path,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Vec<InstalledFile>, String> {
    use std::io::{Read, Write};
    let mut installed = Vec::with_capacity(sources.len());
    let mut copied = 0u64;
    for (source, name) in sources {
        let name = safe_component(name)?;
        let metadata = std::fs::symlink_metadata(source)
            .map_err(|e| format!("cannot read {}: {}", source.display(), e))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", source.display()));
        }
        let mut reader = std::fs::File::open(source)
            .map_err(|e| format!("cannot open {}: {}", source.display(), e))?;
        let target = dest_dir.join(name);
        let mut writer = std::io::BufWriter::new(
            std::fs::File::create(&target).map_err(|e| format!("cannot write {}: {}", target.display(), e))?,
        );
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 4 * 1024 * 1024];
        let mut size = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(CANCELLED.into());
            }
            let read = reader.read(&mut buffer).map_err(|e| format!("read failed: {}", e))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            writer.write_all(&buffer[..read]).map_err(|e| format!("write failed: {}", e))?;
            size += read as u64;
            copied += read as u64;
            if copied > max_bytes {
                // A source grew after the size check: never exceed the model limit.
                return Err("Model files exceed the size limit".into());
            }
        }
        writer.flush().map_err(|e| format!("write failed: {}", e))?;
        writer
            .into_inner()
            .map_err(|e| format!("write failed: {}", e))?
            .sync_all()
            .map_err(|e| format!("write failed: {}", e))?;
        installed.push(InstalledFile { path: name.to_string(), size, sha256: hex(&hasher.finalize()) });
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_blob_sha1_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        let expected = Digest::GitSha1("ce013625030ba8dba906f756967f9e9ca394464a".into());
        let mut hashers = Hashers::new(&expected, 6);
        hashers.update(b"hello\n");
        assert!(hashers.finish(&expected, "x").is_ok());
    }

    #[test]
    fn sha256_mismatch_is_rejected() {
        let expected = Digest::Sha256("0".repeat(64));
        let mut hashers = Hashers::new(&expected, 1);
        hashers.update(b"x");
        assert!(hashers.finish(&expected, "x").is_err());
    }

    #[test]
    fn copy_local_hashes_content() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a"), b"abc").unwrap();
        let installed = copy_local(
            &[(src.path().join("a"), "model.bin".into())],
            dst.path(),
            1024,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(installed[0].size, 3);
        assert_eq!(
            installed[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(std::fs::read(dst.path().join("model.bin")).unwrap(), b"abc");
    }
}
