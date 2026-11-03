use crate::sse::UpstreamSseChunk;
use openproxy_adapters::adapters::kiro::{KiroEventType, KiroFrameEvent};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct KiroStreamState {
    pub tool_index_by_id: HashMap<String, u32>,
    pub next_tool_index: u32,
    pub saw_tool_calls: bool,
}

impl KiroStreamState {
    pub fn get_or_create_tool_index(&mut self, id: &str) -> (u32, bool) {
        if let Some(&idx) = self.tool_index_by_id.get(id) {
            (idx, false)
        } else {
            let idx = self.next_tool_index;
            self.next_tool_index += 1;
            self.tool_index_by_id.insert(id.to_string(), idx);
            (idx, true)
        }
    }
}

pub(crate) fn kiro_frame_to_chunk(
    state: &mut KiroStreamState,
    frame: &KiroFrameEvent,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Option<UpstreamSseChunk> {
    match &frame.event_type {
        KiroEventType::AssistantResponse => {
            let json = serde_json::from_slice::<Value>(&frame.payload).ok()?;
            let text = json.get("content").and_then(Value::as_str)?;
            if text.is_empty() {
                return None;
            }
            Some(crate::sse::make_text_delta(
                chunk_id, created, model_name, text, false,
            ))
        }
        KiroEventType::ToolUse => {
            let json = serde_json::from_slice::<Value>(&frame.payload).ok()?;
            let tool_use_id = json.get("toolUseId").and_then(Value::as_str).unwrap_or("");
            let name = json.get("name").and_then(Value::as_str).unwrap_or("");
            let input = json.get("input").and_then(Value::as_str).unwrap_or("");

            state.saw_tool_calls = true;
            let (idx, is_new) = state.get_or_create_tool_index(tool_use_id);

            if is_new && !name.is_empty() {
                // Emit start chunk with tool name
                Some(crate::sse::make_tool_call_start(
                    chunk_id,
                    created,
                    model_name,
                    idx,
                    tool_use_id,
                    name,
                ))
            } else if !input.is_empty() {
                // Emit delta chunk with argument fragment
                Some(crate::sse::make_tool_call_delta(
                    chunk_id, created, model_name, idx, input,
                ))
            } else {
                None
            }
        }
        KiroEventType::MessageStop => {
            let finish_reason = if state.saw_tool_calls {
                "tool_calls"
            } else {
                "stop"
            };
            Some(kiro_done_chunk(
                chunk_id,
                created,
                model_name,
                finish_reason,
            ))
        }
        KiroEventType::ContextUsage => {
            // Optional: extract usage if needed
            None
        }
        _ => None,
    }
}

pub(crate) fn kiro_done_chunk(
    chunk_id: &str,
    created: u64,
    model_name: &str,
    finish_reason: &str,
) -> UpstreamSseChunk {
    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model_name,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason,
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: true,
        usage: None,
        stop_reason: Some(finish_reason.to_string()),
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: false,
    }
}
