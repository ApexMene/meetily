//! Prudent glossary correction for engines that take no prompt (Parakeet and others): a word, or a
//! run of words, that is almost a glossary term becomes that term. Pure text logic, never fails.
//!
//! Prudence rules: case-only differences are always fixed; a real spelling difference is fixed
//! only for terms of at least 5 letters, when the first letter matches and the similarity
//! (1 - edit distance / length) reaches `min_similarity`. Words with digits are left alone, and a
//! word that is itself a glossary term is never changed into another one.

/// Longest word we compare (anything longer is left alone, which also bounds the CPU cost).
const MAX_CHARS: usize = 40;
const MIN_TERM_CHARS: usize = 5;

struct Token {
    lead: String,
    core: String,
    trail: String,
    /// Whitespace after the token.
    gap: String,
    locked: bool,
}

fn split_token(raw: &str) -> (String, String, String) {
    let is_edge = |c: char| !c.is_alphanumeric();
    let start = raw.find(|c: char| !is_edge(c)).unwrap_or(raw.len());
    let end = raw.rfind(|c: char| !is_edge(c)).map(|i| i + raw[i..].chars().next().map(char::len_utf8).unwrap_or(1)).unwrap_or(start);
    if end <= start {
        return (raw.to_string(), String::new(), String::new());
    }
    (raw[..start].to_string(), raw[start..end].to_string(), raw[end..].to_string())
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, after) = rest.split_at(word_end);
        let gap_end = after.find(|c: char| !c.is_whitespace()).unwrap_or(after.len());
        let (gap, next) = after.split_at(gap_end);
        let (lead, core, trail) = split_token(word);
        tokens.push(Token { lead, core, trail, gap: gap.to_string(), locked: false });
        rest = next;
    }
    tokens
}

/// Levenshtein distance on chars (two rows).
pub fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            current[j + 1] = (previous[j] + cost).min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    1.0 - edit_distance(&a, &b) as f64 / longest as f64
}

fn has_digit(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
}

