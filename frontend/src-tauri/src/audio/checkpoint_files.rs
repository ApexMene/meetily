//! Pure helpers for the incremental recording checkpoints (std only, unit tested).
//!
//! A recording is written to `<meeting>/.checkpoints/audio_chunk_NNN.mp4` every ~30 s.
//! Chunks are written to `audio_chunk_NNN.mp4.part` and renamed when complete, so a crash or a full
//! disk never leaves a truncated `.mp4` that would break the merge.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub const CHUNK_PREFIX: &str = "audio_chunk_";

pub fn chunk_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{}{:03}.mp4", CHUNK_PREFIX, index))
}

/// `audio_chunk_003.mp4` -> `audio_chunk_003.mp4.part`
pub fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// 3 for `audio_chunk_003.mp4`, None for anything else.
pub fn chunk_index(file_name: &str) -> Option<u32> {
    let digits = file_name.strip_prefix(CHUNK_PREFIX)?.strip_suffix(".mp4")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Complete, non-empty chunks of `dir` in recording order (numeric, so chunk 1000 follows 999).
/// Leftovers (`.part`, empty files, `concat_list.txt`, anything else) are ignored.
pub fn valid_checkpoints(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found: BTreeMap<u32, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(chunk_index) else { continue };
        let non_empty = entry.metadata().map(|m| m.is_file() && m.len() > 0).unwrap_or(false);
        if non_empty {
            found.insert(index, entry.path());
        }
    }
    Ok(found.into_values().collect())
}

/// Removes `.part` leftovers of a crashed write. Best effort.
pub fn remove_partial_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("part") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Text of an ffmpeg concat list. Single quotes in paths are escaped the way ffmpeg expects.
pub fn concat_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        let text = path.to_string_lossy().replace('\'', r"'\''");
        out.push_str(&format!("file '{}'\n", text));
    }
    out
}

/// When to write a checkpoint, and how to behave when writing keeps failing (disk full, ffmpeg
/// missing): retry only after another interval of audio, and cap the memory kept in RAM.
#[derive(Debug, Clone)]
pub struct CheckpointPolicy {
    interval_samples: usize,
    cap_samples: usize,
    retry_at: usize,
}

impl CheckpointPolicy {
    /// `cap_intervals`: how many intervals of audio may wait in memory while writes fail.
    pub fn new(interval_samples: usize, cap_intervals: usize) -> Self {
        let interval_samples = interval_samples.max(1);
        Self { interval_samples, cap_samples: interval_samples.saturating_mul(cap_intervals.max(1)), retry_at: 0 }
    }

    pub fn should_save(&self, buffered_samples: usize) -> bool {
        buffered_samples >= self.interval_samples.max(self.retry_at)
    }

    pub fn on_success(&mut self) {
        self.retry_at = 0;
    }

    pub fn on_failure(&mut self, buffered_samples: usize) {
        self.retry_at = buffered_samples.saturating_add(self.interval_samples);
    }

    /// How many samples must be dropped (oldest first) to respect the cap; 0 when within it.
    pub fn excess(&self, buffered_samples: usize) -> usize {
        buffered_samples.saturating_sub(self.cap_samples)
    }

    /// After dropping samples the retry threshold shifts with the buffer.
    pub fn on_dropped(&mut self, dropped: usize) {
        self.retry_at = self.retry_at.saturating_sub(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, bytes: usize) {
        std::fs::write(dir.join(name), vec![1u8; bytes]).unwrap();
    }

    #[test]
    fn chunk_names_roundtrip() {
        let dir = Path::new("/x");
        assert_eq!(chunk_path(dir, 7), Path::new("/x/audio_chunk_007.mp4"));
        assert_eq!(chunk_index("audio_chunk_007.mp4"), Some(7));
        assert_eq!(chunk_index("audio_chunk_1000.mp4"), Some(1000));
        for bad in ["audio_chunk_.mp4", "audio_chunk_1.mp4.part", "audio_chunk_x.mp4", "concat_list.txt", "audio.mp4", "audio_chunk_-1.mp4"] {
            assert_eq!(chunk_index(bad), None, "{bad}");
        }
        assert_eq!(part_path(Path::new("/x/audio_chunk_007.mp4")), Path::new("/x/audio_chunk_007.mp4.part"));
    }

    #[test]
    fn valid_checkpoints_are_ordered_numerically_and_skip_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        touch(d, "audio_chunk_000.mp4", 10);
        touch(d, "audio_chunk_002.mp4", 10); // 001 is missing: the rest is still kept
        touch(d, "audio_chunk_1000.mp4", 10);
        touch(d, "audio_chunk_999.mp4", 10);
        touch(d, "audio_chunk_003.mp4", 0); // empty: crashed write
        touch(d, "audio_chunk_004.mp4.part", 10);
        touch(d, "concat_list.txt", 10);
        std::fs::create_dir(d.join("audio_chunk_005.mp4")).unwrap(); // a directory is not a chunk
        let names: Vec<String> = valid_checkpoints(d).unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(names, ["audio_chunk_000.mp4", "audio_chunk_002.mp4", "audio_chunk_999.mp4", "audio_chunk_1000.mp4"]);
    }

    #[test]
    fn valid_checkpoints_errors_on_missing_dir_and_is_empty_on_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(valid_checkpoints(&tmp.path().join("nope")).is_err());
        assert!(valid_checkpoints(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn partial_files_are_removed_and_chunks_kept() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "audio_chunk_000.mp4", 5);
        touch(tmp.path(), "audio_chunk_001.mp4.part", 5);
        remove_partial_files(tmp.path());
        assert!(tmp.path().join("audio_chunk_000.mp4").exists());
        assert!(!tmp.path().join("audio_chunk_001.mp4.part").exists());
        remove_partial_files(&tmp.path().join("missing")); // no panic
    }

    #[test]
    fn concat_list_escapes_quotes() {
        let text = concat_list(&[PathBuf::from("/a/it's/x.mp4"), PathBuf::from("/b.mp4")]);
        assert_eq!(text, "file '/a/it'\\''s/x.mp4'\nfile '/b.mp4'\n");
        assert_eq!(concat_list(&[]), "");
    }

    #[test]
    fn policy_saves_at_interval_and_backs_off_after_failures() {
        let mut p = CheckpointPolicy::new(100, 5);
        assert!(!p.should_save(99));
        assert!(p.should_save(100));
        p.on_failure(100);
        // a failed write must not be retried on every chunk
        assert!(!p.should_save(101));
        assert!(!p.should_save(199));
        assert!(p.should_save(200));
        p.on_success();
        assert!(p.should_save(100));
    }

    #[test]
    fn policy_caps_memory_while_writes_fail() {
        let mut p = CheckpointPolicy::new(100, 5);
        assert_eq!(p.excess(500), 0);
        assert_eq!(p.excess(530), 30);
        p.on_failure(500); // retry_at 600
        p.on_dropped(30);
        assert!(!p.should_save(569));
        assert!(p.should_save(570));
        // degenerate values never panic
        let q = CheckpointPolicy::new(0, 0);
        assert!(q.should_save(1));
        assert_eq!(q.excess(usize::MAX), usize::MAX - 1);
    }
}
