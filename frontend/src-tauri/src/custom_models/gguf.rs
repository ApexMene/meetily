//! GGUF header reader for custom summary models.
//!
//! Only the metadata key/value section is parsed (never tensor data), with hard limits on every
//! length, so a malformed or hostile file fails cleanly. From it we take the architecture, the
//! chat template (to pick a prompt format llama-helper can be driven with), the layer count
//! (GPU offload) and the trained context length.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// Header bytes read before giving up. Vocabulary arrays of large tokenizers take a few MB.
pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024 * 1024;
/// First read; enough for most models, the reader grows to [`MAX_HEADER_BYTES`] if needed.
pub(crate) const INITIAL_HEADER_BYTES: usize = 8 * 1024 * 1024;

const MAX_KV_COUNT: u64 = 100_000;
const MAX_STRING_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARRAY_LEN: u64 = 50_000_000;
const MAX_ARRAY_DEPTH: u32 = 4;

/// Context used for summaries: the model's trained length, capped like the built-in models.
pub(crate) const MAX_SUMMARY_CONTEXT: u32 = 32_768;
const MIN_SUMMARY_CONTEXT: u32 = 2_048;

/// Prompt formats the summary engine can produce. The first two are upstream's own templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatFormat {
    Gemma,
    QwenThinking,
    ChatMl,
    Llama3,
    Mistral,
}

impl ChatFormat {
    /// Template name stored in the registry and passed to `models::format_prompt`.
    pub(crate) fn template_name(self) -> &'static str {
        match self {
            ChatFormat::Gemma => "gemma3",
            ChatFormat::QwenThinking => "qwen3.5_nonthinking",
            ChatFormat::ChatMl => "fork-chatml",
            ChatFormat::Llama3 => "fork-llama3",
            ChatFormat::Mistral => "fork-mistral",
        }
    }

    /// Recognize the format from the Jinja chat template embedded in the GGUF.
    pub(crate) fn detect(chat_template: &str) -> Option<Self> {
        if chat_template.contains("<start_of_turn>") {
            Some(ChatFormat::Gemma)
        } else if chat_template.contains("<|im_start|>") {
            // Thinking models (Qwen 3 family) get an empty think block, like upstream's Qwen 3.5.
            if chat_template.contains("<think>") {
                Some(ChatFormat::QwenThinking)
            } else {
                Some(ChatFormat::ChatMl)
            }
        } else if chat_template.contains("<|start_header_id|>") {
            Some(ChatFormat::Llama3)
        } else if chat_template.contains("[INST]") {
            Some(ChatFormat::Mistral)
        } else {
            None
        }
    }
}

/// What the summary engine needs from a GGUF file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummaryMetadata {
    pub architecture: String,
    pub format: ChatFormat,
    pub context_size: u32,
    pub layer_count: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// More bytes are needed to finish the metadata section.
    Incomplete,
    Invalid(String),
}

impl From<String> for HeaderError {
    fn from(message: String) -> Self {
        HeaderError::Invalid(message)
    }
}

