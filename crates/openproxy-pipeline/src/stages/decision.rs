//! Decision Routing Stage for combos using System One (Jev / Laya) models.
//!
//! Evaluates incoming prompt complexity and semantic requirements against
//! candidate targets' user-defined descriptions to re-order the targets,
//! placing the optimal model at index 0.

use crate::context::{PipelineContext, ResolvedTarget};
use openproxy_adapters::upstream::UpstreamRequest;
use openproxy_types::combos::Combo;
use openproxy_types::systemone::{
    SystemOneQuestion, SystemOneQuestionType, SystemOneRequest, SystemOneResponse,
};
use std::collections::HashMap;
use std::time::Duration;

pub const DEFAULT_DECISION_TIMEOUT_MS: u64 = 100;
pub const MAX_PROMPT_CHARS: usize = 2000;

/// Safely extract prompt text for System One decision evaluation,
/// respecting character boundaries.
pub fn extract_prompt_state(req: &openproxy_types::OpenAIRequest, max_chars: usize) -> String {
    let mut state = String::new();
    for msg in req.messages.iter().rev() {
        let text = msg.extract_text_cow();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if state.is_empty() {
            state.push_str(trimmed);
        } else {
            let combined = format!("{trimmed}\n{state}");
            state = combined;
        }
        if state.chars().count() >= max_chars {
            break;
        }
    }

    if state.len() > max_chars {
        let mut cut = max_chars;
        while cut > 0 && !state.is_char_boundary(cut) {
            cut -= 1;
        }
        state.truncate(cut);
    }
    state
}

/// Apply decision routing to candidate targets if this combo uses `PriorityMode::Decision`.
pub async fn apply_decision_routing(
    ctx: &mut PipelineContext,
    combo: &Combo,
    resolved_targets: &mut Vec<ResolvedTarget>,
) {
    if resolved_targets.len() < 2 {
        return;
    }

    // Collect targets that have descriptions
    let candidates: Vec<(String, String)> = resolved_targets
        .iter()
        .filter_map(|rt| {
            let desc = rt.target.description.as_deref()?.trim();
            if !desc.is_empty() {
                Some((rt.target.id.0.to_string(), desc.to_string()))
            } else {
                None
            }
        })
        .collect();

    // Need at least 2 described targets to formulate a categorical choice
    if candidates.len() < 2 {
        tracing::debug!(
            combo_id = combo.id.0,
            candidates = candidates.len(),
            "decision routing skipped: fewer than 2 targets have descriptions"
        );
        return;
    }

    let state_text = extract_prompt_state(&ctx.req.openai_request, MAX_PROMPT_CHARS);
    if state_text.is_empty() {
        return;
    }

    let mut criteria_map = HashMap::new();
    for (id, desc) in candidates {
        criteria_map.insert(id, desc);
    }

    let question = SystemOneQuestion {
        question_type: SystemOneQuestionType::Choice,
        instructions:
            "Select the most appropriate target ID to answer this query based on complexity, domain specialization, and requirements."
                .to_string(),
        criteria: serde_json::to_value(&criteria_map).ok(),
    };

    let mut questions = std::collections::BTreeMap::new();
    questions.insert("target_selection".to_string(), question);

    let decision_model = combo
        .decision_model
        .clone()
        .unwrap_or_else(|| "jev-latest".to_string());

    let sys_req = SystemOneRequest {
        state: serde_json::Value::String(state_text),
        model: Some(decision_model.clone()),
        questions,
    };

    let timeout_ms = combo
        .decision_timeout_ms
        .unwrap_or(DEFAULT_DECISION_TIMEOUT_MS);

    match execute_system_one_decision(ctx, &decision_model, &sys_req, timeout_ms).await {
        Ok(Some(chosen_target_id_str)) => {
            if let Some(pos) = resolved_targets
                .iter()
                .position(|rt| rt.target.id.0.to_string() == chosen_target_id_str)
            {
                if pos > 0 {
                    let winner = resolved_targets.remove(pos);
                    resolved_targets.insert(0, winner);
                    tracing::info!(
                        combo_id = combo.id.0,
                        chosen_target = %chosen_target_id_str,
                        "decision router promoted winning target to first priority"
                    );
                }
                ctx.combo_walk_log
                    .push(format!("decision_router:winner={chosen_target_id_str}"));
            }
        }
        Ok(None) => {
            tracing::debug!(
                combo_id = combo.id.0,
                "decision router produced no choice answer; keeping default order"
            );
        }
        Err(e) => {
            tracing::warn!(
                combo_id = combo.id.0,
                error = %e,
                "decision router query failed or timed out; safely using default priority order"
            );
        }
    }
}

async fn execute_system_one_decision(
    ctx: &PipelineContext,
    _decision_model: &str,
    req: &SystemOneRequest,
    timeout_ms: u64,
) -> Result<Option<String>, openproxy_types::error::CoreError> {
    let body_bytes = serde_json::to_vec(req)
        .map(bytes::Bytes::from)
        .map_err(|e| openproxy_types::error::CoreError::Validation(e.to_string()))?;

    // Look for an adapter that supports System One
    let adapter = ctx
        .pipeline
        .config
        .adapters
        .iter()
        .find(|a| a.format() == openproxy_types::ProviderFormat::SystemOne)
        .cloned();

    let (url, auth_header) = if let Some(a) = adapter {
        let base_url = a.build_system_one_url();
        let auth = a.build_auth_header("");
        (base_url, auth)
    } else {
        // Fallback to local Laya server
        ("http://localhost:8000/v1/systemone".to_string(), None)
    };

    let mut upstream_req = UpstreamRequest::post_json(&url, body_bytes);
    if let Some((k, v)) = auth_header
        && let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(&v),
        )
    {
        upstream_req.headers.insert(name, val);
    }

    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let fut = ctx.pipeline.config.upstream_client.call(
        upstream_req,
        openproxy_adapters::upstream::TimeoutProfile::Quota,
        cancel,
    );
    let resp = tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| {
            openproxy_types::error::CoreError::Internal(format!(
                "systemone decision timed out after {timeout_ms}ms"
            ))
        })?
        .map_err(|e| openproxy_types::error::CoreError::UpstreamConnection(format!("{url}: {e:?}")))?;

    if resp.status != 200 {
        return Err(openproxy_types::error::CoreError::Internal(format!(
            "systemone upstream status {}",
            resp.status
        )));
    }

    let body_bytes = resp
        .body
        .collect_all()
        .await
        .map_err(|e| openproxy_types::error::CoreError::UpstreamConnection(format!("{url}: {e:?}")))?;

    let parsed: SystemOneResponse = serde_json::from_slice(&body_bytes).map_err(|e| {
        openproxy_types::error::CoreError::Validation(format!(
            "invalid systemone response: {e}"
        ))
    })?;

    let choice = parsed
        .answers
        .get("target_selection")
        .and_then(|a| a.choice.clone());

    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::message::OpenAIMessage;

    #[test]
    fn test_extract_prompt_state_utf8_safe() {
        let req = openproxy_types::OpenAIRequest {
            model: "test".into(),
            messages: vec![
                OpenAIMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String("¡Hola, mundo! 🚀".into())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    extra: Default::default(),
                },
            ],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            top_k: None,
            user: None,
            stop: None,
            tools: None,
            tool_choice: None,
            extra: Default::default(),
        };

        let state = extract_prompt_state(&req, 100);
        assert_eq!(state, "¡Hola, mundo! 🚀");

        // Truncation should not break UTF-8 boundary
        let short = extract_prompt_state(&req, 5);
        assert!(std::str::from_utf8(short.as_bytes()).is_ok());
    }
}
