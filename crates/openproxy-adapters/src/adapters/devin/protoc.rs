//! Minimal protobuf wire codec for the Devin Connect-RPC protocol.
//!
//! Supports exactly the wire types Devin's `exa.api_server_pb` and
//! `exa.seat_management_pb` services use: varint (0), 64-bit (1),
//! length-delimited (2) and 32-bit (5). Groups (3/4) are rejected.

/// Protobuf wire type identifiers.
pub const WIRE_VARINT: u8 = 0;
pub const WIRE_64BIT: u8 = 1;
pub const WIRE_LEN: u8 = 2;
pub const WIRE_32BIT: u8 = 5;

/// Errors produced while decoding Devin protobuf/Connect payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevinWireError {
    /// The buffer ended before the current item was complete.
    Truncated,
    /// The payload violates protobuf or Connect framing rules.
    Malformed(String),
    /// Connect envelope carried a flag outside `{0x00, 0x01, 0x02, 0x03}`.
    BadFlag(u8),
    /// A declared frame length exceeds the compressed-frame cap.
    FrameTooLarge(usize),
    /// gzip wrapper or deflate payload could not be decoded.
    Gzip(String),
    /// Inflated payload exceeded the decompressed-size cap.
    DecompressedTooLarge(usize),
}

impl std::fmt::Display for DevinWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated protobuf payload"),
            Self::Malformed(m) => write!(f, "malformed protobuf payload: {m}"),
            Self::BadFlag(flag) => write!(f, "invalid connect frame flag {flag:#04x}"),
            Self::FrameTooLarge(len) => write!(f, "connect frame too large: {len} bytes"),
            Self::Gzip(m) => write!(f, "gzip decode failed: {m}"),
            Self::DecompressedTooLarge(len) => {
                write!(f, "decompressed frame exceeds cap: {len} bytes")
            }
        }
    }
}

impl std::error::Error for DevinWireError {}

impl From<DevinWireError> for openproxy_types::CoreError {
    fn from(e: DevinWireError) -> Self {
        openproxy_types::CoreError::Parse(format!("devin wire: {e}"))
    }
}

/// Append `field << 3 | wire` as a varint tag.
pub fn append_tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    append_varint(buf, u64::from(field) << 3 | u64::from(wire));
}

/// Append `value` as a base-128 varint.
pub fn append_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Append a varint field: tag (wire 0) then the value.
pub fn append_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    append_tag(buf, field, WIRE_VARINT);
    append_varint(buf, value);
}

/// Append a length-delimited field: tag (wire 2), varint length, raw bytes.
pub fn append_bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    append_tag(buf, field, WIRE_LEN);
    append_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Append a UTF-8 string field (wire 2). Empty strings are still emitted:
/// callers decide whether the field should be written at all.
pub fn append_string(buf: &mut Vec<u8>, field: u32, value: &str) {
    append_bytes(buf, field, value.as_bytes());
}

/// Append a fixed64 field (wire 1), little-endian.
pub fn append_fixed64(buf: &mut Vec<u8>, field: u32, value: u64) {
    append_tag(buf, field, WIRE_64BIT);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Append a nested message field: tag (wire 2), varint length, `sub` verbatim.
pub fn append_message(buf: &mut Vec<u8>, field: u32, sub: &[u8]) {
    append_bytes(buf, field, sub);
}

/// Cursor over a protobuf message body.
///
/// Every accessor returns `None` on truncated or malformed input; callers map
/// that to [`DevinWireError::Truncated`] / [`DevinWireError::Malformed`].
pub struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Whether the cursor consumed the whole buffer.
    pub fn is_done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or(&[])
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Read a raw varint at the cursor.
    pub fn varint(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = *self.take(1)?.first()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        // 10 continuation bytes without a terminator: malformed.
        None
    }

    /// Read the next tag; `None` when the buffer is exhausted or malformed.
    ///
    /// At a clean end-of-message this returns `None` as well; distinguish via
    /// [`Self::is_done`].
    pub fn tag(&mut self) -> Option<(u32, u8)> {
        let raw = self.varint()?;
        let wire = (raw & 0x07) as u8;
        let field = u32::try_from(raw >> 3).ok()?;
        if field == 0 {
            return None;
        }
        Some((field, wire))
    }

    /// Read a varint field value (wire 0 must already be confirmed).
    pub fn varint_value(&mut self) -> Option<u64> {
        self.varint()
    }

    /// Read a length-delimited value (wire 2 must already be confirmed).
    pub fn bytes_value(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).ok()?;
        self.take(len)
    }

    /// Read a fixed64 value (wire 1 must already be confirmed).
    pub fn fixed64_value(&mut self) -> Option<u64> {
        let bytes = self.take(8)?;
        <[u8; 8]>::try_from(bytes).ok().map(u64::from_le_bytes)
    }

    /// Read a fixed32 value (wire 5 must already be confirmed).
    pub fn fixed32_value(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        <[u8; 4]>::try_from(bytes).ok().map(u32::from_le_bytes)
    }

    /// Skip the value for a tag already read. `None` on unsupported wire type
    /// (groups) or truncation.
    pub fn skip(&mut self, wire: u8) -> Option<()> {
        match wire {
            WIRE_VARINT => self.varint().map(|_| ()),
            WIRE_64BIT => self.take(8).map(|_| ()),
            WIRE_LEN => {
                let len = usize::try_from(self.varint()?).ok()?;
                self.take(len).map(|_| ())
            }
            WIRE_32BIT => self.take(4).map(|_| ()),
            _ => None,
        }
    }
}
