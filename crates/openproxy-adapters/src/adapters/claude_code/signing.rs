//! Claude Code CCH (Client Checksum Hash) signature computation and payload normalization.
//!
//! Replicates Claude Code's native CCH signing mechanism for first-party Anthropic OAuth requests.
//! Computes xxHash64 over the normalized JSON payload where the `model` value is stripped and
//! dispatch-only fields (`max_tokens`, `fallbacks`, `fallback_credit_token`) are omitted.

use openproxy_types::error::CoreError;

pub const CLAUDE_CCH_SEED: u64 = 0x4D659218E32A3268;
pub const CLAUDE_CCH_LENGTH: usize = 5;
pub const CLAUDE_CCH_ZERO: &[u8] = b"00000";
pub const DEFAULT_BILLING_HEADER_TEXT: &str =
    "x-anthropic-billing-header: cc_version=2.1.280.d7b; cc_entrypoint=cli; cch=00000;";

const PRIME64_1: u64 = 0x9E3779B185EBCA87;
const PRIME64_2: u64 = 0xC2B2AE3D27D4EB4F;
const PRIME64_3: u64 = 0x165667B19E3779F9;
const PRIME64_4: u64 = 0x85EBCA77C2B2AE63;
const PRIME64_5: u64 = 0x27D4EB2F165667C5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline]
fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val))
        .wrapping_mul(PRIME64_1)
        .wrapping_add(PRIME64_4)
}

/// Standalone xxHash64 implementation matching Cyan4973 xxHash64 and Go `pierrec/xxHash`.
pub fn xxhash64(data: &[u8], seed: u64) -> u64 {
    let len = data.len();
    let mut h64: u64;

    if len >= 32 {
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);

        let mut chunks = data.chunks_exact(32);
        for chunk in &mut chunks {
            let k1 = u64::from_le_bytes(chunk[0..8].try_into().unwrap_or_default());
            let k2 = u64::from_le_bytes(chunk[8..16].try_into().unwrap_or_default());
            let k3 = u64::from_le_bytes(chunk[16..24].try_into().unwrap_or_default());
            let k4 = u64::from_le_bytes(chunk[24..32].try_into().unwrap_or_default());

            v1 = round(v1, k1);
            v2 = round(v2, k2);
            v3 = round(v3, k3);
            v4 = round(v4, k4);
        }

        h64 = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));

        h64 = merge_round(h64, v1);
        h64 = merge_round(h64, v2);
        h64 = merge_round(h64, v3);
        h64 = merge_round(h64, v4);
    } else {
        h64 = seed.wrapping_add(PRIME64_5);
    }

    h64 = h64.wrapping_add(len as u64);

    let remainder = &data[len - (len % 32)..];
    let mut chunks8 = remainder.chunks_exact(8);
    for chunk in &mut chunks8 {
        let k1 = u64::from_le_bytes(chunk.try_into().unwrap_or_default());
        h64 ^= round(0, k1);
        h64 = h64
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
    }
    let remainder4 = chunks8.remainder();
    let mut chunks4 = remainder4.chunks_exact(4);
    for chunk in &mut chunks4 {
        let k1 = u32::from_le_bytes(chunk.try_into().unwrap_or_default()) as u64;
        h64 ^= k1.wrapping_mul(PRIME64_1);
        h64 = h64
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
    }
    for &byte in chunks4.remainder() {
        h64 ^= (byte as u64).wrapping_mul(PRIME64_5);
        h64 = h64.rotate_left(11).wrapping_mul(PRIME64_1);
    }

    h64 ^= h64 >> 33;
    h64 = h64.wrapping_mul(PRIME64_2);
    h64 ^= h64 >> 29;
    h64 = h64.wrapping_mul(PRIME64_3);
    h64 ^= h64 >> 32;

    h64
}

#[derive(Clone, Copy, Debug)]
struct NormalizationEdit {
    start: usize,
    end: usize,
}

struct JsonScanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<NormalizationEdit>,
}

