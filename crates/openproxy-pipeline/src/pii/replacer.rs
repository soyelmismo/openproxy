//! Streaming window replacer and SSE chunk restoration stage.

use crate::streaming::{StreamAction, StreamingChunkStage};
use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use std::collections::{BTreeSet, HashMap};

mod contact;
use contact::{contact_kind, extends_contact_left, extends_contact_right};

#[inline]
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Placeholders the redactor minted itself (`{prefix}` + 8 lowercase hex, per
/// `get_or_create_placeholder`'s SECRET_PREFIX_MAP, plus the attached CLI
/// alias `-p` + placeholder) are unique fixed-width strings that cannot occur
/// in natural text by accident. For them the token boundaries that protect
/// human-authored placeholders must be skipped: the forward pass can leave
/// them glued to identifiers (`step-5-psec_x` inside a model slug,
/// `sec_x_receipts` as a table name), and a boundary check would make those
/// originals permanently unrestorable.
#[inline]
fn is_synthetic_secret_placeholder(pat: &str) -> bool {
    let body = pat.strip_prefix("-p").unwrap_or(pat);
    for prefix in [
        "sec_",
        "sk-proj-",
        "sk-",
        "op_live_",
        "op_test_",
        "mcp_live_",
        "mcp_test_",
        "ghp_",
        "gho_",
        "xoxb_",
        "Bearer sec_",
    ] {
        if let Some(hex) = body.strip_prefix(prefix)
            && hex.len() == 8
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return true;
        }
    }
    false
}

fn json_escape_string(s: &str) -> String {
    let Ok(json) = serde_json::to_string(s) else {
        return s.to_string();
    };
    if json.starts_with('"') && json.ends_with('"') && json.len() >= 2 {
        json[1..json.len() - 1].to_string()
    } else {
        json
    }
}

#[derive(Debug)]
struct ReplacerDictionary {
    automaton: Option<AhoCorasick>,
    patterns: Vec<String>,
    replacements: Vec<String>,
    first_byte_mask: [u64; 256],
    prefix_first_bytes: [bool; 256],
    max_len: usize,
}

#[derive(Clone, Copy)]
struct DictMatch {
    pat_idx: usize,
    start: usize,
    end: usize,
}

enum DictMatchIter<'a> {
    Bitmask {
        dict: &'a ReplacerDictionary,
        input: &'a [u8],
        pos: usize,
    },
    Automaton(aho_corasick::FindIter<'a, 'a>),
}

impl<'a> Iterator for DictMatchIter<'a> {
    type Item = DictMatch;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            DictMatchIter::Bitmask { dict, input, pos } => {
                let len = input.len();
                let mut cur = *pos;
                while cur < len {
                    let mut mask = dict.first_byte_mask[input[cur] as usize];
                    while mask != 0 {
                        let pat_idx = mask.trailing_zeros() as usize;
                        mask &= mask - 1;
                        let pat_bytes = dict.patterns[pat_idx].as_bytes();
                        let end = cur + pat_bytes.len();
                        if end <= len && input[cur..end].eq_ignore_ascii_case(pat_bytes) {
                            *pos = end.max(cur + 1);
                            return Some(DictMatch {
                                pat_idx,
                                start: cur,
                                end,
                            });
                        }
                    }
                    cur += 1;
                }
                *pos = len;
                None
            }
            DictMatchIter::Automaton(iter) => iter.next().map(|m| DictMatch {
                pat_idx: m.pattern().as_usize(),
                start: m.start(),
                end: m.end(),
            }),
        }
    }
}

impl ReplacerDictionary {
    #[inline]
    fn find_iter<'a>(&'a self, input: &'a str) -> DictMatchIter<'a> {
        if let Some(ref ac) = self.automaton {
            DictMatchIter::Automaton(ac.find_iter(input))
        } else {
            DictMatchIter::Bitmask {
                dict: self,
                input: input.as_bytes(),
                pos: 0,
            }
        }
    }

    #[inline]
    fn has_prefix_ignore_ascii_case(&self, s: &str) -> bool {
        let s_len = s.len();
        if s_len == 0 || s_len >= self.max_len {
            return false;
        }
        let s_bytes = s.as_bytes();
        if !self.prefix_first_bytes[s_bytes[0].to_ascii_lowercase() as usize] {
            return false;
        }
        for pat in &self.patterns {
            if pat.len() <= s_len {
                break;
            }
            if pat.as_bytes()[..s_len].eq_ignore_ascii_case(s_bytes) {
                return true;
            }
        }
        false
    }
}

