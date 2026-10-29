//! Container-level audio duration probing.
//!
//! STT providers bill per hour of audio, so a usage row is only costable once
//! the submitted file's duration is known. Rather than pull a full decoder
//! (`symphonia` costs seconds of build time and hundreds of KB for what is a
//! header walk), this module reads the duration straight out of the container
//! header for the formats that actually reach the endpoint.
//!
//! Everything here is best-effort: an unrecognised or malformed container
//! returns `None` and the caller falls back to the upstream-reported duration.
//! No function here panics on hostile input — every length is bounds-checked.

/// Sniff the container from its magic bytes and return the duration in ms.
///
/// Returns `None` when the format is unknown or the header does not carry
/// enough information to compute a duration (e.g. CBR MP3 without a Xing tag
/// still works, but a truncated file does not).
pub fn probe_duration_ms(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 12 {
        return None;
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WAVE" {
        return wav_duration_ms(bytes);
    }
    if bytes.starts_with(b"OggS") {
        return ogg_duration_ms(bytes);
    }
    if bytes.starts_with(b"fLaC") {
        return flac_duration_ms(bytes);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return mp4_duration_ms(bytes);
    }
    if bytes.starts_with(b"ID3") || looks_like_mpeg_audio(bytes) {
        return mp3_duration_ms(bytes);
    }
    None
}

fn be_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn le_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn be_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn ms_from_frames(frames: u64, rate: u32) -> Option<u32> {
    if rate == 0 {
        return None;
    }
    let ms = frames.saturating_mul(1000) / u64::from(rate);
    u32::try_from(ms).ok().filter(|m| *m > 0)
}

/// WAV: walk chunks to `fmt ` (byte rate) and `data` (payload length).
fn wav_duration_ms(b: &[u8]) -> Option<u32> {
    let mut off = 12usize;
    let mut byte_rate: Option<u32> = None;
    let mut data_len: Option<u32> = None;

    while off + 8 <= b.len() {
        let id = b.get(off..off + 4)?;
        let size = le_u32(b, off + 4)?;
        let body = off + 8;
        match id {
            b"fmt " => byte_rate = le_u32(b, body + 8),
            b"data" => {
                // A streaming WAV may declare 0 or 0xFFFFFFFF; fall back to the
                // bytes actually present.
                let declared = size as usize;
                let available = b.len().saturating_sub(body);
                let effective = if declared == 0 || declared > available {
                    available
                } else {
                    declared
                };
                data_len = u32::try_from(effective).ok();
            }
            _ => {}
        }
        if byte_rate.is_some() && data_len.is_some() {
            break;
        }
        // Chunks are word-aligned.
        let advance = (size as usize).checked_add(8)?;
        let padded = advance.checked_add(advance & 1)?;
        off = off.checked_add(padded)?;
    }

    let rate = byte_rate?;
    let len = data_len?;
    if rate == 0 {
        return None;
    }
    let ms = u64::from(len) * 1000 / u64::from(rate);
    u32::try_from(ms).ok().filter(|m| *m > 0)
}

