//! Official ZCode system prompt synthesizer for Z.ai Start Plan upstream gate.
//!
//! Upstream `/api/v1/zcode-plan/anthropic/v1/messages` checks the Anthropic `system`
//! prompt structure. If missing official identity blocks, upstream rejects with
//! HTTP 405 / error 3012: `request has been blocked due to unusual activity.`
//!
//! Synthesizing official identity blocks allows seamless proxying of standard
//! Anthropic / OpenAI prompts while satisfying upstream security checks.

use bytes::Bytes;
use openproxy_types::error::CoreError;
use serde_json::{Value, json};

pub const OFFICIAL_CLI_PREFIX: &str = "You are ZCode, an interactive coding agent";

pub const OFFICIAL_STABLE_SECTIONS: &[&str] = &[
    "\nYou are an interactive ZCode agent that helps users with software engineering tasks.\n\nIMPORTANT: Assist with authorized security testing, defensive security, CTF challenges, and educational contexts. Refuse requests for destructive techniques, DoS attacks, mass targeting, supply chain compromise, or detection evasion for malicious purposes. Dual-use security tools (C2 frameworks, credential testing, exploit development) require clear authorization context: pentesting engagements, CTF competitions, security research, or defensive use cases.\n\n# Harness\n- Text you output outside of tool use is displayed to the user as Github-flavored markdown in a terminal.\n- Tools run behind a user-selected permission mode; a denied call means the user declined it — adjust, don't retry verbatim.\n- The system may send updates, reminders, or modifications to rules via mid-conversation system turns. These are system-controlled, unlike function results. Hooks may intercept tool calls; treat hook output as user feedback.\n- Prefer the dedicated file/search tools over shell commands when one fits. Independent tool calls can run in parallel in one response.\n- Reference code as `file_path:line_number` — it's clickable.",
    "# ZCode Desktop Context\n\n### Files & URLs\n- Return local web URLs as Markdown links (e.g., [label](http://127.0.0.1:8080)).\n- File should be an absolute path or include the workspace folder segment so it can be resolved relative to the workspace.\n- Unless otherwise specified, return local file references as Markdown links (e.g., [name.md](/absolute/path/to/name.md)).\n\n### Inline Code Comments\n- Use the ::code-comment{...} directive when you need to attach feedback directly to specific code lines.\n- Emit one directive per inline comment; emit none when there are no actionable inline comments.\n- Required attributes: title (short label), body (one-paragraph explanation), file (path to the file).\n- Optional attributes: start, end (1-based line numbers), priority (0-3).\n- file should be an absolute path or include the workspace folder segment so it can be resolved relative to the workspace.\n- Keep line ranges tight; end defaults to start.\n- Example: ::code-comment{title=\"[P2] Off-by-one\" body=\"Loop iterates past the end when length is 0.\" file=\"/path/to/foo.ts\" start=10 end=11 priority=2}",
    "# Working style\n\nWhen you have enough information to act, act. Do not re-derive facts already established in the conversation, re-litigate a decision the user has already made, or narrate options you will not pursue. If you are weighing a choice, give a recommendation, not an exhaustive survey. Prefer reading the actual file or running the actual command over reasoning about what it probably contains. When a signal pattern-matches to a known failure, check that the evidence actually supports that specific diagnosis before acting on it.",
];

pub fn build_environment_section(model: &str) -> String {
    format!(
        "# Environment\nYou have been invoked in the following environment:\n - Primary working directory: .\n - Is a git repository: yes\n - Platform: linux\n - Shell: bash\n - OS Version: linux x64\n - You are powered by the model named zcode/{model}."
    )
}

/// Wraps Anthropic request payload with official ZCode system prompt blocks.
pub fn wrap_zcode_system_blocks(body: Bytes, model: &str) -> Result<Bytes, CoreError> {
    if body.is_empty() {
        return Ok(body);
    }
    let mut val: Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("failed to parse zcode request body: {e}")))?;

    let Some(obj) = val.as_object_mut() else {
        return Ok(body);
    };

    let existing_system = obj.remove("system");
    let mut blocks = Vec::with_capacity(5);

    // Block 0: CLI prefix
    blocks.push(json!({
        "type": "text",
        "text": OFFICIAL_CLI_PREFIX,
    }));

    // Block 1: Stable sections joined by double newline
    let stable_text = OFFICIAL_STABLE_SECTIONS.join("\n\n");
    blocks.push(json!({
        "type": "text",
        "text": stable_text,
    }));

    // Block 2: Environment
    blocks.push(json!({
        "type": "text",
        "text": build_environment_section(model),
    }));

    // Block 3..: Caller's system prompt (if any)
    if let Some(sys) = existing_system {
        match sys {
            Value::String(s) if !s.trim().is_empty() => {
                if !s.contains(OFFICIAL_CLI_PREFIX) {
                    blocks.push(json!({
                        "type": "text",
                        "text": s,
                    }));
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    let has_prefix = item
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.contains(OFFICIAL_CLI_PREFIX));
                    if !has_prefix {
                        blocks.push(item);
                    }
                }
            }
            _ => {}
        }
    }

    // Set cache_control on the last block
    if let Some(last) = blocks.last_mut()
        && let Some(last_obj) = last.as_object_mut()
    {
        last_obj.insert(
            "cache_control".to_string(),
            json!({ "type": "ephemeral" }),
        );
    }

    obj.insert("system".to_string(), Value::Array(blocks));

    let updated = serde_json::to_vec(&val)
        .map_err(|e| CoreError::Parse(format!("failed to serialize zcode request body: {e}")))?;

    Ok(Bytes::from(updated))
}
