-- Fork: user-added models (Hugging Face or local import) for transcription (Parakeet, Whisper)
-- and for built-in AI summaries (GGUF run by llama-helper).
-- A row exists only after the files were downloaded/copied, verified and probed.
CREATE TABLE IF NOT EXISTS custom_models (
    id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    engine TEXT NOT NULL CHECK (engine IN ('parakeet', 'whisper', 'summary')),
    source TEXT NOT NULL CHECK (source IN ('huggingface', 'local')),
    hf_repo TEXT,
    hf_revision TEXT,
    hf_subdir TEXT,
    quantization TEXT CHECK (quantization IS NULL OR quantization IN ('int8', 'fp32')),
    files_json TEXT NOT NULL,
    local_dir TEXT NOT NULL,
    license TEXT,
    size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    -- Summary models only: values read from the GGUF metadata at install time.
    architecture TEXT,
    chat_template TEXT,
    context_size INTEGER,
    layer_count INTEGER,
    CHECK (engine <> 'summary' OR (chat_template IS NOT NULL AND context_size > 0 AND layer_count > 0))
);