/// Ogg: read the granule position of the last page and the sample rate from the
/// first page's identification header.
///
/// The granule position of the final page is the total sample count, so this
/// needs the *end* of the stream. We scan backwards for the last `OggS` capture
/// pattern and read its granule.
fn ogg_duration_ms(b: &[u8]) -> Option<u32> {
    let rate = ogg_sample_rate(b)?;

    // Find the last page header. Scanning backwards keeps this cheap and avoids
    // trusting a page count we cannot verify.
    let mut last: Option<usize> = None;
    let mut i = b.len().saturating_sub(4);
    loop {
        if b.get(i..i + 4)? == b"OggS" {
            last = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let page = last?;
    let granule = b.get(page + 6..page + 14)?;
    let granule = u64::from_le_bytes([
        granule[0], granule[1], granule[2], granule[3], granule[4], granule[5], granule[6],
        granule[7],
    ]);
    // All-ones means "no packet ends on this page".
    if granule == u64::MAX {
        return None;
    }
    ms_from_frames(granule, rate)
}

/// Ogg identification header: `OggS` page → `\x01vorbis` or `OpusHead`.
fn ogg_sample_rate(b: &[u8]) -> Option<u32> {
    // The first page's payload starts after the 27-byte header plus the segment
    // table (one byte per segment).
    let segments = *b.get(26)? as usize;
    let payload = 27usize.checked_add(segments)?;
    let magic = b.get(payload..payload + 8)?;

    if magic.starts_with(b"OpusHead") {
        // Opus always decodes at 48 kHz regardless of the input rate; granule
        // positions are expressed in 48 kHz samples.
        return Some(48_000);
    }
    if magic.starts_with(b"\x01vorbis") {
        // vorbis id header: version(4) channels(1) rate(4)
        return le_u32(b, payload + 12);
    }
    if magic.starts_with(b"Speex   ") {
        // speex header: rate at offset 36 of the payload
        return le_u32(b, payload + 36);
    }
    None
}

/// FLAC: STREAMINFO carries total samples (20 bits) and the sample rate
/// (20 bits), both packed into the same 8-byte field.
fn flac_duration_ms(b: &[u8]) -> Option<u32> {
    // Layout: `fLaC` (4) + metadata block header (4) + STREAMINFO body.
    // STREAMINFO: min/max blocksize (4), min/max framesize (6), then 8 bytes of
    // rate(20) | channels-1(3) | bps-1(5) | total-samples(36).
    let base = 4 + 4;
    let info = b.get(base + 10..base + 18)?;
    let rate = (u32::from(info[0]) << 12) | (u32::from(info[1]) << 4) | (u32::from(info[2]) >> 4);
    let total = (u64::from(info[3] & 0x0F) << 32)
        | (u64::from(info[4]) << 24)
        | (u64::from(info[5]) << 16)
        | (u64::from(info[6]) << 8)
        | u64::from(info[7]);
    ms_from_frames(total, rate)
}

/// MP4/M4A: read `mvhd` for the movie timescale and duration.
fn mp4_duration_ms(b: &[u8]) -> Option<u32> {
    let mvhd = find_box(b, b"mvhd", 0, b.len().min(1 << 20))?;
    // `mvhd` box: size(4) type(4) then the full box body.
    //   version(1) flags(3) creation(4) modification(4) timescale(4) duration(4)
    let body = mvhd + 8;
    let version = *b.get(body)?;
    if version == 1 {
        // v1 widens creation/modification/duration to 64 bits.
        let timescale = be_u32(b, body + 20)?;
        let hi = be_u32(b, body + 24)?;
        let lo = be_u32(b, body + 28)?;
        let dur = (u64::from(hi) << 32) | u64::from(lo);
        ms_from_frames(dur, timescale)
    } else {
        let timescale = be_u32(b, body + 12)?;
        let dur = u64::from(be_u32(b, body + 16)?);
        ms_from_frames(dur, timescale)
    }
}

/// Depth-first search for a box type within `[start, end)`.
fn find_box(b: &[u8], kind: &[u8; 4], start: usize, end: usize) -> Option<usize> {
    let mut off = start;
    while off + 8 <= end {
        let size = be_u32(b, off)? as usize;
        let ty = b.get(off + 4..off + 8)?;
        if ty == kind {
            return Some(off);
        }
        // Containers we must descend into to reach `mvhd`.
        let is_container = matches!(ty, b"moov" | b"trak" | b"mdia");
        let advance = if size == 0 {
            end - off
        } else if size == 1 {
            // 64-bit size follows the type.
            let hi = be_u32(b, off + 8)? as usize;
            let lo = be_u32(b, off + 12)? as usize;
            (hi << 32) | lo
        } else {
            size
        };
        if advance < 8 {
            return None;
        }
        if is_container {
            let inner_end = off.checked_add(advance)?.min(end);
            if let Some(found) = find_box(b, kind, off + 8, inner_end) {
                return Some(found);
            }
        }
        off = off.checked_add(advance)?;
    }
    None
}

/// True when the bytes start with a plausible MPEG audio frame sync.
fn looks_like_mpeg_audio(b: &[u8]) -> bool {
    matches!(b.first(), Some(0xFF)) && b.get(1).is_some_and(|v| v & 0xE0 == 0xE0)
}

/// MP3: prefer a Xing/Info/VBRI frame count, fall back to a CBR byte-rate
/// estimate. ID3v2 tags are skipped before parsing.
fn mp3_duration_ms(b: &[u8]) -> Option<u32> {
    let mut start = 0usize;
    if b.starts_with(b"ID3") {
        // ID3v2 header: 6 bytes of flags/size, then 4 syncsafe size bytes.
        let size = b.get(6..10)?;
        let sz = (usize::from(size[0] & 0x7F) << 21)
            | (usize::from(size[1] & 0x7F) << 14)
            | (usize::from(size[2] & 0x7F) << 7)
            | usize::from(size[3] & 0x7F);
        start = 10usize.checked_add(sz)?;
    }

    let frame = find_mpeg_frame(b, start)?;
    let header = be_u32(b, frame)?;
    let (rate, bitrate, samples_per_frame) = parse_mpeg_header(header)?;

    // Xing/Info sits at a fixed offset after the side information.
    let side_info = if (header >> 19) & 0x3 == 3 { 17 } else { 9 };
    let xing_off = frame + 4 + side_info;
    if let Some(tag) = b.get(xing_off..xing_off + 4) {
        if tag == b"Xing" || tag == b"Info" {
            let flags = be_u32(b, xing_off + 4)?;
            if flags & 0x1 != 0 {
                let frames = be_u32(b, xing_off + 8)?;
                let total_samples = u64::from(frames) * u64::from(samples_per_frame);
                return ms_from_frames(total_samples, rate);
            }
        }
    }

    // CBR fallback: bytes after the tag divided by the frame bitrate.
    if bitrate == 0 {
        return None;
    }
    let audio_bytes = b.len().saturating_sub(frame) as u64;
    let ms = audio_bytes.saturating_mul(8) * 1000 / u64::from(bitrate);
    u32::try_from(ms).ok().filter(|m| *m > 0)
}

/// Scan forward for the first valid MPEG audio frame header.
fn find_mpeg_frame(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 <= b.len() {
        if b[i] == 0xFF && b.get(i + 1).is_some_and(|v| v & 0xE0 == 0xE0) {
            if let Some(h) = be_u32(b, i) {
                if parse_mpeg_header(h).is_some() {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Returns `(sample_rate_hz, bitrate_bps, samples_per_frame)`.
fn parse_mpeg_header(h: u32) -> Option<(u32, u32, u32)> {
    if h >> 21 & 0x7FF != 0x7FF {
        return None;
    }
    let version = (h >> 19) & 0x3; // 0=2.5, 1=reserved, 2=MPEG2, 3=MPEG1
    let layer = (h >> 17) & 0x3; // 1=III, 2=II, 3=I
    let bitrate_idx = (h >> 12) & 0xF;
    let rate_idx = (h >> 10) & 0x3;
    if version == 1 || layer == 0 || bitrate_idx == 0 || bitrate_idx == 0xF || rate_idx == 3 {
        return None;
    }

    let base_rate = [44_100u32, 48_000, 32_000][rate_idx as usize];
    let rate = match version {
        3 => base_rate,     // MPEG1
        2 => base_rate / 2, // MPEG2
        0 => base_rate / 4, // MPEG2.5
        _ => return None,
    };

    // Layer I/II/III bitrate tables, indexed by MPEG1 / MPEG2-2.5.
    let (table1, table2): ([u32; 16], [u32; 16]) = match layer {
        3 => (
            [
                0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
            ],
            [
                0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
            ],
        ),
        2 => (
            [
                0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
            ],
            [
                0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
            ],
        ),
        _ => (
            [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
            ],
            [
                0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
            ],
        ),
    };
    let kbps = if version == 3 {
        table1[bitrate_idx as usize]
    } else {
        table2[bitrate_idx as usize]
    };
    if kbps == 0 {
        return None;
    }

    let samples_per_frame = match layer {
        3 => 384,  // Layer I
        2 => 1152, // Layer II
        _ => {
            if version == 3 {
                1152
            } else {
                576
            }
        } // Layer III
    };

    Some((rate, kbps * 1000, samples_per_frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PCM WAV header around `data_len` bytes of payload.
    fn wav(data_len: usize, rate: u32, channels: u16, bits: u16) -> Vec<u8> {
        let byte_rate = rate * u32::from(channels) * u32::from(bits) / 8;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data_len as u32).to_le_bytes());
        v.resize(v.len() + data_len, 0);
        v
    }

    #[test]
    fn wav_4s_16k_mono_16bit() {
        // 4s * 16000 Hz * 1ch * 2 bytes = 128_000 bytes
        let b = wav(128_000, 16_000, 1, 16);
        assert_eq!(probe_duration_ms(&b), Some(4000));
    }

    #[test]
    fn wav_stereo_44k1() {
        let data = 44_100 * 2 * 2 * 2; // 2 seconds
        let b = wav(data, 44_100, 2, 16);
        assert_eq!(probe_duration_ms(&b), Some(2000));
    }

    #[test]
    fn wav_zero_declared_data_falls_back_to_available() {
        let mut b = wav(0, 16_000, 1, 16);
        // Append 32_000 bytes of payload but leave the `data` size at zero.
        b.resize(b.len() + 32_000, 0);
        assert_eq!(probe_duration_ms(&b), Some(1000));
    }

    #[test]
    fn unknown_container_returns_none() {
        assert_eq!(probe_duration_ms(b"not audio at all, definitely not"), None);
        assert_eq!(probe_duration_ms(b""), None);
        assert_eq!(probe_duration_ms(b"RIFF"), None);
    }

    #[test]
    fn truncated_headers_do_not_panic() {
        // Every prefix of a real WAV must return cleanly.
        let full = wav(4096, 16_000, 1, 16);
        for cut in 0..full.len().min(64) {
            let _ = probe_duration_ms(&full[..cut]);
        }
    }

    /// Build an Ogg page with the given granule and identification header.
    fn ogg_page(granule: u64, ident: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OggS");
        v.push(0); // version
        v.push(0x02); // header type: BOS
        v.extend_from_slice(&granule.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // serial
        v.extend_from_slice(&0u32.to_le_bytes()); // page seq
        v.extend_from_slice(&0u32.to_le_bytes()); // crc
        v.push(1); // one segment
        v.push(ident.len() as u8);
        v.extend_from_slice(ident);
        v
    }

    #[test]
    fn ogg_opus_uses_48k_granule() {
        // Opus granule is always in 48 kHz samples: 4s => 192_000.
        let ident = {
            let mut v = Vec::new();
            v.extend_from_slice(b"OpusHead");
            v.push(1);
            v.push(2);
            v.extend_from_slice(&312u16.to_le_bytes());
            v.extend_from_slice(&48_000u32.to_le_bytes());
            v
        };
        let mut b = ogg_page(0, &ident);
        b.extend_from_slice(&ogg_page(192_000, &ident));
        assert_eq!(probe_duration_ms(&b), Some(4000));
    }

    #[test]
    fn ogg_vorbis_uses_header_rate() {
        let ident = {
            let mut v = Vec::new();
            v.extend_from_slice(b"\x01vorbis");
            v.extend_from_slice(&0u32.to_le_bytes()); // version
            v.push(2); // channels
            v.extend_from_slice(&44_100u32.to_le_bytes()); // rate
            v.extend_from_slice(&0u32.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
            v.push(0);
            v
        };
        let mut b = ogg_page(0, &ident);
        b.extend_from_slice(&ogg_page(88_200, &ident));
        assert_eq!(probe_duration_ms(&b), Some(2000));
    }

    #[test]
    fn ogg_without_granule_is_unknown() {
        let ident = {
            let mut v = Vec::new();
            v.extend_from_slice(b"OpusHead");
            v.extend_from_slice(&[0; 11]);
            v
        };
        let b = ogg_page(u64::MAX, &ident);
        assert_eq!(probe_duration_ms(&b), None);
    }

    #[test]
    fn flac_streaminfo_duration() {
        let mut b = Vec::new();
        b.extend_from_slice(b"fLaC");
        // STREAMINFO block header: last-block flag + type 0, 24-bit length 34.
        b.push(0x80);
        b.extend_from_slice(&[0, 0, 34]);
        b.extend_from_slice(&[0; 10]); // min/max blocksize + framesize
        // rate 44100 (20 bits) | channels-1 (3 bits) | bps-1 (5 bits) | total (36 bits)
        let rate: u64 = 44_100;
        let channels_m1: u64 = 1;
        let bps_m1: u64 = 15;
        let total: u64 = 88_200; // 2s
        let packed = (rate << 44) | (channels_m1 << 41) | (bps_m1 << 36) | total;
        b.extend_from_slice(&packed.to_be_bytes());
        assert_eq!(probe_duration_ms(&b), Some(2000));
    }

    #[test]
    fn mp3_header_parse_rejects_garbage() {
        assert!(parse_mpeg_header(0).is_none());
        assert!(parse_mpeg_header(0xFFFF_FFFF).is_none());
    }

    #[test]
    fn mp3_cbr_fallback_estimates_duration() {
        // MPEG1 Layer III, 128 kbps, 44.1 kHz => 0xFF 0xFB 0x90 0x00
        let frame = [0xFFu8, 0xFB, 0x90, 0x00];
        let mut b = frame.to_vec();
        // 128 kbps => 16_000 bytes/s. 32_000 bytes => 2s.
        b.resize(32_000, 0);
        let d = probe_duration_ms(&b).unwrap();
        assert!((1900..=2100).contains(&d), "got {d}ms");
    }

    #[test]
    fn mp3_xing_frame_count_wins() {
        // MPEG1 Layer III, 128 kbps, 44.1 kHz, stereo (side info 17 bytes)
        let mut b = Vec::new();
        b.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        b.extend_from_slice(&[0; 17]); // side info
        b.extend_from_slice(b"Xing");
        b.extend_from_slice(&1u32.to_be_bytes()); // flags: frames present
        b.extend_from_slice(&76u32.to_be_bytes()); // 76 frames * 1152 = 87552 samples
        b.resize(16_384, 0);
        // 87552 / 44100 = 1.985s
        let d = probe_duration_ms(&b).unwrap();
        assert!((1900..=2050).contains(&d), "got {d}ms");
    }

    #[test]
    fn mp3_skips_id3v2_tag() {
        let mut b = Vec::new();
        b.extend_from_slice(b"ID3");
        b.extend_from_slice(&[4, 0, 0]); // v2.4, no flags
        // syncsafe size: 100 bytes
        b.extend_from_slice(&[0, 0, 0, 100]);
        b.resize(110, 0);
        b.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        b.resize(b.len() + 16_000, 0);
        let d = probe_duration_ms(&b).unwrap();
        assert!((900..=1100).contains(&d), "got {d}ms");
    }

    #[test]
    fn mp4_mvhd_v0_duration() {
        let mut b = Vec::new();
        // ftyp
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(b"M4A ");
        b.extend_from_slice(&0u32.to_be_bytes());
        // moov > mvhd (v0)
        let mvhd_body = {
            let mut m = Vec::new();
            m.push(0); // version 0
            m.extend_from_slice(&[0; 3]); // flags
            m.extend_from_slice(&0u32.to_be_bytes()); // creation
            m.extend_from_slice(&0u32.to_be_bytes()); // modification
            m.extend_from_slice(&1000u32.to_be_bytes()); // timescale
            m.extend_from_slice(&3000u32.to_be_bytes()); // duration => 3s
            m.extend_from_slice(&[0; 80]);
            m
        };
        let mvhd_size = (mvhd_body.len() + 8) as u32;
        let moov_size = mvhd_size + 8;
        b.extend_from_slice(&moov_size.to_be_bytes());
        b.extend_from_slice(b"moov");
        b.extend_from_slice(&mvhd_size.to_be_bytes());
        b.extend_from_slice(b"mvhd");
        b.extend_from_slice(&mvhd_body);
        assert_eq!(probe_duration_ms(&b), Some(3000));
    }

    #[test]
    fn mp4_truncated_does_not_panic() {
        let mut b = Vec::new();
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(b"M4A ");
        b.extend_from_slice(&0u32.to_be_bytes());
        for cut in 0..b.len() {
            let _ = probe_duration_ms(&b[..cut]);
        }
    }
}
