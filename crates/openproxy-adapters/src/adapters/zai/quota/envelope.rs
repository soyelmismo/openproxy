//! Strict envelope validation. Upstream bodies never become diagnostic text.
use serde_json::Value;

pub(crate) fn as_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn envelope_is_success(json: &Value) -> bool {
    if !json.is_object() {
        return false;
    }
    if let Some(code) = json.get("code")
        && !matches!(as_i64(Some(code)), Some(0 | 200))
    {
        return false;
    }
    if let Some(success) = json.get("success")
        && success.as_bool() != Some(true)
    {
        return false;
    }
    if let Some(status) = json.get("status") {
        let accepted = matches!(as_i64(Some(status)), Some(0 | 200))
            || status
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("success") || s.eq_ignore_ascii_case("ok"));
        if !accepted {
            return false;
        }
    }
    json.get("error").is_none_or(Value::is_null)
}

pub(crate) fn normalize_epoch(raw: i64) -> Option<i64> {
    if raw <= 0 {
        return None;
    }
    Some(if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rejects_every_failed_or_malformed_envelope_signal() {
        for value in [
            json!({"code":500}),
            json!({"code":"invalid"}),
            json!({"success":"true"}),
            json!({"code":0,"success":false}),
            json!({"status":"error"}),
            json!({"status":401}),
            json!({"error":{"message":"do not echo me"}}),
            json!([]),
        ] {
            assert!(!envelope_is_success(&value));
        }
        for value in [
            json!({"code":"0","data":[]}),
            json!({"code":200,"success":true}),
            json!({"status":"success"}),
            json!({"data":{}}),
        ] {
            assert!(envelope_is_success(&value));
        }
    }
    #[test]
    fn validates_numeric_scalars_and_epoch_units() {
        assert_eq!(as_i64(Some(&json!("25718"))), Some(25718));
        assert_eq!(as_i64(Some(&json!(1.5))), None);
        assert_eq!(as_i64(Some(&json!(u64::MAX))), None);
        assert_eq!(normalize_epoch(1791766800000), Some(1791766800));
        assert_eq!(normalize_epoch(0), None);
    }
}
