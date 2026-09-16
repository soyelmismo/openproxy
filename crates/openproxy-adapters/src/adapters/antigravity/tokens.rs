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

fn patch_part_thought_signature(part: &mut serde_json::Value) {
    let has_fc = part.get("functionCall").is_some() || part.get("function_call").is_some();
    if has_fc
        && part.get("thoughtSignature").is_none()
        && part.get("thought_signature").is_none()
        && let Some(obj) = part.as_object_mut()
    {
        obj.insert(
            "thoughtSignature".to_string(),
            serde_json::json!("skip_thought_signature_validator"),
        );
        obj.insert(
            "thought_signature".to_string(),
            serde_json::json!("skip_thought_signature_validator"),
        );
    }
}

pub(crate) fn inject_sentinel_thought_signatures(contents: &mut serde_json::Value, model: &str) {
    let bytes = model.as_bytes();
    let contains = |needle: &str| -> bool {
        bytes
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    };
    let is_flash_or_agent = (contains("gemini") && contains("flash"))
        || contains("gemini-pro-agent")
        || contains("gemini-3-flash-agent");
    if !is_flash_or_agent {
        return;
    }
    if let Some(arr) = contents.as_array_mut() {
        for msg in arr {
            if let Some(parts) = msg.get_mut("parts").and_then(|p| p.as_array_mut()) {
                for part in parts {
                    patch_part_thought_signature(part);
                }
            }
        }
    }
}
