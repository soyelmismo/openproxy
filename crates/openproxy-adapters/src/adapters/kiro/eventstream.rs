use bytes::{Bytes, BytesMut};
use openproxy_types::error::CoreError;
use std::collections::HashMap;

/// Event types emitted by Kiro/CodeWhisperer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KiroEventType {
    AssistantResponse,
    ToolUse,
    MessageStop,
    ContextUsage,
    Metering,
    Other(String),
}

/// A parsed frame event from the AWS EventStream.
#[derive(Debug, Clone)]
pub struct KiroFrameEvent {
    pub event_type: KiroEventType,
    pub payload: Bytes,
}

/// Incremental deframer for AWS EventStream binary chunks.
#[derive(Debug, Default)]
pub struct KiroDeframer {
    buf: BytesMut,
}

impl KiroDeframer {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn next_frame(&mut self) -> Result<Option<KiroFrameEvent>, CoreError> {
        if self.buf.len() < 12 {
            return Ok(None);
        }

        let Some(total_bytes) = self.buf.get(0..4) else {
            return Ok(None);
        };
        let Some(header_bytes) = self.buf.get(4..8) else {
            return Ok(None);
        };
        let Some(prelude_crc_bytes) = self.buf.get(8..12) else {
            return Ok(None);
        };

        let Ok(total_arr): std::result::Result<[u8; 4], _> = total_bytes.try_into() else {
            return Ok(None);
        };
        let Ok(header_arr): std::result::Result<[u8; 4], _> = header_bytes.try_into() else {
            return Ok(None);
        };
        let Ok(prelude_crc_arr): std::result::Result<[u8; 4], _> = prelude_crc_bytes.try_into()
        else {
            return Ok(None);
        };

        let total_len = u32::from_be_bytes(total_arr) as usize;
        let header_len = u32::from_be_bytes(header_arr) as usize;
        let prelude_crc = u32::from_be_bytes(prelude_crc_arr);

        // Validate prelude CRC
        let calc_prelude_crc = crc32fast::hash(&self.buf[0..8]);
        if prelude_crc != calc_prelude_crc {
            return Err(CoreError::UpstreamConnection(format!(
                "kiro eventstream prelude crc mismatch: expected {prelude_crc}, got {calc_prelude_crc}"
            )));
        }

        if self.buf.len() < total_len {
            return Ok(None);
        }

        let frame_bytes = self.buf.split_to(total_len).freeze();

        // Validate message CRC (last 4 bytes)
        if total_len < 4 {
            return Err(CoreError::UpstreamConnection(
                "kiro eventstream frame too small".into(),
            ));
        }
        let msg_len = total_len - 4;
        let Some(msg_crc_bytes) = frame_bytes.get(msg_len..total_len) else {
            return Err(CoreError::UpstreamConnection(
                "kiro eventstream missing crc bytes".into(),
            ));
        };
        let Ok(msg_crc_arr): std::result::Result<[u8; 4], _> = msg_crc_bytes.try_into() else {
            return Err(CoreError::UpstreamConnection(
                "kiro eventstream invalid crc slice".into(),
            ));
        };
        let expected_msg_crc = u32::from_be_bytes(msg_crc_arr);
        let calc_msg_crc = crc32fast::hash(&frame_bytes[0..msg_len]);
        if expected_msg_crc != calc_msg_crc {
            return Err(CoreError::UpstreamConnection(format!(
                "kiro eventstream message crc mismatch: expected {expected_msg_crc}, got {calc_msg_crc}"
            )));
        }

        if 12 + header_len > msg_len {
            return Err(CoreError::UpstreamConnection(
                "kiro eventstream invalid header length".into(),
            ));
        }

        let headers_bytes = &frame_bytes[12..12 + header_len];
        let payload = frame_bytes.slice(12 + header_len..msg_len);

        let headers = parse_eventstream_headers(headers_bytes);
        let event_type_str = headers.get(":event-type").map_or("", String::as_str);
        let event_type = match event_type_str {
            "assistantResponseEvent" => KiroEventType::AssistantResponse,
            "toolUseEvent" => KiroEventType::ToolUse,
            "messageStopEvent" => KiroEventType::MessageStop,
            "contextUsageEvent" => KiroEventType::ContextUsage,
            "meteringEvent" => KiroEventType::Metering,
            other => KiroEventType::Other(other.to_string()),
        };

        Ok(Some(KiroFrameEvent {
            event_type,
            payload,
        }))
    }
}

fn parse_eventstream_headers(bytes: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut idx = 0;
    while idx < bytes.len() {
        let name_len = bytes[idx] as usize;
        idx += 1;
        if idx + name_len > bytes.len() {
            break;
        }
        let Ok(name) = std::str::from_utf8(&bytes[idx..idx + name_len]) else {
            break;
        };
        idx += name_len;
        if idx >= bytes.len() {
            break;
        }
        let val_type = bytes[idx];
        idx += 1;
        if val_type == 7 {
            // String value
            if idx + 2 > bytes.len() {
                break;
            }
            let Ok(val_len_arr): std::result::Result<[u8; 2], _> = bytes[idx..idx + 2].try_into()
            else {
                break;
            };
            let val_len = u16::from_be_bytes(val_len_arr) as usize;
            idx += 2;
            if idx + val_len > bytes.len() {
                break;
            }
            if let Ok(val) = std::str::from_utf8(&bytes[idx..idx + val_len]) {
                map.insert(name.to_string(), val.to_string());
            }
            idx += val_len;
        } else {
            // Unsupported header type in this frame, skip
            break;
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kiro_deframer_roundtrip() {
        let sample = b"\x00\x00\x00\x92\x00\x00\x00\\\xac9\xb6\xc6\x0b:event-type\x07\x00\x16assistantResponseEvent\r:content-type\x07\x00\x10application/json\r:message-type\x07\x00\x05event{\"content\":\"\xc2\xa1Hola!\",\"modelId\":\"auto\"}!\xbe^\x90";
        let mut deframer = KiroDeframer::new();
        deframer.push(sample);
        let frame = deframer.next_frame().unwrap().unwrap();
        assert_eq!(frame.event_type, KiroEventType::AssistantResponse);
        let json: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(json["content"], "¡Hola!");
        assert!(deframer.next_frame().unwrap().is_none());
    }
}