/// Placeholders split across SSE chunk boundaries (chunk 1 has `alex.turn`,
/// chunk 2 has `er1@fastmail.com`) are buffered until the match is decidable,
/// so restoration stays O(N) through one Aho-Corasick automaton.
#[derive(Debug, Clone)]
pub struct StreamingWindowReplacer {
    dict: Option<std::sync::Arc<ReplacerDictionary>>,
    pending: String,
    last_char: Option<char>,
}

impl StreamingWindowReplacer {
    pub fn new(mapping: HashMap<String, String>) -> Self {
        if mapping.is_empty() {
            return Self {
                dict: None,
                pending: String::new(),
                last_char: None,
            };
        }

        let mut pairs: Vec<(String, String)> = mapping.into_iter().collect();
        pairs.sort_by(|(k1, v1), (k2, v2)| {
            k2.len()
                .cmp(&k1.len())
                .then_with(|| k1.cmp(k2))
                .then_with(|| v1.cmp(v2))
        });

        let max_len = pairs.first().map_or(0, |(k, _)| k.len());
        let mut patterns = Vec::with_capacity(pairs.len());
        let mut replacements = Vec::with_capacity(pairs.len());
        let mut first_byte_mask = [0u64; 256];
        let mut prefix_first_bytes = [false; 256];
        for (idx, (k, v)) in pairs.into_iter().enumerate() {
            if let Some(&first_byte) = k.as_bytes().first() {
                let lower = first_byte.to_ascii_lowercase() as usize;
                let upper = first_byte.to_ascii_uppercase() as usize;
                if k.len() > 1 {
                    prefix_first_bytes[lower] = true;
                }
                if idx < 64 {
                    first_byte_mask[lower] |= 1u64 << idx;
                    first_byte_mask[upper] |= 1u64 << idx;
                }
            }
            patterns.push(k);
            replacements.push(v);
        }

        let automaton = if patterns.len() > 64 {
            AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .match_kind(MatchKind::LeftmostLongest)
                .kind(Some(AhoCorasickKind::ContiguousNFA))
                .build(&patterns)
                .ok()
        } else {
            None
        };

        Self {
            dict: Some(std::sync::Arc::new(ReplacerDictionary {
                automaton,
                patterns,
                replacements,
                first_byte_mask,
                prefix_first_bytes,
                max_len,
            })),
            pending: String::new(),
            last_char: None,
        }
    }

    pub fn from_session(session: &super::session::PiiSession) -> Self {
        if session.reversible {
            Self::new(session.restoration_mapping())
        } else {
            Self::new(HashMap::new())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.dict.is_none()
    }

    pub fn process(&mut self, chunk: &str) -> String {
        self.process_internal(chunk, false)
    }

    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let pending = std::mem::take(&mut self.pending);
        let out = self.process_internal(&pending, true);
        self.last_char = None;
        out
    }

    fn process_internal(&mut self, chunk: &str, is_eof: bool) -> String {
        let Some(ref dict) = self.dict else {
            return chunk.to_string();
        };
        if dict.patterns.is_empty() {
            return chunk.to_string();
        };

        let input: std::borrow::Cow<'_, str> = if self.pending.is_empty() {
            if chunk.is_empty() {
                return String::new();
            }
            std::borrow::Cow::Borrowed(chunk)
        } else {
            let mut s = std::mem::take(&mut self.pending);
            s.push_str(chunk);
            std::borrow::Cow::Owned(s)
        };

        if input.is_empty() {
            return String::new();
        }

        let len = input.len();
        let undecidable_start = if is_eof {
            None
        } else {
            let mut start = len.saturating_sub(dict.max_len.saturating_sub(1));
            while !input.is_char_boundary(start) {
                start += 1;
            }
            input[start..].char_indices().find_map(|(offset, _)| {
                let pos = start + offset;
                dict.has_prefix_ignore_ascii_case(&input[pos..])
                    .then_some(pos)
            })
        };
        let mut out = String::with_capacity(len + 32);
        let mut last_end = 0;

