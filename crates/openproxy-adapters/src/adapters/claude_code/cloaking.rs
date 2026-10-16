//! Claude Code request cloaking and identity spoofing.
//!
//! Emulates the native Claude Code CLI (claude-cli/2.1.293) top-level system
//! structure, dynamic build fingerprint generation, user_id metadata, model
//! fallbacks, and harness prompt relocation via `<system-reminder>`.
//!
//! Body defaults (thinking, effort, context management, cache TTL) are
//! data-driven: they are read from the captured profile at request time, so a
//! newer installed CLI can change them without a code change. Every default is
//! skipped when the caller already sent an explicit value.
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub const CLAUDE_CODE_CLI_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub const FINGERPRINT_SALT: &str = "59cf53e54c78";

pub const CLAUDE_CODE_FABLE_REPORTING_OUTCOMES: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";

/// `extended-cache-ttl-2025-04-11`: enables the 1h cache TTL. Native emits it
/// only when the resolved body carries a 1h TTL, so it is derived from the
/// finalized body rather than assumed from a header.
pub const BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";

/// `thinking-display-updates-2026-08-18` is the thinking display-update variant gate. Native emits it
/// only when the resolved thinking display is `updates`.
pub const BETA_THINKING_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";

/// `redact-thinking-2026-02-12`: native removes this whenever thinking is
/// active, and keeps it (after `interleaved-thinking`) when it is not.
pub const BETA_REDACT_THINKING: &str = "redact-thinking-2026-02-12";

/// `structured-outputs-2025-12-15`: emitted when the body carries an
/// `output_config.format` json_schema, as observed in the structured captures.
pub const BETA_STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";

pub const BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
pub const BETA_CLAUDE_CODE: &str = "claude-code-20250219";
pub const BETA_EFFORT: &str = "effort-2025-11-24";
pub const BETA_MID_CONV_SYSTEM: &str = "mid-conversation-system-2026-04-07";
pub const BETA_MID_CONV_TOOL: &str = "mid-conversation-tool-changes-2026-07-01";
pub const BETA_PER_TURN_CONTROL: &str = "per-turn-control-2026-07-01";
pub const BETA_OAUTH: &str = "oauth-2025-04-20";
pub const BETA_FALLBACKS: [&str; 2] = [
    "server-side-fallback-2026-06-01",
    "fallback-credit-2026-06-01",
];

/// Computes the 3-character build fingerprint embedded in cc_version:
/// SHA256(salt + messageText[4] + messageText[7] + messageText[20] + version)[:3]
pub fn compute_build_fingerprint(message_text: &str, version: &str) -> String {
    let utf16_units: Vec<u16> = message_text.encode_utf16().collect();
    let sample_indices = [4, 7, 20];
    let mut sampled_units = [b'0' as u16; 3];
    for (i, &idx) in sample_indices.iter().enumerate() {
        if idx < utf16_units.len() {
            sampled_units[i] = utf16_units[idx];
        }
    }
    let sampled_str = String::from_utf16_lossy(&sampled_units);
    let input = format!("{FINGERPRINT_SALT}{sampled_str}{version}");
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    format!("{:02x}{:02x}", hash[0], hash[1])[..3].to_string()
}

/// Generates the Claude Code billing header with dynamic build fingerprint and standard tags.
pub fn generate_billing_header(
    version: &str,
    message_text: &str,
    entrypoint: &str,
    prev_req: Option<&str>,
    prompt_id: Option<&str>,
) -> String {
    let build_hash = compute_build_fingerprint(message_text, version);
    let mut header = format!(
        "x-anthropic-billing-header: cc_version={version}.{build_hash}; cc_entrypoint={entrypoint}; cch=00000;"
    );
    if let Some(prev) = prev_req.filter(|s| !s.is_empty()) {
        let _ = write!(header, " cc_prev_req={prev};");
    }
    if let Some(prompt) = prompt_id.filter(|s| !s.is_empty()) {
        let _ = write!(header, " cc_prompt_id={prompt};");
    }
    header.push_str(" cc_turn_origin=human;");
    header
}

