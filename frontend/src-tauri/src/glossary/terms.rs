//! Glossary term list: cleaning and the Whisper initial prompt. Pure.

pub const MAX_TERMS: usize = 300;
pub const MAX_TERM_CHARS: usize = 60;
/// Whisper reads at most ~224 prompt tokens; stay well under it.
pub const MAX_PROMPT_CHARS: usize = 600;

/// Trims, drops empty or oversized terms and duplicates (case-insensitive), keeps the order.
pub fn clean_terms(raw: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for term in raw {
        let term = term.split_whitespace().collect::<Vec<_>>().join(" ");
        if term.is_empty() || term.chars().count() > MAX_TERM_CHARS || term.chars().any(|c| c.is_control()) {
            continue;
        }
        if seen.insert(term.to_lowercase()) {
            out.push(term);
        }
        if out.len() >= MAX_TERMS {
            break;
        }
    }
    out
}

/// "Glossario: A, B, C." cut at a term boundary; None for an empty glossary.
pub fn initial_prompt(terms: &[String]) -> Option<String> {
    let mut prompt = String::from("Glossario: ");
    let mut any = false;
    for term in terms {
        let extra = term.chars().count() + 2;
        if prompt.chars().count() + extra > MAX_PROMPT_CHARS {
            break;
        }
        if any {
            prompt.push_str(", ");
        }
        prompt.push_str(term);
        any = true;
    }
    if any {
        prompt.push('.');
        Some(prompt)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cleans_terms() {
        assert_eq!(clean_terms(v(&["  Grafana ", "grafana", "", "Visual   Studio", "a\u{7}b", &"x".repeat(61)])), v(&["Grafana", "Visual Studio"]));
        let many: Vec<String> = (0..1000).map(|i| format!("term{}", i)).collect();
        assert_eq!(clean_terms(many).len(), MAX_TERMS);
    }

    #[test]
    fn prompt_is_bounded_and_optional() {
        assert_eq!(initial_prompt(&[]), None);
        assert_eq!(initial_prompt(&v(&["A", "B"])), Some("Glossario: A, B.".to_string()));
        let many: Vec<String> = (0..300).map(|i| format!("termine{}", i)).collect();
        let prompt = initial_prompt(&many).unwrap();
        assert!(prompt.chars().count() <= MAX_PROMPT_CHARS + 1);
        assert!(prompt.ends_with('.'));
    }
}
