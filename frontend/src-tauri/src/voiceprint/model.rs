//! Fork: WeSpeaker ONNX session. Input `feats` [1, frames, 80], output `embs` [1, 256].

use super::{cluster, fbank};
use ndarray::Array3;
use ort::execution_providers::CPUExecutionProvider;
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use std::path::Path;

/// Shorter audio gives unreliable embeddings.
pub const MIN_SECONDS: f64 = 1.2;
/// Longer segments are cut around their middle (cost and memory).
const MAX_SAMPLES: usize = 16_000 * 20;

pub struct SpeakerModel {
    session: Session,
}

impl SpeakerModel {
    pub fn load(path: &Path) -> Result<Self, String> {
        crate::ensure_onnx_runtime_available().map_err(|e| e.to_string())?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4);
        let session = Session::builder()
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
            .and_then(|b| b.with_execution_providers(vec![CPUExecutionProvider::default().build()]))
            .and_then(|b| b.with_intra_threads(threads))
            .and_then(|b| b.with_inter_threads(1))
            .and_then(|b| b.commit_from_file(path))
            .map_err(|e| e.to_string())?;
        Ok(Self { session })
    }

    /// Unit-length embedding of 16 kHz mono samples; None when too short.
    pub fn embed(&mut self, samples: &[f32]) -> Result<Option<Vec<f32>>, String> {
        let samples = if samples.len() > MAX_SAMPLES {
            let start = (samples.len() - MAX_SAMPLES) / 2;
            &samples[start..start + MAX_SAMPLES]
        } else {
            samples
        };
        let features = fbank::compute(samples);
        if features.len() < (MIN_SECONDS * 100.0) as usize {
            return Ok(None);
        }
        let flat: Vec<f32> = features.iter().flat_map(|row| row.iter().copied()).collect();
        let input = Array3::from_shape_vec((1, features.len(), fbank::MEL_BINS), flat).map_err(|e| e.to_string())?;
        let outputs = self
            .session
            .run(inputs!["feats" => TensorRef::from_array_view(input.view()).map_err(|e| e.to_string())?])
            .map_err(|e| e.to_string())?;
        let embedding = outputs.get("embs").ok_or("no embs output")?.try_extract_array::<f32>().map_err(|e| e.to_string())?;
        let mut vector: Vec<f32> = embedding.iter().copied().collect();
        if vector.is_empty() || vector.iter().any(|v| !v.is_finite()) {
            return Ok(None);
        }
        cluster::normalize(&mut vector);
        Ok(Some(vector))
    }
}
