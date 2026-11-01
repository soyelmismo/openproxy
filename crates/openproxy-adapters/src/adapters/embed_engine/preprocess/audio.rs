//! Log-mel spectrogram extraction for the audio tower.
//!
//! Ported from `Gemma4AudioFeatureExtractor`. The pipeline is: semicausal
//! left padding, 320-sample frames on a 160-sample hop, a periodic Hann
//! window, a 512-point real FFT, an HTK mel filterbank over 0-8 kHz, and
//! `log(mel + 1e-3)`. A frame counts as real audio only when every sample of
//! its analysis window is real, which is what the mask encodes.

use super::{
    AUDIO_FFT, AUDIO_FRAME_LEN, AUDIO_HOP, AUDIO_MEL_BINS, AUDIO_MEL_FLOOR, AUDIO_SAMPLE_RATE,
    AudioSettings,
};

/// HTK mel scale, as used by the reference filterbank.
fn hz_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

/// Triangular mel filterbank, `[fft_bins, mel_bins]` row-major, unnormalized.
///
/// The uppermost filter can fall between two FFT bins and end up all zero;
/// that is expected with 257 bins and 128 filters, and harmless.
pub struct MelFilterbank {
    bins: usize,
    values: Vec<f64>,
}

impl MelFilterbank {
    pub fn new() -> Self {
        let bins = AUDIO_FFT / 2 + 1;
        let mel_min = hz_to_mel(0.0);
        let mel_max = hz_to_mel(8000.0);
        let points: Vec<f64> = (0..AUDIO_MEL_BINS + 2)
            .map(|i| {
                mel_to_hz(mel_min + (mel_max - mel_min) * i as f64 / (AUDIO_MEL_BINS + 1) as f64)
            })
            .collect();
        let bin_hz: Vec<f64> = (0..bins)
            .map(|j| j as f64 * (AUDIO_SAMPLE_RATE as f64 / 2.0) / (bins - 1) as f64)
            .collect();

        let mut values = vec![0.0f64; bins * AUDIO_MEL_BINS];
        for i in 0..AUDIO_MEL_BINS {
            let (lo, center, hi) = (points[i], points[i + 1], points[i + 2]);
            for (j, &f) in bin_hz.iter().enumerate() {
                let w = if f > lo && f <= center {
                    (f - lo) / (center - lo)
                } else if f > center && f < hi {
                    (hi - f) / (hi - center)
                } else {
                    0.0
                };
                values[j * AUDIO_MEL_BINS + i] = w;
            }
        }
        Self { bins, values }
    }

    /// Apply the filterbank to a magnitude spectrum.
    ///
    /// Accumulates in `f64` like the reference: the mel bins span 257 FFT bins
    /// and small magnitudes sit right at the `1e-3` log floor, where `f32`
    /// accumulation drifts past the parity tolerance.
    fn apply(&self, magnitude: &[f64], out: &mut [f64]) {
        debug_assert_eq!(magnitude.len(), self.bins);
        debug_assert_eq!(out.len(), AUDIO_MEL_BINS);
        for (j, &m) in magnitude.iter().enumerate() {
            if m == 0.0 {
                continue;
            }
            let row = &self.values[j * AUDIO_MEL_BINS..(j + 1) * AUDIO_MEL_BINS];
            for (o, &w) in out.iter_mut().zip(row) {
                *o += m * w;
            }
        }
    }
}

impl Default for MelFilterbank {
    fn default() -> Self {
        Self::new()
    }
}

/// Periodic Hann window of `AUDIO_FRAME_LEN` samples.
fn hann_window() -> Vec<f64> {
    (0..AUDIO_FRAME_LEN)
        .map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / AUDIO_FRAME_LEN as f64).cos())
        .collect()
}

