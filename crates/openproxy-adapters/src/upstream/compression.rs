//! Transparent HTTP `Content-Encoding` for the shared upstream transport.
//!
//! One module, one call site pair: every provider, schema and request shape
//! (unary JSON, SSE streaming, multipart uploads, plain GET/DELETE) funnels
//! through [`UpstreamClient::call`](super::UpstreamClient::call), which applies
//! [`negotiate_request`] on the way out and installs a [`BodyDecoding`] on the
//! way in. No provider adapter carries compression code.
//!
//! # Negotiation
//!
//! When the caller named no `Accept-Encoding`, `gzip, deflate` is advertised.
//! An explicit `Accept-Encoding` — including `identity` — is an **opt-out** and
//! is forwarded verbatim. The request payload is never touched: no
//! `Content-Encoding` is ever written, so uploads are only compressed if an
//! upstream negotiates it itself, which this transport cannot observe.
//!
//! # Scope
//!
//! Only `gzip` (RFC 1952 over raw DEFLATE) and `deflate` (RFC 1950 zlib, with a
//! raw-DEFLATE fallback for the servers that get it wrong) are negotiated and
//! inverted, both on `miniz_oxide`, which is already a workspace dependency.
//! `br` and `zstd` are **deliberately not advertised**: no crate in
//! `Cargo.lock` provides them, and advertising a codec we cannot invert would
//! hand raw compressed bytes to the JSON/SSE parsers. An upstream that answers
//! with a codec outside this set fails with [`UpstreamError::Decode`] rather
//! than silently corrupting the payload.
//!
//! # Decoding
//!
//! Decoding is incremental: each transport frame is inflated before
//! [`UpstreamBodyStream::next_chunk`](super::UpstreamBodyStream::next_chunk)
//! returns, so parsers and error classification only ever see plaintext. The
//! `CancellationToken`, the idle-chunk gap and the total deadline keep bounding
//! the read, because decoding happens *between* polls, not instead of them.
//! Once decoded, `content-encoding` and `content-length` are dropped: both
//! describe the compressed representation and are now wrong.

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderValue, header};
use miniz_oxide::DataFormat;
use miniz_oxide::MZFlush;
use miniz_oxide::MZStatus;
use miniz_oxide::inflate::stream::{InflateState, inflate};

use super::error::{UpstreamError, UpstreamResult};

/// `Accept-Encoding` value advertised when the caller named none. `identity`
/// needs no `q` parameter: RFC 9110 makes it acceptable unless a request says
/// `identity;q=0`.
///
/// `gzip` and `deflate` are inverted incrementally on `miniz_oxide` (safe for
/// SSE streaming). `br` and `zstd` are also advertised — they give better
/// ratios on large payloads — but are decoded in one shot at EOF, so they are
/// only used on non-streaming responses (see [`Stage::codec_kind`]).
pub(crate) const ADVERTISED_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// Bound on a single gzip container header. `FCOMMENT`/`FNAME` are attacker
/// controlled, so an unterminated field cannot make the transport buffer
/// without limit while waiting for the DEFLATE body.
const MAX_GZIP_HEADER_BYTES: usize = 1 << 20;

/// Upper bound on the bytes buffered from one transport frame before they are
/// handed to the caller. Caps the memory a compression bomb can claim per
/// `next_chunk` without changing the total-size semantics of the stream: input
/// that does not fit is carried over to the next call.
const MAX_DECODED_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Upper bound on the **decoded** bytes a single response may expand to, so a
/// compression bomb cannot exhaust memory. The limit is on the output, never
/// on the compressed length: a few kilobytes of DEFLATE can legitimately
/// expand to megabytes.
const MAX_DECODED_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Per-call inflate output buffer.
const INFLATE_CHUNK_BYTES: usize = 64 * 1024;

// Request side

