use super::cloaking::{
    BETA_CLAUDE_CODE, BETA_EFFORT, BETA_EXTENDED_CACHE_TTL, BETA_FALLBACKS,
    BETA_INTERLEAVED_THINKING, BETA_MID_CONV_SYSTEM, BETA_MID_CONV_TOOL, BETA_OAUTH,
    BETA_PER_TURN_CONTROL, BETA_REDACT_THINKING, BETA_STRUCTURED_OUTPUTS,
    BETA_THINKING_DISPLAY_UPDATES, extended_cache_ttl_active, model_matches, normalize_model,
    structured_outputs_active, supports_adaptive_thinking, supports_effort, thinking_active,
    thinking_display_updates_active,
};

/// Assembles Anthropic-Beta headers dynamically, starting from the captured
/// profile base list and layering the model-conditional capabilities on top.
///
/// The base list comes from `profile.betas`, so a newer captured CLI that
/// reports a different common set is respected without a code change.
pub fn build_claude_code_betas(model_id: &str, has_fallbacks: bool, is_oauth: bool) -> String {
    build_claude_code_betas_from_profile(
        model_id,
        has_fallbacks,
        is_oauth,
        &crate::spoofer::claude_code_profile::current_profile(),
    )
}

/// La misma lista de betas anclada a un perfil concreto: el snapshot pinneado
/// de esta petición, de modo que los headers y el body no puedan discrepar
/// aunque el runtime global refresque en paralelo.
pub fn build_claude_code_betas_from_profile(
    model_id: &str,
    has_fallbacks: bool,
    is_oauth: bool,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) -> String {
    let model = normalize_model(model_id);

    let mut betas: Vec<String> = profile
        .betas
        .iter()
        .filter(|beta| {
            !beta.is_empty()
                && ![
                    BETA_OAUTH,
                    BETA_MID_CONV_SYSTEM,
                    BETA_MID_CONV_TOOL,
                    BETA_PER_TURN_CONTROL,
                    BETA_EFFORT,
                    BETA_FALLBACKS[0],
                    BETA_FALLBACKS[1],
                    BETA_EXTENDED_CACHE_TTL,
                    BETA_REDACT_THINKING,
                    BETA_STRUCTURED_OUTPUTS,
                ]
                .contains(&beta.as_str())
        })
        .cloned()
        .collect();

    if is_oauth && !betas.iter().any(|b| b == BETA_OAUTH) {
        let insert_at = betas
            .iter()
            .position(|b| b == BETA_CLAUDE_CODE)
            .map_or(0, |idx| idx + 1);
        betas.insert(insert_at, BETA_OAUTH.to_string());
    }

    let is_legacy = !supports_adaptive_thinking(&model) || model.contains("sonnet-4-");

    if !is_legacy && !betas.iter().any(|b| b == BETA_MID_CONV_SYSTEM) {
        betas.push(BETA_MID_CONV_SYSTEM.to_string());
    }
    if !is_legacy {
        if (model_matches(&model, "opus-5-5")
            || model_matches(&model, "fable-5-1")
            || model_matches(&model, "fable-5-2")
            || model_matches(&model, "opus-5-4"))
            && !betas.iter().any(|b| b == BETA_PER_TURN_CONTROL)
        {
            betas.push(BETA_PER_TURN_CONTROL.to_string());
        }
        if !model.contains("sonnet-5") && !betas.iter().any(|b| b == BETA_MID_CONV_TOOL) {
            betas.push(BETA_MID_CONV_TOOL.to_string());
        }
    }

    // The effort beta describes `output_config.effort`, which the same
    // model gate controls, so the two can never disagree.
    if supports_effort(&model) && !betas.iter().any(|b| b == BETA_EFFORT) {
        betas.push(BETA_EFFORT.to_string());
    }

    for beta in BETA_FALLBACKS {
        if has_fallbacks && !betas.iter().any(|b| b == beta) {
            betas.push(beta.to_string());
        }
    }

    betas.join(",")
}

