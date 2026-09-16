use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HordeLora {
    pub name: String,
    pub model: f32,
    pub clip: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject_trigger: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HordeTi {
    pub name: String,
    pub strength: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedPromptDirectives {
    pub clean_prompt: String,
    pub negative_prompts: Vec<String>,
    pub loras: Vec<HordeLora>,
    pub tis: Vec<HordeTi>,
    pub sampler_name: Option<String>,
    pub steps: Option<u32>,
    pub cfg_scale: Option<f32>,
    pub clip_skip: Option<u32>,
    pub hires_fix: Option<bool>,
    pub hires_fix_denoising_strength: Option<f32>,
    pub seed: Option<u64>,
    pub control_type: Option<String>,
    pub post_processing: Vec<String>,
    pub slow_workers: Option<bool>,
    pub trusted_workers: Option<bool>,
    pub workers: Vec<String>,
}

pub const MIN_HORDE_DIMENSION: u32 = 64;
pub const MAX_HORDE_DIMENSION: u32 = 3072;
pub const DEFAULT_HORDE_DIMENSION: u32 = 1024;

/// Normalize a dimension to the nearest multiple of 64 within AI Horde bounds.
pub fn normalize_dimension_64(val: u32) -> u32 {
    let clamped = val.clamp(MIN_HORDE_DIMENSION, MAX_HORDE_DIMENSION);
    let rem = clamped % 64;
    let rounded = if rem >= 32 {
        clamped.saturating_add(64 - rem)
    } else {
        clamped.saturating_sub(rem)
    };
    rounded.clamp(MIN_HORDE_DIMENSION, MAX_HORDE_DIMENSION)
}

/// Aspect ratio to pixel dimensions lookup.
pub fn aspect_ratio_to_dimensions(ar: &str) -> (u32, u32) {
    match ar {
        "16:9" => (1024, 576),
        "9:16" => (576, 1024),
        "3:2" => (960, 640),
        "2:3" => (640, 960),
        "4:3" => (1024, 768),
        "3:4" => (768, 1024),
        "21:9" => (1344, 576),
        "9:21" => (576, 1344),
        "1:1" => (1024, 1024),
        _ => (DEFAULT_HORDE_DIMENSION, DEFAULT_HORDE_DIMENSION),
    }
}

/// Parse dimensions from size string (e.g. "1024x1024") or aspect ratio (e.g. "16:9"),
/// guaranteeing both width and height are strict multiples of 64.
pub fn parse_dimensions(size: Option<&str>, aspect_ratio: Option<&str>) -> (u32, u32) {
    if let Some(size) = size
        && let Some((w_str, h_str)) = size.split_once('x')
        && let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>())
    {
        return (normalize_dimension_64(w), normalize_dimension_64(h));
    }

    if let Some(ar) = aspect_ratio {
        let (w, h) = aspect_ratio_to_dimensions(ar);
        (normalize_dimension_64(w), normalize_dimension_64(h))
    } else {
        (DEFAULT_HORDE_DIMENSION, DEFAULT_HORDE_DIMENSION)
    }
}

pub(crate) fn parse_lora_tag(tag_trimmed: &str) -> Option<HordeLora> {
    if !tag_trimmed
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("lora:"))
    {
        return None;
    }
    if !tag_trimmed.is_char_boundary(5) {
        return None;
    }
    let lora_body = &tag_trimmed[5..];
    let parts: Vec<&str> = lora_body.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let (model, clip) = match parts.len() {
        1 => (1.0, 1.0),
        2 => {
            let w = parts[1].trim().parse::<f32>().unwrap_or(1.0);
            (w, w)
        }
        _ => {
            let m = parts[1].trim().parse::<f32>().unwrap_or(1.0);
            let c = parts[2].trim().parse::<f32>().unwrap_or(m);
            (m, c)
        }
    };
    Some(HordeLora {
        name,
        model,
        clip,
        inject_trigger: None,
    })
}

pub(crate) fn parse_ti_tag(tag_trimmed: &str) -> Option<HordeTi> {
    let is_ti = tag_trimmed
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("ti:"));
    let is_emb = tag_trimmed
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("emb:"));
    if !is_ti && !is_emb {
        return None;
    }
    let ti_body = if is_ti {
        if !tag_trimmed.is_char_boundary(3) {
            return None;
        }
        &tag_trimmed[3..]
    } else {
        if !tag_trimmed.is_char_boundary(4) {
            return None;
        }
        &tag_trimmed[4..]
    };
    let parts: Vec<&str> = ti_body.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let strength = if parts.len() >= 2 {
        parts[1].trim().parse::<f32>().unwrap_or(1.0)
    } else {
        1.0
    };
    Some(HordeTi { name, strength })
}

