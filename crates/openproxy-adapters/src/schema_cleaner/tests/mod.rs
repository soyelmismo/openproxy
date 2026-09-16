use super::*;
use serde_json::json;

mod advanced_tests;

#[test]
fn test_drops_boolean_subschemas() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "outer": { "type": "object", "properties": { "forbidden": false, "allowed": { "type": "string" } }, "required": ["forbidden", "allowed"] },
            "list": { "type": "array", "items": { "type": "object", "properties": { "nope": false, "ok": { "type": "number" } } } }
        }
    });
    clean_json_schema(&mut schema);
    let outer_props = &schema["properties"]["outer"]["properties"];
    assert!(outer_props.get("forbidden").is_none());
    assert!(outer_props["allowed"].is_object());
    let item_props = &schema["properties"]["list"]["items"]["properties"];
    assert!(item_props.get("nope").is_none());
    assert!(item_props["ok"].is_object());
    let req = schema["properties"]["outer"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(req.iter().all(|r| r.as_str() != Some("forbidden")));
}

#[test]
fn test_clean_json_schema_draft_2020_12() {
    let mut schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
        "properties": {
            "location": { "type": "string", "minLength": 1, "format": "city" },
            "pattern": { "type": "object", "properties": { "regex": { "type": "string", "pattern": "^[a-z]+$" } } },
            "unit": { "type": ["string", "null"], "default": "celsius" }
        },
        "required": ["location"]
    });
    clean_json_schema(&mut schema);
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["location"]["type"], "string");
    assert!(schema["properties"]["location"].get("minLength").is_none());
    assert!(schema["properties"]["location"].get("format").is_none());
    assert!(
        schema["properties"]["location"]["description"]
            .as_str()
            .unwrap()
            .contains("[Constraint: minLen: 1, format: city]")
    );
    assert_eq!(schema["properties"]["pattern"]["type"], "object");
    assert!(
        schema["properties"]["pattern"]["properties"]["regex"]
            .get("pattern")
            .is_none()
    );
    assert!(
        schema["properties"]["pattern"]["properties"]["regex"]["description"]
            .as_str()
            .unwrap()
            .contains("[Constraint: pattern: ^[a-z]+$]")
    );
    assert_eq!(schema["properties"]["unit"]["type"], "string");
    assert!(schema.get("$schema").is_none());
}

#[test]
fn test_type_fallback() {
    let mut s1 = json!({"type": ["string", "null"]});
    clean_json_schema(&mut s1);
    assert_eq!(s1["type"], "string");
    let mut s2 = json!({"type": ["integer", "null"]});
    clean_json_schema(&mut s2);
    assert_eq!(s2["type"], "integer");
}

#[test]
fn test_flatten_refs() {
    let mut schema = json!({
        "$defs": { "Address": { "type": "object", "properties": { "city": { "type": "string" } } } },
        "properties": { "home": { "$ref": "#/$defs/Address" } }
    });
    clean_json_schema(&mut schema);
    assert_eq!(schema["properties"]["home"]["type"], "object");
    assert_eq!(
        schema["properties"]["home"]["properties"]["city"]["type"],
        "string"
    );
}

#[test]
fn test_clean_json_schema_missing_required() {
    let mut schema = json!({ "type": "object", "properties": { "existing_prop": { "type": "string" } }, "required": ["existing_prop", "missing_prop"] });
    clean_json_schema(&mut schema);
    let required = schema["required"].as_array().unwrap();
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].as_str().unwrap(), "existing_prop");
}

#[test]
fn test_anyof_type_extraction() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "testo": { "anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Testo" },
            "importo": { "anyOf": [{"type": "number"}, {"type": "null"}], "default": null, "title": "Importo" },
            "attivo": { "type": "boolean", "title": "Attivo" }
        }
    });
    clean_json_schema(&mut schema);
    assert!(schema["properties"]["testo"].get("anyOf").is_none());
    assert!(schema["properties"]["importo"].get("anyOf").is_none());
    assert_eq!(schema["properties"]["testo"]["type"], "string");
    assert_eq!(schema["properties"]["importo"]["type"], "number");
    assert_eq!(schema["properties"]["attivo"]["type"], "boolean");
    assert!(schema["properties"]["testo"].get("default").is_none());
}

