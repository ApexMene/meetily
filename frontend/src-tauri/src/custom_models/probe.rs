//! Compatibility probe run before a model is registered: static checks of the files, then a
//! real load with the engine that will use it (one second of silence for transcription, a few
//! generated tokens for summaries).

use super::{ModelEngine, Quantization, SummaryInfo};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(180);
/// Loading a multi-GB GGUF from a slow disk takes longer than a transcription model.
const SUMMARY_PROBE_TIMEOUT: Duration = Duration::from_secs(420);
const PROBE_SAMPLES: usize = 16_000; // 1 s at 16 kHz mono, the format both engines receive

/// Values the Parakeet engine is built for (see `parakeet_engine::model`).
const PARAKEET_FEATURES: u64 = 128;
const PARAKEET_SUBSAMPLING: u64 = 8;

/// Returns the summary metadata for summary models (`None` for transcription models).
/// `model_file` is the single model file (Whisper `.bin`, summary `.gguf`).
pub(crate) async fn probe(
    engine: ModelEngine,
    model_dir: &Path,
    quantization: Option<Quantization>,
    model_file: Option<&str>,
) -> Result<Option<SummaryInfo>, String> {
    match engine {
        ModelEngine::Parakeet => {
            let quantized = quantization == Some(Quantization::Int8);
            check_parakeet_files(model_dir, quantized)?;
            let dir = model_dir.to_path_buf();
            run_blocking(move || probe_parakeet(&dir, quantized)).await.map(|()| None)
        }
        ModelEngine::Whisper => {
            let name = model_file.ok_or("missing Whisper model file")?;
            let path = model_dir.join(name);
            check_ggml_header(&path)?;
            run_blocking(move || probe_whisper(path)).await.map(|()| None)
        }
        ModelEngine::Summary => {
            let name = model_file.ok_or("missing GGUF model file")?;
            probe_summary(model_dir.join(name)).await.map(Some)
        }
    }
}

/// Header check, then a real load and a 4-token generation in a dedicated llama-helper
/// process, so a summary running in the app's own sidecar is not disturbed.
async fn probe_summary(path: PathBuf) -> Result<SummaryInfo, String> {
    use crate::summary::summary_engine::{models, sidecar::SidecarManager};

    let header_path = path.clone();
    let metadata = tokio::task::spawn_blocking(move || super::gguf::read_file_metadata(&header_path))
        .await
        .map_err(|e| format!("model check crashed: {}", e))??;
    let template = metadata.format.template_name();
    let prompt = models::format_prompt(template, "Answer with one word.", "Say OK.")
        .map_err(|e| e.to_string())?;
    let request = serde_json::json!({
        "type": "generate",
        "prompt": prompt,
        "max_tokens": 4,
        "context_size": 2048,
        "model_path": path.to_string_lossy(),
        "temperature": 0.0,
    })
    .to_string();

    let manager = SidecarManager::new(PathBuf::new()).map_err(|e| format!("llama-helper unavailable: {}", e))?;
    let outcome = tokio::time::timeout(SUMMARY_PROBE_TIMEOUT, async {
        manager.ensure_running(path.clone()).await.map_err(|e| e.to_string())?;
        manager.send_request(request, SUMMARY_PROBE_TIMEOUT).await.map_err(|e| e.to_string())
    })
    .await;
    if let Err(error) = manager.shutdown().await {
        log::warn!("Probe sidecar shutdown failed: {}", error);
    }
    let response = match outcome {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(format!("llama.cpp cannot run this model: {}", error)),
        Err(_) => return Err("model check timed out".into()),
    };
    let response: serde_json::Value =
        serde_json::from_str(&response).map_err(|_| "unexpected llama-helper response".to_string())?;
    match (response.get("type").and_then(|t| t.as_str()), response.get("error").and_then(|e| e.as_str())) {
        (Some("response"), None) => Ok(SummaryInfo {
            architecture: metadata.architecture,
            chat_template: template.to_string(),
            context_size: metadata.context_size,
            layer_count: metadata.layer_count,
        }),
        (_, Some(error)) => Err(format!("llama.cpp cannot run this model: {}", error)),
        _ => Err(format!(
            "llama.cpp cannot run this model: {}",
            response.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error")
        )),
    }
}

