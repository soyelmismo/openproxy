use super::*;
use crate::images::png_mask::{paeth_predictor, png_crc32_chunk, write_png_chunk};
use openproxy_db as core_db;
use openproxy_types::CoreError;
use openproxy_types::ids::{ProviderId, RequestId};
use std::path::PathBuf;

fn fresh_pool() -> (core_db::DbPool, PathBuf) {
    let pool = core_db::DbPool::test_pool_with_prefix("openproxy-image-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

#[test]
fn test_resolve_image_targets_not_found() {
    let (pool, _dir) = fresh_pool();
    let plan = RoutingPlan::NotFound {
        model: "nonexistent-model".into(),
        hint: None,
    };
    let res = resolve_image_targets(&pool, plan, "nonexistent-model", None, Instant::now());
    assert!(matches!(res, Err(CoreError::ModelNotFound { .. })));
}

#[test]
fn test_record_image_usage_row() {
    let (pool, _dir) = fresh_pool();
    let provider = ProviderId::new("openai");
    record_unary_usage(
        &pool,
        &UnaryUsageArgs {
            request_id: RequestId::new(),
            api_key_id: None,
            provider_id: &provider,
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "dall-e-3",
            prompt_tokens: None,
            completion_tokens: None,
            status_code: 200,
            error_msg: None,
            total_ms: 120,
            endpoint_kind: EndpointKind::Image,
        },
    );

    let r = pool.reader();
    let count: i64 = r
        .query_row(
            "SELECT COUNT(*) FROM usage WHERE endpoint_kind = 'image'",
            [],
            |row| row.get(0),
        )
        .expect("query usage");
    assert_eq!(count, 1);
}

#[test]
fn test_paeth_predictor() {
    assert_eq!(paeth_predictor(10, 10, 10), 10);
    assert_eq!(paeth_predictor(50, 100, 20), 100);
    assert_eq!(paeth_predictor(100, 50, 20), 100);
}

#[test]
fn test_png_crc32() {
    // CRC32 of chunk b"IEND" with empty data
    let crc = png_crc32_chunk(*b"IEND", &[]);
    assert_eq!(crc, 0xAE42_6082);
}

fn create_test_rgba_png(width: u32, height: u32, pixels: &[[u8; 4]]) -> Vec<u8> {
    let mut raw_scanlines = Vec::new();
    for y in 0..height as usize {
        raw_scanlines.push(0u8); // filter 0
        for x in 0..width as usize {
            raw_scanlines.extend_from_slice(&pixels[y * width as usize + x]);
        }
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw_scanlines, 6);
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8);
    ihdr.push(6); // RGBA
    ihdr.push(0);
    ihdr.push(0);
    ihdr.push(0);
    write_png_chunk(&mut out, *b"IHDR", &ihdr);
    write_png_chunk(&mut out, *b"IDAT", &compressed);
    write_png_chunk(&mut out, *b"IEND", &[]);
    out
}

#[test]
fn test_extract_png_alpha_mask_with_transparency() {
    // 2x2 image: top-left has alpha=0 (transparent), others have alpha=255 (opaque)
    let pixels = [
        [255, 0, 0, 0],     // transparent
        [0, 255, 0, 255],   // opaque
        [0, 0, 255, 255],   // opaque
        [255, 255, 0, 255], // opaque
    ];
    let png = create_test_rgba_png(2, 2, &pixels);
    let mask = extract_png_alpha_mask(&png);
    assert!(mask.is_some(), "expected mask to be extracted");

    let mask_bytes = mask.unwrap();
    assert!(mask_bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
}

#[test]
fn test_extract_png_alpha_mask_opaque_returns_none() {
    // 2x2 image with no transparent pixels
    let pixels = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
    ];
    let png = create_test_rgba_png(2, 2, &pixels);
    let mask = extract_png_alpha_mask(&png);
    assert!(mask.is_none(), "opaque image should return None");
}

#[test]
fn test_extract_png_alpha_mask_invalid_input() {
    assert!(extract_png_alpha_mask(b"").is_none());
    assert!(extract_png_alpha_mask(b"not a png image").is_none());
}

#[test]
fn test_consolidate_image_targets_merges_horde_same_account() {
    let targets = vec![
        ImageTargets {
            provider: ProviderId::new("horde"),
            account_id: None,
            model_row_id: None,
            combo_target_id: None,
            upstream_model: "AlbedoBase XL 3.1".to_string(),
            combo_id: None,
        },
        ImageTargets {
            provider: ProviderId::new("horde"),
            account_id: None,
            model_row_id: None,
            combo_target_id: None,
            upstream_model: "SDXL 1.0".to_string(),
            combo_id: None,
        },
        ImageTargets {
            provider: ProviderId::new("horde"),
            account_id: None,
            model_row_id: None,
            combo_target_id: None,
            upstream_model: "AlbedoBase XL 3.1".to_string(), // duplicate model
            combo_id: None,
        },
        ImageTargets {
            provider: ProviderId::new("openai"),
            account_id: None,
            model_row_id: None,
            combo_target_id: None,
            upstream_model: "dall-e-3".to_string(),
            combo_id: None,
        },
    ];

    let consolidated = consolidate_image_targets(targets);
    assert_eq!(consolidated.len(), 2);
    assert_eq!(consolidated[0].provider.as_str(), "horde");
    assert_eq!(consolidated[0].upstream_model, "AlbedoBase XL 3.1,SDXL 1.0");
    assert_eq!(consolidated[1].provider.as_str(), "openai");
    assert_eq!(consolidated[1].upstream_model, "dall-e-3");
}
