//! Provider-specific credit metadata parsing and auth-error detection.

#[must_use]
pub fn parse_codebuddy_provider_specific(
    provider_specific: Option<&str>,
) -> (Option<i64>, Option<i64>) {
    let Some(raw) = provider_specific else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (None, None);
    };
    let total = json
        .get("total_credits")
        .and_then(serde_json::Value::as_i64);
    let balance = json
        .get("credit_balance")
        .and_then(serde_json::Value::as_i64);
    (balance, total)
}

#[must_use]
pub fn is_codebuddy_auth_error(json: &serde_json::Value) -> bool {
    let code = json
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            json.get("response")
                .and_then(|r| r.get("code").or_else(|| r.pointer("/data/code")))
                .and_then(serde_json::Value::as_i64)
        });

    if let Some(c) = code
        && matches!(c, 14015 | 11140 | 11142 | 10001)
    {
        return true;
    }

    if let Some(msg) = json
        .get("msg")
        .or_else(|| json.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("token expired")
            || lower.contains("invalid token")
            || lower.contains("unauthorized")
            || lower.contains("refreshtoken is empty")
        {
            return true;
        }
    }

    false
}
