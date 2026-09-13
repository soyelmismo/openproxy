//! Streaming window replacer and SSE chunk restoration stage.

use aho_corasick::{AhoCorasick, MatchKind};
use crate::streaming::{StreamAction, StreamingChunkStage};
use std::collections::{HashMap, HashSet};

/// Streaming window replacer that handles placeholders split across SSE chunk boundaries
/// (e.g. chunk 1 has `alex.turn`, chunk 2 has `er1@fastmail.com`) and emits restored text smoothly
/// in strict O(N) linear time using an Aho-Corasick multi-pattern automaton.
#[derive(Debug, Clone)]
pub struct StreamingWindowReplacer {
    /// Mapping of placeholder -> original value
    mapping: HashMap<String, String>,
    /// Precompiled Aho-Corasick automaton with LeftmostLongest semantics
    automaton: Option<AhoCorasick>,
    /// Parallel array of replacement strings corresponding to automaton pattern indices
    replacements: Vec<String>,
    /// Set of proper prefixes for all patterns to support constant-time boundary checking
    prefixes: HashSet<String>,
    /// Pending partial placeholder buffer (e.g. "alex.tur")
    pending: String,
    /// Maximum length of any placeholder in mapping
    max_len: usize,
}

impl StreamingWindowReplacer {
    pub fn new(mapping: HashMap<String, String>) -> Self {
        if mapping.is_empty() {
            return Self {
                mapping,
                automaton: None,
                replacements: Vec::new(),
                prefixes: HashSet::new(),
                pending: String::new(),
                max_len: 0,
            };
        }

        let mut pairs: Vec<(String, String)> = mapping.into_iter().collect();
        // Sort patterns by length descending
        pairs.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

        let max_len = pairs.first().map_or(0, |(k, _)| k.len());
        let patterns: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
        let replacements: Vec<String> = pairs.iter().map(|(_, v)| v.clone()).collect();

        let mut prefixes = HashSet::new();
        for pat in &patterns {
            for (byte_idx, _) in pat.char_indices() {
                if byte_idx > 0 {
                    prefixes.insert(pat[..byte_idx].to_string());
                }
            }
        }

        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .ok();

        let mapping = pairs.into_iter().collect();

        Self {
            mapping,
            automaton,
            replacements,
            prefixes,
            pending: String::new(),
            max_len,
        }
    }

    pub fn from_session(session: &super::session::PiiSession) -> Self {
        if session.reversible {
            Self::new(session.reverse.clone())
        } else {
            Self::new(HashMap::new())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.mapping.is_empty()
    }

    /// Process an incoming chunk of text and return the reconstituted string.
    pub fn process(&mut self, chunk: &str) -> String {
        self.process_internal(chunk, false)
    }

    /// Flush any remaining buffered characters at end of stream.
    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let pending = std::mem::take(&mut self.pending);
        self.process_internal(&pending, true)
    }

    fn process_internal(&mut self, chunk: &str, is_eof: bool) -> String {
        let Some(ref ac) = self.automaton else {
            return chunk.to_string();
        };

        let input = if self.pending.is_empty() {
            if chunk.is_empty() {
                return String::new();
            }
            chunk.to_string()
        } else {
            let mut s = std::mem::take(&mut self.pending);
            s.push_str(chunk);
            s
        };

        if input.is_empty() {
            return String::new();
        }

        // On EOF, run direct complete Aho-Corasick replacement without boundary buffering
        if is_eof {
            return ac.replace_all(&input, &self.replacements);
        }

        let len = input.len();
        let mut out = String::with_capacity(len + 32);
        let mut last_end = 0;

        for mat in ac.find_iter(&input) {
            if mat.start() < last_end {
                continue;
            }

            // If match touches the very trailing edge of chunk and could be a prefix of a longer pattern
            if mat.end() == len {
                let candidate = &input[mat.start()..];
                if self.prefixes.contains(candidate) {
                    out.push_str(&input[last_end..mat.start()]);
                    self.pending = candidate.to_string();
                    return out;
                }
            }

            out.push_str(&input[last_end..mat.start()]);
            let pat_idx = mat.pattern().as_usize();
            out.push_str(&self.replacements[pat_idx]);
            last_end = mat.end();
        }

        // Check if any suffix of trailing uncommitted text is in self.prefixes
        let trailing = &input[last_end..];
        if trailing.is_empty() {
            return out;
        }

        let mut split_point = None;
        for (byte_offset, _) in trailing.char_indices() {
            let suffix = &trailing[byte_offset..];
            if suffix.len() < self.max_len && self.prefixes.contains(suffix) {
                split_point = Some(byte_offset);
                break;
            }
        }

        if let Some(offset) = split_point {
            out.push_str(&trailing[..offset]);
            self.pending = trailing[offset..].to_string();
        } else {
            out.push_str(trailing);
        }

        out
    }
}

