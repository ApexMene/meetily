//! Claude clean-up of the re-transcribed segments: drops the noise speech models invent on
//! silence, fixes obvious mistakes and merges the second model's reading
//! (`transcripts.alt.json`), without adding content. Segments carry the microphone/system hint
//! from `speakers.json` when the recording has one. The prompt is editable in
//! `<app data>/post-meeting/transcript-cleanup.md`.

use crate::claude_code::{Request, Tools};
use crate::speakers::{self, Source};
use sqlx::Row;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tauri::{AppHandle, Runtime};

pub const PROMPT_FILE: &str = "transcript-cleanup.md";
/// Segments per Claude call.
const CHUNK: usize = 60;
const TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SYSTEM_PROMPT: &str = "You clean automatic speech transcripts. Answer only with the requested lines.";
const DEFAULT_PROMPT: &str = "# Transcript clean-up (edit freely; the segments are appended below)

This is the automatic transcript of a work meeting, split into numbered segments.
It may contain English technical terms on purpose (deploy, backlog, pull request, feedback...).
Speech models sometimes mishear words, switch to the wrong language by mistake, or invent
phrases on noise and silence (fragments in unrelated languages or scripts, nonsense words).

When a segment has two versions, A is the main model (multilingual, good with technical terms)
and B a second model specialised in the meeting language (good with ordinary words, weak with
foreign terms). Combine them: take the words of B where A is garbled, keep the technical terms
and names of A. If B is nonsense, ignore it.

For EACH segment write the most likely version of what was actually said:
- keep the language the meeting is held in; fix only obvious errors (garbled words, wrong language);
- keep technical terms, names and numbers; do not invent, summarize or add content;
- if the segment is noise or meaningless write exactly: [noise]
Output: one line per segment, same order, format \"n. text\". Nothing else.
";

/// Writes the default prompt when the file is missing.
pub fn ensure_prompt(dir: &Path) {
    let path = dir.join(PROMPT_FILE);
    if !path.exists() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&path, DEFAULT_PROMPT);
    }
}

fn load_prompt(dir: &Path) -> String {
    ensure_prompt(dir);
    match std::fs::read_to_string(dir.join(PROMPT_FILE)) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => DEFAULT_PROMPT.to_string(),
    }
}

/// Second-model texts from `transcripts.alt.json`, keyed by segment start in milliseconds.
fn load_alt_texts(folder: &Path) -> HashMap<i64, String> {
    let Ok(bytes) = std::fs::read(folder.join("transcripts.alt.json")) else { return HashMap::new() };
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            let start = row.get("start")?.as_f64()?;
            let text = row.get("b")?.as_str()?.trim();
            (!text.is_empty()).then(|| (millis(start), text.to_string()))
        })
        .collect()
}