/// Advertise the codecs this transport can invert, unless the caller named an
/// `Accept-Encoding` of their own (which is an opt-out and is left alone).
pub(crate) fn negotiate_request(headers: &mut HeaderMap) {
    if headers.contains_key(header::ACCEPT_ENCODING) {
        return;
    }
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(ADVERTISED_ACCEPT_ENCODING),
    );
}

/// The codec an upstream applied, resolved to something we can invert.
///
/// `Ok(None)` when the response is not encoded (or is explicitly `identity`).
/// `Err` when it is encoded with something outside this module's scope.
pub(crate) fn response_codec(headers: &HeaderMap) -> UpstreamResult<Option<Codec>> {
    let tokens = content_encoding_tokens(headers);
    classify(&tokens)
}

fn content_encoding_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for token in text.split(',') {
            let token = token.trim();
            if !token.is_empty() {
                tokens.push(token.to_ascii_lowercase());
            }
        }
    }
    tokens
}

fn classify(tokens: &[String]) -> UpstreamResult<Option<Codec>> {
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    if tokens.len() > 1 {
        return Err(UpstreamError::Decode(format!(
            "content-encoding chain `{}` is not supported; only a single gzip or deflate layer is decoded",
            tokens.join(", ")
        )));
    }
    match first.as_str() {
        "gzip" => Ok(Some(Codec::Gzip)),
        "x-gzip" => Ok(Some(Codec::Gzip)),
        "deflate" => Ok(Some(Codec::Deflate)),
        "br" => Ok(Some(Codec::Brotli)),
        "zstd" => Ok(Some(Codec::Zstd)),
        "identity" => Ok(None),
        other => Err(UpstreamError::Decode(format!(
            "content-encoding `{other}` is not supported by this transport; only gzip, deflate, br and zstd are inverted"
        ))),
    }
}

// Codec

/// A response codec this transport can invert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// RFC 1952 container wrapping a raw DEFLATE stream.
    Gzip,
    /// RFC 1950 (zlib), with a raw DEFLATE fallback sniffed from the header.
    Deflate,
    /// RFC 7932 Brotli.
    Brotli,
    /// RFC 8878 Zstandard.
    Zstd,
}

/// How a codec is decoded: incrementally (safe for streaming) or in one shot
/// at EOF (for codecs whose streaming API is not available or not worth it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodecKind {
    /// Inflate on `miniz_oxide`, frame by frame. Streaming-safe.
    Incremental,
    /// Accumulate all compressed bytes, decompress in one shot at `finish`.
    /// Not streaming-safe — only used for non-streaming responses.
    OneShot,
}

impl Codec {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Codec::Gzip => "gzip",
            Codec::Deflate => "deflate",
            Codec::Brotli => "br",
            Codec::Zstd => "zstd",
        }
    }

    pub(super) const fn kind(self) -> CodecKind {
        match self {
            Codec::Gzip | Codec::Deflate => CodecKind::Incremental,
            Codec::Brotli | Codec::Zstd => CodecKind::OneShot,
        }
    }
}

// Response side

/// Decode state carried by an [`UpstreamBodyStream`](super::UpstreamBodyStream).
///
/// `Default` is the passthrough: frames flow through untouched, which is the
/// behaviour of every response that carries no `content-encoding`.
#[derive(Default)]
pub(crate) struct BodyDecoding {
    stage: Option<Stage>,
    /// Decoded bytes not yet handed to the caller.
    out: BytesMut,
    /// Input the inflater could not accept this call (the frame cap was
    /// reached, or a container header / trailer is still incomplete).
    pending: Vec<u8>,
    /// Cursor into `pending`: bytes before it have been consumed.
    pos: usize,
    scratch: Vec<u8>,
    compressed_in: u64,
    decoded_out: u64,
    /// `true` once [`finish`](Self::finish) ran: only then is an empty buffer
    /// after a completed gzip member really the end of the body.
    at_eof: bool,
}

