//! Fork: NeMo Conformer CTC models (e.g. the Italian FastConformer), used as a "second opinion"
//! in the post-meeting re-transcription. Greedy CTC decoding, same output format as Parakeet.
//!
//! Model directory: `model.onnx` or `model.int8.onnx` (inputs `audio_signal` [B,80,T] + `length`,
//! output `logprobs` [B,T',V]), `vocab.txt` ("token id" per line, with `<blk>`), optional
//! `config.json` (`model_type` nemo-conformer-ctc, `features_size` 80).
//!
//! The 80-mel preprocessor `nemo80.onnx` is embedded: it comes from onnx-asr
//! (https://github.com/istupakov/onnx-asr, MIT License, Copyright (c) 2025 Ilya Stupakov),
//! version 0.12.0, sha256 7ab6b0d1f57832e56f59aa7cab4289de50df7f8d5296cdfd281d9c9b7b8d73e0.

use ndarray::{Array1, Array2, ArrayD, Axis, Ix3};
use once_cell::sync::Lazy;
use ort::execution_providers::CPUExecutionProvider;
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use regex::Regex;
use std::path::{Path, PathBuf};

const NEMO80: &[u8] = include_bytes!("nemo80.onnx");
const SUBSAMPLING: i64 = 8;

static DECODE_SPACE_RE: Lazy<Result<Regex, regex::Error>> = Lazy::new(|| Regex::new(r"\A\s|\s\B|(\s)\b"));

pub struct CtcModel {
    preprocessor: Session,
    model: Session,
    vocab: Vec<String>,
    blank: usize,
}

fn session_builder() -> Result<ort::session::builder::SessionBuilder, String> {
    crate::ensure_onnx_runtime_available().map_err(|e| e.to_string())?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4);
    Session::builder()
        .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
        .and_then(|b| b.with_execution_providers(vec![CPUExecutionProvider::default().build()]))
        .and_then(|b| b.with_intra_threads(threads))
        .and_then(|b| b.with_inter_threads(1))
        .map_err(|e| e.to_string())
}

/// The ONNX file to load: prefers the full-precision `model.onnx` (more accurate in benchmarks),
/// otherwise `model.int8.onnx`.
pub fn model_file(dir: &Path) -> Option<PathBuf> {
    ["model.onnx", "model.int8.onnx"].iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

/// Problems that make `dir` unusable as a CTC model, or None.
pub fn check_dir(dir: &Path) -> Option<String> {
    if model_file(dir).is_none() {
        return Some("model.onnx / model.int8.onnx mancante".into());
    }
    if !dir.join("vocab.txt").is_file() {
        return Some("vocab.txt mancante".into());
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("config.json")) {
        let config: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if let Some(kind) = config.get("model_type").and_then(|v| v.as_str()) {
            if !kind.contains("ctc") {
                return Some(format!("tipo {} non supportato (serve un modello CTC)", kind));
            }
        }
        if let Some(size) = config.get("features_size").and_then(|v| v.as_i64()) {
            if size != 80 {
                return Some(format!("{} mel non supportati (serve 80)", size));
            }
        }
    }
    None
}

impl CtcModel {
    pub fn load(dir: &Path) -> Result<Self, String> {
        if let Some(problem) = check_dir(dir) {
            return Err(problem);
        }
        let file = model_file(dir).ok_or("model file missing")?;
        let preprocessor = session_builder()?.commit_from_memory(NEMO80).map_err(|e| e.to_string())?;
        let model = session_builder()?.commit_from_file(&file).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(dir.join("vocab.txt")).map_err(|e| e.to_string())?;
        let mut pairs: Vec<(usize, String)> = Vec::new();
        for line in text.lines() {
            let Some((token, id)) = line.rsplit_once(' ') else { continue };
            let Ok(id) = id.trim().parse::<usize>() else { continue };
            pairs.push((id, token.replace('\u{2581}', " ")));
        }
        let size = pairs.iter().map(|(id, _)| id + 1).max().unwrap_or(0);
        if size == 0 {
            return Err("vocab.txt vuoto".into());
        }
        let mut vocab = vec![String::new(); size];
        let mut blank = None;
        for (id, token) in pairs {
            if token == "<blk>" {
                blank = Some(id);
            }
            vocab[id] = token;
        }
        // NeMo CTC: the blank is the last class (index = vocabulary size) when not listed.
        let blank = blank.unwrap_or(size);
        log::info!("CTC model loaded from {:?} ({} tokens)", file, size);
        Ok(Self { preprocessor, model, vocab, blank })
    }

    /// 16 kHz mono samples -> text.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String, String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let waveforms = Array2::from_shape_vec((1, samples.len()), samples.to_vec()).map_err(|e| e.to_string())?;
        let lens = Array1::from_vec(vec![samples.len() as i64]);
        let (features, features_lens) = {
            let outputs = self
                .preprocessor
                .run(inputs![
                    "waveforms" => TensorRef::from_array_view(waveforms.view()).map_err(|e| e.to_string())?,
                    "waveforms_lens" => TensorRef::from_array_view(lens.view()).map_err(|e| e.to_string())?,
                ])
                .map_err(|e| e.to_string())?;
            let f: ArrayD<f32> = outputs.get("features").ok_or("no features")?.try_extract_array::<f32>().map_err(|e| e.to_string())?.to_owned();
            let l: ArrayD<i64> = outputs.get("features_lens").ok_or("no features_lens")?.try_extract_array::<i64>().map_err(|e| e.to_string())?.to_owned();
            (f, l)
        };
        let feature_len = features_lens.iter().next().copied().unwrap_or(0);
        let logprobs: ArrayD<f32> = {
            let outputs = self
                .model
                .run(inputs![
                    "audio_signal" => TensorRef::from_array_view(features.view()).map_err(|e| e.to_string())?,
                    "length" => TensorRef::from_array_view(features_lens.view()).map_err(|e| e.to_string())?,
                ])
                .map_err(|e| e.to_string())?;
            outputs.get("logprobs").ok_or("no logprobs")?.try_extract_array::<f32>().map_err(|e| e.to_string())?.to_owned()
        };
        let logprobs = logprobs.into_dimensionality::<Ix3>().map_err(|e| e.to_string())?;
        let frames = logprobs.index_axis(Axis(0), 0);
        let valid = (((feature_len - 1) / SUBSAMPLING) + 1).max(0) as usize;
        let mut previous = self.blank;
        let mut text = String::new();
        for frame in frames.outer_iter().take(valid) {
            let best = frame
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(self.blank);
            if best != self.blank && best != previous {
                if let Some(token) = self.vocab.get(best) {
                    text.push_str(token);
                }
            }
            previous = best;
        }
        Ok(match &*DECODE_SPACE_RE {
            Ok(re) => re.replace_all(&text, |c: &regex::Captures| if c.get(1).is_some() { " " } else { "" }).to_string(),
            Err(_) => text.trim().to_string(),
        })
    }
}
