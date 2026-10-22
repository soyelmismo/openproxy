use openproxy_types::{CoreError, UpdateField};
use serde_json::Value;

use crate::error::ApiError;

/// Parse an optional tri-state field using a custom parser for the inner value.
///
/// - Key absent: `Ok(UpdateField::Ignore)`
/// - Key null: `Ok(UpdateField::Reset)`
/// - Key present: calls `f(value)` which should return `Ok(T)` or `Err(ApiError)`
pub fn parse_tristate_with<'a, T>(
    body: &'a Value,
    field: &str,
    f: impl FnOnce(&'a Value) -> Result<T, ApiError>,
) -> Result<UpdateField<T>, ApiError> {
    match body.get(field) {
        None => Ok(UpdateField::Ignore),
        Some(v) if v.is_null() => Ok(UpdateField::Reset),
        Some(v) => f(v).map(UpdateField::Set),
    }
}

/// Parse a nullable string field into `UpdateField<&'a str>`.
pub fn parse_nullable_str<'a>(
    body: &'a Value,
    field: &str,
) -> Result<UpdateField<&'a str>, ApiError> {
    parse_tristate_with(body, field, |v| {
        v.as_str().ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "{field} must be a string or null, got {v}"
            )))
        })
    })
}

/// Parse a nullable non-negative integer into `UpdateField<u64>`.
pub fn parse_nullable_u64(body: &Value, field: &str) -> Result<UpdateField<u64>, ApiError> {
    parse_tristate_with(body, field, |v| {
        v.as_u64().ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "{field} must be a non-negative integer or null"
            )))
        })
    })
}

/// Parse a nullable integer into `UpdateField<i64>`.
pub fn parse_nullable_i64(body: &Value, field: &str) -> Result<UpdateField<i64>, ApiError> {
    parse_tristate_with(body, field, |v| {
        v.as_i64().ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "{field} must be null or an integer"
            )))
        })
    })
}

/// Parse a nullable float into `UpdateField<f64>`.
pub fn parse_nullable_f64(body: &Value, field: &str) -> Result<UpdateField<f64>, ApiError> {
    parse_tristate_with(body, field, |v| {
        v.as_f64().ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "{field} must be a number in [0.0, 1.0] or null"
            )))
        })
    })
}

/// Parse an optional integer property bounded to `i32` range.
///
/// - Field absent: `Ok(None)`
/// - Field present: must be integer and within `min..=max`.
pub fn parse_bounded_i32(
    body: &Value,
    field: &str,
    min: i64,
    max: i64,
    range_err_msg: impl FnOnce(i64) -> String,
) -> Result<Option<i32>, ApiError> {
    let Some(v) = body.get(field) else {
        return Ok(None);
    };
    let val_i64 = v.as_i64().ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "{field} must be an integer when present"
        )))
    })?;
    if !(min..=max).contains(&val_i64) {
        return Err(ApiError(CoreError::Validation(range_err_msg(val_i64))));
    }
    Ok(Some(val_i64 as i32))
}

/// Parse an optional boolean property.
///
/// - Field absent: `Ok(None)`
/// - Field present: must be a bool.
pub fn parse_optional_bool(body: &Value, field: &str) -> Result<Option<bool>, ApiError> {
    let Some(v) = body.get(field) else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "{field} must be a boolean when present"
        )))
    })
}

pub fn parse_target_priority_order(body: &Value) -> Result<Option<i32>, ApiError> {
    parse_bounded_i32(
        body,
        "priority_order",
        i64::from(i32::MIN),
        i64::from(i32::MAX),
        |p| format!("priority_order out of i32 range: {p}"),
    )
}

pub fn parse_target_weight(body: &Value) -> Result<Option<i32>, ApiError> {
    parse_bounded_i32(
        body,
        "weight",
        1,
        i64::from(i32::MAX),
        |w| format!("weight must be a positive i32 (1..={}), got {}", i32::MAX, w),
    )
}

pub fn parse_target_active(body: &Value) -> Result<Option<bool>, ApiError> {
    parse_optional_bool(body, "active")
}