/// Extracts text from the first user message for fingerprint calculation.
pub fn extract_first_user_message_text(json: &serde_json::Value) -> String {
    let Some(messages) = json.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(content_str) = msg.get("content").and_then(|c| c.as_str()) {
                return content_str.to_string();
            }
            if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
                for part in content_arr {
                    if part.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(t) = part.get("text").and_then(|txt| txt.as_str())
                    {
                        return t.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// Collects any caller-supplied system prompt blocks from `system`.
pub fn collect_caller_system_prompts(json: &serde_json::Value) -> Vec<String> {
    let mut prompts = Vec::new();
    let Some(sys) = json.get("system") else {
        return prompts;
    };

    if let Some(s) = sys.as_str() {
        let trimmed = s.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with("x-anthropic-billing-header:")
            && trimmed != CLAUDE_CODE_CLI_IDENTITY
            && trimmed != CLAUDE_CODE_FABLE_REPORTING_OUTCOMES
        {
            prompts.push(trimmed.to_string());
        }
    } else if let Some(arr) = sys.as_array() {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|t| t.as_str())
            {
                let trimmed = text.trim();
                if !trimmed.is_empty()
                    && !trimmed.starts_with("x-anthropic-billing-header:")
                    && trimmed != CLAUDE_CODE_CLI_IDENTITY
                    && trimmed != CLAUDE_CODE_FABLE_REPORTING_OUTCOMES
                {
                    prompts.push(trimmed.to_string());
                }
            }
        }
    }

    prompts
}

/// Injects the caller system prompts inside a `<system-reminder>` block into the first user message.
pub fn inject_system_reminder(json: &mut serde_json::Value, reminder_text: &str) {
    if reminder_text.trim().is_empty() {
        return;
    }
    let formatted_reminder = format!(
        "<system-reminder>\n{}\n</system-reminder>",
        reminder_text.trim()
    );

    let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    let Some(first_user) = messages
        .iter_mut()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return;
    };
    let reminder_text_value = serde_json::Value::String(formatted_reminder);
    if let Some(content_str) = first_user.get("content").and_then(|c| c.as_str()) {
        let mut content = content_str.to_string();
        content.push_str("\n\n");
        content.push_str(reminder_text_value.as_str().unwrap_or(""));
        first_user
            .as_object_mut()
            .map(|obj| obj.insert("content".to_string(), serde_json::Value::String(content)));
        return;
    }
    if let Some(content) = first_user.get_mut("content").and_then(|c| c.as_array_mut()) {
        if let Some(first_block) = content.first_mut().and_then(|b| b.as_object_mut())
            && first_block.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = first_block.get("text").and_then(|t| t.as_str())
        {
            let text = format!("{text}\n\n{}", reminder_text_value.as_str().unwrap_or(""));
            first_block.insert("text".to_string(), serde_json::Value::String(text));
            return;
        }
        content.push(serde_json::json!({
            "type": "text",
            "text": reminder_text_value.as_str().unwrap_or("")
        }));
    }
}

fn normalize_model(model_id: &str) -> String {
    model_id.to_ascii_lowercase().replace('.', "-")
}

fn model_matches(model: &str, needle: &str) -> bool {
    model.contains(&needle.replace('.', "-"))
}

/// Mirrors the native capability predicates: claude-3-* and the early
/// claude-4-* generations never receive adaptive thinking, while claude-4-6+,
/// sonnet-5/haiku-5 and fable/opus-5 do (observed in the local CLI captures).
fn supports_adaptive_thinking(model: &str) -> bool {
    !model.starts_with("claude-3-")
        && !model_matches(model, "opus-4-0")
        && !model_matches(model, "opus-4-1")
        && !model_matches(model, "opus-4-5")
        && !model_matches(model, "sonnet-4-0")
        && !model_matches(model, "sonnet-4-5")
        && !model_matches(model, "haiku-4-5")
}

/// Native effort support follows the same early-generation exclusion list.
fn supports_effort(model: &str) -> bool {
    supports_adaptive_thinking(model)
}

/// The resolved thinking display for a body: `updates` when the caller sent
/// nothing and the model supports adaptive thinking, otherwise whatever the
/// caller explicitly asked for.
pub fn resolved_thinking_display(json: &serde_json::Value, _model_id: &str) -> Option<String> {
    let thinking = json.get("thinking")?;
    if !thinking_active(json) {
        return None;
    }
    if let Some(display) = thinking
        .get("display")
        .and_then(|d| d.as_str())
        .map(|s| s.to_ascii_lowercase())
    {
        return Some(display);
    }
    None
}

