//! System-prefix resolution for dispatch: reads the active target system
//! prefixes (inheritance from parent sub-combo targets, the System One
//! decision router). Split from `target.rs` to keep each stage file under
//! the LOC invariant.

use crate::context::PipelineContext;
/// Resolves which system prefix to inject for the chosen target.
/// - Checks `target_id` first; if empty and `inherited_parent_id` is present, checks the parent target.
/// - 0 active prefixes: returns None (no prefix injected).
/// - 1 active prefix: returns Some(system_text) directly.
/// - Two or more active prefixes: runs System One decision router (Laya/Jev) evaluating
///   each prefix's `criteria` against the user's prompt to select the best persona.
pub(super) async fn resolve_system_prefix_for_target(
    ctx: &PipelineContext,
    target_id: openproxy_types::ids::ComboTargetId,
    inherited_parent_id: Option<openproxy_types::ids::ComboTargetId>,
) -> Option<String> {
    let mut prefixes = ctx
        .pipeline
        .read_db(move |conn| {
            openproxy_db::combos::list_active_target_system_prefixes(conn, target_id)
        })
        .await
        .ok()?;

    let effective_target_id = if prefixes.is_empty()
        && let Some(parent_id) = inherited_parent_id
    {
        let parent_prefixes = ctx
            .pipeline
            .read_db(move |conn| {
                openproxy_db::combos::list_active_target_system_prefixes(conn, parent_id)
            })
            .await
            .ok()?;
        if !parent_prefixes.is_empty() {
            tracing::debug!(
                target_id = target_id.0,
                parent_target_id = parent_id.0,
                count = parent_prefixes.len(),
                "inheriting system prefixes from parent sub-combo target"
            );
            prefixes = parent_prefixes;
            parent_id
        } else {
            target_id
        }
    } else {
        target_id
    };

    if prefixes.is_empty() {
        return None;
    }

    if prefixes.len() == 1 {
        tracing::debug!(
            target_id = effective_target_id.0,
            prefix_name = %prefixes[0].name,
            "single active system prefix; injecting directly"
        );
        return Some(prefixes[0].system_text.clone());
    }

    // >= 2 active prefixes: evaluate semantic criteria with System One decision model
    let combo = ctx.combo.as_ref()?;
    let decision_model = combo
        .decision_model
        .clone()
        .unwrap_or_else(|| "jev-latest".to_string());

    let state_text = crate::stages::decision::extract_prompt_state(
        &ctx.req.openai_request,
        crate::stages::decision::MAX_PROMPT_CHARS,
    );

    let mut criteria_map = std::collections::HashMap::new();
    let mut options_vec = Vec::new();
    for p in &prefixes {
        let opt_id = p.id.to_string();
        criteria_map.insert(opt_id.clone(), p.criteria.clone());
        options_vec.push(opt_id);
    }

    let question = openproxy_types::systemone::SystemOneQuestion {
        question_type: openproxy_types::systemone::SystemOneQuestionType::Choice,
        instructions: "Select the most appropriate persona/system prefix ID to handle this user query according to domain, tone, and requirements.".to_string(),
        criteria: serde_json::to_value(&criteria_map).ok(),
        options: Some(options_vec),
    };

    let mut questions = std::collections::BTreeMap::new();
    questions.insert("prefix_selection".to_string(), question);

    let sys_req = openproxy_types::systemone::SystemOneRequest {
        state: serde_json::Value::String(state_text),
        model: Some(decision_model.clone()),
        questions,
    };

    let timeout_ms = combo
        .decision_timeout_ms
        .unwrap_or(crate::stages::decision::DEFAULT_DECISION_TIMEOUT_MS);

    match crate::stages::decision::execute_system_one_decision(
        ctx,
        &decision_model,
        &sys_req,
        timeout_ms,
    )
    .await
    {
        Ok(Some(answer)) => {
            if let Some(ref choice_str) = answer.choice
                && let Ok(chosen_id) = choice_str.parse::<i64>()
                && let Some(found) = prefixes.iter().find(|p| p.id == chosen_id)
            {
                tracing::info!(
                    target_id = target_id.0,
                    chosen_prefix_id = chosen_id,
                    chosen_prefix_name = %found.name,
                    "semantic system prefix router selected persona"
                );
                return Some(found.system_text.clone());
            }
            // Fallback: choose highest priority / first
            Some(prefixes[0].system_text.clone())
        }
        Ok(None) => {
            tracing::debug!(
                target_id = target_id.0,
                "system prefix decision router returned no answer; defaulting to first prefix"
            );
            Some(prefixes[0].system_text.clone())
        }
        Err(e) => {
            tracing::warn!(
                target_id = target_id.0,
                error = %e,
                "system prefix decision router failed; defaulting to first prefix"
            );
            Some(prefixes[0].system_text.clone())
        }
    }
}

pub(crate) fn prepare_messages_for_formatting(
    ctx: &PipelineContext,
) -> &[openproxy_types::OpenAIMessage] {
    let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
        let pii_enabled = ctx.pipeline.config.pii_config.pii_enabled;
        let compression_needed = openproxy_compression::would_compress(
            &ctx.req.openai_request.messages,
            ctx.pipeline.config.compression_mode,
        );
        // Canonicalise media unconditionally. Every target formatter reads one
        // vocabulary, so a part that keeps the dialect it arrived in (an
        // `input_image` from a Responses client, an Anthropic `document`) is
        // dropped by whichever formatter does not know that spelling. Doing it
        // here — before the formatter is even selected — is what makes the
        // translation matrix work for every client/target pair instead of only
        // for the OpenAI target, whose formatter happens to normalise.
        let media_needs_canonicalisation = ctx
            .req
            .openai_request
            .messages
            .iter()
            .any(|m| matches!(&m.content, Some(serde_json::Value::Array(_))));

        if !pii_enabled && !compression_needed && !media_needs_canonicalisation {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
            return None;
        }

        let mut msgs = ctx.req.openai_request.messages.clone();

        if media_needs_canonicalisation {
            for msg in &mut msgs {
                if let Some(serde_json::Value::Array(parts)) = &msg.content {
                    let canonical = crate::formatting::media::canonical_media_parts(parts);
                    msg.content = Some(if canonical.is_empty() {
                        serde_json::Value::String(msg.extract_text())
                    } else {
                        serde_json::Value::Array(canonical)
                    });
                }
            }
        }

        if pii_enabled {
            let engine = crate::pii::PiiEngine::from_config(&ctx.pipeline.config.pii_config);
            let mut session =
                crate::pii::PiiSession::new(ctx.pipeline.config.pii_config.pii_reversible);
            let redacted = engine.redact_messages(&msgs, &mut session);
            msgs = redacted;
            *ctx.req.pii_session.lock() = Some(session);
        }

        if compression_needed {
            let stats = openproxy_compression::apply_compression(
                &mut msgs,
                ctx.pipeline.config.compression_mode,
            );
            *ctx.req.compression_stats.lock() = Some(stats);
        } else {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
        }

        Some(msgs)
    });

    cloned_messages_ref
        .as_deref()
        .unwrap_or(&ctx.req.openai_request.messages)
}
