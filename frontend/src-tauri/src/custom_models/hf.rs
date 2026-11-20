//! Minimal Hugging Face Hub client: resolve a repository reference to a commit, list its
//! files, and find the variants our engines can run.

use super::paths::{safe_component, safe_repo_path};
use super::{ModelEngine, Quantization};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, LINK};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

const HF_HOST: &str = "huggingface.co";
const API_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const MAX_TREE_PAGES: usize = 50;

/// A parsed user reference: `owner/repo` or a huggingface.co URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HfReference {
    pub repo: String,
    pub revision: Option<String>,
    /// Only consider files under this path (from `/tree/<rev>/<path>` or `/blob/<rev>/<path>`).
    pub subpath: Option<String>,
}

fn valid_repo_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 96
        && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !part.starts_with('.')
        && !part.contains("..")
}

pub(crate) fn valid_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= 128
        && revision.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !revision.starts_with('.')
        && !revision.contains("..")
}

pub(crate) fn is_commit_sha(revision: &str) -> bool {
    revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn parse_reference(input: &str) -> Result<HfReference, String> {
    let trimmed = input.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let rest = rest
        .strip_prefix("www.huggingface.co/")
        .or_else(|| rest.strip_prefix("huggingface.co/"))
        .or_else(|| rest.strip_prefix("hf.co/"))
        .unwrap_or(rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);

    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 2 {
        return Err("Use the format owner/repository or a huggingface.co link".into());
    }
    if matches!(parts[0], "datasets" | "spaces" | "api") {
        return Err("Only model repositories are supported".into());
    }
    if !valid_repo_part(parts[0]) || !valid_repo_part(parts[1]) {
        return Err(format!("Invalid repository name: {}/{}", parts[0], parts[1]));
    }
    let repo = format!("{}/{}", parts[0], parts[1]);

    let (revision, subpath) = match parts.get(2) {
        None => (None, None),
        Some(&kind) if matches!(kind, "tree" | "blob" | "resolve") => {
            let revision = parts
                .get(3)
                .ok_or_else(|| "Missing revision in link".to_string())?
                .to_string();
            if !valid_revision(&revision) {
                return Err(format!("Unsupported revision: {}", revision));
            }
            let path = parts[4..].join("/");
            let path = urlencoding_decode(&path)?;
            if !path.is_empty() {
                safe_repo_path(&path)?;
            }
            (Some(revision), (!path.is_empty()).then_some(path))
        }
        Some(other) => return Err(format!("Unsupported link section: {}", other)),
    };
    Ok(HfReference { repo, revision, subpath })
}

fn urlencoding_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value
                .get(i + 1..i + 3)
                .ok_or_else(|| "Invalid percent-encoding in link".to_string())?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| "Invalid percent-encoding in link")?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "Invalid UTF-8 in link".to_string())
}

