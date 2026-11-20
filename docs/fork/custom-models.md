# Custom models (fork)

Status: **implemented** · Scope: local transcription (Parakeet, Whisper) and built-in AI summaries (llama.cpp GGUF).

## 1. Goal

Use models that are not in the app catalog, from Hugging Face (public, private or gated repos) or from local files. They get the same safety and the same experience as built-in models: downloaded verifiably, validated before they can be selected, and chosen from the same settings screen.

Examples:
- [`oruk/orukeet`](https://huggingface.co/oruk/orukeet), a fine-tune of `nvidia/parakeet-tdt-0.6b-v3`. Its subfolder `onnx/combined-v0.1.0-int8/` has the layout Meetily's Parakeet engine already loads. The rest of the repo is ignored.
- Any single-file GGUF instruct model (Qwen, Llama 3, Mistral, Gemma…) for summaries with the "Built-in AI" provider.

## 2. What is supported

| Engine | Accepted | Rejected |
|---|---|---|
| Parakeet (ONNX Runtime) | onnx-asr/istupakov layout: `encoder-model[.int8].onnx` + `decoder_joint-model[.int8].onnx` (+ FP32 external data `*.onnx.data*`), `vocab.txt` with `<blk>`; `nemo128.onnx` and `config.json` can be next to the graphs or in a parent folder. If `nemo128.onnx` is missing, the standard v3 preprocessor is used (pinned, see §6). | fp16/q4/q8 graphs, sherpa-onnx layout, CTC/RNNT, Canary, `.nemo`, safetensors, `.zst` archives |
| Whisper (whisper.cpp) | A single ggml `.bin` file with `ggml` in its name (also quantized) | GGUF, safetensors/transformers, CTranslate2, `pytorch_model.bin` |
| Summary (llama-helper / llama.cpp) | A single-file `.gguf` instruct model whose embedded chat template is one of: Gemma (`<start_of_turn>`), ChatML/Qwen (`<\|im_start\|>`, also with `<think>`), Llama 3 (`<\|start_header_id\|>`), Mistral (`[INST]`). Trained context ≥ 2048 tokens. | Split GGUF (`-00001-of-0000N`), `mmproj` projectors, base models without a chat template, unknown chat formats, non-LLM GGUF (e.g. ASR) |

Limits: 4 GB per transcription model, 8 GB per summary model.

Models checked during development:

| Repository | Result |
|---|---|
| `oruk/orukeet` | 1 Parakeet int8 variant (`onnx/combined-v0.1.0-int8`), download + probe OK |
| `Masterx/parakeet-tdt-0.6b-ultra-onnx` | int8 and fp32 variants (standard preprocessor) |
| `Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx` | `int8/` and `fp32/` variants (vocab and preprocessor from the root) |
| `moondream/parakeet-ultra` | no variant (safetensors only): use one of the two conversions above |
| `ggerganov/whisper.cpp` | every `ggml-*.bin`; `ggml-tiny.bin` download + probe OK |
| `openai/whisper-large-v3` | no variant (transformers): same model as the built-in "large-v3" |
| `bartowski/SmolLM2-135M-Instruct-GGUF` | summary variants; ChatML detected, header + download + llama-helper probe OK |
| `CohereLabs/cohere-transcribe-03-2026` | not supported: different architecture, would need a new engine (e.g. sherpa-onnx) |

## 3. Architecture

The new code lives in `frontend/src-tauri/src/custom_models/`:

| File | Role |
|---|---|
| `mod.rs` | types, in-memory registry (`watch` channel), `resolved_models()` for the engines, engine refresh after changes |
| `registry.rs` | SQLite CRUD (`custom_models`) |
| `hf.rs` | HF client: reference parsing, commit resolution, paginated file list, variant detection, ranged reads |
| `download.rs` | verified download (SHA-256 / git SHA-1 while streaming, resume, retry, cancel), local copy with hashing and size cap |
| `gguf.rs` | bounded GGUF header parser: architecture, chat template → chat format, context, layers |
| `probe.rs` | static checks + real load and run of every model before it is registered |
| `paths.rs` | path validation, on-disk check, slug, free space |
| `credentials.rs` | HF token in the OS credential store (`keyring`) |
| `engine_hooks.rs` | converts custom transcription models to each engine's `ModelInfo` |
| `summary_hooks.rs` | catalog entries, sampling and prompt formats (ChatML, Llama 3, Mistral) for the summary engine |
| `commands.rs` | Tauri commands `custom_models_*` and install events |

Changes to upstream files (kept small, marked `Fork:`):

- `lib.rs`: `pub mod custom_models`, `custom_models::init` after the DB, command registration.
- `parakeet_engine.rs`, `whisper_engine.rs`: `discover_models` appends the custom models; `delete_model` refuses custom ids.
- `summary_engine/models.rs`: `get_available_models` appends the custom summary models; `format_prompt` falls back to the fork's chat formats.
- `summary_engine/model_manager.rs`: `download_model_detailed` and `delete_model` refuse custom ids.
- `Cargo.toml`: `sha2`, `sha1`, `keyring`.
- `ParakeetModelManager.tsx`, `WhisperModelManager.tsx`, `BuiltInModelManager.tsx`: render `<CustomModelsSection>` (the built-in list hides custom ids).

New frontend files: `src/lib/custom-models.ts`, `src/components/CustomModels/*`.

Selection is unchanged:
- Transcription: `transcript_settings(provider, model)` with `model = custom-<slug>`.
- Summaries: `settings(provider = "builtin-ai", model = custom-<slug>)`. The summary engine resolves the id through its catalog, so `is_model_ready`, the sidecar and context sizing work without other changes. The catalog `gguf_file` is the absolute path of the installed file.

The `custom-` prefix never collides with catalog names.

## 4. Data

Table `custom_models` (migration `20261119000000_add_custom_models.sql`). A row exists only for a model that is verified and has passed the probe. There is no status column: "downloading" exists only in memory. Summary rows also store `architecture`, `chat_template` (the prompt format name), `context_size` and `layer_count`; a CHECK constraint requires them for `engine = 'summary'`.

`files_json` holds `{path, size, sha256}` for every file. During discovery, presence and exact size are checked (fast). The SHA-256 is computed at install time and kept for audit.

On disk:
- `<app data>/models/custom/<engine>/<id>/`: installed models (`parakeet`, `whisper`, `summary`).
- `.staging/<id>/`: installs in progress. Cleared at every startup.

## 5. Flows

**Hugging Face**
1. Paste `owner/repo` or a URL (also `/tree/<rev>/<path>` or `/blob/...`).
2. **Check**: the app resolves the commit and lists the variants for the engine of the section (others are counted, not shown) with size and license.
3. Choose a variant and a name. If the repo has a license, the user accepts it.
4. **Download**: the backend reads the listing for the pinned commit again. The frontend sends only the variant key, never URLs or hashes. For summary models the GGUF header is read first with HTTP Range requests (8 MB, up to 64 MB), so an incompatible model fails before the full download.
5. Download into staging with verification, then the probe, then the model is moved into place, registered, the engines are refreshed and the model is selected.

**Local files**: a native dialog picks a folder (Parakeet), a `.bin` (Whisper) or a `.gguf` (summary). A GGUF header is checked before copying. The files are copied with hashing and a size cap, then the same probe and registration. The originals are never modified.

**Removal**:
1. The model is unloaded if it is in use. For summary models the llama-helper sidecar is stopped (it restarts on the next summary); a summary being generated at that moment is interrupted.
2. The files are deleted (with retries, because Windows may hold a lock briefly). If they cannot be deleted, the model stays registered and the user can retry.
3. If it is the selected model, the selection goes back to the built-in default (`parakeet-tdt-0.6b-v3-int8` / `large-v3-turbo` / the recommended built-in summary model for the machine).
4. The row is deleted.

## 6. Security and integrity

- **Network**:
  - HTTPS only (`https_only`).
  - Redirects are allowed only to `huggingface.co`, `*.huggingface.co`, `hf.co` and `*.hf.co`.
  - Pagination links are accepted only from `huggingface.co`.
  - API responses are capped at 16 MB; ranged header reads at 64 MB.
- **Token**: sent only to requests on huggingface.co for the main repo. It is never sent to files borrowed from other repos. reqwest drops `Authorization` on redirects to a different host. The token is marked `sensitive`, is never logged, and is stored in the OS credential store.
- **Integrity**:
  - Every file is verified while streaming: SHA-256 (LFS/Xet) or git blob SHA-1 (small files).
  - A file that does not match is deleted.
  - A file larger than the published size is rejected.
  - Downloads are always pinned to a commit.
- **Standard preprocessor**: `istupakov/parakeet-tdt-0.6b-v3-onnx@8f23f0c`, `nemo128.onnx`, SHA-256 `a9fde148…`. It is the same file that ships with the built-in v3 model.
- **Filesystem**:
  - Every component coming from outside is validated: no `..`, separators, `:` (ADS), control characters or Windows reserved names.
  - Installed files are flat (single component).
  - Links are rejected.
- **Formats**: data formats only (ONNX, ggml, GGUF). No code from the repo is ever executed. The GGUF chat template (Jinja) is never evaluated: it is only matched against known markers to choose one of the app's fixed prompt formats. `pytorch_model.bin` (pickle) fails the header check.
- **GGUF parser**: bounded (≤ 100k keys, strings ≤ 16 MB, arrays ≤ 50M items, nesting ≤ 4); string arrays such as the vocabulary are skipped, not loaded.
- **Prompt injection**: transcript text is user content; every chat-turn marker of every supported format is neutralized before it is put into the prompt.
- **Resources**:
  - 4 GB limit per transcription model, 8 GB per summary model.
  - Free-space check with 1 GB of headroom.
  - Probe timeout of 180 s (transcription) and 420 s (summary, includes model load).
  - Summary context is the trained context capped at 32768 tokens.

## 7. Compatibility probe

- **Parakeet**:
  1. The files required for the precision are present.
  2. `vocab.txt` is in `token id` format and contains `<blk>`.
  3. `config.json`, when present: `model_type` contains `tdt`, `features_size = 128`, `subsampling_factor = 8`.
  4. `ParakeetModel::new` and `transcribe_samples` run on 1 s of silence.
- **Whisper**:
  1. ggml header (`lmgg`).
  2. `WhisperContext` (CPU) loads the model.
  3. `full()` runs on 1 s of silence.
- **Summary**:
  1. GGUF header: `general.architecture`, `tokenizer.chat_template` (must map to a supported format), `<arch>.context_length`, `<arch>.block_count`.
  2. A dedicated llama-helper process (not the app's shared sidecar) loads the model and generates 4 tokens from a prompt in the detected format; the process is then stopped.

## 8. Tests

- Unit tests: `cargo test -p meetily --lib custom_models` (parsing, hosts, variant detection, GGUF header and chat formats, prompt formats and escaping, paths, hashes, probe checks).
- Network tests (manual, `#[ignore]`):
  - `cargo test -p meetily --lib custom_models::hf::live -- --ignored`: variant detection on the real repos in §2.
  - `cargo test -p meetily --lib custom_models::probe::live -- --ignored --nocapture`: real download + verification + probe of orukeet (~670 MB), whisper tiny (~75 MB) and the smallest SmolLM2-135M GGUF (under 100 MB, runs the bundled llama-helper). Only the SmolLM2 test deletes its download folder; the others keep `%TEMP%/meetily-live-*` for reruns.

## 9. Known limits

- Branch names that contain `/` are not supported in links. A commit SHA can be used instead.
- The SQLite migration is fork-only. If you reinstall the official build on the same data, sqlx would report an unknown applied migration.
- Summary sampling: Gemma and Qwen use the upstream presets; other formats use a conservative preset (temperature 0.3).
- Next phase: a sherpa-onnx engine for Cohere Transcribe and other architectures.
