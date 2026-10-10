//! Request-scoped session holding bidirectional PII mappings.

use super::replacer::StreamingWindowReplacer;
use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIResponse;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct PiiSession {
    /// original -> placeholder (e.g. "alice@example.com" -> "<EMAIL_1>")
    pub forward: HashMap<String, String>,
    /// placeholder -> original (e.g. "<EMAIL_1>" -> "alice@example.com")
    pub reverse: HashMap<String, String>,
    pub counts: HashMap<PiiEntity, usize>,
    pub reversible: bool,
    placeholder_entities: HashMap<String, PiiEntity>,
    ambiguous_aliases: HashSet<String>,
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
            placeholder_entities: HashMap::new(),
            ambiguous_aliases: HashSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

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

    /// Raises the sequential counters above any synthetic placeholder already
    /// in the input text (`<EMAIL_1>`, `<KEY_3>`, `Alex Vance`,
    /// `10.240.0.1`). Resuming at 1 would map two different originals onto the
    /// same placeholder and corrupt the reverse map.
    pub fn seed_existing_placeholders(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        // Bracketed format: <ENTITY_N>
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

        // Person placeholders: (P\d+) or [P\d+]
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

        // Email placeholders: @(outlook|fastmail).com
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

        // IP placeholders: 10.240.x.y
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
                    let n = x.saturating_mul(254).saturating_add(y);
                    let c = self.counts.entry(PiiEntity::Ip).or_insert(0);
                    *c = (*c).max(n);
                }
            }
            curr = next_char_boundary(text, start);
        }

        // Secret placeholders: sec_...{c} or sk-...{c}
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

        // Numeric secret placeholders: 89410294...
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
                if let Some((first_ph, last_ph)) = placeholder.split_once(' ') {
                    let orig_first = original.split_whitespace().next().unwrap_or(original);
                    let orig_last = original.split_whitespace().last().unwrap_or(original);
                    self.register_person_alias(first_ph, orig_first);
                    self.register_person_alias(last_ph, orig_last);
                }
            }
            return placeholder;
        }

        let count = self.counts.entry(entity).or_insert(0);
        *count = count.saturating_add(1);
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
                    // A 20-digit u64 must remain representable as a JSON
                    // number after pseudonymization (89... would overflow).
                    let prefix = if len == 20 && original.parse::<u64>().is_ok() {
                        "10"
                    } else {
                        "89410294"
                    };
                    let (prefix, width) = if len > prefix.len() {
                        (prefix, len - prefix.len())
                    } else {
                        ("9", len - 1)
                    };
                    let decimal = h.to_string();
                    let suffix = &decimal[decimal.len().saturating_sub(width)..];
                    format!("{prefix}{suffix:0>width$}")
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
                let gen_name = |idx: usize| -> String {
                    let zero_based = idx - 1;
                    let first_idx = zero_based % FIRST_NAMES.len();
                    let cycle = zero_based / FIRST_NAMES.len();
                    let last_idx = (first_idx + cycle) % LAST_NAMES.len();
                    let first = FIRST_NAMES[first_idx];
                    let last = LAST_NAMES[last_idx];
                    format!("{first} {last}")
                };
                let mut current_c = c;
                let mut name = gen_name(current_c);
                while name.eq_ignore_ascii_case(original) {
                    *count += 1;
                    current_c = *count;
                    name = gen_name(current_c);
                }
                name
            }
        };

        let mut placeholder = placeholder;
        if placeholder.bytes().all(|b| b.is_ascii_digit()) {
            // Hash suffixes have a small domain for short numeric IDs. Resolve
            // collisions within the numeric domain, never append `_1` to JSON
            // number tokens or reveal an original by returning it unchanged.
            while placeholder == original || self.reverse.contains_key(&placeholder) {
                let mut digits = placeholder.into_bytes();
                for digit in digits.iter_mut().rev() {
                    if *digit == b'9' {
                        *digit = b'0';
                    } else {
                        *digit += 1;
                        break;
                    }
                }
                placeholder = digits.into_iter().map(char::from).collect();
                if placeholder.starts_with('0') {
                    placeholder.replace_range(..1, "9");
                }
            }
        } else if placeholder == original
            || self
                .reverse
                .get(&placeholder)
                .is_some_and(|existing| existing != original)
        {
            let base = placeholder.clone();
            let mut tie_breaker = 1;
            while placeholder == original || self.reverse.contains_key(&placeholder) {
                placeholder = format!("{base}_{tie_breaker}");
                tie_breaker += 1;
            }
        }

        self.forward
            .insert(original.to_string(), placeholder.clone());
        self.reverse
            .insert(placeholder.clone(), original.to_string());
        self.placeholder_entities
            .insert(placeholder.clone(), entity);

        if entity == PiiEntity::Person
            && let Some((first_ph, last_ph)) = placeholder.split_once(' ')
        {
            let orig_first = original.split_whitespace().next().unwrap_or(original);
            let orig_last = original.split_whitespace().last().unwrap_or(original);

            let first_ph = first_ph.to_string();
            let last_ph = last_ph.to_string();
            self.register_person_alias(&first_ph, orig_first);
            self.register_person_alias(&last_ph, orig_last);
        }

        placeholder
    }

    fn register_person_alias(&mut self, alias: &str, original: &str) {
        if self.ambiguous_aliases.contains(alias) {
            return;
        }
        if self
            .reverse
            .get(alias)
            .is_some_and(|existing| existing != original)
        {
            let case_upgrade = self
                .reverse
                .get(alias)
                .is_some_and(|existing| existing.eq_ignore_ascii_case(original));
            if case_upgrade {
                self.reverse.insert(alias.to_string(), original.to_string());
            } else {
                self.reverse.remove(alias);
                self.ambiguous_aliases.insert(alias.to_string());
            }
        } else {
            self.reverse.insert(alias.to_string(), original.to_string());
        }
    }

    /// Only add lossless, unambiguous formatting aliases. Exact mappings win;
    /// fuzzy or partial secrets must never guess which original to restore.
    pub fn restoration_mapping(&self) -> HashMap<String, String> {
        if !self.reversible {
            return HashMap::new();
        }
        let mut aliases = HashMap::<String, String>::new();
        let mut ambiguous = HashSet::new();
        let mut add = |alias: String, original: &String| {
            if self.reverse.contains_key(&alias) || ambiguous.contains(&alias) {
                return;
            }
            if aliases
                .get(&alias)
                .is_some_and(|existing| existing != original)
            {
                aliases.remove(&alias);
                ambiguous.insert(alias);
            } else {
                aliases.insert(alias, original.clone());
            }
        };
        for (placeholder, original) in &self.reverse {
            match self.placeholder_entities.get(placeholder) {
                Some(PiiEntity::Phone) => {
                    let digits: String = placeholder.chars().filter(char::is_ascii_digit).collect();
                    add(format!("+{digits}"), original);
                    add(digits, original);
                    add(placeholder.replace('-', " "), original);
                }
                Some(PiiEntity::Secret) => {
                    // Attached CLI password flags have no boundary between
                    // `-p` and the value. Match the whole bounded flag instead
                    // of allowing secret replacement inside arbitrary words.
                    add(format!("-p{placeholder}"), &format!("-p{original}"));
                }
                Some(PiiEntity::CreditCard) => {
                    add(placeholder.replace([' ', '-'], ""), original);
                    add(placeholder.replace(' ', "-"), original);
                }
                Some(PiiEntity::Ip) => {
                    if let Ok(ip) = placeholder.parse::<std::net::Ipv6Addr>() {
                        add(ip.to_string(), original);
                        let segments = ip.segments();
                        add(
                            segments
                                .iter()
                                .map(|s| format!("{s:04x}"))
                                .collect::<Vec<_>>()
                                .join(":"),
                            original,
                        );
                        add(
                            segments
                                .iter()
                                .map(|s| format!("{s:x}"))
                                .collect::<Vec<_>>()
                                .join(":"),
                            original,
                        );
                    }
                }
                _ => {}
            }
            if placeholder.starts_with('<') && placeholder.ends_with('>') {
                add(
                    placeholder.replace('<', "&lt;").replace('>', "&gt;"),
                    original,
                );
            }
        }
        aliases.extend(self.reverse.clone());
        aliases
    }

    /// Use the same boundary-aware matcher for unary and streaming output.
    pub fn restore_text(&self, text: &str) -> String {
        if !self.reversible || self.reverse.is_empty() || text.is_empty() {
            return text.to_string();
        }

        let mut replacer = StreamingWindowReplacer::new(self.restoration_mapping());
        let mut restored = replacer.process(text);
        restored.push_str(&replacer.flush());
        restored
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
                    if let Some(arguments) =
                        tc.get_mut("function").and_then(|f| f.get_mut("arguments"))
                    {
                        self.restore_json_value(arguments);
                    }
                }
            }
            for (_k, v) in &mut choice.message.extra {
                self.restore_json_value(v);
            }
        }
    }

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
            serde_json::Value::Number(number)
                // IDs/cards can be JSON numbers, not just strings. Never use
                // substring replacement or convert them into quoted strings.
                if (number.is_i64() || number.is_u64()) => {
                    let token = number.to_string();
                    if let Some(original) = self.restoration_mapping().get(&token)
                        && let Ok(restored) = original.parse::<serde_json::Number>()
                        && (restored.is_i64() || restored.is_u64())
                    {
                        *number = restored;
                    }
                }
            _ => {}
        }
    }
}
