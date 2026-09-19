pub const LOAD_CODE_ASSIST_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub const ONBOARD_USER_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";

/// `countTokens` upstream proxy URL.
pub const COUNT_TOKENS_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:countTokens";

/// Parse the `:countTokens` response body and extract `totalTokens`.
pub fn parse_total_tokens(body: &serde_json::Value) -> Option<i64> {
    body.get("response")
        .and_then(|r| r.get("totalTokens"))
        .or_else(|| body.get("totalTokens"))
        .and_then(|v| v.as_i64())
}

/// Call `v1internal:countTokens` upstream and return the exact token count.
pub async fn count_tokens(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    access_token: &str,
    body: &serde_json::Value,
) -> std::result::Result<i64, String> {
    let wrapped = serde_json::json!({ "request": body });
    let body_bytes = crate::antigravity_headers::oauth_post_json(
        upstream,
        COUNT_TOKENS_URL,
        &wrapped,
        access_token,
        crate::upstream::TimeoutProfile::Chat,
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("{COUNT_TOKENS_URL} parse: {e}"))?;
    parse_total_tokens(&value).ok_or_else(|| format!("{COUNT_TOKENS_URL}: missing totalTokens"))
}

/// Call `loadCodeAssist` and extract `projectId` (or `None` when
/// the user is not yet on-boarded).
pub async fn load_code_assist(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    access_token: &str,
    metadata: &serde_json::Value,
) -> std::result::Result<Option<String>, String> {
    let body = serde_json::json!({ "metadata": metadata });
    let body_bytes = crate::antigravity_headers::oauth_post_json(
        upstream,
        LOAD_CODE_ASSIST_URL,
        &body,
        access_token,
        crate::upstream::TimeoutProfile::OAuth,
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("{LOAD_CODE_ASSIST_URL} parse: {e}"))?;
    Ok(value
        .get("cloudaicompanionProject")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .or_else(|| {
            value
                .get("cloudaicompanionProject")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
        }))
}

/// Call `onboardUser` and return `Ok(Some(project_id))` on success,
/// or `Ok(None)` when the server has not finished onboarding yet.
pub async fn onboard_user(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    access_token: &str,
    project_id: &str,
    metadata: &serde_json::Value,
) -> std::result::Result<Option<String>, String> {
    let body = serde_json::json!({
        "projectId": project_id,
        "metadata": metadata,
        "tier": "free-tier",
    });
    let body_bytes = crate::antigravity_headers::oauth_post_json(
        upstream,
        ONBOARD_USER_URL,
        &body,
        access_token,
        crate::upstream::TimeoutProfile::OAuth,
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("{ONBOARD_USER_URL} parse: {e}"))?;
    Ok(value
        .get("cloudaicompanionProject")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| value.get("projectId").and_then(|v| v.as_str()))
        .map(std::string::ToString::to_string))
}

pub const SENTINEL_SIGNATURE: &str = "skip_thought_signature_validator";

pub(crate) fn is_real_signature(sig: &str) -> bool {
    sig.len() >= 50 && sig != SENTINEL_SIGNATURE
}

/// Models that require thinking / thought signatures on Google Cloud Code (Antigravity).
/// Aligned 1:1 with Antigravity-Manager `model_forces_server_thinking`.
pub(crate) fn should_inject_thought_signatures(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.is_empty()
        || m.contains("image")
        || m.contains("imagen")
        || m.contains("embed")
        || m.contains("lite")
    {
        return false;
    }
    m.contains("claude")
        || m.contains("gemini")
        || m.contains("flash")
        || m.contains("pro")
        || m.contains("agent")
        || m.contains("thinking")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("deepseek")
}

pub(crate) fn inject_sentinel_thought_signatures(contents: &mut serde_json::Value, model: &str) {
    let is_thinking_enabled = should_inject_thought_signatures(model);
    let Some(arr) = contents.as_array_mut() else {
        return;
    };

    for msg in arr {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let is_model = role == "model" || role == "assistant";

        let Some(parts) = msg.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };

        if !is_thinking_enabled {
            let mut cleaned_parts = Vec::with_capacity(parts.len());
            for mut part in parts.drain(..) {
                if let Some(obj) = part.as_object_mut() {
                    obj.remove("thought_signature");
                    obj.remove("thoughtSignature");
                }
                let is_thought = part
                    .get("thought")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || (part.get("thoughtSignature").is_some()
                        && part.get("functionCall").is_none()
                        && part.get("functionResponse").is_none());
                if is_thought {
                    let text = part
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() && text != "..." && text != "·" {
                        cleaned_parts.push(serde_json::json!({ "text": text }));
                    }
                } else {
                    cleaned_parts.push(part);
                }
            }
            *parts = cleaned_parts;
            continue;
        }

        // Clean snake_case thought_signature and normalize to camelCase thoughtSignature
        for part in parts.iter_mut() {
            if let Some(obj) = part.as_object_mut()
                && let Some(sig) = obj.remove("thought_signature")
                && !obj.contains_key("thoughtSignature")
            {
                obj.insert("thoughtSignature".to_string(), sig);
            }
        }

        if !is_model {
            continue;
        }

        let mut thinking_parts = Vec::new();
        let mut other_parts = Vec::new();

        for part in parts.drain(..) {
            let is_thought = part
                .get("thought")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || (part.get("thoughtSignature").is_some()
                    && part.get("functionCall").is_none()
                    && part.get("functionResponse").is_none());
            if is_thought {
                thinking_parts.push(part);
            } else {
                other_parts.push(part);
            }
        }

        let turn_real_sig = other_parts.iter().find_map(|p| {
            if p.get("functionCall").is_some() || p.get("function_call").is_some() {
                p.get("thoughtSignature")
                    .and_then(|s| s.as_str())
                    .filter(|s| is_real_signature(s))
                    .map(str::to_string)
            } else {
                None
            }
        });

        if thinking_parts.is_empty() {
            let has_fc = other_parts
                .iter()
                .any(|p| p.get("functionCall").is_some() || p.get("function_call").is_some());
            if has_fc {
                let sig = turn_real_sig.as_deref().unwrap_or(SENTINEL_SIGNATURE);
                thinking_parts.push(serde_json::json!({
                    "text": "...",
                    "thought": true,
                    "thoughtSignature": sig,
                }));
            }
        } else if let Some(ref real_sig) = turn_real_sig {
            for tp in &mut thinking_parts {
                let valid = tp
                    .get("thoughtSignature")
                    .and_then(|s| s.as_str())
                    .is_some_and(is_real_signature);
                if !valid {
                    tp["thoughtSignature"] = serde_json::json!(real_sig);
                }
            }
        } else {
            for tp in &mut thinking_parts {
                if tp.get("thoughtSignature").is_none() {
                    tp["thoughtSignature"] = serde_json::json!(SENTINEL_SIGNATURE);
                }
            }
        }

        for part in &mut other_parts {
            let has_fc = part.get("functionCall").is_some() || part.get("function_call").is_some();
            if has_fc && part.get("thoughtSignature").is_none() {
                part["thoughtSignature"] = serde_json::json!(SENTINEL_SIGNATURE);
            }
        }

        parts.extend(thinking_parts);
        parts.extend(other_parts);
    }
}