/// HTTP client restricted to Hugging Face over HTTPS (including redirects to its CDNs).
pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let host = url.host_str().unwrap_or_default().to_string();
        if url.scheme() == "https" && is_allowed_host(&host) {
            attempt.follow()
        } else {
            attempt.error(format!("redirect to untrusted host blocked: {}", host))
        }
    });
    reqwest::Client::builder()
        .https_only(true)
        .redirect(policy)
        .connect_timeout(Duration::from_secs(20))
        .user_agent(concat!("Meetily-fork/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

pub(crate) fn is_allowed_host(host: &str) -> bool {
    host == HF_HOST
        || host.ends_with(".huggingface.co")
        || host == "hf.co"
        || host.ends_with(".hf.co")
}

/// Headers for a request to huggingface.co itself. The token is never attached to other hosts;
/// reqwest also drops `Authorization` when a redirect leaves the original host.
fn auth_headers(token: Option<&str>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", token))
            .map_err(|_| "Invalid Hugging Face token".to_string())?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    Ok(headers)
}

fn api_url(segments: &[&str]) -> Result<url::Url, String> {
    let mut url = url::Url::parse("https://huggingface.co/").map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|_| "invalid base URL".to_string())?
        .extend(segments);
    Ok(url)
}

/// URL of a file at a pinned commit.
pub(crate) fn file_url(repo: &str, commit: &str, repo_path: &str) -> Result<url::Url, String> {
    let (owner, name) = repo.split_once('/').ok_or("invalid repo")?;
    let mut segments = vec![owner, name, "resolve", commit];
    segments.extend(safe_repo_path(repo_path)?);
    api_url(&segments)
}

async fn read_limited(response: reqwest::Response) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    if response.content_length().unwrap_or(0) as usize > API_RESPONSE_LIMIT {
        return Err("Hugging Face response is too large".into());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Network error: {}", e))?;
        if body.len() + chunk.len() > API_RESPONSE_LIMIT {
            return Err("Hugging Face response is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// First `len` bytes of a file at a pinned commit (HTTP range request; if the server ignores the
/// range, the body is cut after `len` bytes).
pub(crate) async fn fetch_prefix(
    client: &reqwest::Client,
    repo: &str,
    commit: &str,
    repo_path: &str,
    token: Option<&str>,
    len: usize,
) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let url = file_url(repo, commit, repo_path)?;
    let response = client
        .get(url)
        .headers(auth_headers(token)?)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", len.saturating_sub(1)))
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("Cannot reach Hugging Face: {}", e))?;
    if !response.status().is_success() {
        return Err(status_error(response.status(), token.is_some()));
    }
    let mut body = Vec::with_capacity(len.min(API_RESPONSE_LIMIT));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Network error: {}", e))?;
        let room = len - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= len {
            break;
        }
    }
    Ok(body)
}

fn status_error(status: reqwest::StatusCode, has_token: bool) -> String {
    match status.as_u16() {
        401 | 403 if has_token => {
            "Access denied: the token cannot read this repository (accept its terms on huggingface.co if it is gated)".into()
        }
        401 | 403 => "Access denied: this repository is private or gated. Add a Hugging Face token.".into(),
        404 => "Repository, revision or path not found on Hugging Face".into(),
        429 => "Hugging Face rate limit reached, try again later".into(),
        code => format!("Hugging Face returned HTTP {}", code),
    }
}

#[derive(Debug, Deserialize)]
struct RevisionInfo {
    sha: Option<String>,
    #[serde(default)]
    gated: serde_json::Value,
    #[serde(default, rename = "cardData")]
    card_data: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct RepoRevision {
    pub commit: String,
    pub license: Option<String>,
    pub gated: bool,
}

pub(crate) async fn resolve_revision(
    client: &reqwest::Client,
    repo: &str,
    revision: Option<&str>,
    token: Option<&str>,
) -> Result<RepoRevision, String> {
    let (owner, name) = repo.split_once('/').ok_or("invalid repo")?;
    let revision = revision.unwrap_or("main");
    let url = api_url(&["api", "models", owner, name, "revision", revision])?;
    let response = client
        .get(url)
        .headers(auth_headers(token)?)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Cannot reach Hugging Face: {}", e))?;
    if !response.status().is_success() {
        return Err(status_error(response.status(), token.is_some()));
    }
    let info: RevisionInfo = serde_json::from_slice(&read_limited(response).await?)
        .map_err(|e| format!("Unexpected Hugging Face response: {}", e))?;
    let commit = info.sha.ok_or("Hugging Face did not return a commit")?;
    if !is_commit_sha(&commit) {
        return Err("Hugging Face returned an invalid commit id".into());
    }
    let license = info
        .card_data
        .as_ref()
        .and_then(|card| card.get("license"))
        .and_then(|license| match license {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Array(values) => Some(
                values
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => None,
        })
        .filter(|license| !license.is_empty());
    let gated = !matches!(info.gated, serde_json::Value::Bool(false) | serde_json::Value::Null);
    Ok(RepoRevision { commit, license, gated })
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TreeEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub lfs: Option<LfsInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LfsInfo {
    pub oid: String,
    pub size: u64,
}

pub(crate) async fn list_files(
    client: &reqwest::Client,
    repo: &str,
    commit: &str,
    token: Option<&str>,
) -> Result<Vec<TreeEntry>, String> {
    let (owner, name) = repo.split_once('/').ok_or("invalid repo")?;
    let mut url = api_url(&["api", "models", owner, name, "tree", commit])?;
    url.query_pairs_mut().append_pair("recursive", "true");

    let mut entries = Vec::new();
    for _ in 0..MAX_TREE_PAGES {
        let response = client
            .get(url.clone())
            .headers(auth_headers(token)?)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("Cannot reach Hugging Face: {}", e))?;
        if !response.status().is_success() {
            return Err(status_error(response.status(), token.is_some()));
        }
        let next = response
            .headers()
            .get(LINK)
            .and_then(|value| value.to_str().ok())
            .and_then(next_link);
        let page: Vec<TreeEntry> = serde_json::from_slice(&read_limited(response).await?)
            .map_err(|e| format!("Unexpected Hugging Face file listing: {}", e))?;
        entries.extend(page.into_iter().filter(|entry| entry.kind == "file"));

        match next {
            Some(next) if next.scheme() == "https" && next.host_str() == Some(HF_HOST) => url = next,
            Some(_) => return Err("Unexpected pagination link from Hugging Face".into()),
            None => return Ok(entries),
        }
    }
    Err("Repository has too many files to list".into())
}

fn next_link(header: &str) -> Option<url::Url> {
    header.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        if !params.contains("rel=\"next\"") {
            return None;
        }
        let target = target.trim().strip_prefix('<')?.strip_suffix('>')?;
        url::Url::parse(target).ok()
    })
}

/// A file to download, with the digest Hugging Face publishes for it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    /// Path inside the repository.
    pub repo_path: String,
    /// Name the file gets inside the model directory (always a single component).
    pub name: String,
    pub size: u64,
    pub expected: Digest,
    /// Set when the file comes from another pinned repository (the standard preprocessor).
    pub origin: Option<FileOrigin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileOrigin {
    pub repo: String,
    pub commit: String,
}

/// Standard Parakeet TDT v3 preprocessor (128 mel bins). It does not depend on the model
/// weights and is byte-identical across onnx-asr exports; Meetily's built-in v3 ships the
/// same file. Used when a repository omits `nemo128.onnx`.
pub(crate) fn standard_preprocessor() -> RemoteFile {
    RemoteFile {
        repo_path: "nemo128.onnx".into(),
        name: "nemo128.onnx".into(),
        size: 139_764,
        expected: Digest::Sha256(
            "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f".into(),
        ),
        origin: Some(FileOrigin {
            repo: "istupakov/parakeet-tdt-0.6b-v3-onnx".into(),
            commit: "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce".into(),
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum Digest {
    /// SHA-256 of the content (Git LFS / Xet files).
    Sha256(String),
    /// Git blob SHA-1 (small files stored directly in git).
    GitSha1(String),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    /// Stable key used to request this variant again for the same commit.
    pub key: String,
    pub engine: ModelEngine,
    pub label: String,
    pub subdir: String,
    pub quantization: Option<Quantization>,
    pub files: Vec<RemoteFile>,
    pub total_bytes: u64,
    /// Why the variant cannot be installed, if it cannot.
    pub blocked_reason: Option<String>,
}

fn remote_file(entry: &TreeEntry) -> Result<RemoteFile, String> {
    let segments = safe_repo_path(&entry.path)?;
    let name = safe_component(segments.last().copied().unwrap_or_default())?.to_string();
    let expected = match (&entry.lfs, &entry.oid) {
        (Some(lfs), _) if is_hex(&lfs.oid, 64) => Digest::Sha256(lfs.oid.to_ascii_lowercase()),
        (None, Some(oid)) if is_hex(oid, 40) => Digest::GitSha1(oid.to_ascii_lowercase()),
        _ => return Err(format!("{} has no verifiable checksum", entry.path)),
    };
    let size = entry.lfs.as_ref().map(|lfs| lfs.size).unwrap_or(entry.size);
    Ok(RemoteFile { repo_path: entry.path.clone(), name, size, expected, origin: None })
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path)
}

/// Split GGUF shards (`-00001-of-00003.gguf`): llama-helper loads single files only.
fn is_split_gguf(lower: &str) -> bool {
    let stem = lower.trim_end_matches(".gguf");
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
    parts.len() == 4
        && parts[1] == "of"
        && [parts[0], parts[2]].iter().all(|p| p.len() == 5 && p.chars().all(|c| c.is_ascii_digit()))
}

/// GGUF files that can be chat models. Vision projectors are not models on their own.
fn summary_gguf(name: &str) -> Option<Option<String>> {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".gguf") || lower.contains("mmproj") {
        return None;
    }
    Some(is_split_gguf(&lower).then(|| "Split GGUF files are not supported: choose a single-file quantization".to_string()))
}

fn is_whisper_ggml(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".bin") && lower.contains("ggml") && !lower.contains("encoder.mlmodelc")
}

/// Find the variants our engines can run. Everything else in the repository is ignored.
pub(crate) fn detect_variants(entries: &[TreeEntry], subpath: Option<&str>) -> Vec<Variant> {
    let in_scope = |path: &str| match subpath {
        None => true,
        Some(prefix) => path == prefix || path.starts_with(&format!("{}/", prefix)),
    };
    let mut by_dir: BTreeMap<&str, Vec<&TreeEntry>> = BTreeMap::new();
    for entry in entries.iter().filter(|e| in_scope(&e.path)) {
        by_dir.entry(parent_dir(&entry.path)).or_default().push(entry);
    }

    // Shared files (vocabulary, preprocessor, config) may sit next to the graphs or in a parent
    // directory (e.g. `int8/encoder-model.int8.onnx` with `vocab.txt` at the repository root).
    // They are looked up in the whole listing, not only inside `subpath`.
    let find_shared = |dir: &str, name: &str| -> Option<&TreeEntry> {
        let mut current = dir;
        loop {
            let wanted = if current.is_empty() { name.to_string() } else { format!("{}/{}", current, name) };
            if let Some(entry) = entries.iter().find(|e| e.path == wanted) {
                return Some(entry);
            }
            if current.is_empty() {
                return None;
            }
            current = parent_dir(current);
        }
    };

    let mut variants = Vec::new();
    for (dir, files) in &by_dir {
        let find = |name: &str| files.iter().copied().find(|e| file_name(&e.path) == name);
        for (quantization, encoder, decoder) in [
            (Quantization::Int8, "encoder-model.int8.onnx", "decoder_joint-model.int8.onnx"),
            (Quantization::Fp32, "encoder-model.onnx", "decoder_joint-model.onnx"),
        ] {
            let (Some(encoder), Some(decoder)) = (find(encoder), find(decoder)) else {
                continue;
            };
            let mut chosen: Vec<&TreeEntry> = vec![encoder, decoder];
            if quantization == Quantization::Fp32 {
                // FP32 exports keep large weights in ONNX external-data files next to the graph
                // (possibly sharded); compressed `.zst` copies are not usable by the runtime.
                chosen.extend(files.iter().copied().filter(|e| {
                    let name = file_name(&e.path);
                    (name.starts_with("encoder-model.onnx.") || name.starts_with("decoder_joint-model.onnx."))
                        && !name.ends_with(".zst")
                }));
            }
            let vocab = find_shared(dir, "vocab.txt");
            chosen.extend(vocab);
            chosen.extend(find_shared(dir, "config.json"));
            let preprocessor = find_shared(dir, "nemo128.onnx");
            chosen.extend(preprocessor);

            let mut label = format!(
                "Parakeet ONNX {} · {}",
                quantization.as_str(),
                if dir.is_empty() { "/" } else { dir }
            );
            if preprocessor.is_none() {
                label.push_str(" · standard v3 preprocessor");
            }
            let mut variant = build_variant(
                format!("parakeet:{}:{}", dir, quantization.as_str()),
                ModelEngine::Parakeet,
                label,
                dir,
                Some(quantization),
                &chosen,
            );
            if preprocessor.is_none() {
                variant.total_bytes += standard_preprocessor().size;
                variant.files.push(standard_preprocessor());
            }
            if vocab.is_none() {
                variant.blocked_reason = Some("vocab.txt not found next to the model".into());
            }
            variants.push(variant);
        }

        for entry in files.iter().copied() {
            let Some(split_reason) = summary_gguf(file_name(&entry.path)) else {
                continue;
            };
            let mut variant = build_variant(
                format!("summary:{}", entry.path),
                ModelEngine::Summary,
                format!("GGUF · {}", entry.path),
                dir,
                None,
                &[entry],
            );
            // The chat format and architecture are checked from the file header at install.
            if variant.blocked_reason.is_none() {
                variant.blocked_reason = split_reason;
            }
            variants.push(variant);
        }

        for entry in files.iter().filter(|e| is_whisper_ggml(file_name(&e.path))) {
            variants.push(build_variant(
                format!("whisper:{}", entry.path),
                ModelEngine::Whisper,
                format!("Whisper ggml · {}", entry.path),
                dir,
                None,
                &[entry],
            ));
        }
    }
    variants
}

fn build_variant(
    key: String,
    engine: ModelEngine,
    label: String,
    subdir: &str,
    quantization: Option<Quantization>,
    entries: &[&TreeEntry],
) -> Variant {
    let mut blocked_reason = None;
    let mut files = Vec::new();
    for entry in entries {
        match remote_file(entry) {
            Ok(file) => files.push(file),
            Err(error) => blocked_reason = Some(error),
        }
    }
    let total_bytes = files.iter().map(|f| f.size).sum();
    if blocked_reason.is_none() && total_bytes > engine.max_bytes() {
        blocked_reason = Some(format!(
            "Model is {:.1} GB, above the {:.0} GB limit",
            total_bytes as f64 / 1e9,
            engine.max_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if files.iter().any(|f| !seen.insert(f.name.to_ascii_lowercase())) {
        blocked_reason = Some("Variant contains duplicate file names".into());
    }
    Variant {
        key,
        engine,
        label,
        subdir: subdir.to_string(),
        quantization,
        files,
        total_bytes,
        blocked_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lfs(path: &str, size: u64) -> TreeEntry {
        TreeEntry {
            kind: "file".into(),
            path: path.into(),
            size,
            oid: Some("a".repeat(40)),
            lfs: Some(LfsInfo { oid: "b".repeat(64), size }),
        }
    }

    fn git(path: &str, size: u64) -> TreeEntry {
        TreeEntry { kind: "file".into(), path: path.into(), size, oid: Some("c".repeat(40)), lfs: None }
    }

    #[test]
    fn parses_references() {
        let r = parse_reference("oruk/orukeet").unwrap();
        assert_eq!((r.repo.as_str(), r.revision, r.subpath), ("oruk/orukeet", None, None));

        let r = parse_reference("https://huggingface.co/oruk/orukeet/tree/main/onnx/combined-v0.1.0-int8").unwrap();
        assert_eq!(r.repo, "oruk/orukeet");
        assert_eq!(r.revision.as_deref(), Some("main"));
        assert_eq!(r.subpath.as_deref(), Some("onnx/combined-v0.1.0-int8"));

        let r = parse_reference("hf.co/ggerganov/whisper.cpp/blob/main/ggml-base.bin?download=true").unwrap();
        assert_eq!(r.subpath.as_deref(), Some("ggml-base.bin"));

        for bad in ["orukeet", "datasets/a/b", "https://huggingface.co/a/b/tree/main/../x", "a/b/commits/main", "a/../b", "https://huggingface.co/a/b/tree/../x"] {
            assert!(parse_reference(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn only_hugging_face_hosts_are_allowed() {
        for host in ["huggingface.co", "cdn-lfs.huggingface.co", "cas-bridge.xethub.hf.co", "hf.co"] {
            assert!(is_allowed_host(host), "{host}");
        }
        for host in ["evilhuggingface.co", "huggingface.co.evil.com", "example.com", "hf.com"] {
            assert!(!is_allowed_host(host), "{host}");
        }
    }

    #[test]
    fn detects_parakeet_layout_and_ignores_other_formats() {
        let dir = "onnx/combined-v0.1.0-int8";
        let entries = vec![
            lfs(&format!("{dir}/encoder-model.int8.onnx"), 653_182_378),
            lfs(&format!("{dir}/decoder_joint-model.int8.onnx"), 18_202_844),
            lfs(&format!("{dir}/nemo128.onnx"), 139_764),
            git(&format!("{dir}/vocab.txt"), 93_939),
            git(&format!("{dir}/config.json"), 97),
            lfs("onnx/sherpa-v0.1.0-int8/encoder.int8.onnx", 653_182_378),
            lfs("orukeet-v0.1.0-q8.gguf", 714_456_704),
            lfs("model.safetensors", 2_508_311_120),
            lfs("pytorch_model.bin", 10),
        ];
        let variants = detect_variants(&entries, None);
        assert_eq!(variants.len(), 2, "{variants:?}");
        // The GGUF is offered only as a summary candidate; its header is checked at install.
        assert!(variants.iter().any(|v| v.engine == ModelEngine::Summary && v.key == "summary:orukeet-v0.1.0-q8.gguf"));
        let v = variants.iter().find(|v| v.engine == ModelEngine::Parakeet).unwrap();
        assert_eq!(v.quantization, Some(Quantization::Int8));
        assert_eq!(v.files.len(), 5);
        assert!(v.blocked_reason.is_none());
        assert!(v.files.iter().any(|f| f.name == "vocab.txt" && matches!(f.expected, Digest::GitSha1(_))));
    }

    #[test]
    fn detects_fp32_with_external_data_and_whisper_files() {
        let entries = vec![
            lfs("encoder-model.onnx", 40_000_000),
            lfs("encoder-model.onnx.data", 2_400_000_000),
            lfs("decoder_joint-model.onnx", 70_000_000),
            lfs("encoder-model.int8.onnx", 650_000_000),
            lfs("decoder_joint-model.int8.onnx", 18_000_000),
            lfs("nemo128.onnx", 139_764),
            git("vocab.txt", 93_939),
            lfs("whisper/ggml-large-v3-it-q5_0.bin", 1_000_000_000),
        ];
        let variants = detect_variants(&entries, None);
        let fp32 = variants.iter().find(|v| v.quantization == Some(Quantization::Fp32)).unwrap();
        assert!(fp32.files.iter().any(|f| f.name == "encoder-model.onnx.data"));
        assert!(!fp32.files.iter().any(|f| f.name.contains("int8")));
        assert!(variants.iter().any(|v| v.quantization == Some(Quantization::Int8)));
        let whisper = variants.iter().find(|v| v.engine == ModelEngine::Whisper).unwrap();
        assert_eq!(whisper.files[0].name, "ggml-large-v3-it-q5_0.bin");
    }

    #[test]
    fn finds_shared_files_in_parent_and_falls_back_to_standard_preprocessor() {
        // Olicorne-style: graphs in int8/, vocab and preprocessor at the root, .zst duplicates.
        let entries = vec![
            lfs("int8/encoder-model.int8.onnx", 650_000_000),
            lfs("int8/encoder-model.int8.onnx.zst", 600_000_000),
            lfs("int8/decoder_joint-model.int8.onnx", 18_000_000),
            lfs("nemo128.onnx", 139_764),
            git("vocab.txt", 93_939),
            git("config.json", 97),
        ];
        let variants = detect_variants(&entries, Some("int8"));
        assert_eq!(variants.len(), 1);
        let names: Vec<&str> = variants[0].files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"vocab.txt") && names.contains(&"nemo128.onnx") && names.contains(&"config.json"));
        assert!(!names.iter().any(|n| n.ends_with(".zst")));
        assert!(variants[0].files.iter().all(|f| f.origin.is_none()));

        // Masterx-style: no preprocessor anywhere -> pinned standard one from another repo.
        let entries = vec![
            lfs("encoder-model.int8.onnx", 650_000_000),
            lfs("decoder_joint-model.int8.onnx", 18_000_000),
            lfs("encoder-model.fp16.onnx", 1_200_000_000),
            git("vocab.txt", 93_939),
        ];
        let variants = detect_variants(&entries, None);
        assert_eq!(variants.len(), 1, "fp16 must not be offered: {variants:?}");
        let pre = variants[0].files.iter().find(|f| f.name == "nemo128.onnx").unwrap();
        assert_eq!(pre.origin.as_ref().unwrap().repo, "istupakov/parakeet-tdt-0.6b-v3-onnx");
        assert!(variants[0].blocked_reason.is_none());
    }

    #[test]
    fn parakeet_without_vocab_is_blocked() {
        let entries = vec![lfs("encoder-model.int8.onnx", 1), lfs("decoder_joint-model.int8.onnx", 1)];
        assert!(detect_variants(&entries, None)[0].blocked_reason.is_some());
    }

    #[test]
    fn blocks_oversized_and_unverifiable_variants() {
        let mut no_digest = git("ggml-x.bin", 5);
        no_digest.oid = None;
        let variants = detect_variants(&[no_digest, lfs("ggml-big.bin", ModelEngine::Whisper.max_bytes() + 1)], None);
        assert!(variants.iter().all(|v| v.blocked_reason.is_some()));
    }

    #[test]
    fn subpath_limits_scope() {
        let entries = vec![lfs("a/ggml-a.bin", 10), lfs("b/ggml-b.bin", 10)];
        let variants = detect_variants(&entries, Some("b"));
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].files[0].repo_path, "b/ggml-b.bin");
    }

    #[test]
    fn parses_next_link() {
        let header = r#"<https://huggingface.co/api/models/a/b/tree/x?cursor=abc>; rel="next""#;
        assert_eq!(next_link(header).unwrap().query(), Some("cursor=abc"));
        assert!(next_link(r#"<https://x>; rel="prev""#).is_none());
    }
}

/// Live checks against huggingface.co. Run with:
/// `cargo test -p meetily --lib custom_models::hf::live -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::*;

    async fn variants_of(reference: &str) -> (RepoRevision, Vec<Variant>) {
        let reference = parse_reference(reference).unwrap();
        let client = http_client().unwrap();
        let revision = resolve_revision(&client, &reference.repo, reference.revision.as_deref(), None)
            .await
            .unwrap();
        let files = list_files(&client, &reference.repo, &revision.commit, None).await.unwrap();
        let variants = detect_variants(&files, reference.subpath.as_deref());
        for v in &variants {
            println!("{} | {} | {} files | {} bytes | blocked={:?}", reference.repo, v.label, v.files.len(), v.total_bytes, v.blocked_reason);
        }
        (revision, variants)
    }

    #[tokio::test]
    #[ignore]
    async fn orukeet_has_one_meetily_compatible_variant() {
        let (revision, variants) = variants_of("https://huggingface.co/oruk/orukeet").await;
        assert!(is_commit_sha(&revision.commit));
        let parakeet: Vec<_> = variants.iter().filter(|v| v.engine == ModelEngine::Parakeet).collect();
        assert_eq!(parakeet.len(), 1);
        assert_eq!(parakeet[0].subdir, "onnx/combined-v0.1.0-int8");
        assert!(parakeet[0].blocked_reason.is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn parakeet_ultra_conversions() {
        let (_, masterx) = variants_of("Masterx/parakeet-tdt-0.6b-ultra-onnx").await;
        assert!(masterx.iter().any(|v| v.quantization == Some(Quantization::Int8) && v.blocked_reason.is_none()));
        let (_, olicorne) = variants_of("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx").await;
        assert!(olicorne.iter().any(|v| v.subdir == "int8" && v.blocked_reason.is_none()));
        let (_, original) = variants_of("moondream/parakeet-ultra").await;
        assert!(original.is_empty(), "safetensors-only repo must not offer variants");
    }

    #[tokio::test]
    #[ignore]
    async fn whisper_repos() {
        let (_, ggml) = variants_of("ggerganov/whisper.cpp").await;
        assert!(ggml.iter().any(|v| v.engine == ModelEngine::Whisper && v.files[0].name == "ggml-base.bin"));
        let (_, transformers) = variants_of("openai/whisper-large-v3").await;
        assert!(transformers.is_empty(), "transformers repo must not offer variants");
    }
}
