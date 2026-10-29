use super::MAX_GZIP_HEADER_BYTES;
use crate::upstream::error::{UpstreamError, UpstreamResult};
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const GZIP_CM_DEFLATE: u8 = 8;
const GZIP_FHCRC: u8 = 0x02;
/// FLG bits 5..7 are reserved by RFC 1952 and must be zero.
const GZIP_FRESERVED: u8 = 0xe0;
const GZIP_FEXTRA: u8 = 0x04;
const GZIP_FNAME: u8 = 0x08;
const GZIP_FCOMMENT: u8 = 0x10;
pub(crate) const GZIP_FIXED_HEADER_LEN: usize = 10;
/// Length of the CRC32 + ISIZE trailer every gzip member ends with.
pub(crate) const GZIP_TRAILER_LEN: usize = 8;

/// Offset of the raw DEFLATE body inside a gzip container.
///
/// `Ok(None)` means the container header is not complete yet; `Err` means it is
/// malformed and the body must not be fed to an inflater.
pub(crate) fn gzip_body_offset(buf: &[u8]) -> UpstreamResult<Option<usize>> {
    // Bound header parsing, not the transport frame containing the payload.
    let head = &buf[..buf.len().min(MAX_GZIP_HEADER_BYTES + 1)];
    let Some(flags) = gzip_fixed_header(head)? else {
        return Ok(None);
    };
    let offset = gzip_optional_fields(head, flags, GZIP_FIXED_HEADER_LEN)?;
    if offset.is_some_and(|end| end > MAX_GZIP_HEADER_BYTES)
        || (offset.is_none() && buf.len() > MAX_GZIP_HEADER_BYTES)
    {
        return Err(UpstreamError::Decode(format!(
            "gzip header exceeds {MAX_GZIP_HEADER_BYTES} bytes"
        )));
    }
    Ok(offset)
}

/// Magic, compression method and the FLG byte.
///
/// `Ok(None)` while fewer than [`GZIP_FIXED_HEADER_LEN`] bytes have arrived.
/// `Err` when the fixed header is present but malformed.
fn gzip_fixed_header(buf: &[u8]) -> UpstreamResult<Option<u8>> {
    if buf.len() < GZIP_FIXED_HEADER_LEN {
        return Ok(None);
    }
    // `try_from` on the full 10-byte head can never yield a `[u8; 2]`: the
    // conversion is only valid on the two magic bytes themselves.
    let Some(magic) = buf.get(..2) else {
        return Ok(None);
    };
    let Ok(magic) = <[u8; 2]>::try_from(magic) else {
        return Ok(None);
    };
    if magic != GZIP_MAGIC {
        return Err(UpstreamError::Decode(
            "body is not a gzip stream (bad magic)".to_string(),
        ));
    }
    let method = buf.get(2).copied().unwrap_or_default();
    if method != GZIP_CM_DEFLATE {
        return Err(UpstreamError::Decode(format!(
            "unsupported gzip compression method {method}"
        )));
    }
    let flags = buf.get(3).copied().unwrap_or_default();
    if flags & GZIP_FRESERVED != 0 {
        return Err(UpstreamError::Decode(format!(
            "gzip header sets reserved FLG bits {:#04x}",
            flags & GZIP_FRESERVED
        )));
    }
    Ok(Some(flags))
}

/// FEXTRA / FNAME / FCOMMENT / FHCRC, in the order RFC 1952 fixes them.
fn gzip_optional_fields(buf: &[u8], flags: u8, start: usize) -> UpstreamResult<Option<usize>> {
    let mut pos = start;
    if flags & GZIP_FEXTRA != 0 {
        let Some(extra_len) = read_u16_le(buf, pos) else {
            return Ok(None);
        };
        pos += 2;
        let Some(end) = pos.checked_add(usize::from(extra_len)) else {
            return Err(UpstreamError::Decode(
                "gzip FEXTRA length overflows".to_string(),
            ));
        };
        if buf.len() < end {
            return Ok(None);
        }
        pos = end;
    }
    for flag in [GZIP_FNAME, GZIP_FCOMMENT] {
        if flags & flag != 0 {
            let Some(end) = nul_terminated_end(buf, pos) else {
                return Ok(None);
            };
            pos = end;
        }
    }
    if flags & GZIP_FHCRC != 0 {
        let Some(end) = pos.checked_add(2) else {
            return Err(UpstreamError::Decode("gzip header overflows".to_string()));
        };
        if buf.len() < end {
            return Ok(None);
        }
        // RFC 1952 §2.3.1.2: FHCRC is the low 16 bits of the CRC32 over the
        // whole header up to (not including) the FHCRC field itself. Skipping
        // it unchecked lets a corrupt header through.
        let Some(head_bytes) = buf.get(..pos) else {
            return Err(UpstreamError::Decode(
                "gzip FHCRC header is truncated".to_string(),
            ));
        };
        let expected = crc32fast::hash(head_bytes) as u16;
        let Some(stored) = read_u16_le(buf, pos) else {
            return Ok(None);
        };
        if stored != expected {
            return Err(UpstreamError::Decode(format!(
                "gzip header CRC16 mismatch: stored {stored:#06x}, computed {expected:#06x}"
            )));
        }
        pos = end;
    }
    Ok(Some(pos))
}

fn read_u16_le(buf: &[u8], pos: usize) -> Option<u16> {
    let slice = buf.get(pos..pos.checked_add(2)?)?;
    let Ok(bytes) = <[u8; 2]>::try_from(slice) else {
        return None;
    };
    Some(u16::from_le_bytes(bytes))
}

fn nul_terminated_end(buf: &[u8], pos: usize) -> Option<usize> {
    let tail = buf.get(pos..)?;
    let found = tail.iter().position(|byte| *byte == 0)?;
    pos.checked_add(found + 1)
}

/// `Content-Encoding: deflate` is specified as zlib (RFC 1950), but a long tail
/// of servers emits bare RFC 1951. A valid zlib header has CMF/FLG low nibble
/// `8`, a window size no larger than 7 and `(CMF << 8 | FLG) % 31 == 0`.
pub(crate) fn looks_like_zlib(head: &[u8]) -> bool {
    let Some(bytes) = head.get(..2) else {
        return false;
    };
    let Ok(bytes) = <[u8; 2]>::try_from(bytes) else {
        return false;
    };
    let [cmf, flg] = bytes;
    cmf & 0x0f == GZIP_CM_DEFLATE
        && cmf >> 4 <= 7
        && (u32::from(cmf) << 8 | u32::from(flg)) % 31 == 0
}