/// Derives the Anthropic-Beta capability set from the FINAL body (post strip),
/// so body-dependent capabilities stay coherent with the bytes actually sent
/// while the model-conditional base set still comes from the captured profile.
pub fn build_claude_code_betas_for_body(
    model_id: &str,
    json: &serde_json::Value,
    is_oauth: bool,
) -> String {
    build_claude_code_betas_for_body_from_profile(
        model_id,
        json,
        is_oauth,
        &crate::spoofer::claude_code_profile::current_profile(),
    )
}

/// La misma derivación anclada al perfil pinneado de la petición.
pub fn build_claude_code_betas_for_body_from_profile(
    model_id: &str,
    json: &serde_json::Value,
    is_oauth: bool,
    profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
) -> String {
    let display_updates = thinking_display_updates_active(json, model_id);
    let extended_ttl = extended_cache_ttl_active(json);
    let structured = structured_outputs_active(json);
    let has_thinking = thinking_active(json);
    let has_fallbacks = json
        .get("fallbacks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|fallbacks| !fallbacks.is_empty());

    let mut betas: Vec<String> =
        build_claude_code_betas_from_profile(model_id, has_fallbacks, is_oauth, profile)
            .split(',')
            .map(str::trim)
            .filter(|beta| !beta.is_empty())
            .map(ToString::to_string)
            .collect();

    // `thinking-display-updates-2026-08-18` (display updates) is only emitted when the resolved
    // display is `updates`.
    if display_updates {
        if !betas.iter().any(|b| b == BETA_THINKING_DISPLAY_UPDATES) {
            betas.push(BETA_THINKING_DISPLAY_UPDATES.to_string());
        }
    } else {
        betas.retain(|beta| beta != BETA_THINKING_DISPLAY_UPDATES);
    }

    // `redact-thinking` is the default when thinking is off, and is dropped as
    // soon as thinking is active (native removes `Y8t` on `Kc && R_`).
    if has_thinking {
        betas.retain(|beta| beta != BETA_REDACT_THINKING);
    } else if !betas.iter().any(|b| b == BETA_REDACT_THINKING) {
        let insert_at = betas
            .iter()
            .position(|b| b == BETA_INTERLEAVED_THINKING)
            .map_or(betas.len(), |idx| idx + 1);
        betas.insert(insert_at, BETA_REDACT_THINKING.to_string());
    }

    // Structured output requests advertise the structured-outputs beta.
    if structured && !betas.iter().any(|b| b == BETA_STRUCTURED_OUTPUTS) {
        betas.push(BETA_STRUCTURED_OUTPUTS.to_string());
    }

    // The 1h cache TTL beta follows directly from a body TTL of 1h.
    if !betas.iter().any(|b| b == BETA_EXTENDED_CACHE_TTL) && extended_ttl {
        betas.push(BETA_EXTENDED_CACHE_TTL.to_string());
    } else if !extended_ttl {
        betas.retain(|beta| beta != BETA_EXTENDED_CACHE_TTL);
    }

    if json
        .get("output_config")
        .and_then(|output| output.get("effort"))
        .is_none()
    {
        betas.retain(|beta| beta != BETA_EFFORT);
    }
    betas.join(",")
}

/// Extracts existing caller version, cc_prev_req and cc_prompt_id from an incoming billing header if present.
pub fn extract_claude_billing_tags(
    json: &serde_json::Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let system = json.get("system");
    let text = if let Some(s) = system.and_then(|v| v.as_str()) {
        s
    } else if let Some(arr) = system.and_then(|v| v.as_array()) {
        arr.first()
            .and_then(|v| v.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
    } else {
        ""
    };

    if !text.starts_with("x-anthropic-billing-header:") {
        return (None, None, None);
    }

    let caller_version = text.split("cc_version=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        let base_ver = val.split('.').take(3).collect::<Vec<_>>().join(".");
        if base_ver.starts_with("2.1.") {
            Some(base_ver)
        } else {
            None
        }
    });

    let prev_req = text.split("cc_prev_req=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        if val.starts_with("req_") {
            Some(val.to_string())
        } else {
            None
        }
    });

    let prompt_id = text.split("cc_prompt_id=").nth(1).and_then(|s| {
        let val = s.split(';').next()?.trim();
        if !val.is_empty() {
            Some(val.to_string())
        } else {
            None
        }
    });

    (caller_version, prev_req, prompt_id)
}