impl std::fmt::Debug for BodyDecoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let codec = self
            .stage
            .as_ref()
            .map_or("identity", |stage| stage.codec.as_str());
        f.debug_struct("BodyDecoding")
            .field("codec", &codec)
            .field("compressed_in", &self.compressed_in)
            .field("decoded_out", &self.decoded_out)
            .field("carried_input", &self.has_carried_input())
            .finish()
    }
}

impl BodyDecoding {
    /// Build the decode state implied by the response headers.
    pub(crate) fn from_response(headers: &HeaderMap) -> UpstreamResult<Self> {
        match response_codec(headers)? {
            Some(codec) => Ok(Self {
                stage: Some(Stage::new(codec)),
                scratch: vec![0u8; INFLATE_CHUNK_BYTES],
                ..Self::default()
            }),
            None => Ok(Self::default()),
        }
    }

    /// `true` when this response is being inverted, i.e. when the transport
    /// headers describing the wire format no longer describe what the caller
    /// receives.
    pub(crate) fn is_decoding(&self) -> bool {
        self.stage.is_some()
    }

    /// Drop the headers the decode invalidated: `content-encoding` (the caller
    /// gets plaintext) and `content-length` (it measured the compressed body).
    pub(crate) fn strip_decoded_headers(&self, headers: &mut HeaderMap) {
        if self.is_decoding() {
            headers.remove(header::CONTENT_ENCODING);
            headers.remove(header::CONTENT_LENGTH);
        }
    }

    /// Feed one raw transport frame. Errors before any plaintext is returned.
    pub(crate) fn push(&mut self, raw: &[u8]) -> UpstreamResult<()> {
        self.compressed_in = self.compressed_in.saturating_add(raw.len() as u64);
        let is_one_shot = self
            .stage
            .as_ref()
            .is_some_and(|s| s.codec.kind() == CodecKind::OneShot);
        if is_one_shot {
            // Accumulate raw compressed bytes; decompress in finish().
            self.pending.extend_from_slice(raw);
            return Ok(());
        }
        if self.stage.is_none() {
            self.out.extend_from_slice(raw);
            return Ok(());
        }
        self.pending.extend_from_slice(raw);
        self.consume()
    }

    /// Drive the decode state machine over `pending`, leaving behind whatever
    /// cannot be accepted yet: an incomplete container header, an incomplete
    /// gzip trailer, or input beyond the per-frame decoded cap.
    fn consume(&mut self) -> UpstreamResult<()> {
        self.consume_inner()?;
        self.compact();
        Ok(())
    }

