//! Claude Code request cloaking and identity spoofing.
//!
//! Emulates native Claude Code CLI (claude-cli/2.1.280) top-level system structure,
//! dynamic build fingerprint generation, user_id metadata, model fallbacks,
//! and harness prompt relocation via `<system-reminder>`.

use sha2::{Digest, Sha256};
use std::fmt::Write;

pub const CLAUDE_CODE_CLI_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub const FINGERPRINT_SALT: &str = "59cf53e54c78";

pub const CLAUDE_CODE_FABLE_REPORTING_OUTCOMES: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";

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

    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(content_val) = msg.get_mut("content") {
                if let Some(text) = content_val.as_str() {
                    let block1 = serde_json::json!({
                        "type": "text",
                        "text": formatted_reminder
                    });
                    let block2 = serde_json::json!({
                        "type": "text",
                        "text": text
                    });
                    *content_val = serde_json::Value::Array(vec![block1, block2]);
                    return;
                } else if let Some(arr) = content_val.as_array_mut() {
                    let block = serde_json::json!({
                        "type": "text",
                        "text": formatted_reminder
                    });
                    let mut insert_at = 0;
                    while insert_at < arr.len()
                        && arr[insert_at].get("type").and_then(|t| t.as_str())
                            == Some("tool_result")
                    {
                        insert_at += 1;
                    }
                    arr.insert(insert_at, block);
                    return;
                }
            }
            break;
        }
    }
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

/// Assembles Anthropic-Beta headers dynamically matching Claude Code CLI 2.1.280.
pub fn build_claude_code_betas(model_id: &str, has_fallbacks: bool, is_oauth: bool) -> String {
    let mut betas = vec!["claude-code-20250219".to_string()];
    if is_oauth {
        betas.push("oauth-2025-04-20".to_string());
    }
    betas.push("interleaved-thinking-2025-05-14".to_string());
    betas.push("redact-thinking-2026-02-12".to_string());
    betas.push("thinking-token-count-2026-05-13".to_string());
    betas.push("context-management-2025-06-27".to_string());
    betas.push("prompt-caching-scope-2026-01-05".to_string());

    let model = model_id.to_ascii_lowercase();
    let is_legacy = model.starts_with("claude-3-")
        || model.contains("haiku-4-5")
        || model.contains("sonnet-4-");

    if !is_legacy {
        betas.push("mid-conversation-system-2026-04-07".to_string());
        if model.contains("opus-5-5")
            || model.contains("opus-5.5")
            || model.contains("fable-5-1")
            || model.contains("fable-5.1")
        {
            betas.push("per-turn-control-2026-07-01".to_string());
        }
        if !model.contains("sonnet-5") {
            betas.push("mid-conversation-tool-changes-2026-07-01".to_string());
        }
    }

    betas.push("effort-2025-11-24".to_string());

    if has_fallbacks {
        betas.push("server-side-fallback-2026-06-01".to_string());
        betas.push("fallback-credit-2026-06-01".to_string());
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
    let (caller_version, prev_req, prompt_id) = extract_claude_billing_tags(json);
    let caller_prompts = collect_caller_system_prompts(json);
    let first_user_text = extract_first_user_message_text(json);

    let effective_version =
        caller_version.unwrap_or_else(crate::spoofer::claude_code::current_claude_code_version);

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
    let identity_block = serde_json::json!({
        "type": "text",
        "text": CLAUDE_CODE_CLI_IDENTITY,
        "cache_control": {
            "type": "ephemeral"
        }
    });

    let mut system_blocks = vec![billing_block, identity_block];
    let lower_model = model_id.to_ascii_lowercase();
    if lower_model.contains("fable-5-1") || lower_model.contains("fable-5.1") {
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

    apply_model_fallbacks(json, model_id);
    apply_user_id_metadata(json, account_uuid, session_id, device_id);

    crate::anthropic_cache::ensure_anthropic_cache_control(json);
    crate::anthropic_cache::enforce_cache_control_limit(json, 4);
}
