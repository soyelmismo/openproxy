//! Round-trip tests for the shared content-decoder, driven one transport frame
//! at a time. No HTTP: the decoder is a pure state machine over `&[u8]`, so
//! the frame boundaries that matter (mid-header, mid-symbol, truncated,
//! oversized) are all directly reachable here.
//!
//! The gzip and zlib containers are built with `miniz_oxide::deflate`, the same
//! crate the decoder inflates with, so the vectors are real encoder output
//! rather than hand-written bytes.

#![cfg(all(feature = "upstream-hyper", test))]

use super::compression::{BodyDecoding, Codec};
use http::{HeaderMap, HeaderValue, header};
use std::sync::LazyLock;

// Vectors shared with the HTTP-level tests.

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: LazyLock<[u32; 256]> = LazyLock::new(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut crc = u32::try_from(index).unwrap_or_default();
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xEDB8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
            *slot = crc;
        }
        table
    });
    &TABLE
}

pub(super) fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFF;
    for byte in data {
        let index = usize::try_from((crc ^ u32::from(*byte)) & 0xff).unwrap_or_default();
        crc = table.get(index).copied().unwrap_or_default() ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// A complete RFC 1952 container: fixed header, the optional fields selected by
/// `flg`, a raw DEFLATE body, then CRC32 and ISIZE.
pub(super) fn gzip_container(payload: &[u8], flg: u8) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, flg, 0, 0, 0, 0, 0x00, 0xff];
    if flg & 0x04 != 0 {
        out.extend_from_slice(&[0x02, 0x00]); // XLEN = 2
        out.extend_from_slice(b"XY"); // FEXTRA
    }
    if flg & 0x08 != 0 {
        out.extend_from_slice(b"payload.txt\0"); // FNAME
    }
    if flg & 0x10 != 0 {
        out.extend_from_slice(b"a comment\0"); // FCOMMENT
    }
    if flg & 0x02 != 0 {
        // FHCRC: the low 16 bits of the CRC32 of the header so far. The
        // decoder verifies this, so it must be the real value.
        let crc16 = u16::try_from(crc32(&out) & 0xffff).unwrap_or_default();
        out.extend_from_slice(&crc16.to_le_bytes());
    }
    out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(payload, 6));
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or_default()
            .to_le_bytes(),
    );
    out
}

/// `Content-Encoding: deflate` as RFC 1950 specifies it: zlib-wrapped.
pub(super) fn zlib_container(payload: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(payload, 6)
}

/// `Content-Encoding: deflate` as the long tail of servers emits it: bare
/// RFC 1951, no wrapper at all.
pub(super) fn raw_deflate_container(payload: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(payload, 6)
}

const SSE_PAYLOAD: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hola\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" mundo\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
    "data: [DONE]\n\n",
);

// Harness

fn drive(codec: Option<Codec>, frames: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut headers = HeaderMap::new();
    if let Some(codec) = codec
        && let Ok(value) = HeaderValue::from_str(codec.as_str())
    {
        headers.insert(header::CONTENT_ENCODING, value);
    }
    let mut decoding = BodyDecoding::from_response(&headers).map_err(|e| e.to_string())?;
    let mut collected: Vec<u8> = Vec::new();
    for frame in frames {
        decoding.push(frame).map_err(|e| e.to_string())?;
        drain(&mut decoding, &mut collected)?;
    }
    if let Some(tail) = decoding.finish().map_err(|e| e.to_string())? {
        collected.extend_from_slice(&tail);
    }
    Ok(collected)
}

/// Pull every frame the decoder has ready, re-feeding carried input until the
/// stream stops making progress. Mirrors what `next_chunk` does per poll.
fn drain(decoding: &mut BodyDecoding, collected: &mut Vec<u8>) -> Result<(), String> {
    for _ in 0..4096 {
        if let Some(frame) = decoding.take_frame() {
            collected.extend_from_slice(&frame);
            continue;
        }
        // Blocked on bytes that have not arrived (incomplete header or
        // trailer): only the next frame can advance it.
        if !decoding.can_drain_without_input() {
            return Ok(());
        }
        decoding.push(&[]).map_err(|e| e.to_string())?;
    }
    Err("decoder made no progress".to_string())
}

fn frames_of(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
    bytes.chunks(size.max(1)).map(<[u8]>::to_vec).collect()
}

fn one(frame: Vec<u8>) -> Vec<Vec<u8>> {
    vec![frame]
}

// gzip

#[test]
fn gzip_roundtrip_in_one_frame() {
    let payload = SSE_PAYLOAD.as_bytes();
    let frames = one(gzip_container(payload, 0x00));
    let decoded = drive(Some(Codec::Gzip), &frames).expect("gzip decode");
    assert_eq!(decoded, payload);
}

