use bytes::BytesMut;
use miniz_oxide::DataFormat;
use miniz_oxide::MZFlush;
use miniz_oxide::MZStatus;
use miniz_oxide::inflate::stream::{InflateState, inflate};

use super::compression_gzip::{GZIP_TRAILER_LEN, gzip_body_offset, looks_like_zlib};
use super::{Codec, MAX_DECODED_BODY_BYTES, MAX_DECODED_FRAME_BYTES};
use crate::upstream::error::{UpstreamError, UpstreamResult};

/// Where a [`Stage`] is in the container/stream lifecycle.
///
/// The old single `done` flag conflated "the DEFLATE stream ended" with "the
/// body is fully decoded": a gzip member is only complete once its CRC32 +
/// ISIZE trailer has been read and verified, and RFC 1952 allows further
/// members after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// The container header / zlib wrapper has not been resolved yet.
    Header,
    /// Feeding the raw DEFLATE stream to the inflater.
    Body,
    /// gzip only: collecting the 8-byte CRC32 + ISIZE trailer.
    Trailer,
    /// A member just finished; either the next member starts, or EOF.
    NextMember,
    /// A member just finished and the buffer is momentarily empty: the next
    /// member may still be arriving. Not yet EOF, so not [`Phase::Done`].
    AwaitNextMember,
    /// The whole body decoded and verified.
    Done,
}

/// One compression layer plus the state needed to prime its inflater.
pub(super) struct Stage {
    pub(super) codec: Codec,
    /// Inflater for the current member.
    state: Option<Box<InflateState>>,
    /// gzip trailer bytes collected so far (at most [`GZIP_TRAILER_LEN`]).
    pub(super) trailer: Vec<u8>,
    /// Running CRC32 and ISIZE of the current member's decoded output.
    crc: crc32fast::Hasher,
    decoded_len: u32,
    /// `true` once the inflater reported the end of the DEFLATE stream for the
    /// current member. Distinct from [`Phase`]: a gzip member still owes its
    /// trailer after this is set.
    pub(super) done: bool,
    /// `true` when the last inflate accepted every buffered byte and still
    /// wanted more, so re-feeding the same bytes cannot make progress.
    pub(super) stalled: bool,
    pub(super) phase: Phase,
}

impl Stage {
    pub(super) fn new(codec: Codec) -> Self {
        Self {
            codec,
            state: None,
            trailer: Vec::new(),
            crc: crc32fast::Hasher::new(),
            decoded_len: 0,
            done: false,
            stalled: false,
            phase: Phase::Header,
        }
    }

    /// Reset the per-member state so a concatenated gzip member can follow.
    pub(super) fn start_member(&mut self) {
        self.state = None;
        self.trailer.clear();
        self.crc = crc32fast::Hasher::new();
        self.decoded_len = 0;
        self.done = false;
        self.stalled = false;
        self.phase = Phase::Header;
    }

    /// The phase to enter once the inflater reports the end of a DEFLATE
    /// stream: gzip owes a trailer, deflate has nothing left to check.
    pub(super) fn phase_after_body(&self) -> Phase {
        match self.codec {
            Codec::Gzip => Phase::Trailer,
            Codec::Deflate => Phase::Done,
            // One-shot codecs never reach this; they go through finish_one_shot.
            Codec::Brotli | Codec::Zstd => Phase::Done,
        }
    }

    /// Prime `stage`'s inflater once enough bytes are buffered to identify the
    /// container, returning the offset where the DEFLATE body starts.
    /// `Ok(None)` while the header is still incomplete.
    pub(super) fn resolve_container(&mut self, head: &[u8]) -> UpstreamResult<Option<usize>> {
        match self.codec {
            Codec::Gzip => {
                let offset = gzip_body_offset(head)?;
                if offset.is_some() {
                    self.state = Some(InflateState::new_boxed(DataFormat::Raw));
                }
                Ok(offset)
            }
            Codec::Deflate => {
                if head.len() < 2 {
                    return Ok(None);
                }
                let format = if looks_like_zlib(head) {
                    DataFormat::Zlib
                } else {
                    DataFormat::Raw
                };
                self.state = Some(InflateState::new_boxed(format));
                Ok(Some(0))
            }
            // One-shot codecs never reach resolve_container; they go through
            // finish_one_shot which decompresses the whole buffer at once.
            Codec::Brotli | Codec::Zstd => Ok(Some(0)),
        }
    }

