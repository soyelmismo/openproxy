use super::ClientSpoofer;
use openproxy_types::ClientProfile;

/// Builds the default spoofed header list for a given [`ClientProfile`].
///
/// This provides a single, unified source of truth for upstream client spoofing
/// headers across all providers, targets, and models.
#[must_use]
pub fn headers_for_client_profile(
    profile: ClientProfile,
    model: Option<&str>,
) -> Vec<(String, String)> {
    match profile {
        ClientProfile::ClaudeCode => {
            let mut spoofer = crate::spoofer::ClaudeCodeSpoofer::new().with_oauth(true);
            if let Some(m) = model {
                spoofer = spoofer.with_model(m);
            }
            spoofer.headers()
        }
        ClientProfile::Codex => crate::spoofer::CodexSpoofer.headers(),
        ClientProfile::Cline => crate::spoofer::ClineSpoofer.headers(),
        ClientProfile::Kilocode => crate::spoofer::KilocodeSpoofer.headers(),
        ClientProfile::Kiro => crate::spoofer::KiroSpoofer.headers(),
        ClientProfile::Minimax => crate::spoofer::MiniMaxSpoofer.headers(),
        ClientProfile::CommandCode => crate::spoofer::CommandCodeSpoofer.headers(),
        ClientProfile::CodeBuddy => crate::spoofer::CodeBuddySpoofer.headers(),
        ClientProfile::Opencode => crate::spoofer::OpenCodeSpoofer.headers(),
        ClientProfile::Antigravity => crate::spoofer::AntigravitySpoofer::new().headers(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headers_for_every_client_profile() {
        for profile in ClientProfile::ALL {
            let headers = headers_for_client_profile(*profile, Some("test-model"));
            assert!(
                !headers.is_empty(),
                "profile {profile:?} produced empty headers"
            );
        }
    }
}