pub(crate) fn extract_lora_and_ti_tags(raw_prompt: &str) -> (String, Vec<HordeLora>, Vec<HordeTi>) {
    let mut loras = Vec::new();
    let mut tis = Vec::new();
    let mut without_tags = String::with_capacity(raw_prompt.len());

    let mut cursor = 0;
    while cursor < raw_prompt.len() && raw_prompt.is_char_boundary(cursor) {
        let Some(start_idx) = raw_prompt[cursor..].find('<') else {
            without_tags.push_str(&raw_prompt[cursor..]);
            break;
        };

        let absolute_start = cursor + start_idx;
        if !raw_prompt.is_char_boundary(absolute_start) {
            without_tags.push_str(&raw_prompt[cursor..]);
            break;
        }

        without_tags.push_str(&raw_prompt[cursor..absolute_start]);

        let Some(end_idx) = raw_prompt[absolute_start..].find('>') else {
            without_tags.push_str(&raw_prompt[absolute_start..]);
            break;
        };

        let absolute_end = absolute_start + end_idx;
        if !raw_prompt.is_char_boundary(absolute_start + 1)
            || !raw_prompt.is_char_boundary(absolute_end)
            || !raw_prompt.is_char_boundary(absolute_end + 1)
        {
            without_tags.push_str(&raw_prompt[absolute_start..]);
            break;
        }

        let tag_content = &raw_prompt[absolute_start + 1..absolute_end];
        let tag_trimmed = tag_content.trim();

        if let Some(lora) = parse_lora_tag(tag_trimmed) {
            loras.push(lora);
        } else if let Some(ti) = parse_ti_tag(tag_trimmed) {
            tis.push(ti);
        } else {
            without_tags.push_str(&raw_prompt[absolute_start..=absolute_end]);
        }

        cursor = absolute_end + 1;
    }

    (without_tags, loras, tis)
}

pub(crate) fn split_prompt_negative_suffix(without_tags: &str) -> (&str, Vec<String>) {
    let (pos_part, neg_part) = without_tags
        .split_once(" ### ")
        .or_else(|| without_tags.split_once("###"))
        .map_or((without_tags, None), |(p, n)| (p, Some(n)));

    let mut negative_prompts = Vec::new();
    if let Some(n) = neg_part {
        let cleaned = clean_residual_prompt(n);
        if !cleaned.is_empty() {
            negative_prompts.push(cleaned);
        }
    }
    (pos_part, negative_prompts)
}

fn parse_hires_flag(words: &[&str], i: &mut usize) -> bool {
    *i += 1;
    if *i < words.len() && !words[*i].starts_with("--") {
        let val = words[*i];
        if val.eq_ignore_ascii_case("false") || val == "0" || val.eq_ignore_ascii_case("off") {
            *i += 1;
            false
        } else if val.eq_ignore_ascii_case("true") || val == "1" || val.eq_ignore_ascii_case("on") {
            *i += 1;
            true
        } else {
            true
        }
    } else {
        true
    }
}

fn next_arg<'a>(words: &[&'a str], i: &mut usize) -> Option<&'a str> {
    *i += 1;
    if *i < words.len() && !words[*i].starts_with("--") {
        let arg = words[*i];
        *i += 1;
        Some(arg)
    } else {
        None
    }
}

fn parse_string_flag(words: &[&str], i: &mut usize) -> Option<String> {
    next_arg(words, i).map(|s| s.trim_matches(|c| c == '"' || c == '\'').to_string())
}

fn parse_numeric_flag<T: std::str::FromStr>(
    words: &[&str],
    i: &mut usize,
    clamp_fn: impl Fn(T) -> T,
) -> Option<T> {
    next_arg(words, i)
        .and_then(|s| s.parse::<T>().ok())
        .map(clamp_fn)
}

fn parse_post_processing_tokens(words: &[&str], i: &mut usize, post_processing: &mut Vec<String>) {
    if let Some(raw) = parse_string_flag(words, i) {
        for part in raw.split(',') {
            let p = part.trim().to_string();
            if !p.is_empty() && !post_processing.contains(&p) {
                post_processing.push(p);
            }
        }
    }
}

fn parse_worker_token(words: &[&str], i: &mut usize, workers: &mut Vec<String>) {
    if let Some(w) = parse_string_flag(words, i)
        && !workers.contains(&w)
    {
        workers.push(w);
    }
}

fn parse_negative_tokens(words: &[&str], i: &mut usize, negative_prompts: &mut Vec<String>) {
    *i += 1;
    let mut neg_tokens = Vec::new();
    while *i < words.len() && !words[*i].starts_with("--") {
        neg_tokens.push(words[*i]);
        *i += 1;
    }
    if !neg_tokens.is_empty() {
        let neg_str = neg_tokens.join(" ");
        let cleaned_neg = clean_residual_prompt(neg_str.trim_matches(|c| c == '"' || c == '\''));
        if !cleaned_neg.is_empty() {
            negative_prompts.push(cleaned_neg);
        }
    }
}