#[test]
fn test_oneof_type_extraction() {
    let mut schema =
        json!({ "properties": { "value": { "oneOf": [{"type": "integer"}, {"type": "null"}] } } });
    clean_json_schema(&mut schema);
    assert!(schema["properties"]["value"].get("oneOf").is_none());
    assert_eq!(schema["properties"]["value"]["type"], "integer");
}

#[test]
fn test_existing_type_preserved() {
    let mut schema =
        json!({ "properties": { "name": { "type": "string", "anyOf": [{"type": "number"}] } } });
    clean_json_schema(&mut schema);
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert!(schema["properties"]["name"].get("anyOf").is_none());
}

#[test]
fn test_issue_815_anyof_properties_preserved() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "anyOf": [
                    { "type": "object", "properties": { "path": { "type": "string" }, "recursive": { "type": "boolean" } }, "required": ["path"] },
                    { "type": "null" }
                ]
            }
        }
    });
    clean_json_schema(&mut schema);
    let config = &schema["properties"]["config"];
    assert_eq!(config["type"], "object");
    assert!(config.get("properties").is_some());
    assert_eq!(config["properties"]["path"]["type"], "string");
    assert_eq!(config["properties"]["recursive"]["type"], "boolean");
    assert!(
        config["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "path")
    );
    assert!(config.get("anyOf").is_none());
    assert!(config["properties"].get("reason").is_none());
}

#[test]
fn test_clean_json_schema_on_non_schema_object() {
    let mut tool_call = json!({ "functionCall": { "name": "local_shell_call", "args": { "command": ["ls"] }, "id": "call_123" } });
    clean_json_schema(&mut tool_call);
    let fc = &tool_call["functionCall"];
    assert_eq!(fc["name"], "local_shell_call");
    assert_eq!(fc["args"]["command"][0], "ls");
    assert_eq!(fc["id"], "call_123");
}

#[test]
fn test_nullable_handling_with_description() {
    let mut schema = json!({ "type": ["string", "null"], "description": "User name" });
    clean_json_schema(&mut schema);
    assert_eq!(schema["type"], "string");
    assert!(
        schema["description"]
            .as_str()
            .unwrap()
            .contains("User name")
            && schema["description"]
                .as_str()
                .unwrap()
                .contains("(nullable)")
    );
}

#[test]
fn test_clean_anyof_with_propertynames() {
    let mut schema = json!({
        "properties": { "config": { "anyOf": [{ "type": "object", "propertyNames": {"pattern": "^[a-z]+$"}, "properties": { "key": {"type": "string"} } }, {"type": "null"}] } }
    });
    clean_json_schema(&mut schema);
    let config = &schema["properties"]["config"];
    assert!(config.get("anyOf").is_none() && config.get("propertyNames").is_none());
    assert_eq!(config["properties"]["key"]["type"], "string");
}

#[test]
fn test_clean_items_array_with_const() {
    let mut schema = json!({ "type": "array", "items": { "type": "object", "properties": { "status": { "const": "active", "type": "string" } } } });
    clean_json_schema(&mut schema);
    assert!(
        schema["items"]["properties"]["status"]
            .get("const")
            .is_none()
    );
    assert_eq!(schema["items"]["properties"]["status"]["type"], "string");
}

#[test]
fn test_deep_nested_array_cleaning() {
    let mut schema = json!({
        "properties": {
            "data": {
                "anyOf": [{
                    "type": "array",
                    "items": { "anyOf": [{ "type": "object", "propertyNames": {"maxLength": 10}, "const": "test", "properties": { "name": {"type": "string"} } }, {"type": "null"}] }
                }]
            }
        }
    });
    clean_json_schema(&mut schema);
    let data = &schema["properties"]["data"];
    assert!(
        data.get("anyOf").is_none()
            && data.get("propertyNames").is_none()
            && data.get("const").is_none()
    );
    assert_eq!(data["type"], "array");
    if let Some(items) = data.get("items") {
        assert!(
            items.get("anyOf").is_none()
                && items.get("propertyNames").is_none()
                && items.get("const").is_none()
        );
    }
}