/// Stage for the streaming chunk pipeline that restores placeholders in SSE payloads.
/// Implements StreamingChunkStage to cleanly integrate with the upstream pipeline.
pub struct PiiRestorationStage {
    content_replacer: StreamingWindowReplacer,
    reasoning_replacer: StreamingWindowReplacer,
    tool_replacer: StreamingWindowReplacer,
    buffered_json: String,
}

impl PiiRestorationStage {
    pub fn new(session: &super::session::PiiSession) -> Self {
        Self {
            content_replacer: StreamingWindowReplacer::from_session(session),
            reasoning_replacer: StreamingWindowReplacer::from_session(session),
            tool_replacer: StreamingWindowReplacer::from_session(session),
            buffered_json: String::new(),
        }
    }

    pub fn from_mapping(mapping: HashMap<String, String>) -> Self {
        Self {
            content_replacer: StreamingWindowReplacer::new(mapping.clone()),
            reasoning_replacer: StreamingWindowReplacer::new(mapping.clone()),
            tool_replacer: StreamingWindowReplacer::new(mapping),
            buffered_json: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content_replacer.is_empty()
    }

    /// Helper to process a full SSE wire frame or multi-event chunk.
    pub fn process_frame(&mut self, frame: &str) -> String {
        if self.is_empty() || frame.is_empty() {
            return frame.to_string();
        }

        let normalized = frame.replace("\r\n", "\n");
        let has_trailing_event_sep = normalized.ends_with("\n\n");
        let parts: Vec<&str> = normalized.split("\n\n").collect();
        let mut out = String::with_capacity(frame.len() + 32);

        for part in parts {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut out_lines = Vec::new();
            for line in trimmed.lines() {
                let trimmed_line = line.trim_end_matches(['\r', ' ']);
                if let Some(payload) = trimmed_line.strip_prefix("data:").map(str::trim_start) {
                    if payload == "[DONE]" {
                        if let Some(residual_frame) = self.flush_residual_frame() {
                            let trimmed_frame = residual_frame.trim_end_matches('\n');
                            out_lines.push(trimmed_frame.to_string());
                        }
                        out_lines.push("data: [DONE]".to_string());
                        continue;
                    }

                    match self.process_chunk(payload) {
                        StreamAction::Mutate(mutated) => out_lines.push(format!("data: {mutated}")),
                        StreamAction::Passthrough => out_lines.push(trimmed_line.to_string()),
                        StreamAction::Skip => {}
                        StreamAction::Done => out_lines.push("data: [DONE]".to_string()),
                    }
                } else {
                    out_lines.push(trimmed_line.to_string());
                }
            }
            let block = out_lines.join("\n");
            if !block.is_empty() {
                out.push_str(&block);
                out.push_str("\n\n");
            }
        }

        if out.is_empty() && has_trailing_event_sep {
            return "\n\n".to_string();
        }

        out
    }

    /// Flushes any pending partial placeholder buffers as a single OpenAI-compatible SSE data frame.
    pub fn flush_residual_frame(&mut self) -> Option<String> {
        self.finalize().map(|json| format!("data: {json}\n\n"))
    }

    /// Inherent finalization to flush buffered replacers without requiring trait in scope.
    pub fn finalize(&mut self) -> Option<String> {
        let content_flushed = self.content_replacer.flush();
        let reasoning_flushed = self.reasoning_replacer.flush();
        let tool_flushed = self.tool_replacer.flush();

        if content_flushed.is_empty() && reasoning_flushed.is_empty() && tool_flushed.is_empty() {
            return None;
        }

        let mut delta = serde_json::Map::new();
        if !content_flushed.is_empty() {
            delta.insert(
                "content".to_string(),
                serde_json::Value::String(content_flushed),
            );
        }
        if !reasoning_flushed.is_empty() {
            delta.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(reasoning_flushed),
            );
        }
        if !tool_flushed.is_empty() {
            delta.insert(
                "tool_calls".to_string(),
                serde_json::json!([{
                    "index": 0,
                    "function": {
                        "arguments": tool_flushed
                    }
                }]),
            );
        }
        let residual_val = serde_json::json!({
            "choices": [{
                "delta": delta
            }]
        });
        Some(serde_json::to_string(&residual_val).unwrap_or_default())
    }
}