pub struct ComboTargetUpdates<'a> {
    pub priority_order: Option<i32>,
    pub weight: Option<i32>,
    pub active: Option<bool>,
    pub cooldown_mode: UpdateField<&'a str>,
    pub cooldown_base_secs: UpdateField<u64>,
    pub cooldown_max_secs: UpdateField<u64>,
    pub cooldown_factor: UpdateField<u32>,
    pub thinking_effort: UpdateField<&'a str>,
    pub description: UpdateField<&'a str>,
    pub selected_accounts: UpdateField<Option<String>>,
}

impl ComboTargetUpdates<'_> {
    pub fn is_empty(&self) -> bool {
        self.priority_order.is_none()
            && self.weight.is_none()
            && self.active.is_none()
            && self.cooldown_mode.is_ignore()
            && self.cooldown_base_secs.is_ignore()
            && self.cooldown_max_secs.is_ignore()
            && self.cooldown_factor.is_ignore()
            && self.thinking_effort.is_ignore()
            && self.description.is_ignore()
            && self.selected_accounts.is_ignore()
    }
}

pub fn parse_combo_target_updates(
    body: &Value,
) -> Result<ComboTargetUpdates<'_>, ApiError> {
    let priority_order = parse_target_priority_order(body)?;
    let weight = parse_target_weight(body)?;
    let active = parse_target_active(body)?;
    let cooldown_mode = parse_nullable_str(body, "cooldown_mode")?;
    let cooldown_base_secs = parse_nullable_u64(body, "cooldown_base_secs")?;
    let cooldown_max_secs = parse_nullable_u64(body, "cooldown_max_secs")?;
    let cooldown_factor = parse_nullable_u64(body, "cooldown_factor")?.map(|f| f as u32);
    let thinking_effort = parse_nullable_str(body, "thinking_effort")?;
    let description = parse_nullable_str(body, "description")?;

    let selected_accounts = match body.get("selected_accounts") {
        None => UpdateField::Ignore,
        Some(Value::Null) => UpdateField::Reset,
        Some(Value::Array(arr)) => {
            let ids: Result<Vec<i64>, _> = arr
                .iter()
                .map(|v| {
                    v.as_i64().ok_or_else(|| {
                        ApiError(CoreError::Validation(
                            "selected_accounts must contain integers".into(),
                        ))
                    })
                })
                .collect();
            let ids = ids?;
            let json = serde_json::to_string(&ids).map_err(|e| {
                ApiError(CoreError::Validation(format!(
                    "failed to serialize selected_accounts: {e}"
                )))
            })?;
            UpdateField::Set(Some(json))
        }
        Some(_) => {
            return Err(ApiError(CoreError::Validation(
                "selected_accounts must be an array or null".into(),
            )));
        }
    };

    let updates = ComboTargetUpdates {
        priority_order,
        weight,
        active,
        cooldown_mode,
        cooldown_base_secs,
        cooldown_max_secs,
        cooldown_factor,
        thinking_effort,
        description,
        selected_accounts,
    };

    if updates.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "missing update fields in request body".into(),
        )));
    }
    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_nullable_str() {
        let obj = json!({ "valid": "text", "empty": null, "invalid": 123 });
        assert_eq!(
            parse_nullable_str(&obj, "valid").unwrap(),
            UpdateField::Set("text")
        );
        assert_eq!(
            parse_nullable_str(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_str(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_str(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_u64() {
        let obj = json!({ "valid": 42, "empty": null, "invalid": "text", "negative": -1 });
        assert_eq!(
            parse_nullable_u64(&obj, "valid").unwrap(),
            UpdateField::Set(42)
        );
        assert_eq!(
            parse_nullable_u64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_u64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_u64(&obj, "invalid").is_err());
        assert!(parse_nullable_u64(&obj, "negative").is_err());
    }

    #[test]
    fn test_parse_nullable_i64() {
        let obj = json!({ "valid": -42, "empty": null, "invalid": "text" });
        assert_eq!(
            parse_nullable_i64(&obj, "valid").unwrap(),
            UpdateField::Set(-42)
        );
        assert_eq!(
            parse_nullable_i64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_i64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_i64(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_f64() {
        let obj = json!({ "valid": 0.5, "empty": null, "invalid": "text" });
        assert_eq!(
            parse_nullable_f64(&obj, "valid").unwrap(),
            UpdateField::Set(0.5)
        );
        assert_eq!(
            parse_nullable_f64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_f64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_f64(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_bounded_i32() {
        let obj = json!({ "valid": 10, "out_of_bounds": 200, "invalid": "text" });
        assert_eq!(
            parse_bounded_i32(&obj, "valid", 1, 100, |v| format!("{v} out of range")).unwrap(),
            Some(10)
        );
        assert_eq!(
            parse_bounded_i32(&obj, "missing", 1, 100, |v| format!("{v} out of range")).unwrap(),
            None
        );
        assert!(parse_bounded_i32(&obj, "out_of_bounds", 1, 100, |v| format!("{v} out of range")).is_err());
        assert!(parse_bounded_i32(&obj, "invalid", 1, 100, |v| format!("{v} out of range")).is_err());
    }

    #[test]
    fn test_parse_optional_bool() {
        let obj = json!({ "t": true, "f": false, "invalid": 1 });
        assert_eq!(parse_optional_bool(&obj, "t").unwrap(), Some(true));
        assert_eq!(parse_optional_bool(&obj, "f").unwrap(), Some(false));
        assert_eq!(parse_optional_bool(&obj, "missing").unwrap(), None);
        assert!(parse_optional_bool(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_target_weight() {
        assert_eq!(parse_target_weight(&json!({})).unwrap(), None);
        assert_eq!(parse_target_weight(&json!({"weight": 1})).unwrap(), Some(1));
        assert_eq!(
            parse_target_weight(&json!({"weight": i32::MAX})).unwrap(),
            Some(i32::MAX)
        );
        assert!(parse_target_weight(&json!({"weight": 0})).is_err());
        assert!(parse_target_weight(&json!({"weight": i64::from(i32::MAX) + 1})).is_err());
        assert!(parse_target_weight(&json!({"weight": "text"})).is_err());
    }

    #[test]
    fn test_parse_target_priority_order() {
        assert_eq!(parse_target_priority_order(&json!({})).unwrap(), None);
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": 0})).unwrap(),
            Some(0)
        );
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": i32::MIN})).unwrap(),
            Some(i32::MIN)
        );
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": i32::MAX})).unwrap(),
            Some(i32::MAX)
        );
        assert!(
            parse_target_priority_order(&json!({"priority_order": i64::from(i32::MAX) + 1}))
                .is_err()
        );
        assert!(parse_target_priority_order(&json!({"priority_order": "text"})).is_err());
    }

    #[test]
    fn test_parse_combo_target_updates_thinking_effort() {
        let val_set = json!({"thinking_effort": "high"});
        let updates = parse_combo_target_updates(&val_set).unwrap();
        assert_eq!(updates.thinking_effort, UpdateField::Set("high"));

        let val_clear = json!({"thinking_effort": null});
        let updates_clear = parse_combo_target_updates(&val_clear).unwrap();
        assert_eq!(updates_clear.thinking_effort, UpdateField::Reset);

        let val_ignore = json!({"weight": 2});
        let updates_ignore = parse_combo_target_updates(&val_ignore).unwrap();
        assert_eq!(updates_ignore.thinking_effort, UpdateField::Ignore);

        assert!(parse_combo_target_updates(&json!({})).is_err());
        let val_err = json!({"thinking_effort": 123});
        assert!(parse_combo_target_updates(&val_err).is_err());
    }

    #[test]
    fn test_parse_combo_target_updates_selected_accounts() {
        let val_set = json!({"selected_accounts": [1, 2, 3]});
        let updates = parse_combo_target_updates(&val_set).unwrap();
        assert_eq!(
            updates.selected_accounts,
            UpdateField::Set(Some("[1,2,3]".to_string()))
        );

        let val_clear = json!({"selected_accounts": null});
        let updates_clear = parse_combo_target_updates(&val_clear).unwrap();
        assert_eq!(updates_clear.selected_accounts, UpdateField::Reset);

        let val_ignore = json!({"weight": 2});
        let updates_ignore = parse_combo_target_updates(&val_ignore).unwrap();
        assert_eq!(updates_ignore.selected_accounts, UpdateField::Ignore);

        let val_err = json!({"selected_accounts": "invalid"});
        assert!(parse_combo_target_updates(&val_err).is_err());
    }
}
