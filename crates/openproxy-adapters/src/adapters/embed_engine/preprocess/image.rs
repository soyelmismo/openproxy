//! Patchification for the vision tower.
//!
//! Ported from `Gemma4ImageProcessor`. The pipeline is: aspect-ratio
//! preserving resize with a bicubic kernel and antialiasing, rescale to
//! [0, 1], split into 16x16x3 patches, build axial `(x, y)` position ids, and
//! pad the batch to a fixed patch budget with position id -1.

use super::{IMAGE_PATCH_PIXELS, ImageSettings};

/// Bicubic kernel coefficient used by the reference resize.
const CUBIC_A: f64 = -0.75;

fn cubic_convolution(x: f64) -> f64 {
    let ax = x.abs();
    if ax <= 1.0 {
        ((CUBIC_A + 2.0) * ax - (CUBIC_A + 3.0)) * ax * ax + 1.0
    } else if ax < 2.0 {
        // Horner form of A*x^3 - 5A*x^2 + 8A*x - 4A.
        ((CUBIC_A * ax - 5.0 * CUBIC_A) * ax + 8.0 * CUBIC_A) * ax - 4.0 * CUBIC_A
    } else {
        0.0
    }
}

/// Largest `(height, width)` that fit inside the patch budget while staying
/// divisible by `pooling_kernel_size * patch_size`.
///
/// This is the resize target: the image keeps its aspect ratio, and the
/// resulting patch grid always yields a whole number of soft tokens.
pub fn resized_dimensions(
    height: u32,
    width: u32,
    settings: ImageSettings,
) -> Result<(u32, u32), super::CoreError> {
    use super::CoreError;

    let (patch, pooling) = (16u32, 3u32);
    let side_mult = pooling * patch;
    let max_patches = settings.max_soft_tokens * (pooling as usize) * (pooling as usize);

    let (height, width) = (height as u64, width as u64);
    if height == 0 || width == 0 {
        return Err(CoreError::Validation(
            "image dimensions must be non-zero".into(),
        ));
    }
    let total = height * width;
    let target = max_patches as u64 * (patch as u64) * (patch as u64);
    let factor = (target as f64 / total as f64).sqrt();
    let ideal_height = factor * height as f64;
    let ideal_width = factor * width as f64;

    let mut target_height = (ideal_height / side_mult as f64).floor() as u64 * side_mult as u64;
    let mut target_width = (ideal_width / side_mult as f64).floor() as u64 * side_mult as u64;
    if target_height == 0 && target_width == 0 {
        return Err(CoreError::Validation(
            "image is too small to resize to a non-zero patch grid".into(),
        ));
    }
    let max_side = (max_patches / (pooling as usize).pow(2)) as u64 * side_mult as u64;
    if target_height == 0 {
        target_height = side_mult as u64;
        target_width = ((width / height) * side_mult as u64).min(max_side);
    } else if target_width == 0 {
        target_width = side_mult as u64;
        target_height = ((height / width) * side_mult as u64).min(max_side);
    }
    if target_height * target_width > target {
        return Err(CoreError::Validation(format!(
            "resizing {height}x{width} to {target_height}x{target_width} exceeds the patch budget"
        )));
    }
    Ok((target_height as u32, target_width as u32))
}

/// Normalized bicubic weights for one output coordinate along one axis.
///
/// Downscaling widens the kernel support by the scale factor so every
/// contributing source pixel is integrated; out-of-range indices clamp to the
/// edge, matching the reference implementation.
fn axis_weights(in_len: usize, out_len: usize, o: usize) -> Vec<(usize, f32)> {
    let scale = in_len as f64 / out_len as f64;
    let support = if scale > 1.0 { 2.0 * scale } else { 2.0 };
    let center = scale * (o as f64 + 0.5) - 0.5;
    let left = (center - support).ceil() as i64;
    let right = (center + support).floor() as i64;

    let mut weights: Vec<(usize, f64)> = Vec::with_capacity((right - left + 1) as usize);
    let mut sum = 0.0f64;
    for i in left..=right {
        let clamped = i.clamp(0, in_len as i64 - 1) as usize;
        let distance = if scale > 1.0 {
            (i as f64 - center) / scale
        } else {
            i as f64 - center
        };
        let weight = cubic_convolution(distance);
        if weight != 0.0 {
            weights.push((clamped, weight));
            sum += weight;
        }
    }
    if sum == 0.0 {
        // Degenerate kernel: fall back to nearest neighbour.
        let nearest = (center.round() as i64).clamp(0, in_len as i64 - 1) as usize;
        return vec![(nearest, 1.0)];
    }
    let inv = 1.0 / sum;
    weights
        .into_iter()
        .map(|(i, w)| (i, (w * inv) as f32))
        .collect()
}