#[derive(Clone, Copy, Debug)]
struct JsonMember {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

impl<'a> JsonScanner<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            edits: Vec::new(),
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => return,
            }
        }
    }

    fn consume(&mut self, c: u8) -> bool {
        if self.pos < self.body.len() && self.body[self.pos] == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn add_edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push(NormalizationEdit { start, end });
        }
    }

    fn parse_string(&mut self) -> Result<(usize, usize), CoreError> {
        if self.pos >= self.body.len() || self.body[self.pos] != b'"' {
            return Err(CoreError::Validation("missing JSON string".into()));
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(CoreError::Validation("unterminated JSON string".into()))
    }

    fn parse_value(&mut self, collect: bool) -> Result<(), CoreError> {
        self.skip_whitespace();
        if self.pos >= self.body.len() {
            return Err(CoreError::Validation("missing JSON value".into()));
        }
        match self.body[self.pos] {
            b'{' => self.parse_object(collect),
            b'[' => self.parse_array(collect),
            b'"' => {
                self.parse_string()?;
                Ok(())
            }
            _ => {
                let start = self.pos;
                while self.pos < self.body.len() {
                    match self.body[self.pos] {
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' => {
                            if self.pos == start {
                                return Err(CoreError::Validation("missing JSON value".into()));
                            }
                            return Ok(());
                        }
                        _ => self.pos += 1,
                    }
                }
                if self.pos == start {
                    return Err(CoreError::Validation("missing JSON value".into()));
                }
                Ok(())
            }
        }
    }

    fn parse_array(&mut self, collect: bool) -> Result<(), CoreError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(collect)?;
            self.skip_whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(CoreError::Validation("missing array end".into()));
            }
            return Ok(());
        }
    }

    fn parse_object(&mut self, collect: bool) -> Result<(), CoreError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }

        let mut members = Vec::new();
        let mut comma_before = None;

        loop {
            self.skip_whitespace();
            let member_start = self.pos;
            let (key_start, key_end) = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(CoreError::Validation("missing object colon".into()));
            }
            self.skip_whitespace();

            let key = &self.body[key_start..key_end];
            let excluded = collect && is_excluded_key(key);

            if collect
                && key == b"\"model\""
                && self.pos < self.body.len()
                && self.body[self.pos] == b'"'
            {
                let (val_start, val_end) = self.parse_string()?;
                self.add_edit(val_start + 1, val_end - 1);
            } else {
                self.parse_value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.skip_whitespace();

            let mut comma_after = None;
            if self.consume(b',') {
                comma_after = Some(self.pos - 1);
            }

            members.push(JsonMember {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });

            if let Some(ca) = comma_after {
                comma_before = Some(ca);
                continue;
            }
            if !self.consume(b'}') {
                return Err(CoreError::Validation("missing object end".into()));
            }
            break;
        }

        if collect {
            self.add_excluded_member_edits(&members);
        }
        Ok(())
    }

    fn add_excluded_member_edits(&mut self, members: &[JsonMember]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }

            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }

            if end + 1 < members.len() {
                if let Some(ca) = members[end].comma_after {
                    self.add_edit(members[start].start, ca + 1);
                }
            } else if start > 0 && end > start {
                self.add_edit(members[start].start, members[end].end);
            } else if start > 0 {
                if let Some(cb) = members[start].comma_before {
                    self.add_edit(cb, members[end].end);
                }
            } else {
                self.add_edit(members[start].start, members[end].end);
            }
            start = end + 1;
        }
    }
}

fn is_excluded_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
    )
}

