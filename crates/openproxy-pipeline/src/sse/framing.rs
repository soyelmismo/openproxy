//! Bounded SSE framing over owned transport bytes.

use openproxy_types::error::{CoreError, Result};

pub struct SseParser {
    buffer: bytes::Bytes,
    max_line_bytes: usize,
    scanned: usize,
}

impl SseParser {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            buffer: bytes::Bytes::new(),
            max_line_bytes,
            scanned: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.push_bytes(bytes::Bytes::copy_from_slice(chunk))
    }

    /// Retain transport storage. Only fragmented lines require a copy.
    pub fn push_bytes(&mut self, chunk: bytes::Bytes) -> Result<()> {
        let len = self.buffer.len().saturating_add(chunk.len());
        if len > self.max_line_bytes {
            return Err(CoreError::UpstreamConnection(format!(
                "SSE line buffer exceeded {} bytes (memory-DoS guard)",
                self.max_line_bytes
            )));
        }
        if self.buffer.is_empty() {
            self.buffer = chunk;
        } else if !chunk.is_empty() {
            let previous = std::mem::take(&mut self.buffer);
            let mut joined = previous.try_into_mut().unwrap_or_else(|shared| {
                let mut joined = bytes::BytesMut::with_capacity(len);
                joined.extend_from_slice(&shared);
                joined
            });
            joined.extend_from_slice(&chunk);
            self.buffer = joined.freeze();
        }
        Ok(())
    }

    fn newline(&mut self) -> Option<usize> {
        // Fragmented lines must not rescan the entire prefix on each read:
        // one-byte transport fragments otherwise turn framing into O(n²).
        match memchr::memchr(b'\n', self.buffer.get(self.scanned..)?) {
            Some(relative) => {
                let pos = self.scanned + relative;
                self.scanned = 0;
                Some(pos)
            }
            None => {
                self.scanned = self.buffer.len();
                None
            }
        }
    }

    pub fn next_line(&mut self) -> Option<bytes::Bytes> {
        use bytes::Buf;
        let pos = self.newline()?;
        let line = self.buffer.split_to(pos);
        self.buffer.advance(1);
        Some(line)
    }

    /// Preserve complete OpenAI frames so passthrough needs no allocation.
    /// A split delimiter falls back to line framing without delaying a token.
    pub fn next_openai_frame(&mut self) -> Option<bytes::Bytes> {
        let pos = self.newline()?;
        let line_end = pos + 1;
        let tail = self.buffer.get(line_end..).unwrap_or_default();
        let delimiter = if tail.starts_with(b"\n") {
            1
        } else if tail.starts_with(b"\r\n") {
            2
        } else {
            0
        };
        Some(self.buffer.split_to(line_end + delimiter))
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn remaining_bytes(&self) -> &[u8] {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn complete_frame_reuses_transport_allocation() {
        let wire = Bytes::from_static(b"data: {\"text\":\"hola\"}\n\n");
        let pointer = wire.as_ptr();
        let mut parser = SseParser::new(1024);
        parser.push_bytes(wire.clone()).unwrap();
        let frame = parser.next_openai_frame().unwrap();
        assert_eq!(frame, wire);
        assert_eq!(frame.as_ptr(), pointer);
        assert!(parser.is_empty());
    }

    #[test]
    fn fragmented_utf8_and_crlf_preserve_bytes() {
        let wire = "data: {\"text\":\"🌍ñ\"}\r\n\r\n".as_bytes();
        for split in 1..wire.len() {
            let mut parser = SseParser::new(1024);
            let mut output = Vec::new();
            parser.push(&wire[..split]).unwrap();
            while let Some(frame) = parser.next_openai_frame() {
                output.extend_from_slice(&frame);
            }
            parser.push(&wire[split..]).unwrap();
            while let Some(frame) = parser.next_openai_frame() {
                output.extend_from_slice(&frame);
            }
            assert_eq!(output, wire, "split={split}");
            assert!(parser.is_empty());
        }
    }

    #[test]
    fn fragmented_line_scans_each_byte_once() {
        let mut parser = SseParser::new(4096);
        for _ in 0..2048 {
            parser.push(b"x").unwrap();
            assert!(parser.next_openai_frame().is_none());
            assert_eq!(parser.scanned, parser.buffer.len());
        }
        parser.push(b"\n\ndata: tail\n\n").unwrap();
        assert_eq!(parser.next_openai_frame().unwrap().len(), 2050);
        assert_eq!(parser.scanned, 0);
        assert_eq!(parser.next_line().unwrap().as_ref(), b"data: tail");
        assert!(parser.next_line().unwrap().is_empty());
        assert!(parser.next_line().is_none());
    }

    #[test]
    fn named_events_still_use_line_framing() {
        let mut parser = SseParser::new(1024);
        parser.push(b"event: delta\ndata: {}\n\n").unwrap();
        assert_eq!(parser.next_line().unwrap().as_ref(), b"event: delta");
        assert_eq!(parser.next_line().unwrap().as_ref(), b"data: {}");
        assert!(parser.next_line().unwrap().is_empty());
        assert!(parser.next_line().is_none());
    }

    #[test]
    fn retained_line_survives_next_push_and_limit_rejection() {
        let mut parser = SseParser::new(16);
        parser.push(b"data: a\npartial").unwrap();
        let line = parser.next_line().unwrap();
        parser.push(b" tail\n").unwrap();
        assert_eq!(parser.next_line().unwrap().as_ref(), b"partial tail");
        assert_eq!(line.as_ref(), b"data: a");
        assert!(parser.push(b"01234567890123456").is_err());
        assert!(parser.is_empty());
    }
}
