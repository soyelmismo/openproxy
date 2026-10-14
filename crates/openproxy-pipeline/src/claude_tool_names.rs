//! Tool-name restoration for the Claude Code provider.
//!
//! The Claude Code adapter renames the client's tool names to TitleCase before
//! dispatch (upstreams reject agent clients partly by the shape of the tool
//! names, see `claude_code::cloaking`). The response therefore carries the
//! renamed names: every `tool_use` name is mapped back here to what the client
//! originally sent, so the client can still dispatch the tool it declared.

use openproxy_adapters::adapters::claude_code::cloaking::build_tool_rename_map_from_names;
use std::collections::HashMap;

/// Builds the cloaked -> original map from the client's tool list, or `None`
/// when no name was renamed.
pub fn build_restore_map(req: &crate::PipelineRequest) -> Option<HashMap<String, String>> {
    let tools = req.openai_request.tools.as_deref()?;
    build_restore_map_from_tools(tools)
}

/// Same map, straight from the OpenAI-shaped tool list, so it can be built and
/// tested without a full `PipelineRequest`.
pub fn build_restore_map_from_tools(
    tools: &[serde_json::Value],
) -> Option<HashMap<String, String>> {
    let forward = build_tool_rename_map_from_names(tools.iter().filter_map(openai_tool_name));
    if forward.is_empty() {
        return None;
    }
    Some(
        forward
            .into_iter()
            .map(|(original, cloaked)| (cloaked, original))
            .collect(),
    )
}

fn openai_tool_name(tool: &serde_json::Value) -> Option<&str> {
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
}

/// Restores the original tool names of a translated OpenAI response in place.
pub fn restore_response_tool_names(
    resp: &mut crate::translation::OpenAIResponse,
    map: &HashMap<String, String>,
) {
    for choice in &mut resp.choices {
        let Some(tool_calls) = choice.message.tool_calls.as_mut() else {
            continue;
        };
        for call in tool_calls {
            let Some(function) = call.get_mut("function").and_then(|f| f.as_object_mut()) else {
                continue;
            };
            if let Some(name) = function.get("name").and_then(|n| n.as_str())
                && let Some(original) = map.get(name)
            {
                function.insert(
                    "name".to_string(),
                    serde_json::Value::String(original.clone()),
                );
            }
        }
    }
}

/// Restores the original tool names inside a translated SSE chunk payload
/// (`choices[].delta.tool_calls[].function.name`) in place.
pub fn restore_chunk_tool_names(payload: &mut serde_json::Value, map: &HashMap<String, String>) {
    let Some(choices) = payload.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices {
        let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) else {
            continue;
        };
        let Some(tool_calls) = delta.get_mut("tool_calls").and_then(|t| t.as_array_mut()) else {
            continue;
        };
        for call in tool_calls {
            let Some(function) = call.get_mut("function").and_then(|f| f.as_object_mut()) else {
                continue;
            };
            if let Some(name) = function.get("name").and_then(|n| n.as_str())
                && let Some(original) = map.get(name)
            {
                function.insert(
                    "name".to_string(),
                    serde_json::Value::String(original.clone()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_tool(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {"name": name, "parameters": {}}
        })
    }

    #[test]
    fn test_restore_map_inverts_renamed_names_only() {
        let tools = vec![
            openai_tool("browser_exec"),
            openai_tool("read_file"),
            openai_tool("Read"),
        ];
        let map = build_restore_map_from_tools(&tools).expect("renames expected");
        assert_eq!(
            map.get("BrowserExec").map(String::as_str),
            Some("browser_exec")
        );
        assert_eq!(map.get("ReadFile").map(String::as_str), Some("read_file"));
        // Identity names never enter the map.
        assert!(!map.contains_key("Read"));
    }

    #[test]
    fn test_restore_map_none_without_renames() {
        let tools = vec![openai_tool("Read"), openai_tool("Bash")];
        assert!(build_restore_map_from_tools(&tools).is_none());
        assert!(build_restore_map_from_tools(&[]).is_none());
    }

    #[test]
    fn test_restore_map_drops_collisions() {
        // read_file and read-file both cloak to ReadFile: neither is renamed.
        let tools = vec![openai_tool("read_file"), openai_tool("read-file")];
        assert!(build_restore_map_from_tools(&tools).is_none());
    }

    #[test]
    fn test_restore_response_tool_names() {
        let tools = vec![openai_tool("browser_exec")];
        let map = build_restore_map_from_tools(&tools).unwrap();

        let mut resp: crate::translation::OpenAIResponse =
            serde_json::from_value(serde_json::json!({
                "id": "msg_1",
                "object": "chat.completion",
                "created": 0,
                "model": "claude-opus-5-5",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "toolu_1",
                            "type": "function",
                            "function": {"name": "BrowserExec", "arguments": "{\"url\":\"x\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": null
            }))
            .unwrap();

        restore_response_tool_names(&mut resp, &map);
        let call = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call["function"]["name"], "browser_exec");
        assert_eq!(call["function"]["arguments"], "{\"url\":\"x\"}");
    }

    #[test]
    fn test_restore_response_tool_names_leaves_unknown_names() {
        let tools = vec![openai_tool("browser_exec")];
        let map = build_restore_map_from_tools(&tools).unwrap();

        let mut resp: crate::translation::OpenAIResponse =
            serde_json::from_value(serde_json::json!({
                "id": "msg_1",
                "object": "chat.completion",
                "created": 0,
                "model": "claude-opus-5-5",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "hi",
                        "tool_calls": [{
                            "id": "toolu_1",
                            "type": "function",
                            "function": {"name": "Terminal", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": null
            }))
            .unwrap();

        restore_response_tool_names(&mut resp, &map);
        let call = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call["function"]["name"], "Terminal");
    }

    #[test]
    fn test_restore_chunk_tool_names() {
        let tools = vec![openai_tool("tool_search")];
        let map = build_restore_map_from_tools(&tools).unwrap();

        let mut payload = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "toolu_1",
                        "type": "function",
                        "function": {"name": "ToolSearch", "arguments": ""}
                    }]
                },
                "finish_reason": null
            }]
        });

        restore_chunk_tool_names(&mut payload, &map);
        assert_eq!(
            payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "tool_search"
        );
    }

    #[test]
    fn test_restore_chunk_tool_names_ignores_non_tool_chunks() {
        let tools = vec![openai_tool("browser_exec")];
        let map = build_restore_map_from_tools(&tools).unwrap();

        let mut payload = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]
        });

        restore_chunk_tool_names(&mut payload, &map);
        assert_eq!(payload["choices"][0]["delta"]["content"], "hello");
    }
}