    fn consume_inner(&mut self) -> UpstreamResult<()> {
        let Self {
            stage,
            out,
            pending,
            pos,
            scratch,
            decoded_out,
            ..
        } = self;
        loop {
            let Some(stage) = stage.as_mut() else {
                return Ok(());
            };
            match stage.phase {
                Phase::Header => {
                    let Some(offset) =
                        stage.resolve_container(pending.get(*pos..).unwrap_or_default())?
                    else {
                        return Ok(());
                    };
                    *pos += offset;
                    stage.phase = Phase::Body;
                }
                Phase::Body => {
                    if out.len() >= MAX_DECODED_FRAME_BYTES {
                        return Ok(());
                    }
                    let used = stage.inflate(
                        pending.get(*pos..).unwrap_or_default(),
                        out,
                        scratch,
                        decoded_out,
                    )?;
                    *pos += used;
                    if stage.done {
                        stage.phase = stage.phase_after_body();
                    } else if used == 0 {
                        return Ok(());
                    }
                }
                Phase::Trailer => {
                    let available = pending.len().saturating_sub(*pos);
                    let take = GZIP_TRAILER_LEN
                        .saturating_sub(stage.trailer.len())
                        .min(available);
                    stage.trailer.extend_from_slice(&pending[*pos..*pos + take]);
                    *pos += take;
                    if stage.trailer.len() < GZIP_TRAILER_LEN {
                        return Ok(());
                    }
                    stage.verify_trailer()?;
                    stage.phase = Phase::NextMember;
                }
                Phase::NextMember => {
                    // Only declare the body finished when the transport has
                    // nothing left to give. An empty buffer here means the
                    // next member has not started arriving yet, not that the
                    // stream is over: RFC 1952 allows members back to back.
                    if *pos >= pending.len() {
                        if self.at_eof {
                            stage.phase = Phase::Done;
                        } else {
                            stage.phase = Phase::AwaitNextMember;
                        }
                        return Ok(());
                    }
                    stage.start_member();
                }
                Phase::AwaitNextMember => {
                    if *pos >= pending.len() {
                        return Ok(());
                    }
                    stage.phase = Phase::NextMember;
                }
                Phase::Done => {
                    if *pos < pending.len() {
                        return Err(UpstreamError::Decode(
                            "trailing bytes after deflate stream".to_string(),
                        ));
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Release the consumed prefix of `pending`. Called after every
    /// [`consume`](Self::consume) so the buffer never grows without bound.
    fn compact(&mut self) {
        if self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
        }
    }

    /// Take the plaintext produced so far, if any.
    pub(crate) fn take_frame(&mut self) -> Option<Bytes> {
        if self.out.is_empty() {
            return None;
        }
        Some(self.out.split().freeze())
    }

    /// `true` while input the inflater could not accept is carried over to the
    /// next frame (the per-frame decoded cap was reached, or a container
    /// header / trailer is still incomplete).
    pub(crate) fn has_carried_input(&self) -> bool {
        self.pos < self.pending.len()
    }

    /// `true` when re-feeding the bytes the decoder already holds could make
    /// progress — that is, it was blocked by the per-frame output cap, not by
    /// input that has not arrived yet.
    ///
    /// While a container header or gzip trailer is still incomplete, only new
    /// input can advance the decoder; re-feeding the same bytes spins.
    pub(crate) fn can_drain_without_input(&self) -> bool {
        self.stage.as_ref().is_some_and(|stage| {
            (self.has_carried_input() || stage.phase == Phase::Body) && !self.blocked_on_input()
        })
    }

    /// `true` when the decoder is waiting on input bytes it does not have yet:
    /// a container header shorter than the minimum, a partial gzip trailer, or
    /// a DEFLATE stream that consumed every buffered byte without finishing.
    fn blocked_on_input(&self) -> bool {
        self.stage.as_ref().is_some_and(|stage| match stage.phase {
            Phase::Header => {
                let buffered = self.pending.len().saturating_sub(self.pos);
                match stage.codec {
                    Codec::Gzip => {
                        buffered < GZIP_FIXED_HEADER_LEN
                            || gzip_body_offset(&self.pending[self.pos..])
                                .ok()
                                .flatten()
                                .is_none()
                    }
                    Codec::Deflate => buffered < 2,
                    Codec::Brotli | Codec::Zstd => false,
                }
            }
            Phase::Trailer => stage.trailer.len() < GZIP_TRAILER_LEN,
            // The inflater accepted every buffered byte and still wants more:
            // only new input can advance it.
            Phase::Body => stage.stalled,
            // Nothing buffered: the next member may still be in flight.
            Phase::AwaitNextMember => true,
            _ => false,
        })
    }

    /// Confirm the compressed stream actually terminated, returning any
    /// plaintext the final drain produced. Called once, at EOF.
    ///
    /// A `content-encoding` on an empty body is a protocol oddity, not
    /// corruption, so zero input bytes finish cleanly.
    pub(crate) fn finish(&mut self) -> UpstreamResult<Option<Bytes>> {
        // One-shot codecs: decompress the entire accumulated buffer at once.
        if let Some(stage) = self.stage.as_ref()
            && stage.codec.kind() == CodecKind::OneShot
        {
            return self.finish_one_shot();
        }
        if self.stage.is_none() || self.compressed_in == 0 {
            return Ok(self.take_frame());
        }
        // A trailer that only completed in the final frame, or output the
        // inflater still holds back, can both be pending at EOF: drain them
        // before judging whether the stream terminated.
        self.at_eof = true;
        self.consume()?;
        // A bounded frame can fill before the compressed stream is exhausted.
        // Yield it and let the next call continue draining at EOF.
        if !self.out.is_empty() && self.can_drain_without_input() {
            return Ok(self.take_frame());
        }
        if let Some(stage) = self.stage.as_mut()
            && stage.phase == Phase::AwaitNextMember
        {
            stage.phase = Phase::Done;
        }
        let Some(stage) = self.stage.as_ref() else {
            return Ok(self.take_frame());
        };
        if stage.phase == Phase::Done {
            return Ok(self.take_frame());
        }
        Err(UpstreamError::Decode(format!(
            "truncated {} body: {} compressed byte(s) in, {} byte(s) decoded before EOF{}",
            stage.codec.as_str(),
            self.compressed_in,
            self.decoded_out,
            phase_hint(stage.phase)
        )))
    }

    /// Decompress the entire accumulated buffer for one-shot codecs (br, zstd).
    fn finish_one_shot(&mut self) -> UpstreamResult<Option<Bytes>> {
        let codec = self.stage.as_ref().map_or(Codec::Gzip, |s| s.codec);
        let raw = std::mem::take(&mut self.pending);
        if raw.is_empty() {
            return Ok(None);
        }
        let mut out = Vec::new();
        match codec {
            Codec::Brotli => {
                let mut out_writer = Vec::new();
                brotli_decompressor::BrotliDecompress(
                    &mut std::io::Cursor::new(&raw[..]),
                    &mut out_writer,
                )
                .map_err(|e| UpstreamError::Decode(format!("br body: {e}")))?;
                out = out_writer;
            }
            Codec::Zstd => {
                let mut reader = zstd::stream::read::Decoder::new(&raw[..])
                    .map_err(|e| UpstreamError::Decode(format!("zstd body: {e}")))?;
                std::io::Read::read_to_end(&mut reader, &mut out)
                    .map_err(|e| UpstreamError::Decode(format!("zstd body: {e}")))?;
            }
            _ => unreachable!("finish_one_shot called with incremental codec"),
        }
        if out.len() as u64 > MAX_DECODED_BODY_BYTES {
            return Err(UpstreamError::Decode(format!(
                "{} body expands to more than {MAX_DECODED_BODY_BYTES} decoded bytes",
                codec.as_str()
            )));
        }
        self.decoded_out = out.len() as u64;
        if out.is_empty() {
            return Ok(None);
        }
        Ok(Some(Bytes::from(out)))
    }
}

/// Where a [`Stage`] is in the container/stream lifecycle.
///
/// The old single `done` flag conflated "the DEFLATE stream ended" with "the
/// body is fully decoded": a gzip member is only complete once its CRC32 +
/// ISIZE trailer has been read and verified, and RFC 1952 allows further
/// members after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
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
struct Stage {
    codec: Codec,
    /// Inflater for the current member.
    state: Option<Box<InflateState>>,
    /// gzip trailer bytes collected so far (at most [`GZIP_TRAILER_LEN`]).
    trailer: Vec<u8>,
    /// Running CRC32 and ISIZE of the current member's decoded output.
    crc: crc32fast::Hasher,
    decoded_len: u32,
    /// `true` once the inflater reported the end of the DEFLATE stream for the
    /// current member. Distinct from [`Phase`]: a gzip member still owes its
    /// trailer after this is set.
    done: bool,
    /// `true` when the last inflate accepted every buffered byte and still
    /// wanted more, so re-feeding the same bytes cannot make progress.
    stalled: bool,
    phase: Phase,
}

impl Stage {
    fn new(codec: Codec) -> Self {
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
    fn start_member(&mut self) {
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
    fn phase_after_body(&self) -> Phase {
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
    fn resolve_container(&mut self, head: &[u8]) -> UpstreamResult<Option<usize>> {
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
    fn inflate(
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
    fn verify_trailer(&self) -> UpstreamResult<()> {
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
fn phase_hint(phase: Phase) -> &'static str {
    match phase {
        Phase::Header => " (container header incomplete)",
        Phase::Body => "",
        Phase::Trailer => " (gzip trailer incomplete)",
        Phase::NextMember | Phase::AwaitNextMember | Phase::Done => "",
    }
}

// gzip container (RFC 1952)

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const GZIP_CM_DEFLATE: u8 = 8;
const GZIP_FHCRC: u8 = 0x02;
/// FLG bits 5..7 are reserved by RFC 1952 and must be zero.
const GZIP_FRESERVED: u8 = 0xe0;
const GZIP_FEXTRA: u8 = 0x04;
const GZIP_FNAME: u8 = 0x08;
const GZIP_FCOMMENT: u8 = 0x10;
const GZIP_FIXED_HEADER_LEN: usize = 10;
/// Length of the CRC32 + ISIZE trailer every gzip member ends with.
const GZIP_TRAILER_LEN: usize = 8;

/// Offset of the raw DEFLATE body inside a gzip container.
///
/// `Ok(None)` means the container header is not complete yet; `Err` means it is
/// malformed and the body must not be fed to an inflater.
fn gzip_body_offset(buf: &[u8]) -> UpstreamResult<Option<usize>> {
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
fn looks_like_zlib(head: &[u8]) -> bool {
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

#[cfg(test)]
mod codec_tests {
    use super::*;

    fn encoded(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(header) = HeaderValue::from_str(value) {
            headers.insert(header::CONTENT_ENCODING, header);
        }
        headers
    }

    #[test]
    fn absent_content_encoding_is_passthrough() {
        assert_eq!(response_codec(&HeaderMap::new()).ok(), Some(None));
    }

    #[test]
    fn gzip_is_recognized_case_insensitively_and_via_x_gzip() {
        for value in ["gzip", "GZIP", "x-gzip"] {
            assert_eq!(
                response_codec(&encoded(value)).ok(),
                Some(Some(Codec::Gzip)),
                "{value}"
            );
        }
    }

    #[test]
    fn deflate_is_recognized() {
        assert_eq!(
            response_codec(&encoded("Deflate")).ok(),
            Some(Some(Codec::Deflate))
        );
    }

    #[test]
    fn identity_is_passthrough() {
        assert_eq!(response_codec(&encoded("identity")).ok(), Some(None));
    }

    #[test]
    fn unsupported_codec_fails_instead_of_handing_raw_bytes_to_parsers() {
        for value in ["compress", "gzip, deflate"] {
            assert!(
                matches!(
                    response_codec(&encoded(value)),
                    Err(UpstreamError::Decode(_))
                ),
                "{value}"
            );
        }
    }

    #[test]
    fn negotiation_is_skipped_when_caller_named_one() {
        let mut headers = HeaderMap::new();
        negotiate_request(&mut headers);
        assert_eq!(
            headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some(ADVERTISED_ACCEPT_ENCODING)
        );

        for explicit in ["identity", "gzip", "br, zstd", "gzip;q=0"] {
            let mut headers = HeaderMap::new();
            if let Ok(header) = HeaderValue::from_str(explicit) {
                headers.insert(header::ACCEPT_ENCODING, header);
            }
            negotiate_request(&mut headers);
            assert_eq!(
                headers
                    .get(header::ACCEPT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                Some(explicit),
                "{explicit}"
            );
        }
    }

    #[test]
    fn decoded_response_drops_the_wire_format_headers() {
        let mut headers = encoded("gzip");
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("123"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let decoding = BodyDecoding::from_response(&headers).expect("decoder");
        assert!(decoding.is_decoding());
        decoding.strip_decoded_headers(&mut headers);

        assert!(headers.get(header::CONTENT_ENCODING).is_none());
        assert!(headers.get(header::CONTENT_LENGTH).is_none());
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn plain_response_keeps_every_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("7"));
        let decoding = BodyDecoding::from_response(&headers).expect("decoder");
        assert!(!decoding.is_decoding());
        decoding.strip_decoded_headers(&mut headers);
        assert_eq!(
            headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
    }

    #[test]
    fn passthrough_frames_are_byte_identical() {
        let mut decoding = BodyDecoding::default();
        decoding.push(b"hello").expect("push");
        assert_eq!(decoding.take_frame().as_deref(), Some(&b"hello"[..]));
        assert!(decoding.take_frame().is_none());
    }

    #[test]
    fn empty_body_with_content_encoding_is_not_a_decode_error() {
        let mut decoding = BodyDecoding::from_response(&encoded("gzip")).expect("decoder");
        assert!(decoding.finish().is_ok());
    }
}

#[cfg(test)]
mod gzip_header_tests {
    use super::*;

    fn fixed(flg: u8) -> Vec<u8> {
        vec![0x1f, 0x8b, 0x08, flg, 0, 0, 0, 0, 0x00, 0xff]
    }

    #[test]
    fn minimal_header_puts_body_at_ten() {
        let mut buf = fixed(0x00);
        buf.extend_from_slice(&[0xaa, 0xbb]);
        assert_eq!(gzip_body_offset(&buf).ok(), Some(Some(10)));
    }

    #[test]
    fn all_optional_fields_are_walked() {
        // FLG = FHCRC | FEXTRA | FNAME | FCOMMENT
        let mut buf = fixed(0x1e);
        buf.extend_from_slice(&[0x04, 0x00]); // XLEN = 4
        buf.extend_from_slice(b"ABCD"); // FEXTRA
        buf.push(b'n');
        buf.push(0); // FNAME
        buf.push(b'c');
        buf.push(0); // FCOMMENT
        // FHCRC is verified, so it must be the real low 16 bits of the CRC32
        // over the header up to and not including this field.
        let fhcrc = u16::try_from(crc32fast::hash(&buf) & 0xffff).unwrap_or_default();
        buf.extend_from_slice(&fhcrc.to_le_bytes());
        let body_at = buf.len();
        buf.push(0x99); // first body byte
        assert_eq!(gzip_body_offset(&buf).ok(), Some(Some(body_at)));
    }

    #[test]
    fn partial_optional_field_waits_for_more_input() {
        let mut buf = fixed(0x08); // FNAME
        buf.extend_from_slice(b"unterminated");
        assert_eq!(gzip_body_offset(&buf).ok(), Some(None));
    }

    #[test]
    fn incomplete_fixed_header_waits() {
        assert_eq!(gzip_body_offset(&[]).ok(), Some(None));
        assert_eq!(gzip_body_offset(&[0x1f, 0x8b, 0x08]).ok(), Some(None));
    }

    #[test]
    fn bad_magic_and_method_are_rejected() {
        let mut buf = fixed(0x00);
        assert!(gzip_body_offset(&buf).is_ok());
        if let Some(byte) = buf.get_mut(1) {
            *byte = 0x8c;
        }
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
        if let Some(byte) = buf.get_mut(1) {
            *byte = 0x8b;
        }
        if let Some(byte) = buf.get_mut(2) {
            *byte = 9;
        }
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
    }

    #[test]
    fn unterminated_fcomment_is_bounded() {
        let mut buf = fixed(0x10); // FCOMMENT
        buf.resize(MAX_GZIP_HEADER_BYTES + 1, b'x');
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
    }

    #[test]
    fn zlib_sniff_accepts_only_valid_headers() {
        assert!(looks_like_zlib(&[0x78, 0x9c]));
        assert!(looks_like_zlib(&[0x78, 0x01]));
        // CMF low nibble must be 8.
        assert!(!looks_like_zlib(&[0x79, 0x01]));
        // (CMF<<8|FLG) % 31 must be 0.
        assert!(!looks_like_zlib(&[0x78, 0x9d]));
        // Window size must be <= 7.
        assert!(!looks_like_zlib(&[0xf8, 0x00]));
        assert!(!looks_like_zlib(&[0x78]));
    }
}
