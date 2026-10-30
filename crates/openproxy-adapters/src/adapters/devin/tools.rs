//! Devin tool-definition mapping (request field 10).
//!
//! `collect_devin_tools` translates the OpenAI `tools` array into the wire
//! shape, applying Devin's description rewrites and filtering the Codex-app
//! automation hook the upstream must never see.

use serde_json::Value;

use super::protoc;

/// A tool definition for request field 10.
#[derive(Debug, Clone, Default)]
pub struct DevinTool {
    pub name: String,
    pub description: String,
    /// Raw JSON schema bytes for field 3.
    pub parameters: Vec<u8>,
}

impl DevinTool {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        protoc::append_string(&mut buf, 1, &self.name);
        protoc::append_string(&mut buf, 2, &self.description);
        if !self.parameters.is_empty() {
            protoc::append_bytes(&mut buf, 3, &self.parameters);
        }
        buf
    }
}

/// Whether the tool is the Codex app's automation-update hook Devin must not
/// see (`mcp__codex_app__automation_update`, or `mcp__codex_app` namespace +
/// `automation_update` name).
fn is_devin_codex_app_automation_update(name: &str, namespace: &str) -> bool {
    name.eq_ignore_ascii_case("mcp__codex_app__automation_update")
        || (namespace.eq_ignore_ascii_case("mcp__codex_app")
            && name.eq_ignore_ascii_case("automation_update"))
}

/// Replace the first ASCII case-insensitive occurrence of `needle`. Match
/// positions are byte offsets into ASCII needle bytes, so `str::get` always
/// lands on char boundaries (UTF-8 continuation bytes can never equal ASCII).
fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let Some(pos) = haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    else {
        return haystack.to_string();
    };
    let (Some(head), Some(tail)) = (haystack.get(..pos), haystack.get(pos + needle.len()..)) else {
        return haystack.to_string();
    };
    format!("{head}{replacement}{tail}")
}

/// Tool description rewrite Devin applies to its exec/stdin tools — the
/// upstream grammar literally expects the "a session" → "an session" and
/// "an existing" → "a existing" articles.
pub fn sanitize_devin_tool_description(name: &str, description: &str) -> String {
    // Claude Code subagent tool descriptions hardcode snake_case "task_id",
    // but Devin's upstream tool execution environment strictly expects
    // camelCase "taskId" (upstream applies this rewrite before the
    // name-gated article fixes).
    const TASK_ID_NEEDLE: &str = "Takes a task_id parameter identifying the task";
    let description = if description.contains(TASK_ID_NEEDLE) {
        description.replace(
            TASK_ID_NEEDLE,
            "Takes a taskId parameter identifying the task",
        )
    } else {
        description.to_string()
    };
    let description = description.as_str();
    let lower = name.to_ascii_lowercase();
    if lower.ends_with("exec_command") || lower.ends_with("__exec_command") {
        return replace_case_insensitive(
            description,
            "returning output or a session ID for ongoing interaction",
            "returning output or an session ID for ongoing interaction",
        );
    }
    if lower.ends_with("write_stdin") || lower.ends_with("__write_stdin") {
        const NEEDLE: &str =
            "Writes characters to an existing unified exec session and returns recent output";
        const REPL: &str =
            "Writes characters to a existing unified exec session and returns recent output";
        let with_period = format!("{NEEDLE}.");
        let repl_with_period = format!("{REPL}.");
        let replaced = replace_case_insensitive(description, &with_period, &repl_with_period);
        if replaced != description {
            return replaced;
        }
        return replace_case_insensitive(description, NEEDLE, REPL);
    }
    description.to_string()
}

/// Map the OpenAI `tools` array to Devin tool definitions.
pub fn collect_devin_tools(tools: &[Value]) -> Vec<DevinTool> {
    let mut out = Vec::new();
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let function = obj.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let namespace = function
            .and_then(|f| f.get("namespace"))
            .or_else(|| obj.get("namespace"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if is_devin_codex_app_automation_update(&name, namespace) {
            continue;
        }
        let description = function
            .and_then(|f| f.get("description"))
            .or_else(|| obj.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let parameters = function
            .and_then(|f| f.get("parameters"))
            .or_else(|| obj.get("parameters"))
            .or_else(|| obj.get("parametersJsonSchema"))
            .map(|p| serde_json::to_vec(p).unwrap_or_default())
            .unwrap_or_default();
        out.push(DevinTool {
            description: sanitize_devin_tool_description(&name, description),
            name,
            parameters,
        });
    }
    out
}