async fn run_blocking<F>(job: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::task::spawn_blocking(job)).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(format!("model check crashed: {}", join_error)),
        Err(_) => Err("model check timed out".into()),
    }
}

pub(crate) fn parakeet_graph_names(quantized: bool) -> [&'static str; 2] {
    if quantized {
        ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx"]
    } else {
        ["encoder-model.onnx", "decoder_joint-model.onnx"]
    }
}

fn check_parakeet_files(dir: &Path, quantized: bool) -> Result<(), String> {
    for name in parakeet_graph_names(quantized).into_iter().chain(["nemo128.onnx", "vocab.txt"]) {
        if !dir.join(name).is_file() {
            return Err(format!("{} is missing", name));
        }
    }

    let vocab = std::fs::read_to_string(dir.join("vocab.txt"))
        .map_err(|e| format!("vocab.txt is not readable text: {}", e))?;
    let mut has_blank = false;
    for (index, line) in vocab.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let (token, id) = line
            .rsplit_once(' ')
            .ok_or_else(|| format!("vocab.txt line {} is not `token id`", index + 1))?;
        id.trim()
            .parse::<u32>()
            .map_err(|_| format!("vocab.txt line {} has an invalid id", index + 1))?;
        has_blank |= token == "<blk>";
    }
    if !has_blank {
        return Err("vocab.txt has no <blk> token: not a TDT/RNNT Parakeet vocabulary".into());
    }

    let config_path = dir.join("config.json");
    if config_path.is_file() {
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&config_path).map_err(|e| format!("config.json: {}", e))?,
        )
        .map_err(|e| format!("config.json is not valid JSON: {}", e))?;
        if let Some(model_type) = config.get("model_type").and_then(|v| v.as_str()) {
            if !model_type.contains("tdt") {
                return Err(format!("unsupported model_type {:?} (expected a NeMo TDT model)", model_type));
            }
        }
        for (key, expected) in [("features_size", PARAKEET_FEATURES), ("subsampling_factor", PARAKEET_SUBSAMPLING)] {
            if let Some(value) = config.get(key).and_then(|v| v.as_u64()) {
                if value != expected {
                    return Err(format!("unsupported {} = {} (engine expects {})", key, value, expected));
                }
            }
        }
    }
    Ok(())
}

fn probe_parakeet(dir: &PathBuf, quantized: bool) -> Result<(), String> {
    let mut model = crate::parakeet_engine::model::ParakeetModel::new(dir, quantized)
        .map_err(|e| format!("the Parakeet engine cannot load this model: {}", e))?;
    model
        .transcribe_samples(vec![0.0; PROBE_SAMPLES])
        .map_err(|e| format!("test transcription failed (incompatible graph?): {}", e))?;
    Ok(())
}

fn check_ggml_header(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut header = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| format!("cannot read model file: {}", e))?;
    // whisper.cpp model files start with the ggml magic (0x67676d6c, little-endian on disk).
    if &header == b"lmgg" || &header == b"ggml" {
        Ok(())
    } else {
        Err("not a whisper.cpp ggml model (wrong file header)".into())
    }
}

