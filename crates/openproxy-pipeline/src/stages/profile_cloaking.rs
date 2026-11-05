use bytes::Bytes;
use openproxy_types::ClientProfile;
use openproxy_types::context::ResolvedTarget;
use openproxy_types::error::Result;

/// Wraps the request body according to an explicit [`ClientProfile`] if configured
/// and applicable to the given [`openproxy_types::TargetFormat`].
///
/// For example, when target format is Anthropic and the client profile is
/// [`ClientProfile::ClaudeCode`], this wraps the payload with Claude Code CLI
/// system prompt, user-agent metadata, and dynamic identity tokens.
pub fn wrap_profile_request_body(
    body: Bytes,
    profile: ClientProfile,
    target_format: openproxy_types::TargetFormat,
    current: &ResolvedTarget,
) -> Result<Bytes> {
    match profile {
        ClientProfile::ClaudeCode => {
            // Apply Claude Code cloaking when targeting Anthropic
            if target_format == openproxy_types::TargetFormat::Anthropic {
                let claude_adapter = openproxy_adapters::adapters::ClaudeCodeAdapter::new();
                use openproxy_adapters::ProviderAdapter;
                claude_adapter.wrap_request_body(
                    body,
                    target_format,
                    &current.model.model_id,
                    current,
                )
            } else {
                Ok(body)
            }
        }
        _ => Ok(body),
    }
}

/// Finalizes the request body and updates headers according to an explicit
/// [`ClientProfile`] if configured.
///
/// For example, for [`ClientProfile::ClaudeCode`], this aligns billing headers,
/// synchronizes session IDs, regenerates Anthropic-Beta capability headers,
/// and applies cryptographic body signatures (CCH).
pub fn finalize_profile_request(
    body: Bytes,
    headers: &mut Vec<(String, String)>,
    profile: ClientProfile,
    target_format: openproxy_types::TargetFormat,
    current: &ResolvedTarget,
) -> Result<Bytes> {
    match profile {
        ClientProfile::ClaudeCode => {
            if target_format == openproxy_types::TargetFormat::Anthropic {
                let claude_adapter = openproxy_adapters::adapters::ClaudeCodeAdapter::new();
                use openproxy_adapters::ProviderAdapter;
                claude_adapter.finalize_request(
                    body,
                    headers,
                    target_format,
                    &current.model.model_id,
                    current,
                )
            } else {
                Ok(body)
            }
        }
        _ => Ok(body),
    }
}
