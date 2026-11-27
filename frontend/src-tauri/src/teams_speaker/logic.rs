//! Fork: platform independent part of the Teams active speaker reader: protocol, circuit breaker,
//! timeline. Std only, so it is fully unit tested. Nothing here touches the recording.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Longest wait for the next reading; a slower reader counts as a failure.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Readings are about one per second.
const POLL_SECONDS: f64 = 1.0;
/// Consecutive failures (errors or timeouts) that switch the reader off for the recording.
pub const MAX_FAILURES: u32 = 3;
/// Readings without any Teams window, at the start, after which the reader stops by itself.
pub const IDLE_LIMIT: u32 = 60;
const MAX_SPANS: usize = 20_000;
const MAX_NAME_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
    pub name: String,
}

/// A display name usable as a label; None for anything odd.
pub fn clean_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let ok = !name.is_empty() && name.chars().count() <= MAX_NAME_CHARS && !name.chars().any(char::is_control) && !name.contains(['[', ']']);
    ok.then(|| name.to_string())
}

/// One line of the reader: `T<TAB>name<TAB>name...` (Teams found, these people show as speaking,
/// possibly nobody) or `X` (no Teams window). Anything else is an error.
pub fn parse_line(line: &str) -> Result<Option<Vec<String>>, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line == "X" {
        return Ok(None);
    }
    let mut parts = line.split('\t');
    if parts.next() != Some("T") {
        return Err("unexpected output".into());
    }
    Ok(Some(parts.filter_map(clean_name).collect()))
}

/// Circuit breaker: opens after `MAX_FAILURES` failures in a row; a good reading resets it.
#[derive(Default)]
pub struct Breaker {
    failures: u32,
}

impl Breaker {
    pub fn ok(&mut self) {
        self.failures = 0;
    }
    pub fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
    pub fn is_open(&self) -> bool {
        self.failures >= MAX_FAILURES
    }
}

/// Turns readings into spans of one speaker. Only a single name counts: with several people
/// showing as speaking, nobody is attributed.
#[derive(Default)]
pub struct Collector {
    spans: Vec<Span>,
    open: Option<(f64, f64, String)>, // start, last reading, name
}

impl Collector {
    pub fn push(&mut self, t: f64, names: &[String]) {
        if !t.is_finite() {
            return;
        }
        match names {
            [name] => match &mut self.open {
                Some((_, last, open_name)) if open_name == name && t >= *last && t - *last <= 2.5 * POLL_SECONDS => *last = t,
                _ => {
                    self.close();
                    self.open = Some((t, t, name.clone()));
                }
            },
            _ => self.close(),
        }
    }

    pub fn close(&mut self) {
        if let Some((start, last, name)) = self.open.take() {
            if self.spans.len() < MAX_SPANS {
                self.spans.push(Span { start, end: last + POLL_SECONDS, name });
            }
        }
    }

    pub fn finish(mut self) -> Vec<Span> {
        self.close();
        self.spans
    }
}

/// Where the readings come from.
pub trait Reader {
    /// Next reading: `Ok(None)` = no Teams window; `Err` = failed or timed out.
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<String>>, String>;
}

pub struct Outcome {
    pub spans: Vec<Span>,
    /// Why the reader switched itself off, if it did.
    pub disabled: Option<String>,
}