impl StreamingChunkStage for PiiRestorationStage {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        if self.is_empty() {
            return StreamAction::Passthrough;
        }

        // Handle possible TCP packet fragmentation across JSON boundaries
        let (to_parse, is_buffered) = if !self.buffered_json.is_empty() {
            self.buffered_json.push_str(payload);
            let combined = std::mem::take(&mut self.buffered_json);
            (combined, true)
        } else {
            (payload.to_string(), false)
        };

        match serde_json::from_str::<serde_json::Value>(&to_parse) {
            Ok(mut val) => {
                let mut changed = false;

                if let Some(choices) = val.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for choice in choices {
                        if let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) {
                            if let Some(content) = delta.get_mut("content").and_then(|c| c.as_str()) {
                                let restored = self.content_replacer.process(content);
                                delta.insert(
                                    "content".to_string(),
                                    serde_json::Value::String(restored),
                                );
                                changed = true;
                            }
                            if let Some(reasoning) =
                                delta.get_mut("reasoning_content").and_then(|c| c.as_str())
                            {
                                let restored = self.reasoning_replacer.process(reasoning);
                                delta.insert(
                                    "reasoning_content".to_string(),
                                    serde_json::Value::String(restored),
                                );
                                changed = true;
                            }
                            if let Some(tool_calls) =
                                delta.get_mut("tool_calls").and_then(|tc| tc.as_array_mut())
                            {
                                for tc in tool_calls {
                                    if let Some(func) =
                                        tc.get_mut("function").and_then(|f| f.as_object_mut())
                                        && let Some(args) =
                                            func.get_mut("arguments").and_then(|a| a.as_str())
                                    {
                                        let restored = self.tool_replacer.process(args);
                                        func.insert(
                                            "arguments".to_string(),
                                            serde_json::Value::String(restored),
                                        );
                                        changed = true;
                                    }
                                }
                            }
                        }
                    }
                }

                if changed {
                    StreamAction::Mutate(
                        serde_json::to_string(&val).unwrap_or(to_parse),
                    )
                } else if is_buffered {
                    StreamAction::Mutate(to_parse)
                } else {
                    StreamAction::Passthrough
                }
            }
            Err(_) => {
                // If payload starts like a JSON object but failed to parse, it may be fragmented
                if to_parse.trim_start().starts_with('{') {
                    self.buffered_json = to_parse;
                    StreamAction::Skip
                } else {
                    // Non-JSON pure text line (e.g. raw text stream)
                    let restored = self.content_replacer.process(&to_parse);
                    if restored != to_parse {
                        StreamAction::Mutate(restored)
                    } else if is_buffered {
                        StreamAction::Mutate(to_parse)
                    } else {
                        StreamAction::Passthrough
                    }
                }
            }
        }
    }

    fn finalize(&mut self) -> Option<String> {
        self.finalize()
    }
}
