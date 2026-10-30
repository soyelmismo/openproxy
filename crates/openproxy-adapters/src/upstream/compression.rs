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
//! When the caller named no `Accept-Encoding`, the codecs this transport can
//! invert are advertised. Which ones depends on the request: a **streaming**
//! request gets only `gzip, deflate`, the two inverted incrementally on
//! `miniz_oxide`, because a stream must yield plaintext per frame. A
//! **non-streaming** request also gets `br, zstd`, which decode only at EOF
//! but give better ratios — safe there because the body is collected in full
//! anyway. An explicit `Accept-Encoding` — including `identity` — is an
//! **opt-out** and is forwarded verbatim. The request payload is never
//! touched: no `Content-Encoding` is ever written, so uploads are only
//! compressed if an upstream negotiates it itself, which this transport
//! cannot observe.
//!
//! # Scope
//!
//! `gzip` (RFC 1952 over raw DEFLATE) and `deflate` (RFC 1950 zlib, with a
//! raw-DEFLATE fallback for the servers that get it wrong) are negotiated and
//! inverted incrementally on `miniz_oxide`. `br` and `zstd` are negotiated
//! too — via `brotli-decompressor` and `zstd` — but only for non-streaming
//! responses, where they are decoded in one shot at EOF. An upstream that
//! answers with a codec outside this set fails with
//! [`UpstreamError::Decode`] rather than silently corrupting the payload.
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

use super::error::{UpstreamError, UpstreamResult};

#[path = "compression_gzip.rs"]
pub(crate) mod compression_gzip;
#[path = "compression_stage.rs"]
pub(crate) mod compression_stage;

use compression_gzip::{GZIP_FIXED_HEADER_LEN, GZIP_TRAILER_LEN, gzip_body_offset};
use compression_stage::{Phase, Stage, phase_hint};

/// `Accept-Encoding` value advertised when the caller named none. `identity`
/// needs no `q` parameter: RFC 9110 makes it acceptable unless a request says
/// `identity;q=0`.
///
/// Codecs inverted **incrementally** on `miniz_oxide`: each `push` yields
/// plaintext immediately, so they are safe for SSE streaming.
pub(crate) const INCREMENTAL_ACCEPT_ENCODING: &str = "gzip, deflate";

/// Full set, including one-shot codecs with better ratios on large payloads.
/// `br` and `zstd` can only be decoded at EOF (their streaming APIs are
/// pull-based), so advertising them on a streaming request would make us
/// buffer the whole response and emit nothing until the upstream closed the
/// connection — the client would see silence instead of tokens.
pub(crate) const ADVERTISED_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// Bound on a single gzip container header. `FCOMMENT`/`FNAME` are attacker
/// controlled, so an unterminated field cannot make the transport buffer
/// without limit while waiting for the DEFLATE body.
pub(crate) const MAX_GZIP_HEADER_BYTES: usize = 1 << 20;

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
///
/// On a streaming request only the incrementally-decoded codecs are offered:
/// an upstream that answered `br`/`zstd` to a stream would have its whole
/// response buffered here until EOF, which turns streaming into one final
/// block. Non-streaming responses are collected in full anyway, so there the
/// full set (better ratios) is safe.
pub(crate) fn negotiate_request(headers: &mut HeaderMap, is_streaming: bool) {
    if headers.contains_key(header::ACCEPT_ENCODING) {
        return;
    }
    let advertised = if is_streaming {
        INCREMENTAL_ACCEPT_ENCODING
    } else {
        ADVERTISED_ACCEPT_ENCODING
    };
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(advertised),
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
    ///
    /// One-shot codecs (`br`, `zstd`) never answer `true`. They decode only in
    /// [`finish`](Self::finish), at EOF, so the bytes they hold are buffered
    /// input, not carried input: re-feeding them yields no plaintext and no
    /// progress. Reporting `true` made `next_chunk` push an empty frame and
    /// `continue` forever, never polling the transport, so EOF was never
    /// reached and the request burned a core until `total_deadline` expired.
    pub(crate) fn can_drain_without_input(&self) -> bool {
        let Some(stage) = self.stage.as_ref() else {
            return false;
        };
        if stage.codec.kind() == CodecKind::OneShot {
            return false;
        }
        (self.has_carried_input() || stage.phase == Phase::Body) && !self.blocked_on_input()
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
