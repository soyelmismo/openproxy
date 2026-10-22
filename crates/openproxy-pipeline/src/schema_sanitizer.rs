use serde_json::Value;
use std::collections::HashSet;

/// Sanitizes JSON Schema for tool parameters to prevent grammar/FSM sampler traps
/// and stay compatible with OpenAI Structured Outputs / Codex Responses API.
/// Strips regex `"pattern"` (unsupported, and a token trap such as OpenCode's
/// `pattern: "^ses"`) and relaxes `"additionalProperties": false` when optional
/// properties exist (OpenAI forces every defined property to be emitted).
pub fn sanitize_tool_parameters_schema(val: &mut Value) {
    let Some(obj) = val.as_object_mut() else {
        return;
    };

    obj.remove("pattern");

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

/// Sanitizes the `parameters` schema of a tool object in-place.
/// Handles both standard OpenAI tool format (`tool.function.parameters`)
/// and flat tool format (`tool.parameters`).
pub fn sanitize_tool_parameters(tool: &mut Value) {
    let Some(obj) = tool.as_object_mut() else {
        return;
    };
    if let Some(func) = obj.get_mut("function").and_then(|f| f.as_object_mut()) {
        if let Some(params) = func.get_mut("parameters") {
            sanitize_tool_parameters_schema(params);
        }
    } else if let Some(params) = obj.get_mut("parameters") {
        sanitize_tool_parameters_schema(params);
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
        assert!(
            !schema
                .as_object()
                .unwrap()
                .contains_key("additionalProperties")
        );

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

    #[test]
    fn test_sanitize_tool_parameters_nested_and_flat() {
        let mut tool_nested = json!({
            "type": "function",
            "function": {
                "name": "search",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "q": { "type": "string", "pattern": "^.*$" }
                    }
                }
            }
        });
        sanitize_tool_parameters(&mut tool_nested);
        assert!(
            !tool_nested["function"]["parameters"]["properties"]["q"]
                .as_object()
                .unwrap()
                .contains_key("pattern")
        );

        let mut tool_flat = json!({
            "type": "function",
            "name": "search",
            "parameters": {
                "type": "object",
                "properties": {
                    "q": { "type": "string", "pattern": "^.*$" }
                }
            }
        });
        sanitize_tool_parameters(&mut tool_flat);
        assert!(
            !tool_flat["parameters"]["properties"]["q"]
                .as_object()
                .unwrap()
                .contains_key("pattern")
        );
    }
}