/// Separable bicubic resize with antialiasing.
///
/// Two explicit passes: columns first, then rows. Each pass keeps its own input
/// and output stride, so no transposition is implied by the indexing.
fn resize_bicubic_aa(
    src: &[f32],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<f32> {
    const CHANNELS: usize = 3;

    // Columns: [src_h, src_w] -> [src_h, dst_w].
    let mut horizontal = vec![0.0f32; src_h * dst_w * CHANNELS];
    for row in 0..src_h {
        let src_row = &src[row * src_w * CHANNELS..][..src_w * CHANNELS];
        let dst_row = &mut horizontal[row * dst_w * CHANNELS..][..dst_w * CHANNELS];
        for (col, out_pixel) in dst_row.chunks_exact_mut(CHANNELS).enumerate() {
            for (i, weight) in axis_weights(src_w, dst_w, col) {
                let src_pixel = &src_row[i * CHANNELS..][..CHANNELS];
                for c in 0..CHANNELS {
                    out_pixel[c] += src_pixel[c] * weight;
                }
            }
        }
    }

    // Rows: [src_h, dst_w] -> [dst_h, dst_w].
    let mut out = vec![0.0f32; dst_h * dst_w * CHANNELS];
    for row in 0..dst_h {
        let dst_row = &mut out[row * dst_w * CHANNELS..][..dst_w * CHANNELS];
        for (i, weight) in axis_weights(src_h, dst_h, row) {
            let src_row = &horizontal[i * dst_w * CHANNELS..][..dst_w * CHANNELS];
            for (out_pixel, src_pixel) in dst_row
                .chunks_exact_mut(CHANNELS)
                .zip(src_row.chunks_exact(CHANNELS))
            {
                for c in 0..CHANNELS {
                    out_pixel[c] += src_pixel[c] * weight;
                }
            }
        }
    }
    out
}

/// Patchify a decoded RGB image.
///
/// Returns `(pixel_values, position_ids, soft_tokens)` where `pixel_values` is
/// row-major `[IMAGE_PATCHES, IMAGE_PATCH_PIXELS]`, `position_ids` is row-major
/// `[IMAGE_PATCHES, 2]` with `-1` on padding, and `soft_tokens` is the number
/// of real patches after 3x3 spatial pooling.
pub fn image_tensors(
    settings: ImageSettings,
    width: u32,
    height: u32,
    rgb: &[u8],
) -> Result<(Vec<f32>, Vec<i64>, usize), super::CoreError> {
    use super::CoreError;

    settings.validate()?;
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected {
        return Err(CoreError::Validation(format!(
            "image buffer holds {} bytes, expected {expected} for {width}x{height} RGB",
            rgb.len()
        )));
    }
    let (target_height, target_width) = resized_dimensions(height, width, settings)?;

    // Rescale to [0, 1] and resize, both in f32.
    let mut source = vec![0.0f32; expected];
    for (dst, &byte) in source.iter_mut().zip(rgb) {
        *dst = f32::from(byte) / 255.0;
    }
    let resized = if (target_height, target_width) != (height, width) {
        resize_bicubic_aa(
            &source,
            width as usize,
            height as usize,
            target_width as usize,
            target_height as usize,
        )
    } else {
        source
    };

    let (patch, pooling) = (16usize, 3usize);
    let (h, w) = (target_height as usize, target_width as usize);
    let (patch_height, patch_width) = (h / patch, w / patch);
    let num_patches = patch_height * patch_width;
    let soft_tokens = num_patches / (pooling * pooling);
    if soft_tokens == 0 {
        return Err(CoreError::Validation(
            "image produced no soft tokens after pooling".into(),
        ));
    }

    // The patch budget follows the requested soft token count, not the
    // production default: a caller asking for 70 soft tokens gets 70*9 patches.
    let max_patches = settings.max_soft_tokens * pooling * pooling;
    let mut pixel_values = vec![0.0f32; max_patches * IMAGE_PATCH_PIXELS];
    let mut position_ids = vec![-1i64; max_patches * 2];

    // Patch layout is (row, col, patch_row, patch_col, channel) flattened in
    // that order, so each patch reads its 768 values as
    // [patch_row][patch_col][channel].
    for py in 0..patch_height {
        for px in 0..patch_width {
            let patch_index = py * patch_width + px;
            let base = patch_index * IMAGE_PATCH_PIXELS;
            for ry in 0..patch {
                for rx in 0..patch {
                    let src = ((py * patch + ry) * w + px * patch + rx) * 3;
                    let dst = base + (ry * patch + rx) * 3;
                    pixel_values[dst..dst + 3].copy_from_slice(&resized[src..src + 3]);
                }
            }
            position_ids[patch_index * 2] = px as i64;
            position_ids[patch_index * 2 + 1] = py as i64;
        }
    }

    Ok((pixel_values, position_ids, soft_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ImageSettings {
        ImageSettings::default()
    }

    #[test]
    fn a_square_image_resizes_to_the_patch_budget() {
        // 1024x1024 with a 2520-patch budget scales to 48-multiples.
        let (h, w) = resized_dimensions(1024, 1024, settings()).unwrap();
        assert_eq!(h % 48, 0);
        assert_eq!(w % 48, 0);
        assert_eq!(h, w);
        assert!((h as u64 * w as u64) <= 2520 * 16 * 16);
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        assert!(resized_dimensions(0, 10, settings()).is_err());
        assert!(resized_dimensions(10, 0, settings()).is_err());
    }

    #[test]
    fn small_images_are_upscaled_to_fill_the_patch_budget() {
        // The reference always scales toward max_patches * patch_area, up or
        // down, so a 48x48 image lands on a 48x48 patch grid (256 soft tokens)
        // and still pads up to the 2520 patch budget.
        let (h, w) = resized_dimensions(48, 48, settings()).unwrap();
        assert_eq!((h, w), (768, 768));
        let soft = usize::try_from(h / 16).unwrap() * usize::try_from(w / 16).unwrap() / 9;
        assert_eq!(soft, 256);
        assert!(soft * 9 <= super::super::IMAGE_PATCHES);
    }

    #[test]
    fn aspect_ratio_is_preserved_within_one_step() {
        let (h, w) = resized_dimensions(480, 640, settings()).unwrap();
        let original = 480.0 / 640.0;
        let resized = h as f64 / w as f64;
        assert!((original - resized).abs() < 0.1, "{h}x{w}");
    }

    #[test]
    fn patchify_pads_to_the_fixed_budget_and_marks_positions() {
        // 48x48 is upscaled to 768x768: 2304 real patches, 216 padding rows.
        let rgb = vec![128u8; 48 * 48 * 3];
        let (pixels, positions, soft) = image_tensors(settings(), 48, 48, &rgb).unwrap();
        assert_eq!(soft, 256);
        assert_eq!(
            pixels.len(),
            super::super::IMAGE_PATCHES * IMAGE_PATCH_PIXELS
        );
        assert_eq!(positions.len(), super::super::IMAGE_PATCHES * 2);
        // Patch grid is 48 wide; the first row runs x = 0..47.
        assert_eq!(&positions[0..2], &[0, 0]);
        assert_eq!(&positions[16..18], &[8, 0]);
        // Real patches end at 2304, everything after is padding.
        assert_eq!(&positions[2304 * 2 - 2..2304 * 2], &[47, 47]);
        assert_eq!(&positions[2304 * 2..2304 * 2 + 2], &[-1, -1]);
        // Padding rows stay zero.
        assert!(
            pixels[2304 * IMAGE_PATCH_PIXELS..]
                .iter()
                .all(|&v| v == 0.0)
        );
    }

    #[test]
    fn rescale_maps_bytes_into_the_unit_range() {
        // A uniform field survives bicubic interpolation unchanged, so this
        // isolates the byte -> float rescale from the resize.
        let rgb = vec![128u8; 48 * 48 * 3];
        let (pixels, _, _) = image_tensors(settings(), 48, 48, &rgb).unwrap();
        assert!(
            pixels[0..3]
                .iter()
                .all(|&v| (v - 128.0 / 255.0).abs() < 1e-6)
        );
    }

    #[test]
    fn mismatched_buffer_size_is_rejected() {
        assert!(image_tensors(settings(), 10, 10, &[0u8; 5]).is_err());
    }

    #[test]
    fn resize_of_an_identity_size_returns_the_input() {
        let src: Vec<f32> = (0..48 * 48 * 3).map(|i| (i % 251) as f32 / 251.0).collect();
        let out = resize_bicubic_aa(&src, 48, 48, 48, 48);
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn resize_preserves_a_constant_image_exactly() {
        let src = vec![0.25f32; 64 * 64 * 3];
        let out = resize_bicubic_aa(&src, 64, 64, 32, 32);
        assert!(out.iter().all(|&v| (v - 0.25).abs() < 1e-6));
    }

    #[test]
    fn cubic_kernel_is_zero_outside_its_support() {
        assert!((cubic_convolution(0.0) - 1.0).abs() < 1e-9);
        assert_eq!(cubic_convolution(2.5), 0.0);
        assert_eq!(cubic_convolution(-3.0), 0.0);
        assert!(cubic_convolution(1.5) < 0.0);
    }

    #[test]
    fn cubic_kernel_matches_the_reference_polynomial() {
        // A*x^3 - 5A*x^2 + 8A*x - 4A for |x| in [1, 2), zero outside.
        let a = CUBIC_A;
        for x in [
            0.0f64, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 1.999, 2.0, 2.5,
        ] {
            let ax = x.abs();
            let expected = if ax <= 1.0 {
                (a + 2.0) * ax * ax * ax - (a + 3.0) * ax * ax + 1.0
            } else if ax < 2.0 {
                a * ax * ax * ax - 5.0 * a * ax * ax + 8.0 * a * ax - 4.0 * a
            } else {
                0.0
            };
            assert!(
                (cubic_convolution(x) - expected).abs() < 1e-12,
                "x={x}: {} != {expected}",
                cubic_convolution(x)
            );
        }
        assert!((cubic_convolution(1.0) - cubic_convolution(-1.0)).abs() < 1e-12);
        assert!(cubic_convolution(2.0).abs() < 1e-12);
    }

    #[test]
    fn tiny_resize_matches_the_reference_weights() {
        // 4x4 ramp downscaled to 2x2. Output coordinate 0 of a 4 -> 2 axis
        // takes these reference bicubic taps, in source order.
        let weights = axis_weights(4, 2, 0);
        let expected = [
            (0usize, -0.017578125f32),
            (0, -0.052734375),
            (0, 0.13085938),
            (0, 0.43945313),
            (1, 0.43945313),
            (2, 0.13085938),
            (3, -0.052734375),
            (3, -0.017578125),
        ];
        assert_eq!(weights.len(), expected.len());
        for ((src, weight), (want_src, want_weight)) in weights.iter().zip(expected) {
            assert_eq!(*src, want_src);
            assert!(
                (weight - want_weight).abs() < 1e-5,
                "tap {src}: {weight} != {want_weight}"
            );
        }

        let src: Vec<f32> = (0..16).flat_map(|n| [n as f32; 3]).collect();
        let out = resize_bicubic_aa(&src, 4, 4, 2, 2);
        let expected = [2.4511719, 4.470703, 10.529297, 12.548828];
        for (i, want) in expected.iter().enumerate() {
            assert!((out[i * 3] - want).abs() < 1e-4, "{} != {want}", out[i * 3]);
        }
    }
}