        for mat in dict.find_iter(input.as_ref()) {
            // A complete short alias can still be the beginning of a longer
            // placeholder: `Alex ` + `Vance`. Do not emit the first-name alias
            // before the full-name match can be decided on the next chunk.
            if undecidable_start.is_some_and(|start| mat.start >= start) {
                break;
            }
            if mat.start < last_end {
                continue;
            }

            let pat_idx = mat.pat_idx;
            let pat = &dict.patterns[pat_idx];

            let mat_start = mat.start;
            let mat_end = mat.end;

            // 1. Check LEFT boundary
            let prev_char = if mat_start > 0 {
                input[..mat_start].chars().next_back()
            } else {
                self.last_char
            };

            let synthetic = is_synthetic_secret_placeholder(pat);
            let pat_starts_token =
                pat.starts_with(is_token_char) || pat.starts_with('+') || pat.starts_with("-p");
            if !synthetic && pat_starts_token && prev_char.is_some_and(is_token_char) {
                continue;
            }
            let contact = contact_kind(pat);
            if contact.is_some_and(|kind| prev_char.is_some_and(|c| extends_contact_left(kind, c)))
            {
                continue;
            }

            // 2. Check RIGHT boundary
            if mat_end == len {
                if !is_eof {
                    let candidate = &input[mat_start..];
                    let is_prefix = dict.has_prefix_ignore_ascii_case(candidate);
                    let pat_ends_token = pat.ends_with(is_token_char);
                    if is_prefix || pat_ends_token {
                        out.push_str(&input[last_end..mat_start]);
                        self.last_char = input[..mat_start].chars().next_back().or(self.last_char);
                        self.pending = candidate.to_string();
                        return out;
                    }
                }
            } else {
                let next_char = input[mat_end..].chars().next();
                let pat_ends_token = pat.ends_with(is_token_char);
                if !synthetic && pat_ends_token && next_char.is_some_and(is_token_char) {
                    continue;
                }
                if let Some(kind) = contact {
                    match extends_contact_right(kind, &input[mat_end..], is_eof) {
                        Some(true) => continue,
                        Some(false) => {}
                        None => {
                            out.push_str(&input[last_end..mat_start]);
                            self.last_char =
                                input[..mat_start].chars().next_back().or(self.last_char);
                            self.pending = input[mat_start..].to_string();
                            return out;
                        }
                    }
                }
            }

            out.push_str(&input[last_end..mat_start]);
            out.push_str(&dict.replacements[pat_idx]);
            last_end = mat_end;
            if let Some(c) = out.chars().next_back() {
                self.last_char = Some(c);
            }
        }

        let trailing = &input[last_end..];
        if trailing.is_empty() {
            if let Some(c) = out.chars().next_back() {
                self.last_char = Some(c);
            }
            return out;
        }

        if is_eof || dict.max_len == 0 {
            out.push_str(trailing);
            if let Some(c) = out.chars().next_back() {
                self.last_char = Some(c);
            }
            return out;
        }

        // Avoid O(N^2): limit scan to the last max_len bytes
        let scan_start = if trailing.len() >= dict.max_len {
            let mut idx = trailing.len() - (dict.max_len - 1);
            while idx < trailing.len() && !trailing.is_char_boundary(idx) {
                idx += 1;
            }
            idx.min(trailing.len())
        } else {
            0
        };

        out.push_str(&trailing[..scan_start]);

        let mut split_point = None;
        for (offset, _) in trailing[scan_start..].char_indices() {
            let abs_offset = scan_start + offset;
            let suffix = &trailing[abs_offset..];
            if dict.has_prefix_ignore_ascii_case(suffix) {
                split_point = Some(abs_offset);
                break;
            }
        }

        if let Some(offset) = split_point {
            out.push_str(&trailing[scan_start..offset]);
            self.pending = trailing[offset..].to_string();
        } else {
            out.push_str(&trailing[scan_start..]);
        }

        let emitted_end = input.len() - self.pending.len();
        self.last_char = input[..emitted_end].chars().next_back().or(self.last_char);

        out
    }
}

/// Restores placeholders in SSE payloads with per-choice and per-tool isolation.
pub struct PiiRestorationStage {
    content_template: StreamingWindowReplacer,
    tool_template: StreamingWindowReplacer,
    content_replacers: HashMap<usize, StreamingWindowReplacer>,
    reasoning_replacers: HashMap<usize, StreamingWindowReplacer>,
    text_replacers: HashMap<usize, StreamingWindowReplacer>,
    tool_replacers: HashMap<(usize, usize), StreamingWindowReplacer>,
    raw_replacer: StreamingWindowReplacer,
    raw_stream: bool,
    buffered_json: String,
}

impl PiiRestorationStage {
    pub fn new(session: &super::session::PiiSession) -> Self {
        let mapping = if session.reversible {
            session.restoration_mapping()
        } else {
            HashMap::new()
        };
        Self::from_mapping(mapping)
    }