#[test]
fn gzip_roundtrip_one_byte_per_frame() {
    let payload = SSE_PAYLOAD.as_bytes();
    let frames = frames_of(&gzip_container(payload, 0x00), 1);
    let decoded = drive(Some(Codec::Gzip), &frames).expect("gzip decode");
    assert_eq!(decoded, payload);
}

#[test]
fn gzip_roundtrip_with_every_optional_field() {
    let payload = SSE_PAYLOAD.as_bytes();
    // FEXTRA | FNAME | FCOMMENT | FHCRC
    let frames = one(gzip_container(payload, 0x1e));
    let decoded = drive(Some(Codec::Gzip), &frames).expect("gzip decode");
    assert_eq!(decoded, payload);
}

#[test]
fn gzip_container_split_at_the_header_boundary() {
    let payload = SSE_PAYLOAD.as_bytes();
    let container = gzip_container(payload, 0x08); // FNAME pushes the body past 10
    let split = 11;
    let (head_frame, body_frame) = match (container.get(..split), container.get(split..)) {
        (Some(head), Some(body)) => (head.to_vec(), body.to_vec()),
        _ => panic!("container shorter than the intended split"),
    };
    let frames = vec![head_frame, body_frame];
    let decoded = drive(Some(Codec::Gzip), &frames).expect("gzip decode");
    assert_eq!(decoded, payload);
}

#[test]
fn gzip_truncated_body_is_reported_not_silently_shortened() {
    let payload = SSE_PAYLOAD.as_bytes();
    let container = gzip_container(payload, 0x00);
    // Cut into DEFLATE itself; trailer-only truncation is tested separately.
    let cut = container.len() / 2;
    let truncated = container.get(..cut).map(<[u8]>::to_vec).unwrap_or_default();
    let err = drive(Some(Codec::Gzip), &one(truncated)).expect_err("truncation must fail");
    assert!(err.contains("truncated"), "{err}");
}

#[test]
fn gzip_header_only_is_reported_as_truncation() {
    let container = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("header only must fail");
    assert!(err.contains("truncated"), "{err}");
}

#[test]
fn corrupt_gzip_body_is_a_decode_error() {
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = gzip_container(payload, 0x00);
    let corrupted_at = container.len().saturating_sub(20);
    if let Some(byte) = container.get_mut(corrupted_at) {
        *byte ^= 0xff;
    }
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("corruption must fail");
    assert!(err.contains("gzip"), "{err}");
}

#[test]
fn empty_payload_roundtrips() {
    let frames = one(gzip_container(b"", 0x00));
    let decoded = drive(Some(Codec::Gzip), &frames).expect("empty gzip decode");
    assert!(decoded.is_empty());
}

// deflate

#[test]
fn zlib_roundtrip_one_byte_per_frame() {
    let payload = SSE_PAYLOAD.as_bytes();
    let frames = frames_of(&zlib_container(payload), 1);
    let decoded = drive(Some(Codec::Deflate), &frames).expect("zlib decode");
    assert_eq!(decoded, payload);
}

#[test]
fn zlib_checksum_mismatch_is_caught() {
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = zlib_container(payload);
    if let Some(last) = container.last_mut() {
        *last ^= 0xff;
    }
    let err = drive(Some(Codec::Deflate), &one(container)).expect_err("bad adler32 must fail");
    assert!(err.contains("deflate"), "{err}");
}

#[test]
fn raw_deflate_roundtrip() {
    // Not zlib-wrapped. The decoder sniffs the wrapper from the first two
    // bytes; if the sniff misfired on this vector, inflating it as zlib would
    // fail or produce different bytes, so a passing round-trip is itself
    // evidence the raw path was taken.
    let payload = SSE_PAYLOAD.as_bytes();
    let container = raw_deflate_container(payload);
    let decoded = drive(Some(Codec::Deflate), &one(container)).expect("raw deflate decode");
    assert_eq!(decoded, payload);
}

// passthrough / caps

#[test]
fn no_content_encoding_leaves_frames_byte_identical() {
    let payload = SSE_PAYLOAD.as_bytes();
    let decoded = drive(None, &frames_of(payload, 7)).expect("passthrough");
    assert_eq!(decoded, payload);
}

#[test]
fn identity_content_encoding_leaves_frames_byte_identical() {
    let payload = SSE_PAYLOAD.as_bytes();
    let frames = frames_of(payload, 7);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let mut decoding = BodyDecoding::from_response(&headers).expect("decoder");
    let mut collected: Vec<u8> = Vec::new();
    for frame in &frames {
        decoding.push(frame).expect("push");
        drain(&mut decoding, &mut collected).expect("drain");
    }
    assert_eq!(collected, payload);
}

