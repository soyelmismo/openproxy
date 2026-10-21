use openproxy_types::OpenAIRequest;
use openproxy_types::combos::Combo;
use openproxy_types::models::Model;
use std::sync::Arc;

/// Centralized resolution of overrides across Client Request -> Root Combo -> Subcombo -> Target -> Model.
///
/// Ensures all target formats (OpenAI, Anthropic, Gemini, Responses, etc.) receive a consistently
/// normalized request and model representation without ad-hoc mutations or leaky dialect defaults.
#[derive(Debug, Clone, Default)]
pub struct EffectiveTargetPolicy {
    /// Effective maximum output tokens cap (in tokens).
    pub max_output_tokens: Option<u32>,
    /// Effective context window (in tokens).
    pub context_window: Option<i64>,
    /// Thinking effort (e.g. "low", "medium", "high", "max").
    pub thinking_effort: Option<String>,
}

impl EffectiveTargetPolicy {
    /// Resolve the effective policy for a target execution step.
    ///
    /// Precedence hierarchy:
    /// 1. If Root Combo defines `max_output_tokens`, it sets the ceiling, clamped by the target Model's declared ceiling.
    /// 2. If Root Combo has no cap, the Model's declared ceiling applies.
    /// 3. Target's thinking effort overrides default when present.
    pub fn resolve(
        combo: Option<&Combo>,
        target: &openproxy_types::ComboTarget,
        model: &Model,
    ) -> Self {
        let max_output_tokens = match (
            combo.and_then(|c| c.max_output_tokens),
            model.max_output_tokens,
        ) {
            (Some(combo_cap), Some(model_cap)) if combo_cap > 0 && model_cap > 0 => {
                let clamped = combo_cap.min(model_cap);
                Some(clamped.min(i64::from(u32::MAX)) as u32)
            }
            (Some(combo_cap), _) if combo_cap > 0 => {
                Some(combo_cap.min(i64::from(u32::MAX)) as u32)
            }
            (_, Some(model_cap)) if model_cap > 0 => {
                Some(model_cap.min(i64::from(u32::MAX)) as u32)
            }
            _ => None,
        };

        let context_window = combo
            .and_then(|c| c.context_window)
            .or(model.context_length);
        let thinking_effort = target.thinking_effort.clone();

        Self {
            max_output_tokens,
            context_window,
            thinking_effort,
        }
    }

    /// Produce an effective `Model` clone reflecting the resolved policy limits.
    pub fn apply_to_model(&self, model: &Model) -> Model {
        let mut m = model.clone();
        if let Some(cap) = self.max_output_tokens {
            m.max_output_tokens = Some(i64::from(cap));
        }
        if let Some(cw) = self.context_window {
            m.context_length = Some(cw);
        }
        m
    }

    /// Produce an effectively normalized `OpenAIRequest` reflecting the resolved policy.
    ///
    /// Invariant:
    /// - If the client explicitly provided `max_tokens` or `max_completion_tokens`, it is PRESERVED.
    /// - If the client omitted both, `policy.max_output_tokens` is populated into `max_tokens`.
    /// - If `thinking_effort` is configured, it is inserted into `extra["reasoning_effort"]`.
    pub fn normalize_request(&self, req: &Arc<OpenAIRequest>) -> Arc<OpenAIRequest> {
        let client_has_max = req.max_tokens.is_some()
            || req.extra.contains_key("max_completion_tokens")
            || req.extra.contains_key("max_tokens");

        let needs_effort = self.thinking_effort.is_some()
            && !req.extra.contains_key("reasoning_effort")
            && !req.extra.contains_key("thinking_effort");

        let needs_max = !client_has_max && self.max_output_tokens.is_some();

        if !needs_effort && !needs_max {
            return Arc::clone(req);
        }

        let mut patched = (**req).clone();
        if needs_max {
            patched.max_tokens = self.max_output_tokens;
        }
        if let Some(ref effort) = self.thinking_effort
            && needs_effort
        {
            patched
                .extra
                .insert("reasoning_effort".to_string(), serde_json::json!(effort));
        }

        Arc::new(patched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::OpenAIMessage;
    use openproxy_types::ids::{ComboId, ComboTargetId, ModelRowId, ProviderId};
    use openproxy_types::models::Model;

    fn sample_model(max_out: Option<i64>) -> Model {
        Model {
            row_id: ModelRowId(1),
            model_id: "test-model".into(),
            provider_id: ProviderId::new("test-provider"),
            target_format: openproxy_types::TargetFormat::Openai,
            context_length: Some(128_000),
            max_output_tokens: max_out,
            active: true,
            ..Default::default()
        }
    }

    fn sample_target(effort: Option<&str>) -> openproxy_types::ComboTarget {
        openproxy_types::ComboTarget {
            id: ComboTargetId(10),
            combo_id: ComboId(50),
            provider_id: ProviderId::new("test-provider"),
            account_id: None,
            model_row_id: Some(ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: Default::default(),
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: effort.map(String::from),
            description: None,
            selected_accounts: None,
        }
    }

    #[test]
    fn test_policy_combo_clamps_model_ceiling() {
        let mut combo = Combo::default();
        combo.max_output_tokens = Some(32_768);

        let target = sample_target(None);
        let model = sample_model(Some(8_192)); // Model cap is lower

        let policy = EffectiveTargetPolicy::resolve(Some(&combo), &target, &model);
        assert_eq!(policy.max_output_tokens, Some(8_192));

        let model_high = sample_model(Some(128_000)); // Combo cap is lower
        let policy_high = EffectiveTargetPolicy::resolve(Some(&combo), &target, &model_high);
        assert_eq!(policy_high.max_output_tokens, Some(32_768));
    }

    #[test]
    fn test_policy_preserves_client_max_tokens() {
        let mut combo = Combo::default();
        combo.max_output_tokens = Some(32_768);

        let target = sample_target(Some("high"));
        let model = sample_model(Some(128_000));
        let policy = EffectiveTargetPolicy::resolve(Some(&combo), &target, &model);

        // Case A: client pinned max_tokens
        let req_pinned = Arc::new(OpenAIRequest {
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::json!("hi")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                extra: Default::default(),
            }],
            max_tokens: Some(1024),
            ..Default::default()
        });
        let normalized_a = policy.normalize_request(&req_pinned);
        assert_eq!(normalized_a.max_tokens, Some(1024));
        assert_eq!(normalized_a.extra["reasoning_effort"], "high");

        // Case B: client omitted max_tokens
        let req_omitted = Arc::new(OpenAIRequest {
            messages: req_pinned.messages.clone(),
            max_tokens: None,
            ..Default::default()
        });
        let normalized_b = policy.normalize_request(&req_omitted);
        assert_eq!(normalized_b.max_tokens, Some(32_768));
        assert_eq!(normalized_b.extra["reasoning_effort"], "high");
    }
}