/// Normalizes JSON body for CCH hashing: strips model string value and removes excluded keys.
pub fn normalize_claude_cch_input(body: &[u8]) -> Result<Vec<u8>, CoreError> {
    let mut scanner = JsonScanner::new(body);
    scanner.parse_value(true)?;
    scanner.skip_whitespace();
    if scanner.pos != body.len() {
        return Err(CoreError::Validation(
            "unexpected trailing JSON data".into(),
        ));
    }

    scanner.edits.sort_by_key(|e| e.start);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for edit in &scanner.edits {
        if edit.start < last || edit.end > body.len() {
            return Err(CoreError::Validation(
                "overlapping normalization edit".into(),
            ));
        }
        normalized.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

/// Finds the offset of the 5 CCH digits in an Anthropic messages body.
pub fn find_claude_cch_offset(body: &[u8]) -> Option<usize> {
    let target = b"x-anthropic-billing-header:";
    let pos = body.windows(target.len()).position(|w| w == target)?;
    let cch_prefix = b"cch=";
    let relative = body[pos..]
        .windows(cch_prefix.len())
        .position(|w| w == cch_prefix)?;
    let digits = pos + relative + cch_prefix.len();
    if digits + CLAUDE_CCH_LENGTH < body.len()
        && body[digits + CLAUDE_CCH_LENGTH] == b';'
        && body[digits..digits + CLAUDE_CCH_LENGTH]
            .iter()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        Some(digits)
    } else {
        None
    }
}

/// Computes the CCH signature for an Anthropic messages body and patches the 5 CCH digits in place.
pub fn sign_anthropic_messages_body(body: &mut [u8]) -> Result<String, CoreError> {
    let Some(cch_offset) = find_claude_cch_offset(body) else {
        return Err(CoreError::Validation(
            "no CCH placeholder found in body".into(),
        ));
    };

    body[cch_offset..cch_offset + CLAUDE_CCH_LENGTH].copy_from_slice(CLAUDE_CCH_ZERO);
    let normalized = normalize_claude_cch_input(body)?;
    let hash = xxhash64(&normalized, CLAUDE_CCH_SEED);
    let cch = format!("{:05x}", hash & 0xFFFFF);
    body[cch_offset..cch_offset + CLAUDE_CCH_LENGTH].copy_from_slice(cch.as_bytes());
    Ok(cch)
}

/// Prepares the `system` array by ensuring the Claude billing header block is present as the first entry.
pub fn ensure_claude_billing_header(json: &mut serde_json::Value) {
    let billing_block = serde_json::json!({
        "type": "text",
        "text": DEFAULT_BILLING_HEADER_TEXT
    });

    match json.get_mut("system") {
        Some(sys @ serde_json::Value::String(_)) => {
            let original_text = sys.as_str().unwrap_or("").to_string();
            *sys = serde_json::json!([
                billing_block,
                {
                    "type": "text",
                    "text": original_text
                }
            ]);
        }
        Some(serde_json::Value::Array(arr)) => {
            if arr.is_empty() {
                arr.push(billing_block);
            } else {
                let first_is_billing = arr
                    .first()
                    .and_then(|v| v.get("text"))
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"));
                if !first_is_billing {
                    arr.insert(0, billing_block);
                } else if let Some(first_obj) = arr.first_mut().and_then(|v| v.as_object_mut())
                    && let Some(text_val) = first_obj.get_mut("text")
                    && let Some(text_str) = text_val.as_str()
                    && !text_str.contains("cch=")
                {
                    // Add cch placeholder if missing
                    let mut updated = text_str.to_string();
                    if let Some(pos) = updated.find("cc_entrypoint=")
                        && let Some(semi) = updated[pos..].find(';')
                    {
                        let insert_at = pos + semi + 1;
                        updated.insert_str(insert_at, " cch=00000;");
                        *text_val = serde_json::Value::String(updated);
                    }
                }
            }
        }
        _ => {
            if let Some(obj) = json.as_object_mut() {
                obj.insert("system".to_string(), serde_json::json!([billing_block]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xxhash64_vectors() {
        assert_eq!(xxhash64(b"", 0), 0xef46db3751d8e999);
        assert_eq!(xxhash64(b"", CLAUDE_CCH_SEED), 0xb8b30e7de65b46c5);
        assert_eq!(xxhash64(b"a", 0), 0xd24ec4f1a98c6e5b);
        assert_eq!(xxhash64(b"a", CLAUDE_CCH_SEED), 0xde28ae28b0e07bb2);
        assert_eq!(xxhash64(b"abc", 0), 0x44bc2cf5ad770999);
        assert_eq!(xxhash64(b"abc", CLAUDE_CCH_SEED), 0xdfc4f4d6913699b6);
        assert_eq!(
            xxhash64(b"12345678901234567890123456789012", 0),
            0x40fd1aa52d98274c
        );
        assert_eq!(
            xxhash64(b"12345678901234567890123456789012", CLAUDE_CCH_SEED),
            0xa3bce22fc71acdc4
        );
        assert_eq!(xxhash64(b"hello world", 0), 0x45ab6734b21e6968);
        assert_eq!(
            xxhash64(b"hello world", CLAUDE_CCH_SEED),
            0x6626964767a58cef
        );
    }

    #[test]
    fn test_sign_anthropic_messages_body_known_vectors() {
        let base = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

        let tests = [
            ("base", base.to_string(), "7ee87"),
            ("model value ignored", base.replace(r#""model":"model-a""#, r#""model":"model-b""#), "7ee87"),
            ("max tokens ignored", base.replace(r#""max_tokens":1"#, r#""max_tokens":2"#), "7ee87"),
            ("message changes hash", base.replace(r#""text":"x""#, r#""text":"y""#), "b9cc8"),
            ("system changes hash", base.replace(r#""system-x""#, r#""system-y""#), "a30d3"),
            ("metadata changes hash", base.replace(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#), "7a89d"),
            ("thinking changes hash", base.replace(r#""thinking":{"type":"adaptive","display":"omitted"}"#, r#""thinking":{"type":"disabled"}"#), "7205c"),
            ("context changes hash", base.replace(r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#, r#""context_management":{"edits":[]}"#), "05073"),
            ("effort changes hash", base.replace(r#""effort":"high""#, r#""effort":"low""#), "12366"),
            ("stream changes hash", base.replace(r#""stream":true"#, r#""stream":false"#), "60400"),
            ("tool changes hash", base.replace(r#""tools":[]"#, r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#), "3d78d"),
            ("extra field changes hash", base.replace(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#), "2d622"),
            ("field order remains significant", r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#.to_string(), "e5b6c"),
            ("nested model value ignored", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"a"}"#), "0601b"),
            ("nested max tokens member omitted", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":2}"#), "7ee87"),
            ("top level fallbacks member omitted", base.replace(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#), "7ee87"),
            ("nested fallbacks member omitted", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","fallbacks":[{"model":"nested-a"}]}"#), "7ee87"),
            ("top level fallback credit token omitted", base.replace(r#""stream":true}"#, r#""stream":true,"fallback_credit_token":"a"}"#), "7ee87"),
            ("nested fallback credit token omitted", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","fallback_credit_token":"a"}"#), "7ee87"),
            ("trailing dispatch run keeps native comma", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#), "4589b"),
            ("model before trailing dispatch run", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#), "2d312"),
            ("model splits dispatch runs", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#), "0601b"),
            ("ordinary nested member remains", base.replace(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","plain":"a"}"#), "8d74c"),
            ("billing block only", r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_string(), "f2edb"),
        ];

        for (name, body_str, want) in tests {
            let mut bytes = body_str.into_bytes();
            let got = sign_anthropic_messages_body(&mut bytes)
                .unwrap_or_else(|e| panic!("failed vector {name}: {e}"));
            assert_eq!(got, want, "vector {name} failed");
        }
    }
}
