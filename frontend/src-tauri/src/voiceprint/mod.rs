//! Fork: who the remote participants are, from their voice. Optional post-meeting step, off by
//! default; recording and transcription never depend on it.
//!
//! After a meeting, the transcript segments that `speakers` labelled as system audio ("the
//! others") are embedded with a small WeSpeaker ONNX model, grouped by similarity and compared
//! with the voiceprints of people already named. The result is `speaker_ids.json` in the meeting
//! folder: every group gets the name of the matching voiceprint or a neutral "Other n", which the
//! clean-up and the recap show to Claude as a hint (`[system: Anna]`). A group is named by the
//! user (`voiceprint_name_cluster`) or, when `active_speakers.json` exists (names read from the
//! meeting window while it ran), by time alignment; naming stores the voiceprint, so the same
//! voice is recognised in later meetings.
//!
//! The microphone is never embedded: it is "you". Any failure (no model, no network, short or
//! broken audio) only skips the step.

mod cluster;
mod fbank;
mod model;
mod names;

use crate::fork_store::Store;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tauri::{AppHandle, Manager, Runtime};

const IDS_FILE: &str = "speaker_ids.json";
/// Input of the optional active speaker reader: `[{"start": s, "end": s, "name": "..."}]`.
const ACTIVE_FILE: &str = "active_speakers.json";
/// A voiceprint averages at most this many samples, so it keeps adapting.
const MAX_SAMPLES: u32 = 20;
const SAMPLE_RATE: f64 = 16_000.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub enabled: bool,
    /// Cosine similarity to join two segments in one group.
    pub cluster_threshold: f32,
    /// Cosine similarity to give a group the name of a known voice.
    pub match_threshold: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, cluster_threshold: 0.55, match_threshold: 0.55 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Person {
    name: String,
    embedding: Vec<f32>,
    samples: u32,
    updated: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Voiceprints {
    people: Vec<Person>,
}

static SETTINGS: Store<Settings> = Store::new("voiceprints/settings.json");
static VOICEPRINTS: Store<Voiceprints> = Store::new("voiceprints/voiceprints.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Group {
    id: usize,
    /// "Other 1", "Other 2"...
    label: String,
    name: Option<String>,
    seconds: f64,
    centroid: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Piece {
    start: f64,
    end: f64,
    group: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ids {
    groups: Vec<Group>,
    pieces: Vec<Piece>,
}

pub fn load_ids(folder: &Path) -> Ids {
    std::fs::read(folder.join(IDS_FILE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

impl Ids {
    /// Name (or neutral label) of who speaks in `start..end`, when a group clearly covers it.
    pub fn who(&self, start: f64, end: f64) -> Option<String> {
        let spans: Vec<(f64, f64, usize)> = self.pieces.iter().map(|p| (p.start, p.end, p.group)).collect();
        let id = names::cluster_at(&spans, start, end)?;
        self.groups.iter().find(|g| g.id == id).map(|g| g.name.clone().unwrap_or_else(|| g.label.clone()))
    }
}

/// Fork: tag shown to Claude for a segment, e.g. `[system: Anna]`; the plain tag without a name.
pub fn hint_tag(source: crate::speakers::Source, ids: &Ids, start: Option<f64>, end: Option<f64>) -> String {
    use crate::speakers::Source;
    let who = match (source, start, end) {
        (Source::System, Some(start), Some(end)) => ids.who(start, end),
        _ => None,
    };
    match who {
        Some(name) => format!("[system: {}]", name.replace(['[', ']', '\n'], " ")),
        None => source.tag().to_string(),
    }
}

/// Added to the explanation of the tags when names are present.
pub const NAMES_HELP: &str = "[system: Name] = computer audio, voice recognised as Name (\"Other n\" = a voice not named yet).";

fn valid_name(name: &str) -> Option<String> {
    let name = name.trim();
    let ok = !name.is_empty() && name.chars().count() <= 60 && !name.chars().any(char::is_control) && !name.contains(['[', ']']);
    ok.then(|| name.to_string())
}

/// Adds a sample to the voiceprint of `name` (created when new).
fn enroll<R: Runtime>(app: &AppHandle<R>, name: &str, embedding: &[f32]) -> Result<(), String> {
    if embedding.is_empty() || embedding.iter().any(|v| !v.is_finite()) {
        return Err("invalid voice sample".into());
    }
    let mut store = VOICEPRINTS.get(app);
    let now = chrono::Utc::now().to_rfc3339();
    match store.people.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name)) {
        Some(person) if person.embedding.len() == embedding.len() => {
            let mut mixed: Vec<f32> = person.embedding.iter().zip(embedding).map(|(old, new)| old * person.samples as f32 + new).collect();
            cluster::normalize(&mut mixed);
            person.embedding = mixed;
            person.samples = (person.samples + 1).min(MAX_SAMPLES);
            person.updated = now;
        }
        Some(person) => {
            person.embedding = embedding.to_vec(); // a different model produced the old one
            person.samples = 1;
            person.updated = now;
        }
        None => store.people.push(Person { name: name.to_string(), embedding: embedding.to_vec(), samples: 1, updated: now }),
    }
    VOICEPRINTS.set(app, store)
}

fn read_active(folder: &Path) -> Vec<(f64, f64, String)> {
    #[derive(Deserialize)]
    struct Entry {
        start: f64,
        end: f64,
        name: String,
    }
    let entries: Vec<Entry> = std::fs::read(folder.join(ACTIVE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    entries
        .into_iter()
        .filter(|e| e.start.is_finite() && e.end.is_finite() && e.end > e.start)
        .filter_map(|e| valid_name(&e.name).map(|name| (e.start, e.end, name)))
        .collect()
}

/// Post-meeting step. Never fails the job: problems are logged. Skipped while recording, so it
/// never competes with the transcription for the CPU.
pub async fn run<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, meeting_id: &str) {
    let settings = SETTINGS.get(app);
    if !settings.enabled || crate::audio::recording_commands::is_recording().await {
        return;
    }
    let folder: Option<(Option<String>,)> = sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?").bind(meeting_id).fetch_optional(pool).await.ok().flatten();
    let Some(folder) = folder.and_then(|(f,)| f).filter(|f| !f.trim().is_empty()).map(std::path::PathBuf::from) else { return };
    let folder = folder.as_path();
    let started = std::time::Instant::now();
    match run_inner(app, pool, meeting_id, folder, &settings).await {
        Ok(groups) => log::info!("voiceprint: {} voices in {} ({:.0}s)", groups, meeting_id, started.elapsed().as_secs_f64()),
        Err(error) => log::warn!("voiceprint: {} skipped: {}", meeting_id, error),
    }
}

async fn run_inner<R: Runtime>(app: &AppHandle<R>, pool: &sqlx::SqlitePool, meeting_id: &str, folder: &Path, settings: &Settings) -> Result<usize, String> {
    let runs = crate::speakers::load(folder);
    if runs.is_empty() {
        return Err("no speaker timeline".into());
    }
    let rows: Vec<(Option<f64>, Option<f64>)> = sqlx::query_as("SELECT audio_start_time, audio_end_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time")
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    let spans = system_spans(&rows, &runs);
    let _ = std::fs::remove_file(folder.join(IDS_FILE)); // never keep the result of an older transcript
    if spans.is_empty() {
        return Ok(0);
    }
    let model_path = crate::custom_models::speaker_model::ensure_installed(app).await?;
    let audio_path = crate::audio::retranscription::find_audio_file(folder).map_err(|e| e.to_string())?;
    let embeddings = tokio::task::spawn_blocking(move || -> Result<Vec<Option<Vec<f32>>>, String> {
        let decoded = crate::audio::decoder::decode_audio_file(&audio_path).map_err(|e| e.to_string())?;
        let samples = decoded.to_whisper_format();
        let mut model = model::SpeakerModel::load(&model_path)?;
        let mut out = Vec::with_capacity(spans.len());
        for &(start, end) in &spans {
            let from = ((start * SAMPLE_RATE) as usize).min(samples.len());
            let to = ((end * SAMPLE_RATE) as usize).min(samples.len());
            // One bad segment must not lose the others.
            out.push(if to > from { model.embed(&samples[from..to]).unwrap_or(None) } else { None });
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("embedding task: {}", e))??;
    let spans = system_spans(&rows, &runs);
    let (kept_spans, kept): (Vec<(f64, f64)>, Vec<Vec<f32>>) = spans.into_iter().zip(embeddings).filter_map(|(s, e)| e.map(|e| (s, e))).unzip();
    if kept.is_empty() {
        return Ok(0);
    }
    let labels = cluster::cluster(&kept, settings.cluster_threshold);
    let count = labels.iter().copied().max().map_or(0, |m| m + 1);
    let pieces: Vec<Piece> = kept_spans.iter().zip(&labels).map(|(&(start, end), &group)| Piece { start, end, group }).collect();

    let known = VOICEPRINTS.get(app);
    let triples: Vec<(f64, f64, usize)> = pieces.iter().map(|p| (p.start, p.end, p.group)).collect();
    let by_time = names::vote(&triples, &read_active(folder));
    let mut groups = Vec::with_capacity(count);
    for id in 0..count {
        let members: Vec<&[f32]> = kept.iter().zip(&labels).filter(|(_, &l)| l == id).map(|(e, _)| e.as_slice()).collect();
        let centroid = cluster::centroid(&members);
        let seconds = pieces.iter().filter(|p| p.group == id).map(|p| p.end - p.start).sum();
        let matched = cluster::best_match(&centroid, known.people.iter().map(|p| p.embedding.as_slice()), settings.match_threshold);
        let name = by_time.get(&id).cloned().or_else(|| matched.map(|(i, _)| known.people[i].name.clone()));
        if let Some(name) = &name {
            // Named by time alignment, or recognised with a clear margin: the voiceprint learns.
            let learn = by_time.contains_key(&id) || matched.is_some_and(|(_, score)| score >= settings.match_threshold + 0.1);
            if learn {
                if let Err(error) = enroll(app, name, &centroid) {
                    log::warn!("voiceprint: cannot update the voiceprint of {}: {}", name, error);
                }
            }
        }
        groups.push(Group { id, label: format!("Other {}", id + 1), name, seconds, centroid });
    }
    let ids = Ids { groups, pieces };
    crate::fork_store::write_json(&folder.join(IDS_FILE), &ids).map_err(|e| e.to_string())?;
    Ok(count)
}

/// Segments dominated by system audio and long enough to embed.
fn system_spans(rows: &[(Option<f64>, Option<f64>)], runs: &[crate::speakers::Run]) -> Vec<(f64, f64)> {
    rows.iter()
        .filter_map(|(start, end)| start.zip(*end))
        .filter(|(start, end)| start.is_finite() && end.is_finite() && end - start >= model::MIN_SECONDS)
        .filter(|(start, end)| crate::speakers::dominant(runs, *start, *end) == Some(crate::speakers::Source::System))
        .collect()
}

// --- commands -------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonView {
    name: String,
    samples: u32,
    updated: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    id: usize,
    label: String,
    name: Option<String>,
    seconds: f64,
}

#[tauri::command]
pub async fn voiceprint_settings_get<R: Runtime>(app: AppHandle<R>) -> Settings {
    SETTINGS.get(&app)
}

/// Turns the step on or off; the model is downloaded by the first job that needs it.
#[tauri::command]
pub async fn voiceprint_set_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    let mut settings = SETTINGS.get(&app);
    settings.enabled = enabled;
    SETTINGS.set(&app, settings)
}

#[tauri::command]
pub async fn voiceprint_list<R: Runtime>(app: AppHandle<R>) -> Vec<PersonView> {
    VOICEPRINTS.get(&app).people.into_iter().map(|p| PersonView { name: p.name, samples: p.samples, updated: p.updated }).collect()
}

#[tauri::command]
pub async fn voiceprint_delete<R: Runtime>(app: AppHandle<R>, name: String) -> Result<(), String> {
    let mut store = VOICEPRINTS.get(&app);
    store.people.retain(|p| p.name != name);
    VOICEPRINTS.set(&app, store)
}

async fn meeting_folder<R: Runtime>(app: &AppHandle<R>, meeting_id: &str) -> Result<std::path::PathBuf, String> {
    let state = app.try_state::<crate::state::AppState>().ok_or("Meetily is still starting")?;
    let folder: Option<(Option<String>,)> = sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(state.db_manager.pool())
        .await
        .map_err(|e| e.to_string())?;
    folder.and_then(|(f,)| f).filter(|f| !f.trim().is_empty()).map(std::path::PathBuf::from).ok_or_else(|| "meeting folder not found".to_string())
}

/// The voices found in a meeting.
#[tauri::command]
pub async fn voiceprint_meeting_speakers<R: Runtime>(app: AppHandle<R>, meeting_id: String) -> Result<Vec<GroupView>, String> {
    let folder = meeting_folder(&app, &meeting_id).await?;
    Ok(load_ids(&folder).groups.into_iter().map(|g| GroupView { id: g.id, label: g.label, name: g.name, seconds: g.seconds }).collect())
}

/// Names a voice of a meeting; the voiceprint is kept for the next meetings.
#[tauri::command]
pub async fn voiceprint_name_cluster<R: Runtime>(app: AppHandle<R>, meeting_id: String, group: usize, name: String) -> Result<(), String> {
    let name = valid_name(&name).ok_or("Invalid name")?;
    let folder = meeting_folder(&app, &meeting_id).await?;
    let mut ids = load_ids(&folder);
    let entry = ids.groups.iter_mut().find(|g| g.id == group).ok_or("Voice not found")?;
    enroll(&app, &name, &entry.centroid)?;
    entry.name = Some(name);
    crate::fork_store::write_json(&folder.join(IDS_FILE), &ids).map_err(|e| e.to_string())
}
