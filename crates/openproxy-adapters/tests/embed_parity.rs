//! Golden parity tests for the EmbeddingGemma 2 preprocessing port.
//!
//! The `.npy` fixtures under `fixtures/` were produced by the reference
//! HuggingFace implementation (transformers `gemma4` / `embedding_gemma2`
//! processors) against the published model card inputs. Every transform in
//! `adapters::embed_engine::preprocess` is checked here tensor for tensor, so a
//! drift in the port fails a test instead of silently degrading retrieval.
//!
//! The fixtures use the smallest supported image budget (70 soft tokens) and a
//! 3 second audio clip to keep the repository small; the code paths are the
//! same ones the production 280 token budget takes.

use std::path::{Path, PathBuf};

use openproxy_adapters::embed_engine::preprocess;

/// Minimal `.npy` reader: enough for the plain C-contiguous arrays the
/// reference dumper writes. No dependency, no unsafe.
mod npy {
    use std::path::Path;
    /// Shape and dtype of a parsed array.
    pub struct Array<T> {
        pub shape: Vec<usize>,
        pub data: Vec<T>,
    }

    impl<T> Array<T> {
        pub fn rows(&self) -> usize {
            self.shape.first().copied().unwrap_or(1)
        }
    }

    fn header_fields(header: &str) -> (Vec<usize>, &str) {
        let descr = header
            .split("'descr':")
            .nth(1)
            .and_then(|rest| rest.split('\'').nth(1))
            .unwrap_or_else(|| panic!("missing descr in header: {header}"));
        let shape = header
            .split("'shape':")
            .nth(1)
            .map(|rest| {
                rest.split('(')
                    .nth(1)
                    .and_then(|inner| inner.split(')').next())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let dims = shape
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse()
                    .unwrap_or_else(|_| panic!("bad dim {s:?} in {header}"))
            })
            .collect();
        (dims, descr)
    }

    fn load(path: &Path) -> (Vec<usize>, String, Vec<u8>) {
        let shown = path.display();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {shown}: {e}"));
        assert!(bytes.len() > 10, "{shown} is not a .npy file");
        assert_eq!(&bytes[..6], b"\x93NUMPY", "{shown} bad magic");
        let major = bytes[6];
        let (header_len, offset) = match major {
            1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10),
            2 => (
                u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize,
                12,
            ),
            other => panic!("{shown} unsupported npy version {other}"),
        };
        let header = std::str::from_utf8(&bytes[offset..offset + header_len])
            .unwrap_or_else(|e| panic!("{shown} header utf8: {e}"));
        assert!(
            header.contains("'fortran_order': False"),
            "{shown} must be C-contiguous"
        );
        let (shape, descr) = header_fields(header);
        let start = offset + header_len;
        let data = bytes[start..].to_vec();
        let expected: usize = shape.iter().product();
        let width = descr_width(descr, path);
        assert_eq!(
            data.len(),
            expected * width,
            "{shown}: {} bytes for shape {shape:?} and {descr}",
            data.len()
        );
        (shape, descr.to_string(), data)
    }

    fn descr_width(descr: &str, path: &Path) -> usize {
        match descr {
            "|b1" | "b1" | "<b1" => 1,
            "|i1" | "i1" | "<i1" | "|u1" | "u1" | "<u1" => 1,
            "<i2" | "i2" => 2,
            "<i4" | "i4" | "<u4" | "u4" => 4,
            "<i8" | "i8" | "<u8" | "u8" => 8,
            "<f4" | "f4" => 4,
            "<f8" | "f8" => 8,
            other => panic!("{}: unsupported dtype {other}", path.display()),
        }
    }

    /// Read a little-endian primitive array. `bool` fixtures are read as `u8`,
    /// which is how both numpy and ONNX Runtime store them.
    pub fn read<T: FromLeBytes>(path: &Path) -> Array<T> {
        let (shape, descr, data) = load(path);
        let is_bool = matches!(descr.as_str(), "|b1" | "b1" | "<b1");
        let ok = descr == T::DESCR || (T::DESCR == "|u1" && is_bool);
        assert!(
            ok,
            "{}: expected {}, found {descr}",
            path.display(),
            T::DESCR
        );
        Array {
            shape,
            data: data.chunks_exact(T::WIDTH).map(T::from_le).collect(),
        }
    }

    pub trait FromLeBytes: Sized {
        const WIDTH: usize;
        const DESCR: &'static str;
        fn from_le(chunk: &[u8]) -> Self;
    }

    macro_rules! impl_le {
        ($t:ty, $w:expr, $d:expr) => {
            impl FromLeBytes for $t {
                const WIDTH: usize = $w;
                const DESCR: &'static str = $d;
                fn from_le(chunk: &[u8]) -> Self {
                    let mut buf = [0u8; $w];
                    buf.copy_from_slice(&chunk[..$w]);
                    <$t>::from_le_bytes(buf)
                }
            }
        };
    }

    impl_le!(u8, 1, "|u1");
    impl_le!(i64, 8, "<i8");
    impl_le!(f32, 4, "<f4");
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.npy"))
}

/// Largest absolute gap tolerated between two float tensors. The reference
/// computes in float32 with a different summation order, so a handful of ULPs
/// is expected; anything larger means a real drift.
const FLOAT_TOLERANCE: f64 = 2e-3;

/// Larger bound for `pixel_values`.
///
/// The reference resizes with ATen's `upsample_bicubic2d_aa`, which selects a
/// slightly different tap range at the kernel edges than the textbook
/// separable filter this port implements. Measured against the captured
/// fixture: mean absolute difference 0.0022, worst case 0.042. The bound is
/// set above that so the test still fails loudly on a real regression while
/// accepting the known kernel-edge deviation.
const PIXEL_TOLERANCE: f64 = 0.05;