fn millis(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

struct Segment {
    id: String,
    text: String,
    alt: Option<String>,
    source: Option<Source>,
}

/// Cleans the transcript of a meeting. Returns (kept, dropped) segments. A chunk Claude fails on
/// is kept as it is.
pub async fn run<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, model: &str, meeting_id: &str, folder: &Path) -> Result<(usize, usize), String> {
    let dir = super::data_dir(app).ok_or("app data directory unavailable")?;
    let prompt = format!("{}\n{}", load_prompt(&dir), crate::glossary::critical::PROMPT_NOTE);
    let alt = load_alt_texts(folder);
    let runs = speakers::load(folder);
    let rows = sqlx::query("SELECT id, transcript, audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;
    let segments: Vec<Segment> = rows
        .iter()
        .map(|row| {
            let start = row.try_get::<Option<f64>, _>("audio_start_time").ok().flatten();
            let end = row.try_get::<Option<f64>, _>("audio_end_time").ok().flatten();
            Segment {
                id: row.try_get("id").unwrap_or_default(),
                text: row.try_get("transcript").unwrap_or_default(),
                alt: start.and_then(|start| alt.get(&millis(start)).cloned()),
                source: start.zip(end).and_then(|(start, end)| speakers::dominant(&runs, start, end)),
            }
        })
        .collect();
    let env = crate::notion_agent::claude_env(app);
    let mut updates: Vec<(&str, Option<String>)> = Vec::new(); // None = drop
    for chunk in segments.chunks(CHUNK) {
        let content = numbered(chunk);
        let request = Request {
            model,
            system_prompt: SYSTEM_PROMPT,
            instructions: &prompt,
            content: &content,
            tools: Tools::None,
            env: &env,
            timeout: TIMEOUT,
        };
        let answer = match crate::claude_code::run(&request, None).await {
            Ok(answer) => answer,
            Err(error) => {
                log::warn!("post-meeting: a clean-up chunk failed and is kept as it is: {}", error);
                continue;
            }
        };
        let Some(lines) = parse_answer(&answer, chunk.len()) else {
            log::warn!("post-meeting: malformed clean-up answer, chunk kept as it is");
            continue;
        };
        for (segment, line) in chunk.iter().zip(lines) {
            if line.as_deref() != Some(segment.text.as_str()) {
                updates.push((segment.id.as_str(), line));
            }
        }
    }
    let dropped = updates.iter().filter(|(_, text)| text.is_none()).count();
    let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
    for (id, text) in &updates {
        let query = match text {
            Some(text) => sqlx::query("UPDATE transcripts SET transcript = ? WHERE id = ?").bind(text).bind(id),
            None => sqlx::query("DELETE FROM transcripts WHERE id = ?").bind(id),
        };
        query.execute(&mut *tx).await.map_err(|error| error.to_string())?;
    }
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok((segments.len() - dropped, dropped))
}

/// "SEGMENTS" block: `n. text`, or `n. A: text | B: text` when the second model differs, with the
/// `[mic]`/`[system]` hint in front when known (explained once, never to be copied).
fn numbered(chunk: &[Segment]) -> String {
    let mut out = String::new();
    if chunk.iter().any(|segment| segment.source.is_some()) {
        out.push_str(&format!("Speaker hints: {} Use them to tell turns apart; do not write them in your answer.\n\n", speakers::TAGS_HELP));
    }
    out.push_str("SEGMENTS\n");
    for (n, segment) in chunk.iter().enumerate() {
        let tag = segment.source.map(|source| format!("{} ", source.tag())).unwrap_or_default();
        let text = segment.text.replace('\n', " ");
        match &segment.alt {
            Some(alt) if *alt != segment.text => out.push_str(&format!("{}. {}A: {} | B: {}\n", n + 1, tag, text, alt.replace('\n', " "))),
            _ => out.push_str(&format!("{}. {}{}\n", n + 1, tag, text)),
        }
    }
    out
}

/// "n. text" lines -> one entry per segment (None = noise). None when a segment is missing or
/// empty, so a truncated answer never deletes anything.
fn parse_answer(answer: &str, expected: usize) -> Option<Vec<Option<String>>> {
    let mut out: Vec<Option<Option<String>>> = vec![None; expected];
    for line in answer.lines().map(str::trim) {
        let Some((number, text)) = line.split_once('.') else { continue };
        let Ok(n) = number.trim().parse::<usize>() else { continue };
        if n == 0 || n > expected {
            continue;
        }
        // A hint copied back by mistake is not part of the text.
        let text = text.trim();
        let text = [Source::Mic, Source::System]
            .iter()
            .find_map(|source| text.strip_prefix(source.tag()))
            .unwrap_or(text)
            .trim_start();
        if text.is_empty() {
            continue; // unanswered: the chunk is kept as it is
        }
        out[n - 1] = Some((!text.eq_ignore_ascii_case("[noise]")).then(|| text.to_string()));
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_answer() {
        let lines = parse_answer("1. Hello there\n2. [noise]\n3.  ok \n", 3).unwrap();
        assert_eq!(lines, vec![Some("Hello there".into()), None, Some("ok".into())]);
        assert!(parse_answer("1. only one", 2).is_none());
        assert!(parse_answer("1. a\n2.\n", 2).is_none());
        assert_eq!(parse_answer("1. [system] hi", 1), Some(vec![Some("hi".into())]));
        assert_eq!(parse_answer("Here you go\n1. a\n", 1), Some(vec![Some("a".into())]));
    }

    #[test]
    fn numbers_segments_with_second_reading() {
        let segment = |text: &str, alt: Option<&str>| Segment { id: String::new(), text: text.into(), alt: alt.map(Into::into), source: None };
        let block = numbered(&[segment("hello", None), segment("deploy tomorrow", Some("de ploy tomorrow")), segment("x", Some("x"))]);
        assert_eq!(block, "SEGMENTS\n1. hello\n2. A: deploy tomorrow | B: de ploy tomorrow\n3. x\n");
    }

    #[test]
    fn numbers_segments_with_speaker_hints() {
        let segment = |text: &str, source| Segment { id: String::new(), text: text.into(), alt: None, source };
        let block = numbered(&[segment("hello", Some(Source::Mic)), segment("hi", Some(Source::System)), segment("ok", None)]);
        assert!(block.starts_with("Speaker hints: "));
        assert!(block.ends_with("SEGMENTS\n1. [mic] hello\n2. [system] hi\n3. ok\n"));
    }
}