fn probe_whisper(path: PathBuf) -> Result<(), String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
    let params = WhisperContextParameters { use_gpu: false, ..Default::default() };
    let context = WhisperContext::new_with_params(&path.to_string_lossy(), params)
        .map_err(|e| format!("whisper.cpp cannot load this model: {}", e))?;
    let mut state = context
        .create_state()
        .map_err(|e| format!("whisper.cpp cannot create a state: {}", e))?;
    let mut full = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    full.set_print_progress(false);
    full.set_print_realtime(false);
    full.set_print_special(false);
    full.set_print_timestamps(false);
    state
        .full(full, &vec![0.0f32; PROBE_SAMPLES])
        .map_err(|e| format!("test transcription failed: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parakeet_dir(vocab: &str, config: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx", "nemo128.onnx"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::write(dir.path().join("vocab.txt"), vocab).unwrap();
        if let Some(config) = config {
            std::fs::write(dir.path().join("config.json"), config).unwrap();
        }
        dir
    }

    #[test]
    fn accepts_tdt_layout() {
        let dir = parakeet_dir(
            "<unk> 0\n▁the 1\n<blk> 2\n",
            Some(r#"{"model_type":"nemo-conformer-tdt","features_size":128,"subsampling_factor":8}"#),
        );
        assert!(check_parakeet_files(dir.path(), true).is_ok());
    }

    #[test]
    fn rejects_wrong_vocab_or_config() {
        assert!(check_parakeet_files(parakeet_dir("<unk> 0\n", None).path(), true).is_err());
        assert!(check_parakeet_files(parakeet_dir("<unk>\n<blk> 1\n", None).path(), true).is_err());
        let ctc = parakeet_dir("<blk> 0\n", Some(r#"{"model_type":"nemo-conformer-ctc"}"#));
        assert!(check_parakeet_files(ctc.path(), true).is_err());
        let mel80 = parakeet_dir("<blk> 0\n", Some(r#"{"features_size":80}"#));
        assert!(check_parakeet_files(mel80.path(), true).is_err());
    }

    #[test]
    fn requires_graphs_for_selected_precision() {
        let dir = parakeet_dir("<blk> 0\n", None);
        assert!(check_parakeet_files(dir.path(), false).is_err());
    }

    #[test]
    fn ggml_header_check() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.bin");
        std::fs::write(&good, b"lmggrest").unwrap();
        assert!(check_ggml_header(&good).is_ok());
        let pickle = dir.path().join("pytorch_model.bin");
        std::fs::write(&pickle, b"PK\x03\x04rest").unwrap();
        assert!(check_ggml_header(&pickle).is_err());
    }
}

/// End-to-end check against huggingface.co: download a real model with digest verification,
/// then probe it with the real engine. Downloads ~670 MB into the system temp dir (kept, so
/// re-runs resume or skip). Run with:
/// `cargo test -p meetily --lib custom_models::probe::live -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::super::{download, hf};
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn init_onnx_runtime() {
        #[cfg(windows)]
        {
            let dll = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries/onnxruntime/onnxruntime.dll");
            let _ = ort::init_from(dll.to_string_lossy().into_owned()).with_telemetry(false).commit();
        }
    }

    #[tokio::test]
    #[ignore]
    async fn orukeet_downloads_verifies_and_probes() {
        init_onnx_runtime();
        let repo = "oruk/orukeet";
        let client = hf::http_client().unwrap();
        let revision = hf::resolve_revision(&client, repo, Some("e16be4db62032ac445bd3a20b1a360eeb69f21e5"), None)
            .await
            .unwrap();
        let files = hf::list_files(&client, repo, &revision.commit, None).await.unwrap();
        let variant = hf::detect_variants(&files, None)
            .into_iter()
            .find(|v| v.engine == ModelEngine::Parakeet)
            .unwrap();

        let dir = std::env::temp_dir().join("meetily-live-orukeet");
        std::fs::create_dir_all(&dir).unwrap();
        let missing: Vec<_> = variant
            .files
            .iter()
            .filter(|f| std::fs::metadata(dir.join(&f.name)).map(|m| m.len()).ok() != Some(f.size))
            .cloned()
            .collect();
        let started = std::time::Instant::now();
        let last = std::sync::Mutex::new(0u8);
        let installed = download::download_files(
            &client, repo, &revision.commit, &missing, &dir, None, &CancellationToken::new(),
            &|done, total, file| {
                let pct = (done * 100 / total.max(1)) as u8;
                let mut last = last.lock().unwrap();
                if pct >= *last + 10 { *last = pct; println!("  {pct}% ({file})"); }
            },
        )
        .await
        .unwrap();
        println!("downloaded and verified {} files in {:?}", installed.len(), started.elapsed());

        let started = std::time::Instant::now();
        probe(ModelEngine::Parakeet, &dir, Some(Quantization::Int8), None).await.unwrap();
        println!("Parakeet probe passed in {:?}", started.elapsed());

        // The same folder must not pass as a Whisper model.
        assert!(probe(ModelEngine::Whisper, &dir, None, Some("vocab.txt")).await.is_err());
        // Nor as FP32 (graphs for that precision are absent).
        assert!(probe(ModelEngine::Parakeet, &dir, Some(Quantization::Fp32), None).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn whisper_tiny_downloads_verifies_and_probes() {
        let repo = "ggerganov/whisper.cpp";
        let client = hf::http_client().unwrap();
        let revision = hf::resolve_revision(&client, repo, None, None).await.unwrap();
        let files = hf::list_files(&client, repo, &revision.commit, None).await.unwrap();
        let variant = hf::detect_variants(&files, Some("ggml-tiny.bin")).into_iter().next().unwrap();
        let dir = std::env::temp_dir().join("meetily-live-whisper-tiny");
        std::fs::create_dir_all(&dir).unwrap();
        let file = &variant.files[0];
        if std::fs::metadata(dir.join(&file.name)).map(|m| m.len()).ok() != Some(file.size) {
            download::download_files(
                &client, repo, &revision.commit, &variant.files, &dir, None, &CancellationToken::new(), &|_, _, _| {},
            )
            .await
            .unwrap();
        }
        probe(ModelEngine::Whisper, &dir, None, Some(&file.name)).await.unwrap();
        println!("Whisper probe passed for {}@{}", repo, &revision.commit[..7]);
    }

    /// Smallest GGUF of a tiny ChatML instruct model (~140 MB): remote header check, verified
    /// download, then a real generation through llama-helper (the bundled sidecar binary).
    #[tokio::test]
    #[ignore]
    async fn smollm2_gguf_header_download_and_probe() {
        let repo = "bartowski/SmolLM2-135M-Instruct-GGUF";
        let client = hf::http_client().unwrap();
        let revision = hf::resolve_revision(&client, repo, None, None).await.unwrap();
        let files = hf::list_files(&client, repo, &revision.commit, None).await.unwrap();
        let variant = hf::detect_variants(&files, None)
            .into_iter()
            .filter(|v| v.engine == ModelEngine::Summary && v.blocked_reason.is_none())
            .min_by_key(|v| v.total_bytes)
            .expect("no usable GGUF");
        let file = &variant.files[0];
        let remote = super::super::gguf::read_remote_metadata(&client, repo, &revision.commit, file, None)
            .await
            .unwrap();
        assert_eq!(remote.format.template_name(), "fork-chatml");

        // Same layout as an installed model: <root>/summary/<id>/<file>.
        let root = std::env::temp_dir().join("meetily-live-smollm2");
        let id = "custom-smollm2-live";
        let dir = root.join("summary").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let result = async {
            download::download_files(
                &client, repo, &revision.commit, &variant.files, &dir, None, &CancellationToken::new(), &|_, _, _| {},
            )
            .await
            .map_err(|e| e.to_string())?;
            let started = std::time::Instant::now();
            let info = probe(ModelEngine::Summary, &dir, None, Some(&file.name))
                .await?
                .ok_or("missing summary metadata")?;
            println!("{} probe passed in {:?}: {:?}", file.name, started.elapsed(), info);

            // Register it and summarize through the app's summary engine (catalog lookup,
            // fork prompt format, sampling, shared llama-helper sidecar).
            super::super::STATE.send_replace(Some(std::sync::Arc::new(super::super::RegistryState {
                root: Some(root.clone()),
                models: vec![super::super::CustomModel {
                    id: id.into(),
                    display_name: id.into(),
                    engine: ModelEngine::Summary,
                    source: super::super::ModelSource::Local,
                    hf_repo: None,
                    hf_revision: None,
                    hf_subdir: None,
                    quantization: None,
                    files: vec![super::super::InstalledFile { path: file.name.clone(), size: file.size, sha256: String::new() }],
                    local_dir: format!("summary/{}", id),
                    license: None,
                    size_bytes: file.size,
                    created_at: String::new(),
                    summary: Some(info.clone()),
                }],
            })));
            let started = std::time::Instant::now();
            let generated = crate::summary::summary_engine::client::generate_with_builtin(
                &PathBuf::from("unused"),
                id,
                "Summarize the meeting transcript in one short sentence.",
                "Alice: the release moves to Friday. Bob: I will update the changelog.",
                None,
            )
            .await;
            let _ = crate::summary::summary_engine::client::shutdown_sidecar_gracefully().await;
            let generated = generated.map_err(|e| e.to_string())?;
            println!("summary generated in {:?}: {:?}", started.elapsed(), generated);
            Ok::<_, String>((info, generated))
        }
        .await;
        let _ = std::fs::remove_dir_all(&root);
        let (info, generated) = result.unwrap();
        assert_eq!(info.chat_template, "fork-chatml");
        assert!(!generated.trim().is_empty(), "empty summary");
        assert!(!generated.contains("<|im_start|>"), "chat markers leaked into the summary");
    }
}