    pub fn from_mapping(mapping: HashMap<String, String>) -> Self {
        let tool_mapping: HashMap<String, String> = mapping
            .iter()
            .map(|(k, v)| (k.clone(), json_escape_string(v)))
            .collect();

        let content_template = StreamingWindowReplacer::new(mapping);
        let raw_replacer = content_template.clone();

        Self {
            content_template,
            tool_template: StreamingWindowReplacer::new(tool_mapping),
            content_replacers: HashMap::new(),
            reasoning_replacers: HashMap::new(),
            text_replacers: HashMap::new(),
            tool_replacers: HashMap::new(),
            raw_replacer,
            raw_stream: false,
            buffered_json: String::new(),
        }
    }

    fn content_replacer(&mut self, choice_idx: usize) -> &mut StreamingWindowReplacer {
        self.content_replacers
            .entry(choice_idx)
            .or_insert_with(|| self.content_template.clone())
    }

    fn reasoning_replacer(&mut self, choice_idx: usize) -> &mut StreamingWindowReplacer {
        self.reasoning_replacers
            .entry(choice_idx)
            .or_insert_with(|| self.content_template.clone())
    }

    fn text_replacer(&mut self, choice_idx: usize) -> &mut StreamingWindowReplacer {
        self.text_replacers
            .entry(choice_idx)
            .or_insert_with(|| self.content_template.clone())
    }

