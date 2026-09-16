use super::*;
use serde_json::json;

#[test]
fn test_fix_tool_call_args() {
    let mut args = json!({
        "port": "8080", "enabled": "true", "timeout": "5.5",
        "metadata": { "retry": "3" }, "tags": ["1", "2"]
    });
    let schema = json!({
        "properties": {
            "port": { "type": "integer" }, "enabled": { "type": "boolean" }, "timeout": { "type": "number" },
            "metadata": { "type": "object", "properties": { "retry": { "type": "integer" } } },
            "tags": { "type": "array", "items": { "type": "integer" } }
        }
    });
    fix_tool_call_args(&mut args, &schema);
    assert_eq!(args["port"], 8080);
    assert_eq!(args["enabled"], true);
    assert_eq!(args["timeout"], 5.5);
    assert_eq!(args["metadata"]["retry"], 3);
    assert_eq!(args["tags"], json!([1, 2]));
}

#[test]
fn test_fix_tool_call_args_protection() {
    let mut args = json!({ "version": "01.0", "code": "007" });
    let schema =
        json!({ "properties": { "version": { "type": "number" }, "code": { "type": "integer" } } });
    fix_tool_call_args(&mut args, &schema);
    assert_eq!(args["version"], "01.0");
    assert_eq!(args["code"], "007");
}

#[test]
fn test_nested_defs_flattening() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "$defs": { "Address": { "type": "object", "properties": { "city": { "type": "string" }, "zip": { "type": "string" } } } },
                "type": "object",
                "properties": { "home": { "$ref": "#/$defs/Address" }, "work": { "$ref": "#/$defs/Address" } }
            }
        }
    });
    clean_json_schema(&mut schema);
    let home = &schema["properties"]["config"]["properties"]["home"];
    assert_eq!(home["type"], "object");
    assert_eq!(home["properties"]["city"]["type"], "string");
    assert!(home.get("$ref").is_none());
    let work = &schema["properties"]["config"]["properties"]["work"];
    assert_eq!(work["type"], "object");
    assert!(work.get("$ref").is_none());
}

#[test]
fn test_unresolved_ref_fallback() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "external": { "$ref": "https://example.com/schemas/External.json" },
            "missing": { "$ref": "#/$defs/NonExistent" }
        }
    });
    clean_json_schema(&mut schema);
    let external = &schema["properties"]["external"];
    assert_eq!(external["type"], "string");
    assert!(
        external["description"]
            .as_str()
            .unwrap()
            .contains("Unresolved $ref")
    );
    let missing = &schema["properties"]["missing"];
    assert_eq!(missing["type"], "string");
    assert!(
        missing["description"]
            .as_str()
            .unwrap()
            .contains("NonExistent")
    );
}

#[test]
fn test_deeply_nested_multi_level_defs() {
    let mut schema = json!({
        "type": "object", "$defs": { "RootDef": { "type": "integer" } },
        "properties": {
            "level1": {
                "type": "object", "$defs": { "Level1Def": { "type": "boolean" } },
                "properties": {
                    "level2": {
                        "type": "object", "$defs": { "Level2Def": { "type": "number" } },
                        "properties": {
                            "useRoot": { "$ref": "#/$defs/RootDef" },
                            "useLevel1": { "$ref": "#/$defs/Level1Def" },
                            "useLevel2": { "$ref": "#/$defs/Level2Def" }
                        }
                    }
                }
            }
        }
    });
    clean_json_schema(&mut schema);
    let lp = &schema["properties"]["level1"]["properties"]["level2"]["properties"];
    assert_eq!(lp["useRoot"]["type"], "integer");
    assert_eq!(lp["useLevel1"]["type"], "boolean");
    assert_eq!(lp["useLevel2"]["type"], "number");
    assert!(lp["useRoot"].get("$ref").is_none());
    assert!(lp["useLevel1"].get("$ref").is_none());
    assert!(lp["useLevel2"].get("$ref").is_none());
}

#[test]
fn test_non_standard_field_cleaning_and_healing() {
    let mut schema = json!({
        "type": "array",
        "items": { "cornerRadius": { "type": "number" }, "fillColor": { "type": "string" } }
    });
    clean_json_schema(&mut schema);
    let items = &schema["items"];
    assert_eq!(items["type"], "object");
    assert!(items.get("properties").is_some());
    assert_eq!(items["properties"]["cornerRadius"]["type"], "number");
    assert_eq!(items["properties"]["fillColor"]["type"], "string");
    assert!(items.get("cornerRadius").is_none());
    assert!(items.get("fillColor").is_none());
}