/// In-place radix-2 complex FFT. `re`/`im` length must be a power of two.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    debug_assert_eq!(re.len(), im.len());

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut start = 0usize;
        while start < n {
            let (mut cur_r, mut cur_i) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cur_r - im[b] * cur_i, re[b] * cur_i + im[b] * cur_r);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let (nr, ni) = (cur_r * wr - cur_i * wi, cur_r * wi + cur_i * wr);
                cur_r = nr;
                cur_i = ni;
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Number of mel frames produced for a waveform of `samples` samples.
///
/// Mirrors the reference unfold: left-pad by `frame_length / 2`, then take
/// windows of `frame_length + 1` samples every `hop_length` samples.
pub fn audio_frame_count(samples: usize) -> usize {
    let padded = samples + AUDIO_FRAME_LEN / 2;
    let size = AUDIO_FRAME_LEN + 1;
    if padded < size {
        return 0;
    }
    (padded - size) / AUDIO_HOP + 1
}

/// Compute the log-mel spectrogram and its frame mask.
///
/// Returns `(spectrogram, mask, frames)` where `spectrogram` is row-major
/// `[frames, AUDIO_MEL_BINS]` and `mask[i]` marks frame `i` as real audio.
pub fn audio_features(
    settings: AudioSettings,
    samples: &[f32],
) -> Result<(Vec<f32>, Vec<bool>, usize), super::CoreError> {
    use super::CoreError;

    let limit = settings
        .max_samples
        .clamp(AUDIO_HOP, super::AUDIO_MAX_SAMPLES);
    if samples.is_empty() {
        return Err(CoreError::Validation("audio input is empty".into()));
    }
    if samples.len() > limit {
        return Err(CoreError::Validation(format!(
            "audio input of {} samples exceeds the {} sample limit",
            samples.len(),
            limit
        )));
    }

    let pad_left = AUDIO_FRAME_LEN / 2;
    let size = AUDIO_FRAME_LEN + 1;
    let frames = audio_frame_count(samples.len());
    if frames == 0 {
        return Err(CoreError::Validation(
            "audio too short to produce a single mel frame".into(),
        ));
    }

    // Padded waveform and its validity mask; padding is silence and is not
    // real audio, so the mask starts false there.
    let total = samples.len() + pad_left;
    let mut padded = vec![0.0f32; total];
    padded[pad_left..].copy_from_slice(samples);
    let mut padded_mask = vec![false; total];
    for slot in padded_mask.iter_mut().skip(pad_left) {
        *slot = true;
    }

    let window = hann_window();
    let filterbank = MelFilterbank::new();
    let mut spectrogram = vec![0.0f32; frames * AUDIO_MEL_BINS];
    let mut mask = vec![false; frames];
    // The reference transforms in float64; matching it keeps the log-mel
    // parity exact instead of drifting at the 1e-3 log floor.
    let mut re = vec![0.0f64; AUDIO_FFT];
    let mut im = vec![0.0f64; AUDIO_FFT];
    let mut magnitude = vec![0.0f64; AUDIO_FFT / 2 + 1];
    let mut mel = vec![0.0f64; AUDIO_MEL_BINS];

    for (frame, (out_row, mask_slot)) in spectrogram
        .chunks_exact_mut(AUDIO_MEL_BINS)
        .zip(mask.iter_mut())
        .enumerate()
    {
        let start = frame * AUDIO_HOP;
        // Window the frame, then drop the last sample as the reference does.
        let source = &padded[start..start + AUDIO_FRAME_LEN];
        for (slot, (&sample, &weight)) in re.iter_mut().zip(source.iter().zip(window.iter())) {
            *slot = f64::from(sample) * weight;
        }
        // The 512-point FFT zero-pads the 320-sample frame. `re` holds the
        // previous frame's spectrum after `fft`, so the tail must be cleared
        // or every frame after the first is computed on stale data.
        re[AUDIO_FRAME_LEN..].fill(0.0);
        im.fill(0.0);

        fft(&mut re, &mut im);
        for (m, (&r, &i)) in magnitude.iter_mut().zip(re.iter().zip(im.iter())) {
            *m = (r * r + i * i).sqrt();
        }
        mel.fill(0.0);
        filterbank.apply(&magnitude, &mut mel);
        for (o, &m) in out_row.iter_mut().zip(&mel) {
            *o = (m + f64::from(AUDIO_MEL_FLOOR)).ln() as f32;
        }

        // A frame is real only when the last sample of its window is.
        let end = start + size - 1;
        *mask_slot = padded_mask.get(end).copied().unwrap_or(false);
        if !*mask_slot {
            for v in out_row.iter_mut() {
                *v = 0.0;
            }
        }
    }

    Ok((spectrogram, mask, frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_of_a_dc_signal_puts_all_energy_in_bin_zero() {
        let mut re = vec![1.0f64; 8];
        let mut im = vec![0.0f64; 8];
        fft(&mut re, &mut im);
        assert!((re[0] - 8.0).abs() < 1e-4);
        for &r in &re[1..] {
            assert!(r.abs() < 1e-4);
        }
    }

    #[test]
    fn fft_matches_a_known_sine() {
        // sin(2*pi*k/8) has magnitude 4 at bins 1 and 7.
        let n = 8usize;
        let mut re: Vec<f64> = (0..n)
            .map(|k| (2.0 * std::f64::consts::PI * k as f64 / n as f64).sin())
            .collect();
        let mut im = vec![0.0f64; n];
        fft(&mut re, &mut im);
        assert!((re[1] - 0.0).abs() < 1e-4);
        assert!((im[1] + 4.0).abs() < 1e-4, "im[1]={}", im[1]);
    }

    #[test]
    fn hann_window_is_periodic_and_positive_inside() {
        let w = hann_window();
        assert_eq!(w.len(), AUDIO_FRAME_LEN);
        assert!(w[0].abs() < 1e-6);
        assert!((w[AUDIO_FRAME_LEN / 2] - 1.0).abs() < 1e-6);
        assert!(w.iter().all(|&v| v >= 0.0));
    }

    #[test]
    fn mel_filterbank_is_triangular_and_finite() {
        let fb = MelFilterbank::new();
        assert_eq!(fb.bins, AUDIO_FFT / 2 + 1);
        assert!(fb.values.iter().all(|v| v.is_finite() && *v >= 0.0));
        // Each interior filter rises then falls back to zero.
        for i in 1..AUDIO_MEL_BINS - 1 {
            let column: Vec<f64> = (0..fb.bins)
                .map(|j| fb.values[j * AUDIO_MEL_BINS + i])
                .collect();
            let peak = column.iter().copied().fold(0.0f64, f64::max);
            assert!(peak > 0.0, "filter {i} is all zero");
            assert!(peak <= 1.0 + 1e-6, "filter {i} peaks above 1.0");
        }
    }

    #[test]
    fn empty_and_short_audio_are_rejected() {
        let settings = AudioSettings::default();
        assert!(audio_features(settings, &[]).is_err());
        assert!(audio_features(settings, &[0.0; 10]).is_err());
    }

    #[test]
    fn oversize_audio_is_rejected() {
        let settings = AudioSettings { max_samples: 1000 };
        assert!(audio_features(settings, &vec![0.0; 2000]).is_err());
    }

    #[test]
    fn silence_yields_a_flat_spectrogram_at_the_mel_floor() {
        let settings = AudioSettings::default();
        let (spec, mask, frames) = audio_features(settings, &vec![0.0; 16_000]).unwrap();
        assert_eq!(frames, mask.len());
        assert_eq!(spec.len(), frames * AUDIO_MEL_BINS);
        // log(0 + 1e-3) for every bin of every frame.
        let floor = AUDIO_MEL_FLOOR.ln();
        assert!(spec.iter().all(|&v| (v - floor).abs() < 1e-5));
    }
}