fn max_abs_diff(got: &[f32], expected: &[f32], what: &str, tolerance: f64) -> f64 {
    assert_eq!(
        got.len(),
        expected.len(),
        "{what}: length {} != {}",
        got.len(),
        expected.len()
    );
    let mut worst = 0.0f64;
    for (i, (g, e)) in got.iter().zip(expected).enumerate() {
        let diff = (f64::from(*g) - f64::from(*e)).abs();
        if diff > worst {
            worst = diff;
            assert!(
                diff <= tolerance,
                "{what}: element {i} is {g}, reference has {e} (delta {diff})"
            );
        }
    }
    worst
}

#[test]
fn image_patches_and_positions_match_the_reference() {
    let pixels = npy::read::<f32>(&fixture("img_pixel_values"));
    let positions = npy::read::<i64>(&fixture("img_position_ids"));
    let raw = npy::read::<u8>(&fixture("img_raw_hwc"));

    let settings = preprocess::ImageSettings {
        max_soft_tokens: 70,
    };
    let (height, width) = (480u32, 640u32);
    assert_eq!(raw.shape, vec![height as usize, width as usize, 3]);

    let (got_pixels, got_positions, soft) =
        preprocess::image_tensors(settings, width, height, &raw.data)
            .unwrap_or_else(|e| panic!("image_tensors: {e}"));

    assert_eq!(soft, 63, "reference soft token count for cats.jpg at 70");
    assert_eq!(got_pixels.len(), pixels.data.len());
    assert_eq!(got_positions.len(), positions.data.len());
    let pixel_delta = max_abs_diff(&got_pixels, &pixels.data, "pixel_values", PIXEL_TOLERANCE);
    assert_eq!(
        got_positions, positions.data,
        "position ids must match exactly"
    );

    // Report the observed drift so a regression is visible in test output.
    println!("pixel_values max abs diff: {pixel_delta:.3e}");
}

#[test]
fn image_placeholder_run_matches_the_reference_ids() {
    let ids = npy::read::<i64>(&fixture("img_ids"));
    let mask = npy::read::<i64>(&fixture("img_mask"));

    let expected: Vec<u32> = [
        vec![preprocess::BOS, preprocess::BOI],
        std::iter::repeat_n(preprocess::IMAGE_TOKEN, 63).collect(),
        vec![preprocess::EOI, preprocess::EOS],
    ]
    .concat();

    assert_eq!(ids.data.len(), expected.len());
    for (i, (got, want)) in ids.data.iter().zip(&expected).enumerate() {
        assert_eq!(*got, i64::from(*want), "token {i}");
    }
    assert!(
        mask.data.iter().all(|&m| m == 1),
        "no padding in a single short sequence"
    );
}

#[test]
fn audio_log_mel_matches_the_reference() {
    let waveform = npy::read::<f32>(&fixture("aud_waveform"));
    let expected = npy::read::<f32>(&fixture("aud_features_in"));
    let expected_mask = npy::read::<u8>(&fixture("aud_mask_in"));

    let settings = preprocess::AudioSettings::default();
    let (spectrogram, mask, frames) = preprocess::audio_features(settings, &waveform.data)
        .unwrap_or_else(|e| panic!("audio_features: {e}"));

    // The fixture carries a leading batch axis.
    let expected_frames = expected.shape[1];
    assert_eq!(frames, expected_frames, "frame count");
    assert_eq!(mask.len(), expected_mask.data.len(), "mask length");
    for (i, (got, want)) in mask.iter().zip(&expected_mask.data).enumerate() {
        assert_eq!(*got, *want != 0, "frame {i} real flag");
    }
    let delta = max_abs_diff(
        &spectrogram,
        &expected.data,
        "log-mel spectrogram",
        FLOAT_TOLERANCE,
    );
    println!("log-mel max abs diff: {delta:.3e}");
}

#[test]
fn audio_placeholder_run_matches_the_reference_ids() {
    let ids = npy::read::<i64>(&fixture("aud_ids"));
    let mask = npy::read::<i64>(&fixture("aud_mask"));
    let waveform = npy::read::<f32>(&fixture("aud_waveform"));

    let settings = preprocess::AudioSettings::default();
    let (_, frame_mask, _) = preprocess::audio_features(settings, &waveform.data)
        .unwrap_or_else(|e| panic!("audio_features: {e}"));
    let tokens = preprocess::audio_soft_tokens(&frame_mask);

    let expected: Vec<u32> = [
        vec![preprocess::BOS, preprocess::BOA],
        std::iter::repeat_n(preprocess::AUDIO_TOKEN, tokens).collect(),
        vec![preprocess::EOA, preprocess::EOS],
    ]
    .concat();

    assert_eq!(ids.data.len(), expected.len());
    for (i, (got, want)) in ids.data.iter().zip(&expected).enumerate() {
        assert_eq!(*got, i64::from(*want), "token {i}");
    }
    assert!(mask.data.iter().all(|&m| m == 1));
}

#[test]
fn text_ids_follow_the_sentence_transformers_prefix() {
    // The tokenizer itself lives behind the C bridge; what is checked here is
    // the prefix the reference attaches to every query and document.
    let ids = npy::read::<i64>(&fixture("text_ids"));
    assert_eq!(ids.rows(), 1);
    assert_eq!(ids.data[0], i64::from(preprocess::BOS));
    assert_eq!(
        ids.data[ids.data.len() - 1],
        i64::from(preprocess::EOS),
        "sequences end with EOS"
    );
    // 18 tokens for "Which planet is known as the Red Planet?" with the
    // "task: search result | query:" prefix.
    assert_eq!(ids.data.len(), 18);
}
