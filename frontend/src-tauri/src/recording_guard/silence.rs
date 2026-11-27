//! Pure logic of the silent-channel alert (std only, unit tested).
//!
//! The audio pipeline records, per channel, when it last heard something above the threshold
//! (`note_chunk` in `mod.rs`); a watcher asks a `SilenceTracker` every few seconds whether that
//! channel has been silent long enough to warn the user. One alert per silent episode.

/// RMS of a chunk; 0 for empty input and for non-finite samples (never panics, never NaN).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| if s.is_finite() { s * s } else { 0.0 }).sum();
    let value = (sum / samples.len() as f32).sqrt();
    if value.is_finite() { value } else { 0.0 }
}

/// dBFS -> linear RMS. Out-of-range or non-finite input falls back to the default (-70 dB).
pub fn db_to_rms(db: f32) -> f32 {
    let db = if db.is_finite() { db.clamp(-120.0, 0.0) } else { -70.0 };
    10f32.powf(db / 20.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// The channel has been silent for `silent_ms`: warn once.
    Alert { silent_ms: u64 },
    /// Sound came back after an alert.
    Recovered,
}

#[derive(Debug, Default)]
pub struct SilenceTracker {
    alerted: bool,
    /// `last_loud_ms` at the time of the alert: sound newer than this ends the episode.
    alert_mark_ms: u64,
    /// When the channel started being watched (silence is counted from here at the earliest).
    watch_since_ms: Option<u64>,
}

impl SilenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `now_ms` and `last_loud_ms` share the same monotonic clock. `watching` is false when the
    /// channel is expected to be quiet (paused, nobody else talking...): the silence counter
    /// restarts when watching resumes.
    pub fn check(&mut self, now_ms: u64, last_loud_ms: u64, limit_ms: u64, watching: bool) -> Verdict {
        if self.alerted && last_loud_ms > self.alert_mark_ms {
            self.alerted = false;
            return Verdict::Recovered;
        }
        if !watching {
            self.watch_since_ms = None;
            return Verdict::Ok;
        }
        let since = *self.watch_since_ms.get_or_insert(now_ms);
        if self.alerted {
            return Verdict::Ok;
        }
        let last_active = last_loud_ms.max(since);
        let silent_ms = now_ms.saturating_sub(last_active);
        if limit_ms > 0 && silent_ms >= limit_ms {
            self.alerted = true;
            self.alert_mark_ms = last_loud_ms;
            return Verdict::Alert { silent_ms };
        }
        Verdict::Ok
    }
}

// ---- gap marker text (for the transcript) ----

fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds as u64 } else { 0 };
    if total >= 3600 {
        format!("{:02}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

/// "[audio interrotto 12:34 (8 s)]": where in the meeting the audio was lost and for how long.
pub fn gap_marker_text(start_secs: f64, duration_secs: f64) -> String {
    let secs = if duration_secs.is_finite() && duration_secs > 0.0 { duration_secs.round() as u64 } else { 0 };
    if secs == 0 {
        format!("[audio interrotto {}]", clock(start_secs))
    } else {
        format!("[audio interrotto {} ({} s)]", clock(start_secs), secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 90_000;

    #[test]
    fn rms_basics_and_bad_input() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 100]), 0.0);
        assert!((rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[f32::NAN, f32::INFINITY]), 0.0);
        assert!(rms(&[1e30, 1e30]).is_finite());
    }

    #[test]
    fn db_conversion_is_clamped() {
        assert!((db_to_rms(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_rms(-60.0) - 0.001).abs() < 1e-6);
        assert!((db_to_rms(f32::NAN) - db_to_rms(-70.0)).abs() < 1e-9);
        assert!((db_to_rms(-500.0) - db_to_rms(-120.0)).abs() < 1e-12);
        assert!((db_to_rms(50.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn alerts_once_after_the_limit() {
        let mut t = SilenceTracker::new();
        assert_eq!(t.check(0, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(LIMIT - 1, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(LIMIT, 0, LIMIT, true), Verdict::Alert { silent_ms: LIMIT });
        // same episode: no second alert, however long it lasts
        assert_eq!(t.check(LIMIT * 5, 0, LIMIT, true), Verdict::Ok);
    }

    #[test]
    fn sound_resets_the_counter_and_a_new_episode_alerts_again() {
        let mut t = SilenceTracker::new();
        assert_eq!(t.check(0, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(60_000, 50_000, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(139_000, 50_000, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(140_000, 50_000, LIMIT, true), Verdict::Alert { silent_ms: 90_000 });
        assert_eq!(t.check(150_000, 149_000, LIMIT, true), Verdict::Recovered);
        assert_eq!(t.check(200_000, 149_000, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(239_000, 149_000, LIMIT, true), Verdict::Alert { silent_ms: 90_000 });
    }

    #[test]
    fn not_watching_restarts_the_count() {
        let mut t = SilenceTracker::new();
        assert_eq!(t.check(0, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(80_000, 0, LIMIT, false), Verdict::Ok); // e.g. paused
        assert_eq!(t.check(85_000, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(174_000, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(175_000, 0, LIMIT, true), Verdict::Alert { silent_ms: 90_000 });
    }

    #[test]
    fn pausing_the_watch_does_not_end_or_repeat_an_episode() {
        let mut t = SilenceTracker::new();
        t.check(0, 0, LIMIT, true);
        assert!(matches!(t.check(90_000, 0, LIMIT, true), Verdict::Alert { .. }));
        t.check(100_000, 0, LIMIT, false);
        // watching again, still silent: same episode, no new alert
        assert_eq!(t.check(500_000, 0, LIMIT, true), Verdict::Ok);
        assert_eq!(t.check(900_000, 0, LIMIT, true), Verdict::Ok);
    }

    #[test]
    fn zero_limit_disables_and_clock_going_backwards_is_safe() {
        let mut t = SilenceTracker::new();
        assert_eq!(t.check(1_000_000, 0, 0, true), Verdict::Ok);
        let mut t = SilenceTracker::new();
        assert_eq!(t.check(10, 5_000, LIMIT, true), Verdict::Ok); // last_loud in the "future"
        assert_eq!(t.check(u64::MAX, 0, LIMIT, true), Verdict::Alert { silent_ms: u64::MAX - 10 });
    }

    #[test]
    fn gap_marker_formats() {
        assert_eq!(gap_marker_text(12.0, 0.0), "[audio interrotto 00:12]");
        assert_eq!(gap_marker_text(754.4, 8.2), "[audio interrotto 12:34 (8 s)]");
        assert_eq!(gap_marker_text(3725.0, 3.0), "[audio interrotto 01:02:05 (3 s)]");
    }

    #[test]
    fn gap_marker_survives_bad_numbers() {
        assert_eq!(gap_marker_text(f64::NAN, f64::INFINITY), "[audio interrotto 00:00]");
        assert_eq!(gap_marker_text(-5.0, -1.0), "[audio interrotto 00:00]");
    }
}