/// True when the resolved thinking display is `updates`, which is what native
/// predicates the `thinking-display-updates-2026-08-18` (display update) beta on.
pub fn thinking_display_updates_active(json: &serde_json::Value, model_id: &str) -> bool {
    matches!(
        resolved_thinking_display(json, model_id).as_deref(),
        Some("updates")
    )
}

/// True when any `cache_control` block in the body requests the 1h TTL.
pub fn extended_cache_ttl_active(json: &serde_json::Value) -> bool {
    cache_control_blocks(json).any(|block| {
        block
            .get("cache_control")
            .and_then(|cc| cc.get("ttl"))
            .and_then(|ttl| ttl.as_str())
            .is_some_and(|ttl| ttl.eq_ignore_ascii_case("1h"))
    })
}

/// True when the body declares a structured output format, which is the
/// observed trigger for the structured-outputs beta.
pub fn structured_outputs_active(json: &serde_json::Value) -> bool {
    json.get("output_config")
        .and_then(|c| c.get("format"))
        .and_then(|f| f.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t.eq_ignore_ascii_case("json_schema"))
}

/// True when the body carries an active thinking block (adaptive or enabled).
pub fn thinking_active(json: &serde_json::Value) -> bool {
    json.get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t.eq_ignore_ascii_case("adaptive") || t.eq_ignore_ascii_case("enabled"))
}

fn cache_control_blocks(json: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    let system = json
        .get("system")
        .and_then(|s| s.as_array())
        .map_or(&[][..], Vec::as_slice);
    let tools = json
        .get("tools")
        .and_then(|t| t.as_array())
        .map_or(&[][..], Vec::as_slice);
    let messages = json
        .get("messages")
        .and_then(|m| m.as_array())
        .map_or(&[][..], Vec::as_slice);

    let system_blocks = system.iter().chain(tools.iter());
    let message_blocks = messages.iter().flat_map(|message| {
        message
            .get("content")
            .and_then(|c| c.as_array())
            .map_or(&[][..], Vec::as_slice)
    });

    system_blocks.chain(message_blocks)
}

