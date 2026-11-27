//! Fork: Kaldi-style log mel filterbank features for the WeSpeaker model (80 bins, 25 ms frames
//! every 10 ms, no dither), then mean normalisation over time, as in WeSpeaker's own pipeline
//! (`torchaudio.compliance.kaldi.fbank` on samples scaled to the 16-bit range).

use realfft::RealFftPlanner;

const SAMPLE_RATE: f32 = 16_000.0;
const FRAME_LEN: usize = 400; // 25 ms
const FRAME_SHIFT: usize = 160; // 10 ms
const FFT_LEN: usize = 512;
const BINS: usize = 80;
const LOW_FREQ: f32 = 20.0;
const PREEMPHASIS: f32 = 0.97;

pub const MEL_BINS: usize = BINS;

fn mel(freq: f32) -> f32 {
    1127.0 * (1.0 + freq / 700.0).ln()
}

/// Triangular filters over the first `FFT_LEN / 2` FFT bins, like Kaldi.
fn mel_banks() -> Vec<Vec<f32>> {
    let (low, high) = (mel(LOW_FREQ), mel(SAMPLE_RATE / 2.0));
    let delta = (high - low) / (BINS + 1) as f32;
    let bin_width = SAMPLE_RATE / FFT_LEN as f32;
    (0..BINS)
        .map(|b| {
            let left = low + b as f32 * delta;
            let center = left + delta;
            let right = center + delta;
            (0..FFT_LEN / 2)
                .map(|i| {
                    let m = mel(bin_width * i as f32);
                    if m > left && m < right {
                        if m <= center {
                            (m - left) / (center - left)
                        } else {
                            (right - m) / (right - center)
                        }
                    } else {
                        0.0
                    }
                })
                .collect()
        })
        .collect()
}

/// Features of 16 kHz mono samples (range -1..1): `frames x 80`, empty when shorter than one frame.
pub fn compute(samples: &[f32]) -> Vec<[f32; BINS]> {
    if samples.len() < FRAME_LEN {
        return Vec::new();
    }
    let frames = 1 + (samples.len() - FRAME_LEN) / FRAME_SHIFT;
    let window: Vec<f32> = (0..FRAME_LEN)
        .map(|i| (0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME_LEN - 1) as f32).cos()).powf(0.85))
        .collect();
    let banks = mel_banks();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut out: Vec<[f32; BINS]> = Vec::with_capacity(frames);
    let mut frame = [0.0_f32; FRAME_LEN];
    for f in 0..frames {
        let start = f * FRAME_SHIFT;
        for (dst, src) in frame.iter_mut().zip(&samples[start..start + FRAME_LEN]) {
            *dst = src * 32768.0;
        }
        let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
        frame.iter_mut().for_each(|v| *v -= mean);
        for i in (1..FRAME_LEN).rev() {
            frame[i] -= PREEMPHASIS * frame[i - 1];
        }
        frame[0] -= PREEMPHASIS * frame[0];
        input.iter_mut().for_each(|v| *v = 0.0);
        for i in 0..FRAME_LEN {
            input[i] = frame[i] * window[i];
        }
        if fft.process(&mut input, &mut spectrum).is_err() {
            return Vec::new();
        }
        let power: Vec<f32> = spectrum.iter().take(FFT_LEN / 2).map(|c| c.re * c.re + c.im * c.im).collect();
        let mut row = [0.0_f32; BINS];
        for (value, bank) in row.iter_mut().zip(&banks) {
            let energy: f32 = bank.iter().zip(&power).map(|(w, p)| w * p).sum();
            *value = energy.max(f32::EPSILON).ln();
        }
        out.push(row);
    }
    // Cepstral mean normalisation over the whole utterance.
    let n = out.len() as f32;
    for bin in 0..BINS {
        let mean = out.iter().map(|row| row[bin]).sum::<f32>() / n;
        out.iter_mut().for_each(|row| row[bin] -= mean);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_count_and_shape() {
        assert!(compute(&vec![0.0; 399]).is_empty());
        let features = compute(&vec![0.1; 16_000]); // 1 s -> 98 frames
        assert_eq!(features.len(), 1 + (16_000 - 400) / 160);
        assert!(features.iter().all(|row| row.iter().all(|v| v.is_finite())));
    }

    #[test]
    fn features_are_mean_normalised() {
        let samples: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let features = compute(&samples);
        for bin in 0..BINS {
            let mean = features.iter().map(|row| row[bin]).sum::<f32>() / features.len() as f32;
            assert!(mean.abs() < 1e-3);
        }
    }

    #[test]
    fn tone_lands_in_the_matching_band() {
        // 1 kHz tone: the loudest band must sit around mel(1000), not at the edges.
        // Tone for half a second, then silence (mean normalisation needs some contrast).
        let samples: Vec<f32> = (0..16_000)
            .map(|i| if i < 8_000 { (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 16_000.0).sin() * 0.5 } else { 0.0 })
            .collect();
        let features = compute(&samples);
        let best = features[20].iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).map(|(i, _)| i).unwrap();
        assert!((20..35).contains(&best), "peak band {}", best);
    }
}
