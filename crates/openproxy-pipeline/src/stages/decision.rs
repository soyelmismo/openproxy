//! Decision Routing Stage for combos using System One (Jev / Laya) models.
//!
//! Evaluates incoming prompt complexity and semantic requirements against
//! candidate targets' user-defined descriptions to re-order the targets,
//! placing the optimal model at index 0.

use crate::context::{PipelineContext, ResolvedTarget};
use openproxy_adapters::upstream::UpstreamRequest;
use openproxy_types::combos::Combo;
use openproxy_types::ids::ComboTargetId;
use openproxy_types::systemone::{
    SystemOneQuestion, SystemOneQuestionType, SystemOneRequest, SystemOneResponse,
};
use std::collections::HashMap;
use std::time::Duration;

pub const DEFAULT_DECISION_TIMEOUT_MS: u64 = 100;
pub const MAX_PROMPT_CHARS: usize = 2000;
pub const ELASTIC_HYSTERESIS_MARGIN: f64 = 0.05;
pub const ELASTIC_CONFIDENCE_THRESHOLD: f64 = 0.05;

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

/// Filters candidate targets for Laya decision routing, skipping:
/// - Inactive targets or models
/// - Targets in active cooldown (unless cooldown is disabled on the target/combo)
/// - Targets saturated by predictive rate limit (when predictive skip is active and healthy alternatives exist)
pub fn filter_decision_candidates(
    resolved_targets: &[ResolvedTarget],
    combo: &Combo,
    active_cooldowns: &std::collections::HashSet<ComboTargetId>,
    predictive_limiter: &crate::predictive_rate_limit::PredictiveRateLimiter,
    now_ms: u64,
) -> Vec<(String, String)> {
    let has_any_predictive_healthy = if combo.preventive_rate_limit {
        resolved_targets.iter().any(|rt| {
            if !rt.target.active || !rt.model.active {
                return false;
            }
            if !rt.target.is_cooldown_disabled(combo) && active_cooldowns.contains(&rt.target.id) {
                return false;
            }
            if rt.target.is_cooldown_disabled(combo) {
                return true;
            }
            let key =
                crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&rt.target);
            !predictive_limiter.evaluate_key(key, now_ms).is_saturated()
        })
    } else {
        false
    };

    let mut seen_descriptions = std::collections::HashSet::new();
    resolved_targets
        .iter()
        .filter_map(|rt| {
            // Exclude disabled targets or models
            if !rt.target.active || !rt.model.active {
                tracing::debug!(
                    combo_id = combo.id.0,
                    target_id = rt.target.id.0,
                    "decision routing: candidate excluded (target or model inactive)"
                );
                return None;
            }

            // Exclude targets in active cooldown (unless cooldown is disabled for this target/combo)
            if !rt.target.is_cooldown_disabled(combo) && active_cooldowns.contains(&rt.target.id) {
                tracing::debug!(
                    combo_id = combo.id.0,
                    target_id = rt.target.id.0,
                    "decision routing: candidate excluded (target in active cooldown)"
                );
                return None;
            }

            // Predictive rate limit skip: exclude saturated target if at least one healthy alternative exists
            if combo.preventive_rate_limit
                && !rt.target.is_cooldown_disabled(combo)
                && has_any_predictive_healthy
            {
                let key = crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(
                    &rt.target,
                );
                if predictive_limiter.evaluate_key(key, now_ms).is_saturated() {
                    tracing::info!(
                        combo_id = combo.id.0,
                        target_id = rt.target.id.0,
                        provider = %rt.target.provider_id,
                        account_id = ?rt.target.account_id,
                        model_row_id = ?rt.target.model_row_id,
                        "decision routing: candidate excluded due to predictive rate limit saturation"
                    );
                    return None;
                }
            }

            let desc = rt.target.description.as_deref()?.trim();
            if !desc.is_empty() && seen_descriptions.insert(desc) {
                Some((rt.target.id.0.to_string(), desc.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Apply decision routing to candidate targets if this combo uses `PriorityMode::Decision`.
/// Supports elastic session scaling (upscale/downscale with damping towards `current_pinned`).
pub async fn apply_decision_routing(
    ctx: &mut PipelineContext,
    combo: &Combo,
    resolved_targets: &mut Vec<ResolvedTarget>,
    current_pinned: Option<ComboTargetId>,
) {
    if resolved_targets.len() < 2 {
        return;
    }

    // Query active cooldown targets from DB if cooldowns are enabled
    let active_cooldowns = if combo.is_cooldown_disabled() {
        std::collections::HashSet::new()
    } else {
        let repo = ctx.pipeline.repo();
        let combo_id = combo.id;
        tokio::task::spawn_blocking(move || repo.get_active_cooldown_targets(combo_id))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    combo_id = combo_id.0,
                    "failed to query cooldowns for decision routing: {e}"
                );
                Ok(std::collections::HashSet::new())
            })
            .unwrap_or_default()
    };

    let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();

    // Baseline: if an active, healthy target is pinned, pre-promote it to position 0
    // so that failures/timeouts in JEV preserve the pinned model.
    let pinned_is_available = current_pinned.is_some_and(|pid| {
        resolved_targets.iter().any(|rt| {
            rt.target.id == pid
                && rt.target.active
                && rt.model.active
                && (rt.target.is_cooldown_disabled(combo) || !active_cooldowns.contains(&pid))
        })
    });
    let current_pinned = if pinned_is_available {
        current_pinned
    } else {
        None
    };

    if let Some(pinned_id) = current_pinned
        && let Some(pos) = resolved_targets
            .iter()
            .position(|rt| rt.target.id == pinned_id)
    {
        let min_prio = resolved_targets
            .iter()
            .map(|rt| rt.target.priority_order)
            .min()
            .unwrap_or(1);
        let mut winner = resolved_targets.remove(pos);
        winner.target.priority_order = min_prio.saturating_sub(1);
        resolved_targets.insert(0, winner);
    }

    let candidates = filter_decision_candidates(
        resolved_targets,
        combo,
        &active_cooldowns,
        &ctx.pipeline.predictive_limiter,
        now_ms,
    );

    // Need at least 2 described, eligible targets to formulate a categorical choice
    if candidates.len() < 2 {
        tracing::debug!(
            combo_id = combo.id.0,
            candidates = candidates.len(),
            "decision routing skipped: fewer than 2 eligible targets have descriptions"
        );
        return;
    }

    let state_text = extract_prompt_state(&ctx.req.openai_request, MAX_PROMPT_CHARS);
    if state_text.is_empty() {
        return;
    }

    let mut criteria_map = HashMap::new();
    let mut options_vec = Vec::new();
    for (id, desc) in candidates {
        criteria_map.insert(id.clone(), desc);
        options_vec.push(id);
    }

    let instructions = if let Some(pinned_id) = current_pinned {
        format!(
            "The current session is actively pinned to target ID '{pinned_id}'. \
            Evaluate the recent query requirements: \
            Prefer to keep '{pinned_id}' if it adequately fulfills the query (preserve session continuity and KV-cache). \
            Only select a different target ID if there is a clear necessity to ESCALATE (significantly higher complexity/code/reasoning required) \
            or DOWNSCALE (pure casual conversation with no technical dependency on previous context)."
        )
    } else {
        "Select the most appropriate target ID to answer this query based on complexity, domain specialization, and requirements.".to_string()
    };

    let question = SystemOneQuestion {
        question_type: SystemOneQuestionType::Choice,
        instructions,
        criteria: serde_json::to_value(&criteria_map).ok(),
        options: Some(options_vec),
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
        Ok(Some(answer)) => {
            let Some(chosen_target_id_str) = answer.choice.as_deref() else {
                tracing::debug!(
                    combo_id = combo.id.0,
                    "decision router produced no choice answer; keeping default order"
                );
                return;
            };

            let window_secs = combo
                .selection_window_secs
                .unwrap_or(crate::load_balancing::DEFAULT_SELECTION_WINDOW_SECS);

            // Operational Reputation weighting:
            // Combine semantic model affinity with real-time operational health (success rate, timeouts, latency).
            let effective_choice_str: String = if let Some(ref probs) = answer.probabilities {
                let mut best_target_id = chosen_target_id_str.to_string();
                let mut best_score = -1.0;

                for rt in resolved_targets.iter() {
                    let tid_str = rt.target.id.0.to_string();
                    if let Some(&p_sem) = probs.get(&tid_str) {
                        let rep = ctx
                            .pipeline
                            .selection_registry
                            .reputation_score(rt.target.id, window_secs);
                        let score = p_sem * rep;
                        if score > best_score {
                            best_score = score;
                            best_target_id = tid_str;
                        }
                    }
                }

                if best_target_id != chosen_target_id_str {
                    tracing::info!(
                        combo_id = combo.id.0,
                        laya_choice = %chosen_target_id_str,
                        reputation_winner = %best_target_id,
                        "decision router reputation re-rank: promoted healthy candidate over degraded choice"
                    );
                    ctx.combo_walk_log.push(format!(
                        "decision_router:reputation_rerank={best_target_id}<orig={chosen_target_id_str}"
                    ));
                }

                best_target_id
            } else {
                let chosen_tid =
                    openproxy_types::ids::ComboTargetId(chosen_target_id_str.parse().unwrap_or(0));
                let rep = ctx
                    .pipeline
                    .selection_registry
                    .reputation_score(chosen_tid, window_secs);

                if rep < 0.40 {
                    let healthy_alt = resolved_targets
                        .iter()
                        .find(|rt| {
                            rt.target.id != chosen_tid
                                && ctx
                                    .pipeline
                                    .selection_registry
                                    .reputation_score(rt.target.id, window_secs)
                                    >= 0.70
                        })
                        .map(|rt| rt.target.id.0.to_string());

                    if let Some(alt_id) = healthy_alt {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            degraded_target = %chosen_target_id_str,
                            degraded_reputation = rep,
                            promoted_target = %alt_id,
                            "decision router reputation guard: candidate target degraded; failing over to healthy alternative"
                        );
                        ctx.combo_walk_log.push(format!(
                            "decision_router:reputation_guard_failover={alt_id}<degraded={chosen_target_id_str}"
                        ));
                        alt_id
                    } else {
                        chosen_target_id_str.to_string()
                    }
                } else {
                    chosen_target_id_str.to_string()
                }
            };

            if let Some(pos) = resolved_targets
                .iter()
                .position(|rt| rt.target.id.0.to_string() == effective_choice_str)
            {
                let chosen_target_id = resolved_targets[pos].target.id;
                if current_pinned == Some(chosen_target_id) {
                    tracing::info!(
                        combo_id = combo.id.0,
                        target_id = %effective_choice_str,
                        "decision router confirmed active session target (inertia/keep)"
                    );
                    ctx.combo_walk_log
                        .push(format!("decision_router:keep={effective_choice_str}"));
                } else {
                    // Elastic Hysteresis: if session is already pinned, require significant margin or confidence
                    if let Some(pinned_id) = current_pinned {
                        let should_switch = if let Some(ref probs) = answer.probabilities {
                            let prob_chosen =
                                probs.get(&effective_choice_str).copied().unwrap_or(0.0);
                            let prob_pinned =
                                probs.get(&pinned_id.0.to_string()).copied().unwrap_or(0.0);
                            let margin = prob_chosen - prob_pinned;
                            if margin < ELASTIC_HYSTERESIS_MARGIN {
                                tracing::info!(
                                    combo_id = combo.id.0,
                                    current_pinned = pinned_id.0,
                                    candidate_target = %effective_choice_str,
                                    prob_chosen = prob_chosen,
                                    prob_pinned = prob_pinned,
                                    margin = margin,
                                    threshold = ELASTIC_HYSTERESIS_MARGIN,
                                    "elastic switch rejected by hysteresis damping: margin below threshold; keeping pinned target"
                                );
                                false
                            } else {
                                true
                            }
                        } else if let Some(confidence) = answer.confidence {
                            if confidence < ELASTIC_CONFIDENCE_THRESHOLD {
                                tracing::info!(
                                    combo_id = combo.id.0,
                                    current_pinned = pinned_id.0,
                                    candidate_target = %effective_choice_str,
                                    confidence = confidence,
                                    threshold = ELASTIC_CONFIDENCE_THRESHOLD,
                                    "elastic switch rejected by hysteresis damping: confidence below threshold; keeping pinned target"
                                );
                                false
                            } else {
                                true
                            }
                        } else {
                            true
                        };

                        if !should_switch {
                            ctx.combo_walk_log
                                .push(format!("decision_router:hysteresis_keep={}", pinned_id.0));
                            return;
                        }
                    }

                    // Safety check 1: Context length validation for downscale/switch
                    let total_chars: usize = ctx
                        .req
                        .openai_request
                        .messages
                        .iter()
                        .map(|m| m.extract_text_cow().len())
                        .sum();
                    let est_tokens = (total_chars / 3) as i64;

                    if let Some(ctx_len) = resolved_targets[pos].model.context_length
                        && est_tokens > (ctx_len * 85 / 100)
                    {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            target = %effective_choice_str,
                            est_tokens = est_tokens,
                            context_length = ctx_len,
                            "elastic switch rejected: estimated context tokens exceed target capacity; keeping current target"
                        );
                        return;
                    }

                    let mut winner = resolved_targets.remove(pos);
                    let min_prio = resolved_targets
                        .iter()
                        .map(|rt| rt.target.priority_order)
                        .min()
                        .unwrap_or(1);
                    winner.target.priority_order = min_prio.saturating_sub(1);
                    resolved_targets.insert(0, winner);

                    tracing::info!(
                        combo_id = combo.id.0,
                        from_target = ?current_pinned.map(|t| t.0),
                        to_target = %effective_choice_str,
                        "decision router elastic switch: updated session target"
                    );
                    ctx.combo_walk_log.push(format!(
                        "decision_router:elastic_switch={effective_choice_str}"
                    ));
                }
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
    decision_model: &str,
    req: &SystemOneRequest,
    timeout_ms: u64,
) -> Result<Option<openproxy_types::systemone::SystemOneAnswer>, openproxy_types::error::CoreError>
{
    let conn_arc = std::sync::Arc::clone(&ctx.pipeline.conn);
    let decision_model_owned = decision_model.to_string();
    let (resolved_prov, upstream_model) = tokio::task::spawn_blocking(move || {
        let conn = conn_arc.lock();
        openproxy_db::models::resolve_model_identity(&conn, &decision_model_owned)
            .unwrap_or((None, decision_model_owned))
    })
    .await
    .unwrap_or((None, decision_model.to_string()));

    #[cfg(feature = "laya-engine")]
    {
        let is_laya = resolved_prov.as_deref() == Some("laya")
            || decision_model.to_ascii_lowercase().contains("laya");

        if is_laya && openproxy_adapters::laya_engine::is_available() {
            let req_clone = req.clone();
            let res = tokio::task::spawn_blocking(move || {
                openproxy_adapters::laya_engine::execute_decision(&req_clone)
            })
            .await;

            match res {
                Ok(Ok(mut resp)) => {
                    let answer = resp
                        .answers
                        .remove("action")
                        .or_else(|| resp.answers.into_values().next());
                    return Ok(answer);
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Laya in-process inference failed, falling back to HTTP");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Laya spawn_blocking join error, falling back to HTTP");
                }
            }
        }
    }

    // Look for an adapter that matches the resolved provider or model
    let adapter = ctx
        .pipeline
        .config
        .adapters
        .iter()
        .find(|a| {
            if let Some(ref p) = resolved_prov {
                a.id() == p || a.config().id == *p
            } else {
                a.id().as_str() == decision_model
            }
        })
        .or_else(|| {
            ctx.pipeline
                .config
                .adapters
                .iter()
                .find(|a| a.format() == openproxy_types::ProviderFormat::SystemOne)
        })
        .cloned();

    let (url, auth_header, body_bytes) = if let Some(a) = adapter {
        let base_url = if a.format() == openproxy_types::ProviderFormat::SystemOne {
            a.build_system_one_url()
        } else {
            let b = a.config().base_url.trim_end_matches('/');
            format!("{b}/systemone")
        };
        let conn_arc = std::sync::Arc::clone(&ctx.pipeline.conn);
        let master_key = std::sync::Arc::clone(&ctx.pipeline.config.master_key);
        let prov_id = a.id().clone();
        let api_key = tokio::task::spawn_blocking(move || {
            let conn = conn_arc.lock();
            openproxy_db::accounts::list_api_keys_for_provider(&conn, &prov_id, &master_key)
                .ok()
                .and_then(|keys| keys.into_iter().next())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        let auth = a.build_auth_header(&api_key);
        let formatted = a.format_system_one_request(req, &upstream_model)?;
        (base_url, auth, formatted)
    } else {
        // Fallback to local Laya server
        let raw = serde_json::to_vec(req)
            .map(bytes::Bytes::from)
            .map_err(|e| openproxy_types::error::CoreError::Validation(e.to_string()))?;
        ("http://localhost:8770/v1/systemone".to_string(), None, raw)
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
        .map_err(|e| {
            openproxy_types::error::CoreError::UpstreamConnection(format!("{url}: {e:?}"))
        })?;

    if resp.status != 200 {
        return Err(openproxy_types::error::CoreError::Internal(format!(
            "systemone upstream status {}",
            resp.status
        )));
    }

    let body_bytes = resp.body.collect_all().await.map_err(|e| {
        openproxy_types::error::CoreError::UpstreamConnection(format!("{url}: {e:?}"))
    })?;

    let mut parsed: SystemOneResponse = serde_json::from_slice(&body_bytes).map_err(|e| {
        openproxy_types::error::CoreError::Validation(format!("invalid systemone response: {e}"))
    })?;

    let answer = parsed.answers.remove("target_selection");
    Ok(answer)
}

#[cfg(test)]
mod tests;
