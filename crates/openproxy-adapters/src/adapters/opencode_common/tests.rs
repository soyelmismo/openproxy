use super::*;
use serde_json::Value;

#[test]
fn test_parse_opencode_go_usage_response() {
    let json = serde_json::json!({
        "usage": {
            "rolling": { "status": "ok", "percent": 5, "resetsAt": "2026-10-23T03:04:10.000Z" },
            "weekly": { "status": "ok", "percent": 25, "resetsAt": "2026-10-26T00:00:00.000Z" },
            "monthly": { "status": "ok", "percent": 80, "resetsAt": "2026-11-07T00:25:10.000Z" }
        }
    });

    let quota = parse_opencode_go_usage_response(&json);
    assert_eq!(quota.session_used, Some(5));
    assert_eq!(quota.session_limit, Some(100));
    assert_eq!(
        quota.session_reset_at.as_deref(),
        Some("2026-10-23T03:04:10.000Z")
    );

    assert_eq!(quota.weekly_used, Some(25));
    assert_eq!(quota.weekly_limit, Some(100));
    assert_eq!(
        quota.weekly_reset_at.as_deref(),
        Some("2026-10-26T00:00:00.000Z")
    );

    assert_eq!(quota.plan_name.as_deref(), Some("OpenCode Go"));

    let details = quota
        .model_details
        .expect("model_details should be present");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].model_id, "Monthly");
    assert_eq!(details[0].session_used, 80);
    assert_eq!(details[0].session_limit, 100);
    assert_eq!(
        details[0].session_reset_at.as_deref(),
        Some("2026-11-07T00:25:10.000Z")
    );
    assert!((details[0].remaining_fraction - 0.20).abs() < f64::EPSILON);
}

#[test]
fn test_parse_opencode_go_usage_response_empty() {
    let json = serde_json::json!({});
    let quota = parse_opencode_go_usage_response(&json);
    assert!(quota.session_used.is_none());
    assert!(quota.weekly_used.is_none());
    assert!(quota.model_details.is_none());
}

#[test]
fn test_inject_opencode_agent_quartet_tools_responses_strict_false() {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tools".to_string(),
        serde_json::json!([
            {
                "type": "function",
                "name": "edit",
                "description": "Edit file",
                "parameters": { "type": "object" }
            }
        ]),
    );

    inject_opencode_agent_quartet_tools(&mut obj, TargetFormat::Responses);

    let tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert_eq!(tools.len(), 5);

    for tool in tools {
        assert_eq!(tool.get("strict"), Some(&Value::Bool(false)));
    }
}