    /// Inflate `input` into `out`, stopping at the frame cap or stream end.
    /// Returns how many input bytes were accepted; the rest stays in `pending`
    /// for the next frame.
    ///
    /// Limits are applied to **decoded** bytes, never to the compressed length,
    /// so a small frame can still expand to the full per-frame allowance.
    pub(super) fn inflate(
        &mut self,
        input: &[u8],
        out: &mut BytesMut,
        scratch: &mut [u8],
        decoded_out: &mut u64,
    ) -> UpstreamResult<usize> {
        let Some(state) = self.state.as_mut() else {
            return Ok(0);
        };
        let mut consumed = 0usize;
        while out.len() < MAX_DECODED_FRAME_BYTES {
            let result = inflate(state, &input[consumed..], scratch, MZFlush::Sync);
            let status = match result.status {
                Ok(status) => status,
                Err(miniz_oxide::MZError::Buf)
                    if result.bytes_consumed == 0 && result.bytes_written == 0 =>
                {
                    self.stalled = true;
                    break;
                }
                Err(err) => {
                    return Err(UpstreamError::Decode(format!(
                        "{} body: {err:?}",
                        self.codec.as_str()
                    )));
                }
            };
            consumed += result.bytes_consumed;
            let written = result.bytes_written;
            let decoded = scratch.get(..written).unwrap_or_default();
            out.extend_from_slice(decoded);
            self.crc.update(decoded);
            self.decoded_len = self.decoded_len.wrapping_add(written as u32);
            *decoded_out = decoded_out.saturating_add(written as u64);
            if *decoded_out > MAX_DECODED_BODY_BYTES {
                return Err(UpstreamError::Decode(format!(
                    "{} body expands to more than {MAX_DECODED_BODY_BYTES} decoded bytes",
                    self.codec.as_str()
                )));
            }
            if status == MZStatus::StreamEnd {
                self.done = true;
                break;
            }
            if result.bytes_consumed == 0 && written == 0 {
                // The inflater needs more input to make progress.
                self.stalled = true;
                break;
            }
            self.stalled = false;
        }
        Ok(consumed)
    }

    /// Verify the gzip trailer: CRC32 and ISIZE must both match the decoded
    /// output of the member that just ended.
    pub(super) fn verify_trailer(&self) -> UpstreamResult<()> {
        let Ok(trailer) = <[u8; GZIP_TRAILER_LEN]>::try_from(self.trailer.as_slice()) else {
            return Err(UpstreamError::Decode(
                "gzip trailer is truncated".to_string(),
            ));
        };
        let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let stored_isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        let computed_crc = self.crc.clone().finalize();
        if stored_crc != computed_crc {
            return Err(UpstreamError::Decode(format!(
                "gzip CRC32 mismatch: stored {stored_crc:#010x}, computed {computed_crc:#010x}"
            )));
        }
        if stored_isize != self.decoded_len {
            return Err(UpstreamError::Decode(format!(
                "gzip ISIZE mismatch: stored {stored_isize}, decoded {} byte(s)",
                self.decoded_len
            )));
        }
        Ok(())
    }
}

/// A one-line explanation of where the decoder stalled, appended to the
/// truncation error so a partial trailer is not reported as a short body.
pub(super) fn phase_hint(phase: Phase) -> &'static str {
    match phase {
        Phase::Header => " (container header incomplete)",
        Phase::Body => "",
        Phase::Trailer => " (gzip trailer incomplete)",
        Phase::NextMember | Phase::AwaitNextMember | Phase::Done => "",
    }
}