/// Corrects `text` against `terms`. `min_similarity` is clamped to 0.7..=1.0.
pub fn correct(text: &str, terms: &[String], min_similarity: f64) -> String {
    if terms.is_empty() || text.trim().is_empty() {
        return text.to_string();
    }
    let min_similarity = if min_similarity.is_finite() { min_similarity.clamp(0.7, 1.0) } else { 0.85 };
    let mut tokens = tokenize(text);
    // Longer terms first, so "Visual Studio Code" wins over "Visual".
    let mut ordered: Vec<(&String, usize)> = terms
        .iter()
        .map(|t| (t, t.split_whitespace().count()))
        .filter(|(t, n)| *n > 0 && t.chars().count() <= MAX_CHARS * 2)
        .collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1));
    let known: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();

    for (term, words) in ordered {
        let cores_of_term: Vec<&str> = term.split_whitespace().collect();
        if tokens.len() < words {
            continue;
        }
        let mut i = 0;
        while i + words <= tokens.len() {
            let window = &tokens[i..i + words];
            if window.iter().any(|t| t.locked || t.core.is_empty()) {
                i += 1;
                continue;
            }
            let joined = window.iter().map(|t| t.core.as_str()).collect::<Vec<_>>().join(" ");
            let exact = joined == **term;
            let case_only = !exact && joined.to_lowercase() == term.to_lowercase();
            let fuzzy = !exact
                && !case_only
                && term.chars().filter(|c| c.is_alphabetic()).count() >= MIN_TERM_CHARS
                && joined.chars().count() <= MAX_CHARS * 2
                && !has_digit(&joined)
                && !known.contains(&joined.to_lowercase())
                && joined.chars().next().map(|c| c.to_lowercase().eq(term.chars().next().unwrap_or(' ').to_lowercase())).unwrap_or(false)
                && similarity(&joined, term) >= min_similarity;
            if exact || case_only || fuzzy {
                if !exact {
                    // Put the term in the window: its words in the first tokens, the rest emptied.
                    let lead = tokens[i].lead.clone();
                    let trail = tokens[i + words - 1].trail.clone();
                    let last_gap = tokens[i + words - 1].gap.clone();
                    for (k, word) in cores_of_term.iter().enumerate() {
                        tokens[i + k].core = (*word).to_string();
                        if k > 0 {
                            tokens[i + k].lead = String::new();
                        }
                        if k + 1 < words {
                            tokens[i + k].trail = String::new();
                        }
                    }
                    tokens[i].lead = lead;
                    tokens[i + words - 1].trail = trail;
                    tokens[i + words - 1].gap = last_gap;
                }
                for token in &mut tokens[i..i + words] {
                    token.locked = true;
                }
                i += words;
            } else {
                i += 1;
            }
        }
    }
    tokens.iter().map(|t| format!("{}{}{}{}", t.lead, t.core, t.trail, t.gap)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fixes_case_and_near_misses() {
        let t = terms(&["Kubernetes", "Grafana", "Itatel"]);
        assert_eq!(correct("abbiamo deployato su kubernetes oggi", &t, 0.82), "abbiamo deployato su Kubernetes oggi");
        assert_eq!(correct("la dashboard Grafanna e' pronta.", &t, 0.82), "la dashboard Grafana e' pronta.");
        assert_eq!(correct("Kubernetis, poi basta", &t, 0.82), "Kubernetes, poi basta");
    }

    #[test]
    fn leaves_unrelated_and_short_words_alone() {
        let t = terms(&["Teams", "Grafana", "Rete"]);
        assert_eq!(correct("i team parlano di grafica", &t, 0.82), "i team parlano di grafica");
        // short term (4 letters): a spelling difference is left alone, a case difference is fixed
        assert_eq!(correct("la rate e la rete", &t, 0.82), "la rate e la Rete");
        assert_eq!(correct("la RETE", &t, 0.82), "la Rete");
    }

    #[test]
    fn multi_word_terms_keep_punctuation_and_spacing() {
        let t = terms(&["Visual Studio Code"]);
        assert_eq!(correct("uso visual studio code, ok", &t, 0.82), "uso Visual Studio Code, ok");
        assert_eq!(correct("uso (visual studio kode)  ok", &t, 0.8), "uso (Visual Studio Code)  ok");
    }

    #[test]
    fn never_touches_digits_and_exact_terms_stay() {
        let t = terms(&["Grafana", "Graphana"]);
        assert_eq!(correct("il release2.0g e' fuori", &terms(&["Release"]), 0.7), "il release2.0g e' fuori");
        // a word that is itself a term is not turned into the other term
        assert_eq!(correct("Grafana e Graphana", &t, 0.8), "Grafana e Graphana");
    }

    #[test]
    fn degenerate_input_is_safe() {
        assert_eq!(correct("", &terms(&["Grafana"]), 0.8), "");
        assert_eq!(correct("ciao", &[], 0.8), "ciao");
        assert_eq!(correct("   ", &terms(&["Grafana"]), 0.8), "   ");
        assert_eq!(correct("...", &terms(&["Grafana"]), 0.8), "...");
        assert_eq!(correct("Grafanna", &terms(&["", "  ", "Grafana"]), f64::NAN), "Grafana");
        assert_eq!(correct("àèì òù Grafanà", &terms(&["Grafana"]), 0.8), "àèì òù Grafana");
        let long = "x".repeat(10_000);
        assert_eq!(correct(&long, &terms(&["Grafana"]), 0.8), long);
    }

    #[test]
    fn similarity_basics() {
        assert!((similarity("abc", "abc") - 1.0).abs() < 1e-9);
        assert!(similarity("abc", "xyz") < 0.1);
        assert_eq!(similarity("", ""), 1.0);
    }
}