#[test]
fn decoded_output_is_capped_per_frame_without_losing_bytes() {
    // Highly compressible: one small frame expands well past the per-frame
    // decoded cap, so the carry-over path is exercised.
    let payload = vec![b'a'; 12 * 1024 * 1024];
    let container = gzip_container(&payload, 0x00);
    let decoded = drive(Some(Codec::Gzip), &one(container)).expect("large gzip decode");
    assert_eq!(decoded.len(), payload.len());
    assert_eq!(decoded, payload);
}

#[test]
fn large_wire_frame_does_not_count_as_a_large_header() {
    let mut state = 0x1234_5678u32;
    let payload: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let container = gzip_container(&payload, 0x00);
    assert!(container.len() > 1024 * 1024);
    assert_eq!(drive(Some(Codec::Gzip), &one(container)).unwrap(), payload);
}

#[test]
fn deflate_trailing_bytes_are_not_silently_lost() {
    let mut container = zlib_container(b"hello");
    container.extend_from_slice(b"garbage");
    let err = drive(Some(Codec::Deflate), &one(container)).unwrap_err();
    assert!(err.contains("trailing"), "{err}");
}

#[test]
fn deflate_drains_internal_output_with_no_more_input() {
    let payload = vec![b'd'; 12 * 1024 * 1024];
    assert_eq!(
        drive(Some(Codec::Deflate), &one(zlib_container(&payload))).unwrap(),
        payload
    );
}

// gzip trailer: CRC32 and ISIZE

#[test]
fn gzip_crc32_mismatch_is_rejected() {
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = gzip_container(payload, 0x00);
    // Flip a bit inside the stored CRC32, which is the first 4 bytes of the
    // 8-byte trailer (ISIZE occupies the last 4).
    let crc_at = container.len().saturating_sub(8);
    if let Some(byte) = container.get_mut(crc_at) {
        *byte ^= 0x01;
    }
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("CRC mismatch must fail");
    assert!(err.contains("CRC32"), "{err}");
}

#[test]
fn gzip_isize_mismatch_is_rejected() {
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = gzip_container(payload, 0x00);
    // Corrupt only ISIZE, leaving CRC32 intact: the two must be checked
    // independently so the error names the field that actually lied.
    let isize_at = container.len().saturating_sub(4);
    if let Some(byte) = container.get_mut(isize_at) {
        *byte ^= 0xff;
    }
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("ISIZE mismatch must fail");
    assert!(err.contains("ISIZE"), "{err}");
}

#[test]
fn gzip_truncated_trailer_is_reported_as_truncation() {
    let payload = SSE_PAYLOAD.as_bytes();
    let container = gzip_container(payload, 0x00);
    // Drop bytes off the end of the trailer only: the DEFLATE stream itself
    // is complete, so this is a missing-trailer error, not a short body.
    for missing in 1..=8usize {
        let cut = container.len().saturating_sub(missing);
        let truncated = container.get(..cut).map(<[u8]>::to_vec).unwrap_or_default();
        let err = drive(Some(Codec::Gzip), &one(truncated))
            .expect_err("a partial trailer must not decode cleanly");
        assert!(err.contains("truncated"), "missing {missing}: {err}");
    }
}

#[test]
fn gzip_trailer_split_across_frames_is_verified() {
    // The trailer arriving in pieces must still be checked, not assumed.
    let payload = SSE_PAYLOAD.as_bytes();
    let container = gzip_container(payload, 0x00);
    let body_end = container.len().saturating_sub(8);
    let (head, trailer) = match (container.get(..body_end), container.get(body_end..)) {
        (Some(head), Some(trailer)) => (head.to_vec(), trailer.to_vec()),
        _ => panic!("container shorter than its own trailer"),
    };
    let frames = vec![head, trailer];
    let decoded = drive(Some(Codec::Gzip), &frames).expect("split trailer decodes");
    assert_eq!(decoded, payload);
}

// gzip concatenated members

#[test]
fn concatenated_gzip_members_are_decoded_in_full() {
    // RFC 1952 allows several members back to back; all of their plaintext
    // must surface, and every trailer must verify.
    let first = SSE_PAYLOAD.as_bytes();
    let second = b"data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n\n";
    let mut container = gzip_container(first, 0x00);
    container.extend_from_slice(&gzip_container(second, 0x00));
    let decoded = drive(Some(Codec::Gzip), &one(container)).expect("concatenated decode");
    let mut expected = first.to_vec();
    expected.extend_from_slice(second);
    assert_eq!(decoded, expected);
}