/// Reads until `stop` is set or the breaker opens. `now` = seconds since the recording started.
pub fn run(reader: &mut dyn Reader, stop: &AtomicBool, now: &dyn Fn() -> f64) -> Outcome {
    let (mut breaker, mut collector) = (Breaker::default(), Collector::default());
    let (mut seen, mut idle) = (false, 0u32);
    let mut disabled = None;
    while !stop.load(Ordering::Relaxed) {
        match reader.read(READ_TIMEOUT) {
            Ok(Some(names)) => {
                breaker.ok();
                seen = true;
                collector.push(now(), &names);
            }
            Ok(None) => {
                breaker.ok();
                collector.close();
                idle += 1;
                if !seen && idle >= IDLE_LIMIT {
                    disabled = Some("no Teams window found".to_string());
                    break;
                }
            }
            Err(error) => {
                breaker.fail();
                collector.close();
                if breaker.is_open() {
                    disabled = Some(format!("{} failures in a row, last: {}", MAX_FAILURES, error));
                    break;
                }
            }
        }
    }
    Outcome { spans: collector.finish(), disabled }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Script(Vec<Result<Option<Vec<String>>, String>>, usize, Option<&'static AtomicBool>);

    impl Reader for Script {
        fn read(&mut self, _: Duration) -> Result<Option<Vec<String>>, String> {
            let item = self.0.get(self.1).cloned().unwrap_or(Err("end".into()));
            self.1 += 1;
            if self.1 >= self.0.len() {
                if let Some(stop) = self.2 {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            item
        }
    }

    fn names(list: &[&str]) -> Result<Option<Vec<String>>, String> {
        Ok(Some(list.iter().map(|s| s.to_string()).collect()))
    }

    fn clock() -> impl Fn() -> f64 {
        let t = Cell::new(0.0);
        move || {
            t.set(t.get() + 1.0);
            t.get()
        }
    }

    #[test]
    fn parses_lines() {
        assert_eq!(parse_line("X\r\n"), Ok(None));
        assert_eq!(parse_line("T"), Ok(Some(vec![])));
        assert_eq!(parse_line("T\tAnna Rossi\tMarco\n"), Ok(Some(vec!["Anna Rossi".into(), "Marco".into()])));
        assert!(parse_line("garbage").is_err());
        assert!(parse_line("").is_err());
        assert_eq!(parse_line("T\t[x]\t\t \tLuca"), Ok(Some(vec!["Luca".into()])));
    }

    #[test]
    fn rejects_odd_names() {
        assert_eq!(clean_name("  Anna "), Some("Anna".into()));
        assert_eq!(clean_name(&"a".repeat(61)), None);
        assert_eq!(clean_name("a\u{0007}b"), None);
        assert_eq!(clean_name("[system] fake"), None);
    }

    #[test]
    fn breaker_opens_on_consecutive_failures_only() {
        let mut b = Breaker::default();
        b.fail();
        b.fail();
        assert!(!b.is_open());
        b.ok();
        b.fail();
        b.fail();
        assert!(!b.is_open());
        b.fail();
        assert!(b.is_open());
    }

    #[test]
    fn merges_readings_and_ignores_overlaps() {
        let mut c = Collector::default();
        let a = vec!["Anna".to_string()];
        c.push(1.0, &a);
        c.push(2.0, &a);
        c.push(3.0, &a);
        c.push(4.0, &["Anna".to_string(), "Marco".to_string()]); // two at once: nobody
        c.push(5.0, &["Marco".to_string()]);
        c.push(9.0, &["Marco".to_string()]); // gap too long: new span
        c.push(f64::NAN, &a);
        let spans = c.finish();
        assert_eq!(
            spans,
            vec![
                Span { start: 1.0, end: 4.0, name: "Anna".into() },
                Span { start: 5.0, end: 6.0, name: "Marco".into() },
                Span { start: 9.0, end: 10.0, name: "Marco".into() },
            ]
        );
    }

    #[test]
    fn breaker_switches_the_reader_off_and_keeps_what_was_read() {
        let stop = AtomicBool::new(false);
        let mut reader = Script(vec![names(&["Anna"]), names(&["Anna"]), Err("boom".into()), Err("timeout".into()), Err("timeout".into()), names(&["Luca"])], 0, None);
        let outcome = run(&mut reader, &stop, &clock());
        assert!(outcome.disabled.is_some());
        assert_eq!(reader.1, 5, "stops reading as soon as the breaker opens");
        assert_eq!(outcome.spans.len(), 1);
        assert_eq!(outcome.spans[0].name, "Anna");
    }

    #[test]
    fn isolated_failures_do_not_disable() {
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let mut reader = Script(vec![Err("x".into()), names(&["Anna"]), Err("x".into()), Err("x".into()), names(&["Anna"])], 0, Some(stop));
        let outcome = run(&mut reader, stop, &clock());
        assert!(outcome.disabled.is_none());
        assert_eq!(outcome.spans.len(), 2);
    }

    #[test]
    fn gives_up_when_teams_never_appears() {
        let stop = AtomicBool::new(false);
        let mut reader = Script((0..200).map(|_| Ok(None)).collect(), 0, None);
        let outcome = run(&mut reader, &stop, &clock());
        assert_eq!(outcome.disabled.as_deref(), Some("no Teams window found"));
        assert_eq!(reader.1 as u32, IDLE_LIMIT);
    }

    #[test]
    fn stop_flag_ends_the_loop() {
        let stop = AtomicBool::new(true);
        let mut reader = Script(vec![names(&["Anna"])], 0, None);
        let outcome = run(&mut reader, &stop, &clock());
        assert!(outcome.spans.is_empty() && outcome.disabled.is_none() && reader.1 == 0);
    }
}
