use dashmap::DashMap;
use openproxy_types::ids::{ComboId, ComboTargetId};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

pub const DEFAULT_SESSION_AFFINITY_TTL: Duration = Duration::from_secs(900); // 15 minutes

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionAffinityKey {
    pub session_hash: u64,
    pub combo_id: ComboId,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionAffinityEntry {
    pub target_id: ComboTargetId,
    pub expires_at: Instant,
}

#[derive(Default)]
pub struct SessionAffinityRegistry {
    entries: DashMap<SessionAffinityKey, SessionAffinityEntry>,
}

impl SessionAffinityRegistry {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Extract a 64-bit session fingerprint hash from a request.
    ///
    /// Priority order:
    /// 1. Explicit headers (`x-session-id`, `x-conversation-id`, `x-thread-id`)
    /// 2. `req.openai_request.user`
    /// 3. Conversation fingerprint: hash of the root non-system user message.
    ///    Returns None if no session identity or valid message can be found.
    pub fn extract_session_hash(req: &crate::PipelineRequest) -> Option<u64> {
        // 1. Explicit headers
        for header_key in &["x-session-id", "x-conversation-id", "x-thread-id"] {
            if let Some(val) = req.request_headers.get(*header_key) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    let mut hasher = DefaultHasher::new();
                    trimmed.hash(&mut hasher);
                    return Some(hasher.finish());
                }
            }
        }

        // 2. OpenAI request `user` field
        if let Some(ref user) = req.openai_request.user {
            let trimmed = user.trim();
            if !trimmed.is_empty() {
                let mut hasher = DefaultHasher::new();
                trimmed.hash(&mut hasher);
                return Some(hasher.finish());
            }
        }

        // 3. Conversation fingerprint: hash of the root user message
        // In multi-turn agent/chat conversations, the initial user prompt is invariant.
        for msg in &req.openai_request.messages {
            if msg.role == "user" {
                let text = msg.extract_text_cow();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let mut hasher = DefaultHasher::new();
                    trimmed.hash(&mut hasher);
                    return Some(hasher.finish());
                }
            }
        }

        None
    }

    /// Check if the request explicitly asks to reset session affinity.
    pub fn should_reset(req: &crate::PipelineRequest) -> bool {
        req.request_headers
            .get("x-openproxy-reset-affinity")
            .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
            || req
                .request_headers
                .get("x-route")
                .is_some_and(|v| v.eq_ignore_ascii_case("force") || v.eq_ignore_ascii_case("reset"))
    }

    /// Get the pinned target for a given session key, extending its TTL if valid.
    pub fn get(&self, key: SessionAffinityKey) -> Option<ComboTargetId> {
        let now = Instant::now();
        if let Some(mut entry) = self.entries.get_mut(&key)
            && entry.expires_at > now
        {
            entry.expires_at = now + DEFAULT_SESSION_AFFINITY_TTL;
            return Some(entry.target_id);
        }
        self.entries.remove(&key);
        None
    }

    /// Set or update the pinned target for a session key.
    pub fn set(&self, key: SessionAffinityKey, target_id: ComboTargetId) {
        let now = Instant::now();
        self.entries.insert(
            key,
            SessionAffinityEntry {
                target_id,
                expires_at: now + DEFAULT_SESSION_AFFINITY_TTL,
            },
        );
    }

    /// Invalidate an affinity entry (e.g. if the target became unhealthy).
    pub fn invalidate(&self, key: &SessionAffinityKey) {
        self.entries.remove(key);
    }

    /// Prune expired entries to keep memory bounded.
    pub fn prune(&self) -> usize {
        let now = Instant::now();
        let mut pruned = 0;
        self.entries.retain(|_, v| {
            if v.expires_at > now {
                true
            } else {
                pruned += 1;
                false
            }
        });
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_affinity_lifecycle() {
        let registry = SessionAffinityRegistry::new();
        let key = SessionAffinityKey {
            session_hash: 12345,
            combo_id: ComboId(505),
        };
        let target = ComboTargetId(5368);

        assert_eq!(registry.get(key), None);

        registry.set(key, target);
        assert_eq!(registry.get(key), Some(target));

        registry.invalidate(&key);
        assert_eq!(registry.get(key), None);
    }
}
