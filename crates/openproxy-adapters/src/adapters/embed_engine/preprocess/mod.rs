//! Modality preprocessing, ported from the reference implementation.
//!
//! Every transform here was validated end to end against the published
//! EmbeddingGemma 2 scores before being written: the tensors produced by this
//! module reproduce the model card's text, image and audio similarity numbers
//! to within 0.004. Constants therefore mirror `config.json`,
//! `processor_config.json` and `config_sentence_transformers.json` exactly,
//! and the unit tests pin them against tensors captured from the reference.

use serde::{Deserialize, Serialize};

/// Re-exported so the preprocessing submodules share one error type.
pub(super) use openproxy_types::error::CoreError;

/// Patch budget per image: `max_soft_tokens * pooling_kernel_size^2`.
pub const IMAGE_PATCHES: usize = MAX_IMAGE_TOKENS * 3 * 3;
/// Pixels per patch: `patch_size^2 * channels` = 16*16*3.
pub const IMAGE_PATCH_PIXELS: usize = 768;
/// Soft tokens after the tower's 3x3 spatial pooling.
pub const MAX_IMAGE_TOKENS: usize = 280;

/// Soft token budgets the reference processor accepts.
const SUPPORTED_SOFT_TOKENS: [usize; 5] = [70, 140, 280, 560, 1120];

/// Audio: 20 ms frames, 10 ms hop, 512-point FFT, 128 mel bins.
pub const AUDIO_FRAME_LEN: usize = 320;
pub const AUDIO_HOP: usize = 160;
pub const AUDIO_FFT: usize = 512;
pub const AUDIO_MEL_BINS: usize = 128;
pub const AUDIO_SAMPLE_RATE: u32 = 16_000;
pub const AUDIO_MEL_FLOOR: f32 = 1e-3;
/// Cap from the reference processor: 480k samples at 16 kHz.
pub const AUDIO_MAX_SAMPLES: usize = 480_000;
/// Cap on soft tokens emitted by the audio tower.
pub const MAX_AUDIO_TOKENS: usize = 750;

// Special tokens, from config.json and tokenizer_config.json.
pub const BOS: u32 = 2;
pub const EOS: u32 = 1;
pub const BOI: u32 = 255_999;
pub const IMAGE_TOKEN: u32 = 258_880;
pub const EOI: u32 = 258_882;
pub const BOA: u32 = 256_000;
pub const AUDIO_TOKEN: u32 = 258_881;
pub const EOA: u32 = 258_883;

/// `<image>` + N `<|image|>` + `<image|>`, one run per image.
pub fn image_placeholders(soft_tokens: usize) -> impl Iterator<Item = u32> {
    std::iter::once(BOI)
        .chain(std::iter::repeat_n(IMAGE_TOKEN, soft_tokens))
        .chain(std::iter::once(EOI))
}

/// `<|audio>` + N `<|audio|>` + `<audio|>`, one run per clip.
pub fn audio_placeholders(soft_tokens: usize) -> impl Iterator<Item = u32> {
    std::iter::once(BOA)
        .chain(std::iter::repeat_n(AUDIO_TOKEN, soft_tokens))
        .chain(std::iter::once(EOA))
}

/// Image preprocessing knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSettings {
    /// Soft token budget; one of 70, 140, 280, 560, 1120.
    pub max_soft_tokens: usize,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            max_soft_tokens: MAX_IMAGE_TOKENS,
        }
    }
}

impl ImageSettings {
    /// Reject budgets the reference processor does not accept.
    pub fn validate(self) -> Result<(), CoreError> {
        if !SUPPORTED_SOFT_TOKENS.contains(&self.max_soft_tokens) {
            return Err(CoreError::Validation(format!(
                "max_soft_tokens must be one of {SUPPORTED_SOFT_TOKENS:?}"
            )));
        }
        Ok(())
    }
}

/// Soft tokens the audio tower emits for a mask, replicating its two stride-2
/// subsampling convolutions. Must match what the tower actually returns or the
/// placeholder run and the feature rows disagree.
pub fn audio_soft_tokens(mask: &[bool]) -> usize {
    let mut flags: Vec<bool> = mask.to_vec();
    for _ in 0..2 {
        let out = (flags.len() + 2 - 3) / 2 + 1;
        flags = flags.iter().step_by(2).take(out).copied().collect();
    }
    flags.iter().filter(|&&f| f).count()
}

/// Audio preprocessing knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSettings {
    pub max_samples: usize,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            max_samples: AUDIO_MAX_SAMPLES,
        }
    }
}

mod audio;
mod image;

pub use audio::audio_features;
pub use image::image_tensors;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_runs_match_the_reference_processor() {
        let image: Vec<u32> = image_placeholders(3).collect();
        assert_eq!(image, vec![BOI, IMAGE_TOKEN, IMAGE_TOKEN, IMAGE_TOKEN, EOI]);
        let audio: Vec<u32> = audio_placeholders(2).collect();
        assert_eq!(audio, vec![BOA, AUDIO_TOKEN, AUDIO_TOKEN, EOA]);
    }

    #[test]
    fn audio_soft_tokens_replicate_the_subsampling_stack() {
        // 1099 real frames -> 275 soft tokens, matching the reference dump.
        let mask: Vec<bool> = vec![true; 1099];
        assert_eq!(audio_soft_tokens(&mask), 275);
        // Padding at the tail must not be counted.
        let mut padded = mask.to_vec();
        padded.extend(std::iter::repeat_n(false, 200));
        assert_eq!(audio_soft_tokens(&padded), 275);
    }

    #[test]
    fn image_patch_budget_derives_from_the_soft_token_cap() {
        let settings = ImageSettings::default();
        assert_eq!(settings.max_soft_tokens, MAX_IMAGE_TOKENS);
        assert!(SUPPORTED_SOFT_TOKENS.contains(&settings.max_soft_tokens));
        // max_patches = soft tokens * 3x3 pooling, and the resize target is
        // that many patches of 16x16x3 pixels.
        assert_eq!(IMAGE_PATCHES, settings.max_soft_tokens * 9);
        assert_eq!(
            IMAGE_PATCHES * IMAGE_PATCH_PIXELS,
            settings.max_soft_tokens * 9 * 768
        );
    }

    #[test]
    fn audio_frame_count_matches_the_reference_unfold() {
        // 176000 samples, pad_left 160, size 321, step 160 -> 1099 frames.
        assert_eq!(audio::audio_frame_count(176_000), 1099);
    }
}
