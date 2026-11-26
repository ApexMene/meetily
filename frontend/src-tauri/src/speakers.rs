//! Fork: who is speaking, microphone or system audio.
//!
//! Transcription only sees the mixed audio, so the source of the speech is lost there. The
//! pipeline reports every mixing window (microphone and system samples, before mixing) to
//! [`record`]; each window is labelled by its loudest source, and runs of equal labels form a
//! timeline saved to `speakers.json` in the meeting folder when the recording stops. Its time is
//! the mixed audio's, so it matches both the live and the re-transcribed segments.
//!
//! The labels are hints: "mic" is the user or anyone in the same room, "system" the remote
//! participants. Without a headset the microphone also picks up the speakers, which is why system
//! audio wins unless the microphone is clearly louder.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

pub const FILE: &str = "speakers.json";
/// RMS below this (about -42 dBFS) is silence.
const SILENCE_RMS: f32 = 0.008;
/// The microphone wins over active system audio only when this much louder (about 6 dB).
const MIC_DOMINANCE: f32 = 2.0;
/// A segment gets a label only when one source covers this share of its labelled time.
const DOMINANT_SHARE: f64 = 0.7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Mic,
    System,
}

impl Source {
    pub fn tag(self) -> &'static str {
        match self {
            Source::Mic => "[mic]",
            Source::System => "[system]",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub start: f64,
    pub end: f64,
    pub source: Source,
}

struct Recorder {
    sample_rate: f64,
    /// Samples of mixed audio so far.
    position: u64,
    runs: Vec<Run>,
}

static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);

/// A new recording: forgets the previous timeline.
pub fn start(sample_rate: u32) {
    if let Ok(mut recorder) = RECORDER.lock() {
        *recorder = Some(Recorder { sample_rate: f64::from(sample_rate.max(1)), position: 0, runs: Vec::new() });
    }
}

/// One mixing window. Cheap: two RMS and a short lock, once per window (600 ms).
pub fn record(mic: &[f32], system: &[f32]) {
    let source = classify(rms(mic), rms(system));
    let Ok(mut guard) = RECORDER.lock() else { return };
    let Some(recorder) = guard.as_mut() else { return };
    let start = recorder.position as f64 / recorder.sample_rate;
    recorder.position += mic.len().max(system.len()) as u64;
    let end = recorder.position as f64 / recorder.sample_rate;
    let Some(source) = source else { return };
    match recorder.runs.last_mut() {
        Some(last) if last.source == source && (last.end - start).abs() < 1e-6 => last.end = end,
        _ => recorder.runs.push(Run { start, end, source }),
    }
}

/// Writes the timeline of the recording that just stopped into its folder.
pub fn save(folder: &Path) {
    let runs = match RECORDER.lock() {
        Ok(mut recorder) => recorder.take().map(|recorder| recorder.runs).unwrap_or_default(),
        Err(_) => return,
    };
    if runs.is_empty() {
        return;
    }
    if let Err(error) = crate::fork_store::write_json(&folder.join(FILE), &runs) {
        log::warn!("speakers: cannot save the timeline: {}", error);
    }
}

/// The timeline saved with a meeting; empty for meetings recorded before this feature or imported.
pub fn load(folder: &Path) -> Vec<Run> {
    std::fs::read(folder.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The source that clearly dominates `start..end` (seconds), if any.
pub fn dominant(runs: &[Run], start: f64, end: f64) -> Option<Source> {
    let (mut mic, mut system) = (0.0, 0.0);
    for run in runs.iter().filter(|run| run.end > start && run.start < end) {
        let overlap = run.end.min(end) - run.start.max(start);
        match run.source {
            Source::Mic => mic += overlap,
            Source::System => system += overlap,
        }
    }
    let total = mic + system;
    if total <= 0.0 {
        None
    } else if mic >= DOMINANT_SHARE * total {
        Some(Source::Mic)
    } else if system >= DOMINANT_SHARE * total {
        Some(Source::System)
    } else {
        None
    }
}

/// Explanation of the tags for Claude, given with the transcript.
pub const TAGS_HELP: &str = "[mic] = captured by the microphone (the user, or people in the same room); \
[system] = computer audio (remote participants). Tags are automatic hints: they can be wrong when people talk over each other.";

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

fn classify(mic: f32, system: f32) -> Option<Source> {
    match (mic >= SILENCE_RMS, system >= SILENCE_RMS) {
        (false, false) => None,
        (true, false) => Some(Source::Mic),
        (false, true) => Some(Source::System),
        (true, true) if mic >= MIC_DOMINANCE * system => Some(Source::Mic),
        (true, true) => Some(Source::System),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_windows() {
        assert_eq!(classify(0.0, 0.0), None);
        assert_eq!(classify(0.1, 0.0), Some(Source::Mic));
        assert_eq!(classify(0.0, 0.1), Some(Source::System));
        // Echo of the speakers in the microphone: the system audio wins.
        assert_eq!(classify(0.1, 0.08), Some(Source::System));
        assert_eq!(classify(0.3, 0.05), Some(Source::Mic));
    }

    #[test]
    fn builds_runs_and_finds_the_dominant_source() {
        start(10);
        let (loud, quiet) = (vec![0.5; 10], vec![0.0; 10]);
        record(&loud, &quiet); // 0-1 mic
        record(&loud, &quiet); // 1-2 mic
        record(&quiet, &quiet); // 2-3 silence
        record(&quiet, &loud); // 3-4 system
        let runs = RECORDER.lock().unwrap().take().unwrap().runs;
        assert_eq!(
            runs,
            vec![Run { start: 0.0, end: 2.0, source: Source::Mic }, Run { start: 3.0, end: 4.0, source: Source::System }]
        );
        assert_eq!(dominant(&runs, 0.0, 2.5), Some(Source::Mic));
        assert_eq!(dominant(&runs, 3.0, 4.0), Some(Source::System));
        assert_eq!(dominant(&runs, 1.0, 4.0), None); // half and half
        assert_eq!(dominant(&runs, 5.0, 6.0), None);
    }
}