#[test]
fn concatenated_members_split_across_frames_are_decoded() {
    let first = SSE_PAYLOAD.as_bytes();
    let second = b"data: [DONE]\n\n";
    let mut container = gzip_container(first, 0x00);
    container.extend_from_slice(&gzip_container(second, 0x00));
    let frames = frames_of(&container, 3);
    let decoded = drive(Some(Codec::Gzip), &frames).expect("framed concatenated decode");
    let mut expected = first.to_vec();
    expected.extend_from_slice(second);
    assert_eq!(decoded, expected);
}

#[test]
fn second_concatenated_member_crc_is_still_verified() {
    let first = SSE_PAYLOAD.as_bytes();
    let second = b"data: [DONE]\n\n";
    let mut container = gzip_container(first, 0x00);
    container.extend_from_slice(&gzip_container(second, 0x00));
    // Corrupt the CRC of the *second* member: a decoder that stops checking
    // after the first trailer would pass this.
    let second_crc_at = container.len().saturating_sub(8);
    if let Some(byte) = container.get_mut(second_crc_at) {
        *byte ^= 0x02;
    }
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("second CRC must be checked");
    assert!(err.contains("CRC32"), "{err}");
}

#[test]
fn trailing_garbage_after_a_final_member_is_truncation() {
    // Bytes that are not a valid member header must not be silently dropped.
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = gzip_container(payload, 0x00);
    container.extend_from_slice(&[0x00, 0x01, 0x02]);
    let err = drive(Some(Codec::Gzip), &one(container))
        .expect_err("a bad member header must not be ignored");
    assert!(err.contains("gzip") || err.contains("truncated"), "{err}");
}

// gzip header validation

#[test]
fn gzip_reserved_flg_bits_are_rejected() {
    let mut container = vec![0x1f, 0x8b, 0x08, 0xe0, 0, 0, 0, 0, 0x00, 0xff];
    container.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(b"x", 6));
    container.extend_from_slice(&crc32(b"x").to_le_bytes());
    container.extend_from_slice(&1u32.to_le_bytes());
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("reserved FLG must fail");
    assert!(err.contains("FLG") || err.contains("reserved"), "{err}");
}

#[test]
fn gzip_fhcrc_mismatch_is_rejected() {
    let payload = SSE_PAYLOAD.as_bytes();
    let mut container = gzip_container(payload, 0x02);
    // FHCRC is the 2 bytes right after the 10-byte fixed header; the DEFLATE
    // body and the 8-byte trailer follow it.
    let at = 10;
    if let Some(byte) = container.get_mut(at) {
        *byte ^= 0xff;
    }
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("bad FHCRC must fail");
    assert!(err.contains("FHCRC") || err.contains("CRC16"), "{err}");
}

#[test]
fn gzip_unknown_compression_method_is_rejected() {
    let mut container = vec![0x1f, 0x8b, 0x09, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    container.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(b"x", 6));
    container.extend_from_slice(&crc32(b"x").to_le_bytes());
    container.extend_from_slice(&1u32.to_le_bytes());
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("CM != 8 must fail");
    assert!(err.contains("method") || err.contains("CM"), "{err}");
}

#[test]
fn gzip_bad_magic_is_rejected() {
    let mut container = vec![0x1e, 0x8c, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    container.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(b"x", 6));
    container.extend_from_slice(&crc32(b"x").to_le_bytes());
    container.extend_from_slice(&1u32.to_le_bytes());
    let err = drive(Some(Codec::Gzip), &one(container)).expect_err("bad magic must fail");
    assert!(err.contains("magic") || err.contains("gzip"), "{err}");
}

// decoded-size limit

#[test]
fn decoded_body_limit_is_enforced() {
    // A few KiB of DEFLATE that expands past the 32 MiB decoded ceiling must
    // be rejected rather than buffered into memory.
    let payload = vec![b'z'; 33 * 1024 * 1024];
    let container = gzip_container(&payload, 0x00);
    let err = drive(Some(Codec::Gzip), &frames_of(&container, 64 * 1024))
        .expect_err("expansion past the decoded limit must fail");
    assert!(err.contains("decoded") || err.contains("expands"), "{err}");
}

#[test]
fn frame_cap_never_loses_bytes_across_many_frames() {
    // Same expansion as the cap test, but under the limit: every byte must
    // survive being handed back in per-frame-sized pieces.
    let payload: Vec<u8> = (0..9 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let container = gzip_container(&payload, 0x00);
    let decoded =
        drive(Some(Codec::Gzip), &frames_of(&container, 4096)).expect("chunked large decode");
    assert_eq!(decoded.len(), payload.len());
    assert_eq!(decoded, payload);
}
