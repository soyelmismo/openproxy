//! Request-scoped session holding bidirectional PII mappings.

use aho_corasick::{AhoCorasick, MatchKind};
use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIResponse;
use std::collections::HashMap;

/// Request-scoped session holding bidirectional PII mappings.
#[derive(Debug, Clone, Default)]
pub struct PiiSession {
    /// original -> placeholder (e.g. "alice@example.com" -> "<EMAIL_1>")
    pub forward: HashMap<String, String>,
    /// placeholder -> original (e.g. "<EMAIL_1>" -> "alice@example.com")
    pub reverse: HashMap<String, String>,
    /// Counters per entity type to generate sequential IDs (1, 2, ...)
    pub counts: HashMap<PiiEntity, usize>,
    /// Whether reversibility is enabled (restore placeholders on responses)
    pub reversible: bool,
}

#[inline]
fn next_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx.min(s.len())
}

#[inline]
pub fn fnv1a_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

impl PiiSession {
    pub fn new(reversible: bool) -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
            counts: HashMap::new(),
            reversible,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    /// Return a human-readable summary of redacted entities (e.g. "email: 1, secret: 2")
    /// or None if no entities were redacted.
    pub fn summary(&self) -> Option<String> {
        if self.counts.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        for entity in PiiEntity::ALL {
            if let Some(&count) = self.counts.get(&entity).filter(|&&c| c > 0) {
                parts.push(format!("{}: {count}", entity.as_str()));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }

    /// Seed sequential counters above any pre-existing synthetic placeholders
    /// (e.g. `<EMAIL_1>`, `<KEY_3>`, `Alex Vance (P1)`, `10.240.0.1`) present in the input text to prevent collision bugs.
    pub fn seed_existing_placeholders(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        // 1. Bracketed format: <ENTITY_N>
        if text.contains('<') && text.contains('>') {
            for entity in PiiEntity::ALL {
                let prefix = format!("<{}_", entity.placeholder_prefix());
                let mut curr = 0;
                while let Some(rel) = text[curr..].find(&prefix) {
                    let start_idx = curr + rel + prefix.len();
                    if let Some(rel_gt) = text[start_idx..].find('>') {
                        let num_str = &text[start_idx..start_idx + rel_gt];
                        if let Ok(n) = num_str.parse::<usize>() {
                            let c = self.counts.entry(entity).or_insert(0);
                            *c = (*c).max(n);
                        }
                        curr = next_char_boundary(text, start_idx + rel_gt + 1);
                    } else {
                        break;
                    }
                }
            }
        }

        // 2. Realistic Person placeholders: (P\d+) or [P\d+]
        for (open, close) in [(" (P", ')'), (" [P", ']')] {
            let mut curr = 0;
            while let Some(rel) = text[curr..].find(open) {
                let start_idx = curr + rel + open.len();
                if let Some(rel_close) = text[start_idx..].find(close) {
                    let num_str = &text[start_idx..start_idx + rel_close];
                    if let Ok(n) = num_str.parse::<usize>() {
                        let c = self.counts.entry(PiiEntity::Person).or_insert(0);
                        *c = (*c).max(n);
                    }
                    curr = next_char_boundary(text, start_idx + rel_close + 1);
                } else {
                    break;
                }
            }
        }

        // 3. Realistic Email placeholders: @(outlook|fastmail).com
        for domain in ["@outlook.com", "@fastmail.com"] {
            let mut curr = 0;
            while let Some(rel) = text[curr..].find(domain) {
                let end_name = curr + rel;
                let prefix_str = &text[..end_name];
                let digit_start = prefix_str
                    .char_indices()
                    .rfind(|&(_, c)| !c.is_ascii_digit())
                    .map_or(0, |(idx, c)| idx + c.len_utf8());
                if digit_start < end_name
                    && let Ok(n) = prefix_str[digit_start..end_name].parse::<usize>()
                {
                    let c = self.counts.entry(PiiEntity::Email).or_insert(0);
                    *c = (*c).max(n);
                }
                curr = next_char_boundary(text, end_name + domain.len());
            }
        }

        // 4. Realistic IP placeholders: 10.240.x.y
        let mut curr = 0;
        const IP_PREFIX: &str = "10.240.";
        while let Some(rel) = text[curr..].find(IP_PREFIX) {
            let start = curr + rel + IP_PREFIX.len();
            if let Some((first, rest)) = text[start..].split_once('.') {
                let second = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or("");
                if let (Ok(x), Ok(y)) = (first.parse::<usize>(), second.parse::<usize>()) {
                    let n = x * 254 + y;
                    let c = self.counts.entry(PiiEntity::Ip).or_insert(0);
                    *c = (*c).max(n);
                }
            }
            curr = next_char_boundary(text, start);
        }

        // 5. Realistic Secret placeholders: sec_...{c} or sk-...{c}
        for sec_prefix in ["sec_", "sk-proj-", "sk-", "op_live_", "op_test_"] {
            let mut curr = 0;
            while let Some(rel) = text[curr..].find(sec_prefix) {
                let start = curr + rel + sec_prefix.len();
                let token = text[start..]
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if token.len() >= 2 {
                    let suffix = &token[token.len() - 2..];
                    if let Ok(n) = suffix.parse::<usize>() {
                        let c = self.counts.entry(PiiEntity::Secret).or_insert(0);
                        *c = (*c).max(n);
                    }
                }
                curr = next_char_boundary(text, start + token.len());
            }
        }

        // 6. Numeric Secret placeholders: 89410294...
        let mut curr = 0;
        const NUM_SECRET_PREFIX: &str = "89410294";
        while let Some(rel) = text[curr..].find(NUM_SECRET_PREFIX) {
            let start = curr + rel + NUM_SECRET_PREFIX.len();
            let digits = text[start..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("");
            if !digits.is_empty()
                && let Ok(n) = digits.parse::<usize>()
            {
                let c = self.counts.entry(PiiEntity::Secret).or_insert(0);
                *c = (*c).max(n);
            }
            curr = next_char_boundary(text, start + digits.len());
        }
    }
}

fn compute_luhn_check_digit(digits: &str) -> u32 {
    let mut sum = 0;
    let mut double = true;
    for ch in digits.chars().rev() {
        let mut d = ch.to_digit(10).unwrap_or(0);
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    (10 - (sum % 10)) % 10
}

fn generate_fake_credit_card(count: usize, spaced: bool) -> String {
    let base = if count <= 900 {
        format!("453201511283{:03}", (count % 900) + 100)
    } else {
        let offset = count - 900;
        format!("4532015{:08}", (offset % 90_000_000) + 1000)
    };
    let check = compute_luhn_check_digit(&base);
    let full = format!("{base}{check}");
    if spaced {
        format!(
            "{} {} {} {}",
            &full[0..4],
            &full[4..8],
            &full[8..12],
            &full[12..16]
        )
    } else {
        full
    }
}

impl PiiSession {
    /// Retrieve existing placeholder or generate a realistic, natural synthetic placeholder for this request.
    pub fn get_or_create_placeholder(&mut self, entity: PiiEntity, original: &str) -> String {
        if let Some(existing) = self.forward.get(original) {
            return existing.clone();
        }

        if entity == PiiEntity::Person
            && let Some((existing_key, existing_placeholder)) = self
                .forward
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(original))
        {
            let placeholder = existing_placeholder.clone();
            let existing_key = existing_key.clone();
            self.forward
                .insert(original.to_string(), placeholder.clone());
            if original.chars().any(|c| c.is_uppercase())
                && !existing_key.chars().any(|c| c.is_uppercase())
            {
                self.reverse
                    .insert(placeholder.clone(), original.to_string());
            }
            return placeholder;
        }

        let count = self.counts.entry(entity).or_insert(0);
        *count += 1;
        let c = *count;

        let placeholder = match entity {
            PiiEntity::Ip => {
                if original.contains(':') {
                    format!("fd00:10:240::{c}")
                } else {
                    let zero_based = c - 1;
                    format!(
                        "10.240.{}.{}",
                        (zero_based / 254) % 254,
                        (zero_based % 254) + 1
                    )
                }
            }
            PiiEntity::Email => {
                const FIRST_NAMES: &[&str] = &[
                    "alex.turner",
                    "jordan.lee",
                    "morgan.reed",
                    "sam.taylor",
                    "chris.evans",
                    "pat.parker",
                    "casey.miller",
                    "riley.cooper",
                ];
                let name = FIRST_NAMES[(c - 1) % FIRST_NAMES.len()];
                let domain = if c.is_multiple_of(2) {
                    "outlook.com"
                } else {
                    "fastmail.com"
                };
                format!("{name}{c}@{domain}")
            }
            PiiEntity::Secret => {
                let h = fnv1a_hash(original);
                let h32 = (h & 0xFFFF_FFFF) as u32;

                if original.chars().all(|ch| ch.is_ascii_digit()) && original.len() >= 6 {
                    let len = original.len();
                    let prefix = "89410294";
                    if len > prefix.len() {
                        let width = len - prefix.len();
                        let max_v = 10_usize.pow(width as u32);
                        format!("{prefix}{:0width$}", (h as usize) % max_v, width = width)
                    } else {
                        let width = len - 1;
                        let max_v = 10_usize.pow(width as u32);
                        format!("9{:0width$}", (h as usize) % max_v, width = width)
                    }
                } else {
                    const SECRET_PREFIX_MAP: &[(&str, &str)] = &[
                        ("sk-proj-", "sk-proj-"),
                        ("sk-", "sk-"),
                        ("op_live_", "op_live_"),
                        ("op_test_", "op_test_"),
                        ("mcp_live_", "mcp_live_"),
                        ("mcp_test_", "mcp_test_"),
                        ("ghp_", "ghp_"),
                        ("gho_", "gho_"),
                        ("xoxb-", "xoxb_"),
                        ("Bearer ", "Bearer sec_"),
                    ];
                    let prefix = SECRET_PREFIX_MAP
                        .iter()
                        .find_map(|(pattern, out_prefix)| {
                            original.starts_with(pattern).then_some(*out_prefix)
                        })
                        .unwrap_or("sec_");
                    format!("{prefix}{h32:08x}")
                }
            }
            PiiEntity::Phone => {
                if c <= 89 {
                    format!("+1-202-555-01{:02}", (c % 90) + 10)
                } else {
                    const AREA_CODES: &[u16] = &[212, 312, 415, 617, 206, 512, 305, 404, 702, 214];
                    let offset = c - 90;
                    let area = AREA_CODES[(offset / 9000) % AREA_CODES.len()];
                    let line = (offset % 9000) + 1000;
                    format!("+1-{area}-555-{line:04}")
                }
            }
            PiiEntity::CreditCard => {
                generate_fake_credit_card(c, original.contains(' ') || original.contains('-'))
            }
            PiiEntity::Person => {
                const FIRST_NAMES: &[&str] = &[
                    "Alex", "David", "Sarah", "Michael", "Elena", "James", "Marcus", "Laura",
                    "Carlos", "Emily", "Robert", "Anna", "Daniel", "Rachel", "Thomas", "Sofia",
                    "Lucas", "Maria", "Brian", "Jessica",
                ];
                const LAST_NAMES: &[&str] = &[
                    "Vance",
                    "Chen",
                    "Jenkins",
                    "Sterling",
                    "Mercer",
                    "Wilson",
                    "Reed",
                    "Bennett",
                    "Lindqvist",
                    "Hawthorne",
                    "Taylor",
                    "Kowalski",
                    "Anderson",
                    "Martinez",
                    "Wright",
                    "Scott",
                    "Torres",
                    "Nguyen",
                    "Murphy",
                    "Rivera",
                ];
                let zero_based = c - 1;
                let first_idx = zero_based % FIRST_NAMES.len();
                let cycle = zero_based / FIRST_NAMES.len();
                let last_idx = (first_idx + cycle) % LAST_NAMES.len();
                let first = FIRST_NAMES[first_idx];
                let last = LAST_NAMES[last_idx];
                format!("{first} {last} (P{c})")
            }
        };

        let mut placeholder = placeholder;
        if let Some(existing_original) = self.reverse.get(&placeholder)
            && existing_original != original
        {
            let mut tie_breaker = 1;
            while self.reverse.contains_key(&placeholder) {
                placeholder = format!("{placeholder}_{tie_breaker}");
                tie_breaker += 1;
            }
        }

        self.forward
            .insert(original.to_string(), placeholder.clone());
        self.reverse
            .insert(placeholder.clone(), original.to_string());
        placeholder
    }

    /// Restore all placeholders in text back to original values if reversible is true.
    /// Uses Aho-Corasick automaton with LeftmostLongest match kind to guarantee
    /// strict O(N) multi-pattern linear replacement without Tokio thread lockups.
    pub fn restore_text(&self, text: &str) -> String {
        if !self.reversible || self.reverse.is_empty() || text.is_empty() {
            return text.to_string();
        }

        let mut pairs: Vec<(&String, &String)> = self.reverse.iter().collect();
        pairs.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

        let patterns: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        let replacements: Vec<&str> = pairs.iter().map(|(_, v)| v.as_str()).collect();

        match AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
        {
            Ok(ac) => ac.replace_all(text, &replacements),
            Err(_) => text.to_string(),
        }
    }

    /// Restore placeholders in an OpenAIResponse in-place.
    pub fn restore_openai_response(&self, resp: &mut OpenAIResponse) {
        if !self.reversible || self.reverse.is_empty() {
            return;
        }

        for choice in &mut resp.choices {
            if let Some(content) = choice.message.content.as_mut() {
                self.restore_json_value(content);
            }
            if let Some(tool_calls) = choice.message.tool_calls.as_mut() {
                for tc in tool_calls {
                    self.restore_json_value(tc);
                }
            }
            for (_k, v) in &mut choice.message.extra {
                self.restore_json_value(v);
            }
        }
    }

    /// Recursively restore strings inside a serde_json::Value.
    pub fn restore_json_value(&self, val: &mut serde_json::Value) {
        if !self.reversible || self.reverse.is_empty() {
            return;
        }

        match val {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    self.restore_json_value(&mut parsed);
                    if let Ok(serialized) = serde_json::to_string(&parsed) {
                        *s = serialized;
                        return;
                    }
                }
                let restored = self.restore_text(s);
                *s = restored;
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    self.restore_json_value(item);
                }
            }
            serde_json::Value::Object(map) => {
                for (_k, v) in map.iter_mut() {
                    self.restore_json_value(v);
                }
            }
            _ => {}
        }
    }
}
