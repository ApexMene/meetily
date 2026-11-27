//! Fork: time alignment between speaker clusters and the names of who was speaking
//! (`active_speakers.json`, written by the optional Teams active speaker reader). Plain data.

use std::collections::HashMap;

/// Overlap needed before a name is trusted for a cluster.
const MIN_SECONDS: f64 = 3.0;
/// Share of the cluster's named time the winning name needs.
const MIN_SHARE: f64 = 0.6;

fn overlap(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.1.min(b.1) - a.0.max(b.0)).max(0.0)
}

/// Name of each cluster that clearly matches one name. `segments` = (start, end, cluster),
/// `active` = (start, end, name).
pub fn vote(segments: &[(f64, f64, usize)], active: &[(f64, f64, String)]) -> HashMap<usize, String> {
    let mut seconds: HashMap<usize, HashMap<&str, f64>> = HashMap::new();
    for &(start, end, cluster) in segments {
        for (a_start, a_end, name) in active.iter().filter(|(s, e, _)| *e > start && *s < end) {
            *seconds.entry(cluster).or_default().entry(name.as_str()).or_default() += overlap((start, end), (*a_start, *a_end));
        }
    }
    let mut out = HashMap::new();
    for (cluster, names) in seconds {
        let total: f64 = names.values().sum();
        let best = names.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.0.cmp(a.0)));
        if let Some((name, secs)) = best {
            if *secs >= MIN_SECONDS && *secs >= MIN_SHARE * total {
                out.insert(cluster, (*name).to_string());
            }
        }
    }
    out
}

/// Cluster covering most of `start..end`.
pub fn cluster_at(segments: &[(f64, f64, usize)], start: f64, end: f64) -> Option<usize> {
    let mut seconds: HashMap<usize, f64> = HashMap::new();
    for &(s, e, cluster) in segments {
        let o = overlap((s, e), (start, end));
        if o > 0.0 {
            *seconds.entry(cluster).or_default() += o;
        }
    }
    seconds.into_iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.0.cmp(&a.0))).map(|(cluster, _)| cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(name: &str, start: f64, end: f64) -> (f64, f64, String) {
        (start, end, name.to_string())
    }

    #[test]
    fn names_a_cluster_that_overlaps_one_name() {
        let segments = [(0.0, 5.0, 0), (10.0, 15.0, 1), (20.0, 25.0, 0)];
        let names = [active("Anna", 0.0, 6.0), active("Marco", 10.0, 16.0), active("Anna", 19.0, 26.0)];
        let voted = vote(&segments, &names);
        assert_eq!(voted.get(&0).map(String::as_str), Some("Anna"));
        assert_eq!(voted.get(&1).map(String::as_str), Some("Marco"));
    }

    #[test]
    fn ignores_short_or_contested_overlap() {
        let segments = [(0.0, 2.0, 0), (10.0, 16.0, 1)];
        let names = [active("Anna", 0.0, 2.0), active("Marco", 10.0, 13.5), active("Luca", 13.5, 16.0)];
        let voted = vote(&segments, &names);
        assert!(voted.get(&0).is_none(), "2 s is not enough");
        assert!(voted.get(&1).is_none(), "a 58/42 split is not clear");
    }

    #[test]
    fn finds_the_cluster_of_a_segment() {
        let segments = [(0.0, 4.0, 0), (4.0, 10.0, 1)];
        assert_eq!(cluster_at(&segments, 3.0, 9.0), Some(1));
        assert_eq!(cluster_at(&segments, 20.0, 25.0), None);
    }
}
