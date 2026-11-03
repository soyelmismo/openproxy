//! Client session thread management for Anthropic Message Threads.
//!
//! Provides synthetic thread state tracking across conversation turns. When a client
//! (such as Hermes or gpthon) connects without native message-thread support, OpenProxy
//! can maintain the session prefix history and translate turns into `thread: { type: "create" }`
//! or `thread: { type: "continue", previous_message_id }`, slicing identical prefixes to reduce
//! upstream payload sizes and time-to-first-token (TTFT).

use dashmap::DashMap;
use openproxy_types::OpenAIMessage;
use openproxy_types::ids::ComboTargetId;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Idle TTL for an upstream server-side thread (Anthropic document states ~5 minutes / 300s).
/// We invalidate at 270 seconds (4.5 minutes) to avoid edge timeouts.
pub const THREAD_IDLE_TIMEOUT: Duration = Duration::from_secs(270);

/// Recorded state of an active synthetic conversation thread.
#[derive(Debug, Clone)]
pub struct ActiveThreadState {
    /// Pinned target ID to preserve target/account affinity.
    pub target_id: Option<ComboTargetId>,
    /// Last message ID returned by the upstream (e.g. `msg_01...` or synthetic turn id).
    pub last_message_id: String,
    /// Hashes of the messages included up to this turn.
    pub message_hashes: Vec<u64>,
    /// Monotonic timestamp of the latest activity.
    pub last_activity: Instant,
    /// Whether the server has indicated this thread must stay stateless.
    pub degraded_to_stateless: bool,
}

/// Registry mapping canonical session keys to active synthetic thread state.
#[derive(Clone, Default)]
pub struct MessageThreadRegistry {
    entries: Arc<DashMap<String, ActiveThreadState>>,
}

impl MessageThreadRegistry {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
        }
    }

    /// Computes a 64-bit content hash of an OpenAI-shaped message for prefix comparison.
    pub fn hash_message(msg: &OpenAIMessage) -> u64 {
        let mut hasher = DefaultHasher::new();
        msg.role.hash(&mut hasher);
        openproxy_types::extract_content_text(&msg.content).hash(&mut hasher);
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                if let Some(s) = tc.as_str() {
                    s.hash(&mut hasher);
                } else {
                    serde_json::to_string(tc)
                        .unwrap_or_default()
                        .hash(&mut hasher);
                }
            }
        }
        if let Some(id) = &msg.tool_call_id {
            id.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Evaluates whether the current request matches an active cached thread prefix.
    /// Returns either:
    /// - `Some((previous_message_id, slice_from))` if continuation is valid.
    /// - `None` if a new thread should be created or request should remain stateless.
    pub fn plan_continuation(
        &self,
        session_key: &str,
        messages: &[OpenAIMessage],
    ) -> Option<(String, usize)> {
        let entry = self.entries.get(session_key)?;
        if entry.degraded_to_stateless {
            return None;
        }

        if entry.last_activity.elapsed() >= THREAD_IDLE_TIMEOUT {
            drop(entry);
            self.entries.remove(session_key);
            return None;
        }

        let current_hashes: Vec<u64> = messages.iter().map(Self::hash_message).collect();
        let cached_hashes = &entry.message_hashes;

        // Continuation is only possible if current messages strictly extend cached messages.
        if cached_hashes.is_empty() || current_hashes.len() <= cached_hashes.len() {
            return None;
        }

        // Check that cached_hashes is an exact prefix of current_hashes.
        if current_hashes[..cached_hashes.len()] != cached_hashes[..] {
            return None;
        }

        Some((entry.last_message_id.clone(), cached_hashes.len()))
    }

    /// Records or updates the state of a thread following a successful upstream response.
    /// `messages_in_turn` are all messages sent up to and including the current turn (e.g. user + assistant).
    pub fn record_success(
        &self,
        session_key: &str,
        target_id: Option<ComboTargetId>,
        messages: &[OpenAIMessage],
        response_message_id: &str,
    ) {
        let hashes: Vec<u64> = messages.iter().map(Self::hash_message).collect();

        self.entries.insert(
            session_key.to_string(),
            ActiveThreadState {
                target_id,
                last_message_id: response_message_id.to_string(),
                message_hashes: hashes,
                last_activity: Instant::now(),
                degraded_to_stateless: false,
            },
        );
    }

    pub fn should_opt_in_synthetic_threads(&self, session_key: &str) -> bool {
        let entry = self.entries.get(session_key);
        entry.is_none_or(|e| !e.degraded_to_stateless)
    }

    /// Clears or invalidates an active thread on mismatch, reset, or expiration.
    pub fn invalidate(&self, session_key: &str) {
        self.entries.remove(session_key);
    }

    /// Marks a thread as permanently degraded to stateless for the rest of this session.
    pub fn mark_stateless(&self, session_key: &str) {
        if let Some(mut entry) = self.entries.get_mut(session_key) {
            entry.degraded_to_stateless = true;
        }
    }
}

pub(crate) fn resolve_synthetic_thread_plan<'a>(
    ctx: &'a crate::context::PipelineContext,
    target_format: openproxy_types::TargetFormat,
    adapter_id: &str,
    messages_ref: &'a [openproxy_types::OpenAIMessage],
) -> (
    &'a [openproxy_types::OpenAIMessage],
    Option<serde_json::Value>,
) {
    if target_format != openproxy_types::TargetFormat::Anthropic
        || adapter_id != "claude-code"
        || ctx.req.openai_request.extra.contains_key("thread")
    {
        return (messages_ref, None);
    }

    let canonical_session = crate::stages::target_headers::session::resolve_canonical_session(
        &ctx.req.request_headers,
        &ctx.req.openai_request,
    );

    if !ctx
        .pipeline
        .message_threads
        .should_opt_in_synthetic_threads(&canonical_session)
    {
        return (messages_ref, None);
    }

    if let Some((prev_msg_id, slice_from)) = ctx
        .pipeline
        .message_threads
        .plan_continuation(&canonical_session, messages_ref)
    {
        if slice_from < messages_ref.len() {
            let thread = serde_json::json!({
                "type": "continue",
                "previous_message_id": prev_msg_id,
            });
            return (&messages_ref[slice_from..], Some(thread));
        }
    } else {
        let thread = serde_json::json!({
            "type": "create"
        });
        return (messages_ref, Some(thread));
    }

    (messages_ref, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_thread_registry_plan_and_record() {
        let registry = MessageThreadRegistry::new();
        let session = "sess-test-123";

        let msg1 = OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("hello 1".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let initial_msgs = vec![msg1.clone()];
        assert!(registry.plan_continuation(session, &initial_msgs).is_none());

        // Assistant responds
        let assistant_msg = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::String("hi there".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let turn1_history = vec![msg1.clone(), assistant_msg.clone()];
        registry.record_success(session, None, &turn1_history, "msg_01abc");

        // Turn 2 with continuation
        let msg2 = OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("what is my name?".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };

        let turn2_msgs = vec![msg1, assistant_msg.clone(), msg2.clone()];
        let plan = registry.plan_continuation(session, &turn2_msgs);
        assert!(plan.is_some());
        let (prev_id, slice_from) = plan.unwrap();
        assert_eq!(prev_id, "msg_01abc");
        assert_eq!(slice_from, 2);

        // If history diverges, plan returns None
        let altered_msg1 = OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("different hello".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        let diverged_msgs = vec![altered_msg1, assistant_msg, msg2];
        assert!(
            registry
                .plan_continuation(session, &diverged_msgs)
                .is_none()
        );
    }
}
