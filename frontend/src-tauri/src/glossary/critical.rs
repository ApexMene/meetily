//! Values that must not be wrong in a recap or a task (numbers, versions like 1.0.0g, IPs, dates,
//! times) and the comparison of two readings of the same speech. When the readings disagree about
//! such a value, the value in the final text gets a " (?)" mark so nobody trusts it blindly.

pub const MARK: &str = " (?)";

/// Added to the Claude prompts (clean-up, recap, tasks) so the mark survives into the results.
pub const PROMPT_NOTE: &str = "UNCERTAIN VALUES: in the transcript a value (number, version, IP, date, time) followed by \"(?)\" was heard differently by two transcription passes. Never state such a value as certain: keep it with the \"(?)\" mark everywhere you use it (summary, decisions and every task, e.g. \"upgrade to 1.0.0g (?)\"), and in tasks add that the value is to be verified.";

fn trim_edges(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric())
}

/// True when the word carries a value worth double-checking: any word with a digit.
pub fn is_critical_word(word: &str) -> bool {
    trim_edges(word).chars().any(|c| c.is_ascii_digit())
}

/// Comparison key: lowercase, with ':' and ',' read as '.' (10:30 == 10.30, 1,5 == 1.5).
pub fn key(word: &str) -> String {
    trim_edges(word)
        .to_lowercase()
        .chars()
        .map(|c| if c == ':' || c == ',' { '.' } else { c })
        .collect()
}

/// The critical words of a text, in order.
pub fn critical_words(text: &str) -> Vec<String> {
    text.split_whitespace().filter(|w| is_critical_word(w)).map(|w| trim_edges(w).to_string()).collect()
}

/// True when a segment is worth a second look (it carries a critical value).
pub fn is_critical_segment(text: &str) -> bool {
    text.split_whitespace().any(is_critical_word)
}

#[derive(Debug, PartialEq)]
pub struct Reconciled {
    pub text: String,
    /// The values marked as uncertain.
    pub uncertain: Vec<String>,
}

/// `first` is the live reading, `second` the precise one (kept as the final text). Every critical
/// value of `second` that the first reading does not have is marked. Nothing is marked when the
/// first reading has no critical value at all (it may simply have missed or spelled out the
/// number), and a value that is already marked is not marked twice.
pub fn reconcile(first: &str, second: &str) -> Reconciled {
    let first_keys: Vec<String> = first.split_whitespace().filter(|w| is_critical_word(w)).map(key).collect();
    if first_keys.is_empty() || !is_critical_segment(second) {
        return Reconciled { text: second.to_string(), uncertain: Vec::new() };
    }
    let mut uncertain = Vec::new();
    let mut out = String::with_capacity(second.len() + 16);
    let words: Vec<&str> = second.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        let already_marked = words.get(index + 1) == Some(&"(?)");
        if is_critical_word(word) && !already_marked && !first_keys.contains(&key(word)) {
            uncertain.push(trim_edges(word).to_string());
            out.push_str(MARK);
        }
    }
    Reconciled { text: out, uncertain }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_critical_kinds() {
        for text in ["versione 1.0.0g", "ip 10.20.30.40", "il 12/05/2026", "alle 10:30", "sono 1500 euro", "porta 8443."] {
            assert!(is_critical_segment(text), "{}", text);
        }
        for text in ["niente di speciale", "", "   ", "venti persone, una riunione"] {
            assert!(!is_critical_segment(text), "{}", text);
        }
        assert_eq!(critical_words("rilascio 1.0.0g, ip (10.0.0.1) domani"), vec!["1.0.0g", "10.0.0.1"]);
    }

    #[test]
    fn agreeing_readings_are_not_marked() {
        let r = reconcile("rilascio 1.0.0g alle 10:30", "Rilascio 1.0.0g alle 10.30");
        assert_eq!(r.uncertain, Vec::<String>::new());
        assert_eq!(r.text, "Rilascio 1.0.0g alle 10.30");
    }

    #[test]
    fn disagreement_marks_only_the_value() {
        let r = reconcile("la versione 1.0.0 va su 10.0.0.5", "la versione 1.0.0g va su 10.0.0.5");
        assert_eq!(r.uncertain, vec!["1.0.0g"]);
        assert_eq!(r.text, "la versione 1.0.0g (?) va su 10.0.0.5");
    }

    #[test]
    fn missing_number_in_first_reading_is_not_a_conflict() {
        assert!(reconcile("duemila euro", "2000 euro").uncertain.is_empty());
        assert!(reconcile("", "2000 euro").uncertain.is_empty());
        assert!(reconcile("1.0.0", "niente numeri qui").uncertain.is_empty());
    }

    #[test]
    fn already_marked_values_stay_single_and_input_is_safe() {
        let r = reconcile("v 2.0", "v 3.0 (?) e 4.0");
        assert_eq!(r.text, "v 3.0 (?) e 4.0 (?)");
        assert_eq!(r.uncertain, vec!["4.0"]);
        assert_eq!(reconcile("", "").text, "");
        assert_eq!(reconcile("1", "").uncertain.len(), 0);
    }
}
