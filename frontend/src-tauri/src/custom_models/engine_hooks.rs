//! Adapters that turn registered custom models into each engine's `ModelInfo`, so the
//! engines only need a one-line hook in `discover_models`.

use super::{resolved_models, ModelEngine, ResolvedModel};

fn corrupted_sizes(model: &ResolvedModel) -> (u64, u64) {
    (model.size_on_disk, model.size_mb as u64 * 1024 * 1024)
}

pub async fn parakeet_models() -> Vec<crate::parakeet_engine::parakeet_engine::ModelInfo> {
    use crate::parakeet_engine::parakeet_engine::{ModelInfo, ModelStatus, QuantizationType};
    resolved_models(ModelEngine::Parakeet)
        .await
        .into_iter()
        .map(|model| {
            let status = match &model.integrity {
                Ok(()) => ModelStatus::Available,
                Err(problem) => {
                    log::warn!("Custom Parakeet model {} failed its file check: {}", model.id, problem);
                    let (file_size, expected_min_size) = corrupted_sizes(&model);
                    ModelStatus::Corrupted { file_size, expected_min_size }
                }
            };
            ModelInfo {
                name: model.id,
                path: model.path,
                size_mb: model.size_mb,
                quantization: if model.quantized { QuantizationType::Int8 } else { QuantizationType::FP32 },
                speed: "Custom".to_string(),
                status,
                description: model.description,
            }
        })
        .collect()
}

pub async fn whisper_models() -> Vec<crate::whisper_engine::whisper_engine::ModelInfo> {
    use crate::whisper_engine::whisper_engine::{ModelInfo, ModelStatus};
    resolved_models(ModelEngine::Whisper)
        .await
        .into_iter()
        .map(|model| {
            let status = match &model.integrity {
                Ok(()) => ModelStatus::Available,
                Err(problem) => {
                    log::warn!("Custom Whisper model {} failed its file check: {}", model.id, problem);
                    let (file_size, expected_min_size) = corrupted_sizes(&model);
                    ModelStatus::Corrupted { file_size, expected_min_size }
                }
            };
            ModelInfo {
                name: model.id,
                path: model.path,
                size_mb: model.size_mb,
                accuracy: "Custom".to_string(),
                speed: "Custom".to_string(),
                status,
                description: model.description,
            }
        })
        .collect()
}

/// Error for engine operations that must go through the custom models commands instead.
pub fn managed_elsewhere(model_name: &str) -> Option<anyhow::Error> {
    super::is_custom_id(model_name).then(|| {
        anyhow::anyhow!("'{}' is a custom model: manage it from the custom models section", model_name)
    })
}
