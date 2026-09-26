use serde_json::Value;
use std::collections::HashSet;

/// Sanitizes JSON Schema for tool parameters to prevent grammar/FSM sampler traps
/// and ensure compatibility with OpenAI Structured Outputs and Codex Responses API.
///
/// Specific fixes:
/// 1. Strips regex `"pattern"` from schema properties (unsupported by OpenAI Structured
///    Outputs, and causes regex-constrained token traps such as OpenCode's `pattern: "^ses"`).
/// 2. Relaxes `"additionalProperties": false` when optional properties exist (fields in `properties`
///    not listed in `required`). In OpenAI Responses / Codex, `additionalProperties: false` forces
///    the model to emit all defined properties even when optional.
pub fn sanitize_tool_parameters_schema(val: &mut Value) {
    let Some(obj) = val.as_object_mut() else {
        return;
    };

    // 1. Remove regex "pattern" from property definitions.
    obj.remove("pattern");

    // 2. If additionalProperties: false is set but there are optional properties, remove it.
    if let Some(add_props) = obj.get("additionalProperties")
        && add_props.as_bool() == Some(false)
        && let Some(props) = obj.get("properties").and_then(|p| p.as_object())
    {
        let req_set: Option<HashSet<&str>> = obj
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect());

        let has_optional_props = match req_set {
            Some(reqs) => props.keys().any(|k| !reqs.contains(k.as_str())),
            None => !props.is_empty(),
        };

        if has_optional_props {
            obj.remove("additionalProperties");
        }
    }

    // 3. Recurse into nested structures.
    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_, prop_val) in props.iter_mut() {
            sanitize_tool_parameters_schema(prop_val);
        }
    }

    if let Some(items) = obj.get_mut("items") {
        sanitize_tool_parameters_schema(items);
    }

    for compound in ["anyOf", "allOf", "oneOf"] {
        if let Some(arr) = obj.get_mut(compound).and_then(|v| v.as_array_mut()) {
            for sub in arr.iter_mut() {
                sanitize_tool_parameters_schema(sub);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sanitizes_opencode_subagent_schema() {
        let mut schema = json!({
            "additionalProperties": false,
            "properties": {
                "agent": {
                    "description": "The type of specialized agent",
                    "type": "string"
                },
                "background": {
                    "description": "Run the subagent in background",
                    "type": "boolean"
                },
                "description": {
                    "description": "Label",
                    "type": "string"
                },
                "model": {
                    "description": "Model ID",
                    "type": "string"
                },
                "prompt": {
                    "description": "The task",
                    "type": "string"
                },
                "sessionID": {
                    "description": "Continue session",
                    "pattern": "^ses",
                    "type": "string"
                }
            },
            "required": ["agent", "description", "prompt"],
            "type": "object"
        });

        sanitize_tool_parameters_schema(&mut schema);

        // additionalProperties: false should be removed because sessionID, model, background are optional
        assert!(!schema.as_object().unwrap().contains_key("additionalProperties"));

        // pattern: "^ses" should be stripped from sessionID
        let session_id_prop = &schema["properties"]["sessionID"];
        assert!(!session_id_prop.as_object().unwrap().contains_key("pattern"));
        assert_eq!(session_id_prop["type"], "string");

        // Required fields remain untouched
        assert_eq!(
            schema["required"],
            json!(["agent", "description", "prompt"])
        );
    }

    #[test]
    fn test_preserves_additional_properties_false_when_all_required() {
        let mut schema = json!({
            "additionalProperties": false,
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"],
            "type": "object"
        });

        sanitize_tool_parameters_schema(&mut schema);

        // additionalProperties: false is kept when strictly all fields are required
        assert_eq!(schema["additionalProperties"], json!(false));
    }
}
