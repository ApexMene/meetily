//! Fork: grouping of speaker embeddings and matching with known voices. Plain vectors, no I/O.

pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Cosine similarity; 0 for empty, mismatched or zero vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (a.iter().map(|x| x * x).sum::<f32>().sqrt(), b.iter().map(|x| x * x).sum::<f32>().sqrt());
    if na < 1e-9 || nb < 1e-9 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Mean of unit vectors, re-normalised.
pub fn centroid(vectors: &[&[f32]]) -> Vec<f32> {
    let Some(first) = vectors.first() else { return Vec::new() };
    let len = first.len();
    let mut sum = vec![0.0_f32; len];
    for v in vectors.iter().filter(|v| v.len() == len) {
        sum.iter_mut().zip(*v).for_each(|(s, x)| *s += x);
    }
    normalize(&mut sum);
    sum
}

/// Cluster index of every embedding. Greedy pass in time order (an embedding joins the closest
/// cluster when its centroid is at least `threshold` similar, otherwise starts one), then clusters
/// whose centroids are at least `threshold` similar are merged. Indices are numbered by first
/// appearance.
pub fn cluster(embeddings: &[Vec<f32>], threshold: f32) -> Vec<usize> {
    let mut members: Vec<Vec<usize>> = Vec::new();
    let mut centers: Vec<Vec<f32>> = Vec::new();
    for (i, embedding) in embeddings.iter().enumerate() {
        let best = centers
            .iter()
            .enumerate()
            .map(|(k, center)| (k, cosine(center, embedding)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        match best {
            Some((k, score)) if score >= threshold => {
                members[k].push(i);
                centers[k] = centroid(&members[k].iter().map(|&m| embeddings[m].as_slice()).collect::<Vec<_>>());
            }
            _ => {
                members.push(vec![i]);
                centers.push(embedding.clone());
            }
        }
    }
    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for a in 0..centers.len() {
            for b in a + 1..centers.len() {
                let score = cosine(&centers[a], &centers[b]);
                if score >= threshold && best.map_or(true, |(_, _, s)| score > s) {
                    best = Some((a, b, score));
                }
            }
        }
        let Some((a, b, _)) = best else { break };
        let moved = members.remove(b);
        centers.remove(b);
        members[a].extend(moved);
        centers[a] = centroid(&members[a].iter().map(|&m| embeddings[m].as_slice()).collect::<Vec<_>>());
    }
    let mut labels = vec![0; embeddings.len()];
    let mut order: Vec<(usize, usize)> = members.iter().enumerate().map(|(k, m)| (m.iter().copied().min().unwrap_or(0), k)).collect();
    order.sort();
    for (rank, (_, k)) in order.into_iter().enumerate() {
        for &m in &members[k] {
            labels[m] = rank;
        }
    }
    labels
}

/// Index of the best known voice for `embedding` when at least `threshold` similar.
pub fn best_match<'a>(embedding: &[f32], known: impl Iterator<Item = &'a [f32]>, threshold: f32) -> Option<(usize, f32)> {
    known
        .enumerate()
        .map(|(i, voice)| (i, cosine(voice, embedding)))
        .filter(|(_, score)| *score >= threshold)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut v = v.to_vec();
        normalize(&mut v);
        v
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn clusters_two_voices_numbered_by_appearance() {
        let a = [unit(&[1.0, 0.1, 0.0]), unit(&[0.9, 0.0, 0.1]), unit(&[1.0, 0.0, 0.0])];
        let b = [unit(&[0.0, 1.0, 0.1]), unit(&[0.1, 0.9, 0.0])];
        let embeddings = vec![a[0].clone(), b[0].clone(), a[1].clone(), b[1].clone(), a[2].clone()];
        assert_eq!(cluster(&embeddings, 0.8), vec![0, 1, 0, 1, 0]);
    }

    #[test]
    fn empty_input() {
        assert!(cluster(&[], 0.5).is_empty());
    }

    #[test]
    fn matches_the_closest_known_voice_above_the_threshold() {
        let known = [unit(&[1.0, 0.0]), unit(&[0.0, 1.0])];
        let probe = unit(&[0.1, 1.0]);
        let found = best_match(&probe, known.iter().map(|v| v.as_slice()), 0.6);
        assert_eq!(found.map(|(i, _)| i), Some(1));
        assert!(best_match(&unit(&[1.0, 1.0]), known.iter().map(|v| v.as_slice()), 0.9).is_none());
    }
}
