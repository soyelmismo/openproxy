use super::types::{
    KiroAssistantResponseMessage, KiroConversationState, KiroCurrentMessage, KiroHistoryTurn,
    KiroInferenceConfig, KiroRequest, KiroUserInputMessage, KiroUserInputMessageContext,
};
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::Value;

pub const KIRO_DEFAULT_MODEL: &str = "kiro-default-model";

/// Build a [`KiroRequest`] from an OpenAI [`OpenAIRequest`].
pub fn build_kiro_request(openai: &OpenAIRequest, profile_arn: Option<&str>) -> KiroRequest {
    let tools = openai.tools.as_ref().map(|tool_list| {
        tool_list
            .iter()
            .filter_map(|t| {
                let func = t.get("function")?;
                let name = func.get("name")?.as_str()?;
                let description = func
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(name);
                let params = func
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));

                Some(serde_json::json!({
                    "toolSpecification": {
                        "name": name,
                        "description": description,
                        "inputSchema": {
                            "json": params
                        }
                    }
                }))
            })
            .collect::<Vec<_>>()
    });

    let (history, current_user_msg) = convert_messages(&openai.messages, &openai.model, tools);
    let inference_config = build_kiro_inference_config(openai);

    KiroRequest {
        conversation_state: KiroConversationState {
            current_message: KiroCurrentMessage {
                user_input_message: current_user_msg,
            },
            history: (!history.is_empty()).then_some(history),
            chat_trigger_type: "MANUAL".to_string(),
        },
        profile_arn: profile_arn.map(std::string::ToString::to_string),
        inference_config,
    }
}

fn build_kiro_inference_config(openai: &OpenAIRequest) -> Option<KiroInferenceConfig> {
    let has_any = openai.max_tokens.is_some()
        || openai.temperature.is_some()
        || openai.top_p.is_some()
        || openai.stop.is_some();
    has_any.then(|| KiroInferenceConfig {
        max_tokens: openai.max_tokens,
        temperature: openai.temperature,
        top_p: openai.top_p,
        stop: openai.stop.clone(),
    })
}

fn convert_messages(
    messages: &[OpenAIMessage],
    model: &str,
    tools: Option<Vec<Value>>,
) -> (Vec<KiroHistoryTurn>, KiroUserInputMessage) {
    if messages.is_empty() {
        return (
            Vec::new(),
            KiroUserInputMessage {
                content: "(empty)".to_string(),
                model_id: model.to_string(),
                origin: "AI_EDITOR".to_string(),
                user_input_message_context: tools.map(|t| KiroUserInputMessageContext {
                    tools: Some(t),
                    tool_results: None,
                }),
            },
        );
    }

    let mut raw_turns: Vec<KiroHistoryTurn> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" | "user" => {
                let text = msg
                    .content
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                raw_turns.push(KiroHistoryTurn::User {
                    user_input_message: KiroUserInputMessage {
                        content: text,
                        model_id: model.to_string(),
                        origin: "AI_EDITOR".to_string(),
                        user_input_message_context: None,
                    },
                });
            }
            "tool" => {
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                let tool_content = msg
                    .content
                    .as_ref()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();

                let tool_result = serde_json::json!({
                    "toolUseId": tool_use_id,
                    "status": "success",
                    "content": [{"text": tool_content}]
                });

                raw_turns.push(KiroHistoryTurn::User {
                    user_input_message: KiroUserInputMessage {
                        content: String::new(),
                        model_id: model.to_string(),
                        origin: "AI_EDITOR".to_string(),
                        user_input_message_context: Some(KiroUserInputMessageContext {
                            tools: None,
                            tool_results: Some(vec![tool_result]),
                        }),
                    },
                });
            }
            "assistant" => {
                let text = msg
                    .content
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                let tool_uses = msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .filter_map(|tc| {
                            let tool_use_id = tc.get("id")?.as_str()?;
                            let func = tc.get("function")?;
                            let name = func.get("name")?.as_str()?;
                            let args_val = func.get("arguments");
                            let input_val = match args_val {
                                Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                                    .unwrap_or(serde_json::json!({})),
                                Some(v) => v.clone(),
                                None => serde_json::json!({}),
                            };
                            Some(serde_json::json!({
                                "toolUseId": tool_use_id,
                                "name": name,
                                "input": input_val
                            }))
                        })
                        .collect::<Vec<_>>()
                });

                raw_turns.push(KiroHistoryTurn::Assistant {
                    assistant_response_message: KiroAssistantResponseMessage {
                        content: text,
                        tool_uses,
                    },
                });
            }
            _ => {}
        }
    }

    // Collapse adjacent User and Assistant turns so history strictly alternates
    let mut collapsed: Vec<KiroHistoryTurn> = Vec::new();
    for turn in raw_turns {
        match (collapsed.last_mut(), turn) {
            (
                Some(KiroHistoryTurn::User {
                    user_input_message: prev,
                }),
                KiroHistoryTurn::User {
                    user_input_message: next,
                },
            ) => {
                if !next.content.is_empty() {
                    if prev.content.is_empty() {
                        prev.content = next.content;
                    } else {
                        prev.content.push_str("\n\n");
                        prev.content.push_str(&next.content);
                    }
                }
                if let Some(next_ctx) = next.user_input_message_context {
                    let prev_ctx = prev
                        .user_input_message_context
                        .get_or_insert_with(Default::default);
                    if let Some(results) = next_ctx.tool_results {
                        let prev_results = prev_ctx.tool_results.get_or_insert_with(Vec::new);
                        prev_results.extend(results);
                    }
                }
            }
            (
                Some(KiroHistoryTurn::Assistant {
                    assistant_response_message: prev,
                }),
                KiroHistoryTurn::Assistant {
                    assistant_response_message: next,
                },
            ) => {
                if !next.content.is_empty() {
                    if prev.content.is_empty() {
                        prev.content = next.content;
                    } else {
                        prev.content.push_str("\n\n");
                        prev.content.push_str(&next.content);
                    }
                }
                if let Some(next_uses) = next.tool_uses {
                    let prev_uses = prev.tool_uses.get_or_insert_with(Vec::new);
                    prev_uses.extend(next_uses);
                }
            }
            (_, next_turn) => {
                collapsed.push(next_turn);
            }
        }
    }

    // Extract the final user turn for currentMessage
    let mut current_user_msg = match collapsed.pop() {
        Some(KiroHistoryTurn::User { user_input_message }) => user_input_message,
        Some(KiroHistoryTurn::Assistant {
            assistant_response_message,
        }) => {
            collapsed.push(KiroHistoryTurn::Assistant {
                assistant_response_message,
            });
            KiroUserInputMessage {
                content: "(empty)".to_string(),
                model_id: model.to_string(),
                origin: "AI_EDITOR".to_string(),
                user_input_message_context: None,
            }
        }
        None => KiroUserInputMessage {
            content: "(empty)".to_string(),
            model_id: model.to_string(),
            origin: "AI_EDITOR".to_string(),
            user_input_message_context: None,
        },
    };

    // Attach tools to currentMessage
    if let Some(tool_specs) = tools {
        let ctx = current_user_msg
            .user_input_message_context
            .get_or_insert_with(Default::default);
        ctx.tools = Some(tool_specs);
    }

    (collapsed, current_user_msg)
}