    fn tool_replacer(
        &mut self,
        choice_idx: usize,
        tool_idx: usize,
    ) -> &mut StreamingWindowReplacer {
        self.tool_replacers
            .entry((choice_idx, tool_idx))
            .or_insert_with(|| self.tool_template.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.content_template.is_empty()
    }

    pub fn process_frame(&mut self, frame: &str) -> String {
        if self.is_empty() || frame.is_empty() {
            return frame.to_string();
        }

        let normalized = frame.replace("\r\n", "\n");
        let has_trailing_event_sep = normalized.ends_with("\n\n");
        let parts: Vec<&str> = normalized.split("\n\n").collect();
        let mut out = String::with_capacity(frame.len() + 32);

        for part in parts {
            let trimmed = part.trim_matches('\n');
            if trimmed.is_empty() {
                continue;
            }
            let mut out_lines = Vec::new();
            for line in trimmed.lines() {
                let trimmed_line = line.trim_end_matches('\r');
                if let Some(payload) = trimmed_line
                    .strip_prefix("data:")
                    .map(|s| s.strip_prefix(' ').unwrap_or(s))
                {
                    if payload == "[DONE]" {
                        if let Some(residual_frame) = self.flush_residual_frame() {
                            if !out_lines.is_empty() {
                                out.push_str(&out_lines.join("\n"));
                                out.push_str("\n\n");
                                out_lines.clear();
                            }
                            out.push_str(&residual_frame);
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

    /// Flush pending placeholders while preserving raw or OpenAI JSON payloads.
    pub fn flush_residual_frame(&mut self) -> Option<String> {
        self.finalize().map(|json| format!("data: {json}\n\n"))
    }

    pub fn finalize(&mut self) -> Option<String> {
        if self.content_replacers.is_empty()
            && self.reasoning_replacers.is_empty()
            && self.text_replacers.is_empty()
            && self.tool_replacers.is_empty()
        {
            let raw = self.raw_replacer.flush();
            return (!raw.is_empty()).then_some(raw);
        }
        let mut choice_indices = BTreeSet::new();
        for &k in self.content_replacers.keys() {
            choice_indices.insert(k);
        }
        for &k in self.reasoning_replacers.keys() {
            choice_indices.insert(k);
        }
        for &k in self.text_replacers.keys() {
            choice_indices.insert(k);
        }
        for &(c, _) in self.tool_replacers.keys() {
            choice_indices.insert(c);
        }
        if choice_indices.is_empty() {
            choice_indices.insert(0);
        }

        let mut choices_json = Vec::new();

        for choice_idx in choice_indices {
            let mut delta = serde_json::Map::new();

            if let Some(r) = self.content_replacers.get_mut(&choice_idx) {
                let flushed = r.flush();
                if !flushed.is_empty() {
                    delta.insert("content".to_string(), serde_json::Value::String(flushed));
                }
            }

            if let Some(r) = self.reasoning_replacers.get_mut(&choice_idx) {
                let flushed = r.flush();
                if !flushed.is_empty() {
                    delta.insert(
                        "reasoning_content".to_string(),
                        serde_json::Value::String(flushed),
                    );
                }
            }

            if let Some(r) = self.text_replacers.get_mut(&choice_idx) {
                let flushed = r.flush();
                if !flushed.is_empty() {
                    delta.insert("text".to_string(), serde_json::Value::String(flushed));
                }
            }

            let mut tool_residuals = Vec::new();
            for (&(c, t), r) in &mut self.tool_replacers {
                if c == choice_idx {
                    let flushed = r.flush();
                    if !flushed.is_empty() {
                        tool_residuals.push((t, flushed));
                    }
                }
            }
            if !tool_residuals.is_empty() {
                tool_residuals.sort_by_key(|(t, _)| *t);
                let tool_calls_val: Vec<serde_json::Value> = tool_residuals
                    .into_iter()
                    .map(|(t, flushed)| {
                        serde_json::json!({
                            "index": t,
                            "function": {
                                "arguments": flushed
                            }
                        })
                    })
                    .collect();
                delta.insert(
                    "tool_calls".to_string(),
                    serde_json::Value::Array(tool_calls_val),
                );
            }

            if !delta.is_empty() {
                choices_json.push(serde_json::json!({
                    "index": choice_idx,
                    "delta": delta
                }));
            }
        }

        if choices_json.is_empty() {
            return None;
        }

        let residual_val = serde_json::json!({
            "choices": choices_json
        });
        Some(serde_json::to_string(&residual_val).unwrap_or_default())
    }
}

impl StreamingChunkStage for PiiRestorationStage {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        if self.is_empty() {
            return StreamAction::Passthrough;
        }

        if self.raw_stream {
            return StreamAction::Mutate(self.raw_replacer.process(payload));
        }
        let (to_parse, is_buffered) = if !self.buffered_json.is_empty() {
            self.buffered_json.push_str(payload);
            let combined = std::mem::take(&mut self.buffered_json);
            (combined, true)
        } else {
            (payload.to_string(), false)
        };

        match serde_json::from_str::<serde_json::Value>(&to_parse) {
            Ok(val) if !val.is_object() => {
                // Scalars such as the first `10` chunk of an IPv4 address
                // belong to raw text, not an OpenAI JSON event envelope.
                self.raw_stream = true;
                StreamAction::Mutate(self.raw_replacer.process(&to_parse))
            }
            Ok(mut val) => {
                let mut changed = false;

                if let Some(choices) = val.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for (arr_idx, choice) in choices.iter_mut().enumerate() {
                        let choice_idx = choice
                            .get("index")
                            .and_then(|i| i.as_u64())
                            .map_or(arr_idx, |i| i as usize);

                        let has_finish_reason =
                            choice.get("finish_reason").is_some_and(|f| !f.is_null());

                        if let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut())
                        {
                            // 1. content
                            if let Some(content) = delta.get_mut("content").and_then(|c| c.as_str())
                            {
                                let mut restored =
                                    self.content_replacer(choice_idx).process(content);
                                if has_finish_reason {
                                    restored.push_str(&self.content_replacer(choice_idx).flush());
                                }
                                delta.insert(
                                    "content".to_string(),
                                    serde_json::Value::String(restored),
                                );
                                changed = true;
                            } else if has_finish_reason
                                && let Some(r) = self.content_replacers.get_mut(&choice_idx)
                            {
                                let flushed = r.flush();
                                if !flushed.is_empty() {
                                    delta.insert(
                                        "content".to_string(),
                                        serde_json::Value::String(flushed),
                                    );
                                    changed = true;
                                }
                            }

                            // 2. reasoning_content
                            if let Some(reasoning) =
                                delta.get_mut("reasoning_content").and_then(|c| c.as_str())
                            {
                                let mut restored =
                                    self.reasoning_replacer(choice_idx).process(reasoning);
                                if has_finish_reason {
                                    restored.push_str(&self.reasoning_replacer(choice_idx).flush());
                                }
                                delta.insert(
                                    "reasoning_content".to_string(),
                                    serde_json::Value::String(restored),
                                );
                                changed = true;
                            } else if has_finish_reason
                                && let Some(r) = self.reasoning_replacers.get_mut(&choice_idx)
                            {
                                let flushed = r.flush();
                                if !flushed.is_empty() {
                                    delta.insert(
                                        "reasoning_content".to_string(),
                                        serde_json::Value::String(flushed),
                                    );
                                    changed = true;
                                }
                            }

                            // 3. tool_calls
                            if let Some(tool_calls) =
                                delta.get_mut("tool_calls").and_then(|tc| tc.as_array_mut())
                            {
                                for (tc_arr_idx, tc) in tool_calls.iter_mut().enumerate() {
                                    let tool_idx = tc
                                        .get("index")
                                        .and_then(|i| i.as_u64())
                                        .map_or(tc_arr_idx, |i| i as usize);

                                    if let Some(func) =
                                        tc.get_mut("function").and_then(|f| f.as_object_mut())
                                        && let Some(args) =
                                            func.get_mut("arguments").and_then(|a| a.as_str())
                                    {
                                        let mut restored =
                                            self.tool_replacer(choice_idx, tool_idx).process(args);
                                        if has_finish_reason {
                                            restored.push_str(
                                                &self.tool_replacer(choice_idx, tool_idx).flush(),
                                            );
                                        }
                                        func.insert(
                                            "arguments".to_string(),
                                            serde_json::Value::String(restored),
                                        );
                                        changed = true;
                                    }
                                }
                            } else if has_finish_reason {
                                let mut flushed_tools = Vec::new();
                                for (&(c_idx, t_idx), r) in &mut self.tool_replacers {
                                    if c_idx == choice_idx {
                                        let flushed = r.flush();
                                        if !flushed.is_empty() {
                                            flushed_tools.push((t_idx, flushed));
                                        }
                                    }
                                }
                                if !flushed_tools.is_empty() {
                                    flushed_tools.sort_by_key(|(t_idx, _)| *t_idx);
                                    let tc_val: Vec<serde_json::Value> = flushed_tools
                                        .into_iter()
                                        .map(|(t_idx, flushed)| {
                                            serde_json::json!({
                                                "index": t_idx,
                                                "function": {
                                                    "arguments": flushed
                                                }
                                            })
                                        })
                                        .collect();
                                    delta.insert(
                                        "tool_calls".to_string(),
                                        serde_json::Value::Array(tc_val),
                                    );
                                    changed = true;
                                }
                            }

                            // 4. text
                            if let Some(text) = delta.get_mut("text").and_then(|c| c.as_str()) {
                                let mut restored = self.text_replacer(choice_idx).process(text);
                                if has_finish_reason {
                                    restored.push_str(&self.text_replacer(choice_idx).flush());
                                }
                                delta.insert(
                                    "text".to_string(),
                                    serde_json::Value::String(restored),
                                );
                                changed = true;
                            } else if has_finish_reason
                                && let Some(r) = self.text_replacers.get_mut(&choice_idx)
                            {
                                let flushed = r.flush();
                                if !flushed.is_empty() {
                                    delta.insert(
                                        "text".to_string(),
                                        serde_json::Value::String(flushed),
                                    );
                                    changed = true;
                                }
                            }
                        } else if let Some(text) = choice.get_mut("text").and_then(|t| t.as_str()) {
                            let mut restored = self.text_replacer(choice_idx).process(text);
                            if has_finish_reason {
                                restored.push_str(&self.text_replacer(choice_idx).flush());
                            }
                            if let Some(obj) = choice.as_object_mut() {
                                obj.insert("text".to_string(), serde_json::Value::String(restored));
                                changed = true;
                            }
                        } else if has_finish_reason
                            && let Some(r) = self.content_replacers.get_mut(&choice_idx)
                        {
                            let flushed = r.flush();
                            if !flushed.is_empty() {
                                let mut delta = serde_json::Map::new();
                                delta.insert(
                                    "content".to_string(),
                                    serde_json::Value::String(flushed),
                                );
                                if let Some(obj) = choice.as_object_mut() {
                                    obj.insert(
                                        "delta".to_string(),
                                        serde_json::Value::Object(delta),
                                    );
                                    changed = true;
                                }
                            }
                        }
                    }
                }

                if changed {
                    StreamAction::Mutate(serde_json::to_string(&val).unwrap_or(to_parse))
                } else if is_buffered {
                    StreamAction::Mutate(to_parse)
                } else {
                    StreamAction::Passthrough
                }
            }
            Err(_) => {
                // A JSON-looking payload that failed to parse may be fragmented.
                if to_parse.trim_start().starts_with('{') {
                    self.buffered_json = to_parse;
                    StreamAction::Skip
                } else {
                    // Once identified as raw, even fragments that happen to
                    // parse as JSON numbers are text in this same stream.
                    self.raw_stream = true;
                    let restored = self.raw_replacer.process(&to_parse);
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
