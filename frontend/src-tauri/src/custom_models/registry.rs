//! Persistence of custom models in SQLite (`custom_models`).

use super::{CustomModel, InstalledFile, ModelEngine, ModelSource, Quantization, SummaryInfo};
use sqlx::{Row, SqlitePool};

pub(crate) async fn list(pool: &SqlitePool) -> Result<Vec<CustomModel>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, display_name, engine, source, hf_repo, hf_revision, hf_subdir, quantization,
                files_json, local_dir, license, size_bytes, created_at,
                architecture, chat_template, context_size, layer_count
         FROM custom_models ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get("id")?;
        match decode(&row) {
            Ok(model) => models.push(model),
            // A malformed row must not hide the other models or break discovery.
            Err(error) => log::warn!("Skipping unreadable custom model {}: {}", id, error),
        }
    }
    Ok(models)
}

fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<CustomModel, String> {
    let get = |column: &str| -> Result<String, String> {
        row.try_get::<String, _>(column).map_err(|e| e.to_string())
    };
    let get_opt = |column: &str| -> Result<Option<String>, String> {
        row.try_get::<Option<String>, _>(column).map_err(|e| e.to_string())
    };

    let engine = ModelEngine::parse(&get("engine")?).ok_or("unknown engine")?;
    let source = match get("source")?.as_str() {
        "huggingface" => ModelSource::Huggingface,
        "local" => ModelSource::Local,
        other => return Err(format!("unknown source {other}")),
    };
    let quantization = match get_opt("quantization")?.as_deref() {
        None => None,
        Some("int8") => Some(Quantization::Int8),
        Some("fp32") => Some(Quantization::Fp32),
        Some(other) => return Err(format!("unknown quantization {other}")),
    };
    let files: Vec<InstalledFile> =
        serde_json::from_str(&get("files_json")?).map_err(|e| e.to_string())?;
    let size_bytes: i64 = row.try_get("size_bytes").map_err(|e| e.to_string())?;
    let summary = match engine {
        ModelEngine::Summary => {
            let number = |column: &str| -> Result<u32, String> {
                let value: Option<i64> = row.try_get(column).map_err(|e| e.to_string())?;
                value
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| *v > 0)
                    .ok_or_else(|| format!("invalid {column}"))
            };
            Some(SummaryInfo {
                architecture: get_opt("architecture")?.unwrap_or_default(),
                chat_template: get_opt("chat_template")?.ok_or("missing chat_template")?,
                context_size: number("context_size")?,
                layer_count: number("layer_count")?,
            })
        }
        _ => None,
    };

    Ok(CustomModel {
        id: get("id")?,
        display_name: get("display_name")?,
        engine,
        source,
        hf_repo: get_opt("hf_repo")?,
        hf_revision: get_opt("hf_revision")?,
        hf_subdir: get_opt("hf_subdir")?,
        quantization,
        files,
        local_dir: get("local_dir")?,
        license: get_opt("license")?,
        size_bytes: size_bytes.max(0) as u64,
        created_at: get("created_at")?,
        summary,
    })
}

pub(crate) async fn id_exists(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM custom_models WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

pub(crate) async fn insert(pool: &SqlitePool, model: &CustomModel) -> Result<(), String> {
    let files_json = serde_json::to_string(&model.files).map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO custom_models
            (id, display_name, engine, source, hf_repo, hf_revision, hf_subdir, quantization,
             files_json, local_dir, license, size_bytes, created_at,
             architecture, chat_template, context_size, layer_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
    )
    .bind(&model.id)
    .bind(&model.display_name)
    .bind(model.engine.as_str())
    .bind(model.source.as_str())
    .bind(&model.hf_repo)
    .bind(&model.hf_revision)
    .bind(&model.hf_subdir)
    .bind(model.quantization.map(Quantization::as_str))
    .bind(files_json)
    .bind(&model.local_dir)
    .bind(&model.license)
    .bind(i64::try_from(model.size_bytes).map_err(|e| e.to_string())?)
    .bind(&model.created_at)
    .bind(model.summary.as_ref().map(|s| s.architecture.clone()))
    .bind(model.summary.as_ref().map(|s| s.chat_template.clone()))
    .bind(model.summary.as_ref().map(|s| s.context_size as i64))
    .bind(model.summary.as_ref().map(|s| s.layer_count as i64))
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM custom_models WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