#[derive(Debug, Clone)]
enum Value {
    Int(u64),
    Text(String),
    Other,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: u64) -> Result<&'a [u8], HeaderError> {
        let len = usize::try_from(len).map_err(|_| HeaderError::Invalid("length overflow".into()))?;
        let end = self.pos.checked_add(len).ok_or_else(|| HeaderError::Invalid("length overflow".into()))?;
        if end > self.bytes.len() {
            return Err(HeaderError::Incomplete);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, HeaderError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64, HeaderError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn string(&mut self) -> Result<String, HeaderError> {
        let len = self.u64()?;
        if len > MAX_STRING_BYTES {
            return Err(HeaderError::Invalid(format!("string of {} bytes in GGUF header", len)));
        }
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn skip_string(&mut self) -> Result<(), HeaderError> {
        let len = self.u64()?;
        if len > MAX_STRING_BYTES {
            return Err(HeaderError::Invalid(format!("string of {} bytes in GGUF header", len)));
        }
        self.take(len).map(|_| ())
    }

    /// Read a value of GGUF type `kind`. Only integers and strings are kept.
    fn value(&mut self, kind: u32, depth: u32) -> Result<Value, HeaderError> {
        Ok(match kind {
            0 | 1 | 7 => Value::Int(self.take(1)?[0] as u64), // u8, i8, bool
            2 | 3 => Value::Int(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")) as u64),
            4 => Value::Int(self.u32()? as u64),
            5 => {
                let value = self.u32()? as i32;
                if value < 0 { Value::Other } else { Value::Int(value as u64) }
            }
            6 => {
                self.take(4)?;
                Value::Other
            }
            8 => Value::Text(self.string()?),
            9 => {
                if depth >= MAX_ARRAY_DEPTH {
                    return Err(HeaderError::Invalid("GGUF arrays nested too deeply".into()));
                }
                let item_kind = self.u32()?;
                let len = self.u64()?;
                if len > MAX_ARRAY_LEN {
                    return Err(HeaderError::Invalid(format!("array of {} items in GGUF header", len)));
                }
                match fixed_size(item_kind) {
                    Some(size) => {
                        self.take(len.checked_mul(size).ok_or_else(|| HeaderError::Invalid("array overflow".into()))?)?;
                    }
                    None if item_kind == 8 => {
                        for _ in 0..len {
                            self.skip_string()?;
                        }
                    }
                    None => {
                        for _ in 0..len {
                            self.value(item_kind, depth + 1)?;
                        }
                    }
                }
                Value::Other
            }
            10 => Value::Int(self.u64()?),
            11 => {
                let value = self.u64()? as i64;
                if value < 0 { Value::Other } else { Value::Int(value as u64) }
            }
            12 => {
                self.take(8)?;
                Value::Other
            }
            other => return Err(HeaderError::Invalid(format!("unknown GGUF value type {}", other))),
        })
    }
}

fn fixed_size(kind: u32) -> Option<u64> {
    match kind {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4 | 5 | 6 => Some(4),
        10 | 11 | 12 => Some(8),
        _ => None,
    }
}

/// Parse the metadata of a GGUF file from its first bytes.
pub(crate) fn parse_metadata(bytes: &[u8]) -> Result<SummaryMetadata, HeaderError> {
    let mut cursor = Cursor { bytes, pos: 0 };
    if cursor.take(4)? != b"GGUF" {
        return Err(HeaderError::Invalid("not a GGUF file".into()));
    }
    let version = cursor.u32()?;
    if !(2..=3).contains(&version) {
        return Err(HeaderError::Invalid(format!("unsupported GGUF version {}", version)));
    }
    let _tensor_count = cursor.u64()?;
    let kv_count = cursor.u64()?;
    if kv_count > MAX_KV_COUNT {
        return Err(HeaderError::Invalid(format!("{} metadata keys in GGUF header", kv_count)));
    }

    let mut values: HashMap<String, Value> = HashMap::new();
    for _ in 0..kv_count {
        let key = cursor.string()?;
        let kind = cursor.u32()?;
        let value = cursor.value(kind, 0)?;
        if key == "general.architecture"
            || key == "tokenizer.chat_template"
            || key.ends_with(".block_count")
            || key.ends_with(".context_length")
        {
            values.insert(key, value);
        }
    }

    let text = |key: &str| match values.get(key) {
        Some(Value::Text(value)) => Some(value.clone()),
        _ => None,
    };
    let int = |key: &str| match values.get(key) {
        Some(Value::Int(value)) => Some(*value),
        _ => None,
    };

    let architecture = text("general.architecture")
        .filter(|arch| !arch.is_empty() && arch.len() <= 64 && arch.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .ok_or_else(|| HeaderError::Invalid("GGUF has no valid general.architecture".into()))?;
    let template = text("tokenizer.chat_template").ok_or_else(|| {
        HeaderError::Invalid("the model has no chat template: use an instruct/chat GGUF".into())
    })?;
    let format = ChatFormat::detect(&template).ok_or_else(|| {
        HeaderError::Invalid(
            "unsupported chat format (supported: Gemma, Qwen/ChatML, Llama 3, Mistral)".into(),
        )
    })?;
    let layer_count = int(&format!("{}.block_count", architecture))
        .filter(|layers| (1..=1024).contains(layers))
        .ok_or_else(|| HeaderError::Invalid("GGUF has no valid block_count".into()))? as u32;
    let trained_context = int(&format!("{}.context_length", architecture))
        .ok_or_else(|| HeaderError::Invalid("GGUF has no context_length".into()))?;
    if trained_context < MIN_SUMMARY_CONTEXT as u64 {
        return Err(HeaderError::Invalid(format!(
            "context window of {} tokens is too small for summaries (minimum {})",
            trained_context, MIN_SUMMARY_CONTEXT
        )));
    }
    let context_size = trained_context.min(MAX_SUMMARY_CONTEXT as u64) as u32;

    Ok(SummaryMetadata { architecture, format, context_size, layer_count })
}

/// Read and parse the metadata of a local GGUF file.
pub(crate) fn read_file_metadata(path: &Path) -> Result<SummaryMetadata, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    let mut buffer = Vec::new();
    file.take(MAX_HEADER_BYTES as u64)
        .read_to_end(&mut buffer)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    match parse_metadata(&buffer) {
        Ok(metadata) => Ok(metadata),
        Err(HeaderError::Incomplete) => Err("GGUF metadata is truncated or larger than 64 MB".into()),
        Err(HeaderError::Invalid(message)) => Err(message),
    }
}

/// Check a GGUF on Hugging Face from its header only, before downloading gigabytes.
pub(crate) async fn read_remote_metadata(
    client: &reqwest::Client,
    repo: &str,
    commit: &str,
    file: &super::hf::RemoteFile,
    token: Option<&str>,
) -> Result<SummaryMetadata, String> {
    let file_size = usize::try_from(file.size).unwrap_or(usize::MAX);
    for limit in [INITIAL_HEADER_BYTES, MAX_HEADER_BYTES] {
        let len = limit.min(file_size);
        let bytes = super::hf::fetch_prefix(client, repo, commit, &file.repo_path, token, len).await?;
        match parse_metadata(&bytes) {
            Ok(metadata) => return Ok(metadata),
            Err(HeaderError::Incomplete) if len < file_size && limit < MAX_HEADER_BYTES => continue,
            Err(HeaderError::Incomplete) => break,
            Err(HeaderError::Invalid(message)) => return Err(message),
        }
    }
    Err("GGUF metadata is truncated or larger than 64 MB".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a GGUF v3 header with the given string and u32 keys.
    pub(crate) fn header(strings: &[(&str, &str)], ints: &[(&str, u32)], vocab: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let push_str = |out: &mut Vec<u8>, s: &str| {
            out.extend((s.len() as u64).to_le_bytes());
            out.extend(s.as_bytes());
        };
        out.extend(b"GGUF");
        out.extend(3u32.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        out.extend(((strings.len() + ints.len() + 1) as u64).to_le_bytes());
        for (key, value) in strings {
            push_str(&mut out, key);
            out.extend(8u32.to_le_bytes());
            push_str(&mut out, value);
        }
        // A string array, like tokenizer.ggml.tokens, that the parser must skip.
        push_str(&mut out, "tokenizer.ggml.tokens");
        out.extend(9u32.to_le_bytes());
        out.extend(8u32.to_le_bytes());
        out.extend((vocab as u64).to_le_bytes());
        for i in 0..vocab {
            push_str(&mut out, &format!("tok{}", i));
        }
        for (key, value) in ints {
            push_str(&mut out, key);
            out.extend(4u32.to_le_bytes());
            out.extend(value.to_le_bytes());
        }
        out
    }

    fn qwen_like() -> Vec<u8> {
        header(
            &[
                ("general.architecture", "qwen3"),
                ("tokenizer.chat_template", "{% for m in messages %}<|im_start|>{{ m.role }}<think>{% endfor %}"),
            ],
            &[("qwen3.block_count", 36), ("qwen3.context_length", 40_960)],
            1000,
        )
    }

    #[test]
    fn parses_metadata_and_caps_context() {
        let meta = parse_metadata(&qwen_like()).unwrap();
        assert_eq!(meta.architecture, "qwen3");
        assert_eq!(meta.format, ChatFormat::QwenThinking);
        assert_eq!(meta.layer_count, 36);
        assert_eq!(meta.context_size, MAX_SUMMARY_CONTEXT);
    }

    #[test]
    fn truncated_header_asks_for_more_bytes() {
        let bytes = qwen_like();
        assert_eq!(parse_metadata(&bytes[..bytes.len() / 2]), Err(HeaderError::Incomplete));
    }

    #[test]
    fn rejects_base_models_and_unknown_formats() {
        let base = header(&[("general.architecture", "llama")], &[("llama.block_count", 32), ("llama.context_length", 8192)], 10);
        assert!(matches!(parse_metadata(&base), Err(HeaderError::Invalid(_))));
        let unknown = header(
            &[("general.architecture", "x"), ("tokenizer.chat_template", "<|user|>{{ content }}")],
            &[("x.block_count", 2), ("x.context_length", 4096)],
            10,
        );
        assert!(matches!(parse_metadata(&unknown), Err(HeaderError::Invalid(_))));
    }

    #[test]
    fn rejects_non_gguf_and_hostile_lengths() {
        assert!(matches!(parse_metadata(b"lmggxxxxxxxxxxxxxxxxxxxxxxxx"), Err(HeaderError::Invalid(_))));
        let mut hostile = Vec::new();
        hostile.extend(b"GGUF");
        hostile.extend(3u32.to_le_bytes());
        hostile.extend(0u64.to_le_bytes());
        hostile.extend(1u64.to_le_bytes());
        hostile.extend(u64::MAX.to_le_bytes()); // key length
        assert!(matches!(parse_metadata(&hostile), Err(HeaderError::Invalid(_))));
    }

    #[test]
    fn detects_chat_formats() {
        assert_eq!(ChatFormat::detect("<start_of_turn>user"), Some(ChatFormat::Gemma));
        assert_eq!(ChatFormat::detect("<|im_start|>system"), Some(ChatFormat::ChatMl));
        assert_eq!(ChatFormat::detect("<|start_header_id|>user<|end_header_id|>"), Some(ChatFormat::Llama3));
        assert_eq!(ChatFormat::detect("[INST] {{ m }} [/INST]"), Some(ChatFormat::Mistral));
        assert_eq!(ChatFormat::detect("<|user|>"), None);
    }
}