/// Applies the adaptive thinking default for models that support it. The
/// captured default is `{"type":"adaptive","display":"updates"}`; a caller's
/// explicit `disabled`/`enabled`/display value is always preserved.
fn apply_thinking_defaults(
    json: &mut serde_json::Value,
    model_id: &str,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) {
    let model = normalize_model(model_id);
    if !supports_adaptive_thinking(&model) {
        return;
    }
    // Structured-only turns (observed in the auxiliary captures) never carry
    // thinking, so injecting it there would change the payload contract.
    if structured_outputs_active(json)
        || json
            .get("tool_choice")
            .and_then(|choice| choice.get("type"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| matches!(kind, "tool" | "any"))
    {
        return;
    }
    let Some(defaults) = profile.request_defaults.get("thinking").cloned() else {
        return;
    };
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    if matches!(obj.get("thinking"), None | Some(serde_json::Value::Null)) {
        obj.insert("thinking".to_string(), defaults);
    }
}

/// Merge captured effort defaults without changing explicit values or tool choice.
fn apply_output_config_defaults(
    json: &mut serde_json::Value,
    model_id: &str,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) {
    if !supports_effort(&normalize_model(model_id)) {
        return;
    }
    let Some(defaults) = profile.request_defaults.get("output_config") else {
        return;
    };
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    let output = obj
        .entry("output_config")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(output) = output.as_object_mut()
        && !output.contains_key("effort")
        && let Some(effort) = defaults.get("effort")
    {
        output.insert("effort".into(), effort.clone());
    }
}

/// Adds the captured `context_management` edits when thinking is active and
/// the caller sent none. The shape is taken verbatim from the profile so it
/// stays exactly what the installed CLI sends.
fn apply_context_management_defaults(
    json: &mut serde_json::Value,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) {
    if !thinking_active(json) {
        return;
    }
    if json.get("context_management").is_some() {
        return;
    }
    let Some(defaults) = profile.request_defaults.get("context_management").cloned() else {
        return;
    };
    json["context_management"] = defaults;
}

/// Promotes existing `cache_control` breakpoints to the native 1h TTL. Only
/// blocks that already carry a `cache_control` are touched, and an explicit
/// `ttl` (for example a caller's `5m`) is never overwritten.
fn apply_claude_code_cache_ttl(json: &mut serde_json::Value, ttl: &str) {
    for block in cache_control_blocks_mut(json) {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        let Some(cache_control) = obj.get_mut("cache_control").and_then(|c| c.as_object_mut())
        else {
            continue;
        };
        if cache_control.contains_key("ttl") {
            continue;
        }
        cache_control.insert("ttl".to_string(), serde_json::Value::String(ttl.into()));
    }
}

fn cache_control_blocks_mut(json: &mut serde_json::Value) -> Vec<&mut serde_json::Value> {
    let mut blocks = Vec::new();
    if let Some(obj) = json.as_object_mut() {
        for (key, value) in obj.iter_mut() {
            match key.as_str() {
                "system" | "tools" => {
                    if let Some(array) = value.as_array_mut() {
                        blocks.extend(array.iter_mut());
                    }
                }
                "messages" => {
                    if let Some(messages) = value.as_array_mut() {
                        for message in messages {
                            if let Some(content) =
                                message.get_mut("content").and_then(|c| c.as_array_mut())
                            {
                                blocks.extend(content.iter_mut());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    blocks
}

/// Injects native CLI fallbacks for Opus 5.5 and Fable 5.1.
pub fn apply_model_fallbacks(json: &mut serde_json::Value, model_id: &str) {
    if json.get("fallbacks").is_some() {
        return;
    }
    let lower = model_id.to_ascii_lowercase();
    let fallback_model = if lower.contains("opus-5-5") || lower.contains("opus-5.5") {
        Some("claude-opus-4-8")
    } else if lower.contains("fable-5-1") || lower.contains("fable-5.1") {
        Some("claude-opus-5")
    } else {
        None
    };

    if let Some(fb) = fallback_model
        && let Some(obj) = json.as_object_mut()
    {
        obj.insert("fallbacks".to_string(), serde_json::json!([{"model": fb}]));
    }
}

/// Applies native Claude Code metadata.user_id format.
pub fn apply_user_id_metadata(
    json: &mut serde_json::Value,
    account_uuid: &str,
    session_id: &str,
    device_id: &str,
) {
    let user_id_obj = serde_json::json!({
        "device_id": device_id,
        "account_uuid": account_uuid,
        "session_id": session_id,
    });
    let user_id_str = user_id_obj.to_string();

    if let Some(obj) = json.as_object_mut() {
        let meta_val = obj
            .entry("metadata")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(meta_obj) = meta_val.as_object_mut() {
            meta_obj.insert(
                "user_id".to_string(),
                serde_json::Value::String(user_id_str),
            );
        }
    }
}

/// Builds the forward rename map (original -> cloaked) for a set of tool names.
///
/// A name already in TitleCase is identity and stays out of the map. Two names
/// that collide on the same cloaked form are both left unchanged: renaming them
/// would make them indistinguishable to the upstream.
pub fn build_tool_rename_map_from_names<'a, I>(
    names: I,
) -> std::collections::HashMap<String, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut candidates: Vec<(String, String)> = Vec::new();
    for name in names {
        let cloaked = title_case_tool_name(name);
        if cloaked.is_empty() || cloaked == name {
            continue;
        }
        if candidates.iter().any(|(original, _)| original == name) {
            continue;
        }
        candidates.push((name.to_string(), cloaked));
    }

    let mut map = std::collections::HashMap::with_capacity(candidates.len());
    for (original, cloaked) in &candidates {
        let collides = candidates.iter().any(|(other_original, other_cloaked)| {
            other_cloaked == cloaked && other_original != original
        });
        if !collides {
            map.insert(original.clone(), cloaked.clone());
        }
    }
    map
}

/// Assembles Anthropic-Beta headers dynamically, starting from the captured
/// profile base list and layering the model-conditional capabilities on top.
///
/// The base list comes from `profile.betas`, so a newer captured CLI that
/// reports a different common set is respected without a code change.
pub fn build_claude_code_betas(model_id: &str, has_fallbacks: bool, is_oauth: bool) -> String {
    build_claude_code_betas_from_profile(
        model_id,
        has_fallbacks,
        is_oauth,
        &crate::spoofer::claude_code_profile::current_profile(),
    )
}

/// La misma lista de betas anclada a un perfil concreto: el snapshot pinneado
/// de esta petición, de modo que los headers y el body no puedan discrepar
/// aunque el runtime global refresque en paralelo.
pub fn build_claude_code_betas_from_profile(
    model_id: &str,
    has_fallbacks: bool,
    is_oauth: bool,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) -> String {
    let model = normalize_model(model_id);

    let mut betas: Vec<String> = profile
        .betas
        .iter()
        .filter(|beta| {
            !beta.is_empty()
                && ![
                    BETA_OAUTH,
                    BETA_MID_CONV_SYSTEM,
                    BETA_MID_CONV_TOOL,
                    BETA_PER_TURN_CONTROL,
                    BETA_EFFORT,
                    BETA_FALLBACKS[0],
                    BETA_FALLBACKS[1],
                    BETA_EXTENDED_CACHE_TTL,
                    BETA_REDACT_THINKING,
                    BETA_STRUCTURED_OUTPUTS,
                ]
                .contains(&beta.as_str())
        })
        .cloned()
        .collect();

    if is_oauth && !betas.iter().any(|b| b == BETA_OAUTH) {
        let insert_at = betas
            .iter()
            .position(|b| b == BETA_CLAUDE_CODE)
            .map_or(0, |idx| idx + 1);
        betas.insert(insert_at, BETA_OAUTH.to_string());
    }

    let is_legacy = !supports_adaptive_thinking(&model) || model.contains("sonnet-4-");

    if !is_legacy && !betas.iter().any(|b| b == BETA_MID_CONV_SYSTEM) {
        betas.push(BETA_MID_CONV_SYSTEM.to_string());
    }
    if !is_legacy {
        if (model_matches(&model, "opus-5-5")
            || model_matches(&model, "fable-5-1")
            || model_matches(&model, "fable-5-2")
            || model_matches(&model, "opus-5-4"))
            && !betas.iter().any(|b| b == BETA_PER_TURN_CONTROL)
        {
            betas.push(BETA_PER_TURN_CONTROL.to_string());
        }
        if !model.contains("sonnet-5") && !betas.iter().any(|b| b == BETA_MID_CONV_TOOL) {
            betas.push(BETA_MID_CONV_TOOL.to_string());
        }
    }

    // The effort beta describes `output_config.effort`, which the same
    // model gate controls, so the two can never disagree.
    if supports_effort(&model) && !betas.iter().any(|b| b == BETA_EFFORT) {
        betas.push(BETA_EFFORT.to_string());
    }

    for beta in BETA_FALLBACKS {
        if has_fallbacks && !betas.iter().any(|b| b == beta) {
            betas.push(beta.to_string());
        }
    }

    betas.join(",")
}

/// Derives the Anthropic-Beta capability set from the FINAL body (post strip),
/// so body-dependent capabilities stay coherent with the bytes actually sent
/// while the model-conditional base set still comes from the captured profile.
pub fn build_claude_code_betas_for_body(
    model_id: &str,
    json: &serde_json::Value,
    is_oauth: bool,
) -> String {
    build_claude_code_betas_for_body_from_profile(
        model_id,
        json,
        is_oauth,
        &crate::spoofer::claude_code_profile::current_profile(),
    )
}

/// La misma derivación anclada al perfil pinneado de la petición.
pub fn build_claude_code_betas_for_body_from_profile(
    model_id: &str,
    json: &serde_json::Value,
    is_oauth: bool,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) -> String {
    let display_updates = thinking_display_updates_active(json, model_id);
    let extended_ttl = extended_cache_ttl_active(json);
    let structured = structured_outputs_active(json);
    let has_thinking = thinking_active(json);
    let has_fallbacks = json
        .get("fallbacks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|fallbacks| !fallbacks.is_empty());

    let mut betas: Vec<String> =
        build_claude_code_betas_from_profile(model_id, has_fallbacks, is_oauth, profile)
            .split(',')
            .map(str::trim)
            .filter(|beta| !beta.is_empty())
            .map(ToString::to_string)
            .collect();

    // `thinking-display-updates-2026-08-18` (display updates) is only emitted when the resolved
    // display is `updates`.
    if display_updates {
        if !betas.iter().any(|b| b == BETA_THINKING_DISPLAY_UPDATES) {
            betas.push(BETA_THINKING_DISPLAY_UPDATES.to_string());
        }
    } else {
        betas.retain(|beta| beta != BETA_THINKING_DISPLAY_UPDATES);
    }

    // `redact-thinking` is the default when thinking is off, and is dropped as
    // soon as thinking is active (native removes `Y8t` on `Kc && R_`).
    if has_thinking {
        betas.retain(|beta| beta != BETA_REDACT_THINKING);
    } else if !betas.iter().any(|b| b == BETA_REDACT_THINKING) {
        let insert_at = betas
            .iter()
            .position(|b| b == BETA_INTERLEAVED_THINKING)
            .map_or(betas.len(), |idx| idx + 1);
        betas.insert(insert_at, BETA_REDACT_THINKING.to_string());
    }

    // Structured output requests advertise the structured-outputs beta.
    if structured && !betas.iter().any(|b| b == BETA_STRUCTURED_OUTPUTS) {
        betas.push(BETA_STRUCTURED_OUTPUTS.to_string());
    }

    // The 1h cache TTL beta follows directly from a body TTL of 1h.
    if !betas.iter().any(|b| b == BETA_EXTENDED_CACHE_TTL) && extended_ttl {
        betas.push(BETA_EXTENDED_CACHE_TTL.to_string());
    } else if !extended_ttl {
        betas.retain(|beta| beta != BETA_EXTENDED_CACHE_TTL);
    }

    if json
        .get("output_config")
        .and_then(|output| output.get("effort"))
        .is_none()
    {
        betas.retain(|beta| beta != BETA_EFFORT);
    }
    betas.join(",")
}

/// Extracts existing caller version, cc_prev_req and cc_prompt_id from an incoming billing header if present.
pub fn extract_claude_billing_tags(
    json: &serde_json::Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let system = json.get("system");
    let text = if let Some(s) = system.and_then(|v| v.as_str()) {
        s
    } else if let Some(arr) = system.and_then(|v| v.as_array()) {
        arr.first()
            .and_then(|v| v.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
    } else {
        ""
    };

    if !text.starts_with("x-anthropic-billing-header:") {
        return (None, None, None);
    }

    let caller_version = text.split("cc_version=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        let base_ver = val.split('.').take(3).collect::<Vec<_>>().join(".");
        if base_ver.starts_with("2.1.") {
            Some(base_ver)
        } else {
            None
        }
    });

    let prev_req = text.split("cc_prev_req=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        if val.starts_with("req_") {
            Some(val.to_string())
        } else {
            None
        }
    });

    let prompt_id = text.split("cc_prompt_id=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        if !val.is_empty() {
            Some(val.to_string())
        } else {
            None
        }
    });

    (caller_version, prev_req, prompt_id)
}

/// Splits a tool name into TitleCase segments: `browser_exec` becomes
/// `BrowserExec`, `read-file` becomes `ReadFile`. Segments split on `_` and `-`
/// and keep their inner casing, so an already-TitleCase name maps to itself.
pub fn title_case_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for segment in name.split(['_', '-']) {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

/// Builds the rename map from the `tools` declared in an Anthropic request body.
pub fn build_tool_rename_map(
    json: &serde_json::Value,
) -> std::collections::HashMap<String, String> {
    let Some(tools) = json.get("tools").and_then(|t| t.as_array()) else {
        return std::collections::HashMap::new();
    };
    build_tool_rename_map_from_names(
        tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str())),
    )
}

/// Renames the tool names of an Anthropic request body in place: the declared
/// `tools`, any `tool_use` blocks in the conversation history, and a pinned
/// `tool_choice`. `tool_result` blocks reference tools by id, so they carry no
/// name to rewrite.
pub fn rename_tool_names_in_body(
    json: &mut serde_json::Value,
    map: &std::collections::HashMap<String, String>,
) {
    if map.is_empty() {
        return;
    }
    if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools {
            rename_name_field(tool, map);
        }
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for message in messages {
            let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    rename_name_field(block, map);
                }
            }
        }
    }
    if let Some(tool_choice) = json.get_mut("tool_choice") {
        rename_name_field(tool_choice, map);
    }
}

