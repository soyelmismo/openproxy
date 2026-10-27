//! Independent bounded SSE consumer: validate bytes actually delivered to the client.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct Validator {
    #[serde(skip)]
    line: Vec<u8>,
    #[serde(skip)]
    data: Vec<u8>,
    pub events: u64,
    pub content_events: u64,
    pub content_bytes: u64,
    pub tokens: u64,
    pub checksum: u64,
    pub done: bool,
    pub finish: bool,
    pub usage: Option<(u64, u64, u64)>,
}

impl Validator {
    pub fn push(&mut self, bytes: &[u8]) -> anyhow::Result<bool> {
        let before = self.content_bytes;
        for byte in bytes {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.consume_line(&line)?;
                line.clear();
                self.line = line;
            } else {
                ensure!(self.line.len() < 1024 * 1024, "oversized SSE line");
                self.line.push(*byte);
            }
        }
        Ok(self.content_bytes > before)
    }

    fn consume_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        if line.is_empty() {
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data);
                self.event(&data)?;
                self.data = data;
                self.data.clear();
            }
        } else if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            ensure!(
                self.data.len() + data.len() < 1024 * 1024,
                "oversized SSE event"
            );
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(data);
        }
        Ok(())
    }

    fn event(&mut self, data: &[u8]) -> anyhow::Result<()> {
        ensure!(!self.done, "SSE data after DONE");
        self.events += 1;
        if data == b"[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_slice(data).context("invalid JSON event")?;
        ensure!(value.get("error").is_none(), "inline SSE error");
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            ensure!(self.usage.is_none(), "duplicate usage event");
            let field = |key| {
                usage
                    .get(key)
                    .and_then(Value::as_u64)
                    .context("missing usage counter")
            };
            self.usage = Some((
                field("prompt_tokens")?,
                field("completion_tokens")?,
                field("total_tokens")?,
            ));
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            ensure!(choices.len() <= 1, "unexpected extra choice");
            for choice in choices {
                if let Some(content) = choice
                    .pointer("/delta/content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    ensure!(!self.finish, "content after finish_reason");
                    self.content_events += 1;
                    for byte in content.bytes() {
                        let offset = (self.content_bytes % 4) as usize;
                        ensure!(
                            byte == b" tok"[offset],
                            "text corruption at byte {}",
                            self.content_bytes
                        );
                        if self.content_bytes == 0 {
                            self.checksum = 0xcbf29ce484222325;
                        }
                        self.checksum =
                            (self.checksum ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                        self.content_bytes += 1;
                        if self.content_bytes.is_multiple_of(4) {
                            self.tokens += 1;
                        }
                    }
                }
                if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                    ensure!(
                        !self.finish && reason.as_str() == Some("stop"),
                        "invalid or duplicate finish_reason"
                    );
                    self.finish = true;
                }
            }
        }
        Ok(())
    }

    pub fn complete(&self, expected: usize) -> anyhow::Result<()> {
        ensure!(
            self.line.is_empty() && self.data.is_empty(),
            "truncated SSE event"
        );
        ensure!(self.done && self.finish, "missing DONE or finish_reason");
        ensure!(
            self.tokens == expected as u64 && self.content_bytes == expected as u64 * 4,
            "delivered text length mismatch"
        );
        ensure!(
            self.usage == Some((10, expected as u64, expected as u64 + 10)),
            "usage does not match received text: {:?}",
            self.usage
        );
        Ok(())
    }
}

pub fn self_test() -> anyhow::Result<()> {
    let wire = b": heartbeat\r\ndata: {\"choices\":[{\"delta\":{\"content\":\" tok tok\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\n data-ignored: x\ndata: \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";
    for step in 1..=wire.len() {
        let mut validator = Validator::default();
        let mut timeline_tokens = 0;
        let mut done_transitions = 0;
        for bytes in wire.chunks(step) {
            let before = validator.tokens;
            let was_done = validator.done;
            validator.push(bytes)?;
            timeline_tokens += validator.tokens - before;
            done_transitions += u64::from(!was_done && validator.done);
        }
        validator.complete(2)?;
        ensure!(
            timeline_tokens == 2 && done_transitions == 1,
            "fragmentation-invariant timing counters"
        );
        ensure!(
            validator.events == 3 && validator.content_events == 1,
            "event/frame conflation"
        );
    }
    for invalid in [
        wire[..wire.len() - 2].to_vec(),
        wire.replace_bytes(b" tok tok", b" bad tok"),
        wire.replace_bytes(b"\"completion_tokens\":2", b"\"completion_tokens\":3"),
        wire.replace_bytes(b"data: [DONE]\n\n", b""),
        wire.replace_bytes(b"\"usage\"", b"\"missing\""),
    ] {
        let mut v = Validator::default();
        let outcome = v.push(&invalid).and_then(|_| v.complete(2));
        ensure!(outcome.is_err(), "invalid stream accepted");
    }
    let mut v = Validator::default();
    v.push(wire)?;
    ensure!(
        v.push(b"data: [DONE]\n\n").is_err(),
        "duplicate terminal accepted"
    );
    Ok(())
}

trait ReplaceBytes {
    fn replace_bytes(&self, from: &[u8], to: &[u8]) -> Vec<u8>;
}
impl ReplaceBytes for [u8] {
    fn replace_bytes(&self, from: &[u8], to: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut remaining = self;
        while let Some(pos) = remaining.windows(from.len()).position(|w| w == from) {
            out.extend_from_slice(&remaining[..pos]);
            out.extend_from_slice(to);
            remaining = &remaining[pos + from.len()..];
        }
        out.extend_from_slice(remaining);
        out
    }
}