fn apply_boolean_directive(lower: &str, parsed: &mut ParsedPromptDirectives) -> bool {
    match lower {
        "--no-hires" | "--no_hires" | "--nohires" => {
            parsed.hires_fix = Some(false);
            true
        }
        "--allow_slow" => {
            parsed.slow_workers = Some(true);
            true
        }
        "--any_worker" => {
            parsed.trusted_workers = Some(false);
            true
        }
        _ => false,
    }
}

fn apply_numeric_directive(
    lower: &str,
    words: &[&str],
    i: &mut usize,
    parsed: &mut ParsedPromptDirectives,
) -> bool {
    match lower {
        "--steps" => {
            parsed.steps = parse_numeric_flag(words, i, |v: u32| v.clamp(10, 100));
            true
        }
        "--cfg" | "--cfg_scale" => {
            parsed.cfg_scale = parse_numeric_flag(words, i, |v: f32| v.clamp(1.0, 30.0));
            true
        }
        "--clip_skip" => {
            parsed.clip_skip = parse_numeric_flag(words, i, |v: u32| v.clamp(1, 12));
            true
        }
        "--hires_denoising" => {
            parsed.hires_fix_denoising_strength =
                parse_numeric_flag(words, i, |v: f32| v.clamp(0.0, 1.0));
            true
        }
        "--seed" => {
            parsed.seed = parse_numeric_flag(words, i, |v: u64| v);
            true
        }
        _ => false,
    }
}

fn apply_string_directive(
    lower: &str,
    words: &[&str],
    i: &mut usize,
    parsed: &mut ParsedPromptDirectives,
) -> bool {
    match lower {
        "--sampler" | "--sampler_name" => {
            parsed.sampler_name = parse_string_flag(words, i);
            true
        }
        "--control" | "--control_type" => {
            parsed.control_type = parse_string_flag(words, i);
            true
        }
        "--post" | "--upscale" | "--post_processing" => {
            parse_post_processing_tokens(words, i, &mut parsed.post_processing);
            true
        }
        "--worker" => {
            parse_worker_token(words, i, &mut parsed.workers);
            true
        }
        "--no" | "--neg" | "--negative_prompt" => {
            parse_negative_tokens(words, i, &mut parsed.negative_prompts);
            true
        }
        _ => false,
    }
}

fn apply_directive(
    lower: &str,
    words: &[&str],
    i: &mut usize,
    parsed: &mut ParsedPromptDirectives,
) -> bool {
    if apply_boolean_directive(lower, parsed) {
        *i += 1;
        return true;
    }
    if apply_numeric_directive(lower, words, i, parsed) {
        return true;
    }
    if apply_string_directive(lower, words, i, parsed) {
        return true;
    }
    if lower == "--hires" || lower == "--hires_fix" {
        parsed.hires_fix = Some(parse_hires_flag(words, i));
        return true;
    }
    false
}

/// Extract AI Horde prompt directives (<lora:...>, <ti:...>, --sampler, --steps, etc.) from a prompt.
pub fn parse_prompt_directives(raw_prompt: &str) -> ParsedPromptDirectives {
    let (without_tags, loras, tis) = extract_lora_and_ti_tags(raw_prompt);
    let (pos_part, negative_prompts) = split_prompt_negative_suffix(&without_tags);

    let words: Vec<&str> = pos_part.split_whitespace().collect();
    let mut clean_words = Vec::new();
    let mut parsed = ParsedPromptDirectives {
        negative_prompts,
        loras,
        tis,
        ..Default::default()
    };

    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let lower = word.to_ascii_lowercase();
        if !apply_directive(&lower, &words, &mut i, &mut parsed) {
            clean_words.push(word);
            i += 1;
        }
    }

    parsed.clean_prompt = clean_residual_prompt(&clean_words.join(" "));
    parsed
}

/// Clean up stray punctuation, multiple spaces, and dangling commas left after extracting directives.
pub fn clean_residual_prompt(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut pending_comma = false;
    let mut pending_space = false;

    for c in s.chars() {
        if c == ',' {
            pending_comma = true;
            pending_space = false;
        } else if c.is_whitespace() {
            pending_space = true;
        } else {
            if !result.is_empty() {
                if pending_comma {
                    result.push(',');
                    if pending_space {
                        result.push(' ');
                    }
                } else if pending_space {
                    result.push(' ');
                }
            }
            result.push(c);
            pending_comma = false;
            pending_space = false;
        }
    }

    result
}