fn rename_name_field(
    value: &mut serde_json::Value,
    map: &std::collections::HashMap<String, String>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let Some(renamed) = map.get(name) else {
        return;
    };
    obj.insert(
        "name".to_string(),
        serde_json::Value::String(renamed.clone()),
    );
}

/// Cloaks the Anthropic JSON request body to match official Claude Code CLI.
pub fn cloak_claude_request_body(
    json: &mut serde_json::Value,
    model_id: &str,
    account_uuid: &str,
    session_id: &str,
    device_id: &str,
) {
    cloak_claude_request_body_from_profile(
        json,
        model_id,
        account_uuid,
        session_id,
        device_id,
        &crate::spoofer::claude_code_profile::current_profile(),
    );
}

/// El mismo cloaking anclado al perfil pinneado de la petición: defaults,
/// betas, TTL de cache y versión de billing salen todos de ESA generación, de
/// modo que un refresco concurrente no puede mezclar dos perfiles a mitad de
/// petición.
pub fn cloak_claude_request_body_from_profile(
    json: &mut serde_json::Value,
    model_id: &str,
    account_uuid: &str,
    session_id: &str,
    device_id: &str,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) {
    let (caller_version, prev_req, prompt_id) = extract_claude_billing_tags(json);
    let caller_prompts = collect_caller_system_prompts(json);
    let caller_system_ttl = json
        .get("system")
        .and_then(serde_json::Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|block| block.get("cache_control")?.get("ttl").cloned())
        });
    let first_user_text = extract_first_user_message_text(json);

    let effective_version = caller_version
        .unwrap_or_else(|| crate::spoofer::claude_code::current_claude_code_version_from(profile));

    let billing_text = generate_billing_header(
        &effective_version,
        &first_user_text,
        "cli",
        prev_req.as_deref(),
        prompt_id.as_deref(),
    );
    let billing_block = serde_json::json!({
        "type": "text",
        "text": billing_text
    });
    let mut identity_block = serde_json::json!({
        "type": "text",
        "text": CLAUDE_CODE_CLI_IDENTITY,
        "cache_control": {
            "type": "ephemeral"
        }
    });

    if let Some(ttl) = caller_system_ttl {
        identity_block["cache_control"]["ttl"] = ttl;
    }
    let mut system_blocks = vec![billing_block, identity_block];
    let lower_model = normalize_model(model_id);
    if model_matches(&lower_model, "fable-5-1") {
        system_blocks.push(serde_json::json!({
            "type": "text",
            "text": CLAUDE_CODE_FABLE_REPORTING_OUTCOMES
        }));
    }

    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "system".to_string(),
            serde_json::Value::Array(system_blocks),
        );
    }

    if !caller_prompts.is_empty() {
        let combined = caller_prompts.join("\n\n");
        inject_system_reminder(json, &combined);
    }

    // Upstreams classify third-party clients partly by the shape of the tool
    // names: snake_case in volume is rejected. Rename them to TitleCase here so
    // the signature below and every later stage see the cloaked bytes; the
    // response side maps the names back to what the client sent.
    let tool_rename_map = build_tool_rename_map(json);
    rename_tool_names_in_body(json, &tool_rename_map);

    // Body defaults come from the pinned profile snapshot and never overwrite
    // an explicit caller value.
    apply_thinking_defaults(json, model_id, profile);
    apply_output_config_defaults(json, model_id, profile);
    apply_context_management_defaults(json, profile);

    apply_model_fallbacks(json, model_id);
    apply_user_id_metadata(json, account_uuid, session_id, device_id);

    crate::anthropic_cache::ensure_anthropic_cache_control(json);
    crate::anthropic_cache::inject_tools_cache_control(json);
    // Promote the freshly placed breakpoints to the native 1h TTL, preserving
    // any explicit caller TTL such as a pinned `5m`.
    apply_claude_code_cache_ttl(json, &profile.cache_ttl);
    crate::anthropic_cache::enforce_cache_control_limit(json, 4);
}
