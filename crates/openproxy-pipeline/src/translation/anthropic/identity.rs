pub const CLAUDE_AGENT_SDK_IDENTITY: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
pub const CLAUDE_CODE_CLI_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Normalize the standalone identity block injected by Claude Agent SDK clients.
///
/// Antigravity's upstream classifies this SDK identity differently from Claude Code's
/// CLI identity and can reject an otherwise identical request with RESOURCE_EXHAUSTED.
/// Keep the match exact so user-authored text that merely mentions the SDK identity is
/// not rewritten.
pub fn normalize_claude_client_identity(text: &str) -> &str {
    if text == CLAUDE_AGENT_SDK_IDENTITY {
        CLAUDE_CODE_CLI_IDENTITY
    } else {
        text
    }
}
