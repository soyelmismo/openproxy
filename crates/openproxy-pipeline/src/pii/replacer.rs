//! Streaming window replacer and SSE chunk restoration stage.

use crate::streaming::{StreamAction, StreamingChunkStage};
use aho_corasick::{AhoCorasick, MatchKind};
use std::collections::{BTreeSet, HashMap, HashSet};

mod contact;
use contact::{contact_kind, extends_contact_left, extends_contact_right};

#[inline]
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

/// Placeholders split across SSE chunk boundaries (chunk 1 has `alex.turn`,
/// chunk 2 has `er1@fastmail.com`) are buffered until the match is decidable,
/// so restoration stays O(N) through one Aho-Corasick automaton.
#[derive(Debug, Clone)]
pub struct StreamingWindowReplacer {
    mapping: HashMap<String, String>,
    automaton: Option<AhoCorasick>,
    patterns: Vec<String>,
    replacements: Vec<String>,
    prefixes: HashSet<String>,
    pending: String,
    max_len: usize,
    last_char: Option<char>,
}

impl StreamingWindowReplacer {
    pub fn new(mapping: HashMap<String, String>) -> Self {
        if mapping.is_empty() {
            return Self {
                mapping,
                automaton: None,
                patterns: Vec::new(),
                replacements: Vec::new(),
                prefixes: HashSet::new(),
                pending: String::new(),
                max_len: 0,
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
        let patterns: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
        let replacements: Vec<String> = pairs.iter().map(|(_, v)| v.clone()).collect();

        let mut prefixes = HashSet::new();
        for pat in &patterns {
            for (byte_idx, _) in pat.char_indices() {
                if byte_idx > 0 {
                    prefixes.insert(pat[..byte_idx].to_ascii_lowercase());
                }
            }
        }

        let automaton = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .ok();

        let mapping = pairs.into_iter().collect();

        Self {
            mapping,
            automaton,
            patterns,
            replacements,
            prefixes,
            pending: String::new(),
            max_len,
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
        self.mapping.is_empty()
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

        let len = input.len();
        let undecidable_start = if is_eof {
            None
        } else {
            let mut start = len.saturating_sub(self.max_len.saturating_sub(1));
            while !input.is_char_boundary(start) {
                start += 1;
            }
            input[start..].char_indices().find_map(|(offset, _)| {
                let pos = start + offset;
                self.prefixes
                    .contains(&input[pos..].to_ascii_lowercase())
                    .then_some(pos)
            })
        };
        let mut out = String::with_capacity(len + 32);
        let mut last_end = 0;

        for mat in ac.find_iter(&input) {
            // A complete short alias can still be the beginning of a longer
            // placeholder: `Alex ` + `Vance`. Do not emit the first-name alias
            // before the full-name match can be decided on the next chunk.
            if undecidable_start.is_some_and(|start| mat.start() >= start) {
                break;
            }
            if mat.start() < last_end {
                continue;
            }

            let pat_idx = mat.pattern().as_usize();
            let pat = &self.patterns[pat_idx];

            let mat_start = mat.start();
            let mat_end = mat.end();

            // 1. Check LEFT boundary
            let prev_char = if mat_start > 0 {
                input[..mat_start].chars().next_back()
            } else {
                self.last_char
            };

            let pat_starts_token =
                pat.starts_with(is_token_char) || pat.starts_with('+') || pat.starts_with("-p");
            if pat_starts_token && prev_char.is_some_and(is_token_char) {
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
                    let is_prefix = self.prefixes.contains(&candidate.to_ascii_lowercase());
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
                if pat_ends_token && next_char.is_some_and(is_token_char) {
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
            out.push_str(&self.replacements[pat_idx]);
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

        if is_eof || self.max_len == 0 {
            out.push_str(trailing);
            if let Some(c) = out.chars().next_back() {
                self.last_char = Some(c);
            }
            return out;
        }

        // Avoid O(N^2): limit scan to the last max_len bytes
        let scan_start = if trailing.len() >= self.max_len {
            let mut idx = trailing.len() - (self.max_len - 1);
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
            if suffix.len() < self.max_len && self.prefixes.contains(&suffix.to_ascii_lowercase()) {
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

        Self {
            content_template: StreamingWindowReplacer::new(mapping.clone()),
            tool_template: StreamingWindowReplacer::new(tool_mapping),
            content_replacers: HashMap::new(),
            reasoning_replacers: HashMap::new(),
            text_replacers: HashMap::new(),
            tool_replacers: HashMap::new(),
            raw_replacer: StreamingWindowReplacer::new(mapping),
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