#[test]
fn test_implicit_type_injection() {
    let mut schema = json!({ "properties": { "values": { "items": { "cornerRadius": { "type": "number" } } } } });
    clean_json_schema(&mut schema);
    assert_eq!(schema["properties"]["values"]["type"], "array");
    assert_eq!(schema["properties"]["values"]["items"]["type"], "object");
    assert!(
        schema["properties"]["values"]["items"]["properties"]
            .get("cornerRadius")
            .is_some()
    );
}

#[test]
fn test_gemini_strict_validation_injection() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "patterns": { "items": { "properties": { "type": { "enum": ["A", "B"] } } } },
            "nested_props": { "properties": { "foo": { "type": "string" } } }
        }
    });
    clean_json_schema(&mut schema);
    let type_node = &schema["properties"]["patterns"]["items"]["properties"]["type"];
    assert_eq!(type_node["type"], "string");
    assert!(type_node.get("enum").is_some());
    assert_eq!(schema["properties"]["nested_props"]["type"], "object");
    assert_eq!(schema["properties"]["patterns"]["type"], "array");
}

#[test]
fn test_malformed_items_as_properties() {
    let mut schema = json!({
        "type": "object",
        "properties": { "config": { "type": "object", "items": { "color": { "type": "string" }, "size": { "type": "number" } } } }
    });
    clean_json_schema(&mut schema);
    let config = &schema["properties"]["config"];
    assert!(config.get("items").is_none());
    assert_eq!(config["properties"]["color"]["type"], "string");
    assert_eq!(config["properties"]["size"]["type"], "number");
    assert_eq!(config["type"], "object");
}

#[test]
fn test_merge_all_of() {
    let mut schema = json!({
        "allOf": [
            { "type": "object", "properties": { "base_prop": { "type": "string" } }, "required": ["base_prop"] },
            { "type": "object", "properties": { "extended_prop": { "type": "number" } }, "required": ["extended_prop"] }
        ]
    });
    clean_json_schema(&mut schema);
    assert!(schema.get("allOf").is_none());
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["base_prop"]["type"], "string");
    assert_eq!(schema["properties"]["extended_prop"]["type"], "number");
    let req = schema["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v == "base_prop") && req.iter().any(|v| v == "extended_prop"));
}

#[test]
fn test_circular_ref_flattening() {
    let mut schema = json!({
        "$defs": {
            "A": { "type": "object", "properties": { "toB": { "$ref": "#/$defs/B" } } },
            "B": { "type": "object", "properties": { "toA": { "$ref": "#/$defs/A" } } }
        },
        "properties": { "start": { "$ref": "#/$defs/A" } }
    });
    clean_json_schema(&mut schema);
    assert_eq!(schema["properties"]["start"]["type"], "object");
    assert!(
        schema["properties"]["start"]["properties"]
            .get("toB")
            .is_some()
    );
}

#[test]
fn test_any_of_best_branch_selection() {
    let mut schema = json!({
        "anyOf": [{ "type": "string" }, { "type": "object", "properties": { "foo": { "type": "string" } } }, { "type": "null" }]
    });
    clean_json_schema(&mut schema);
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["foo"]["type"], "string");
    assert!(
        schema["description"]
            .as_str()
            .unwrap()
            .contains("Accepts: string | object")
    );
}

#[test]
fn test_coerce_helpers() {
    use crate::schema_cleaner::sanitize::{coerce_str_to_boolean, is_preserved_string_number};
    let bool_cases = [
        ("true", Some(true)),
        ("1", Some(true)),
        ("YES", Some(true)),
        ("on", Some(true)),
        ("false", Some(false)),
        ("0", Some(false)),
        ("NO", Some(false)),
        ("off", Some(false)),
        ("invalid", None),
    ];
    for (s, exp) in bool_cases {
        assert_eq!(coerce_str_to_boolean(s), exp);
    }
    for (s, exp) in [("01", true), ("007", true), ("0.5", false), ("123", false)] {
        assert_eq!(is_preserved_string_number(s), exp);
    }
}
