//! SSE sink emission for a completed unary response.

use crate::translation::OpenAIResponse;

pub(super) async fn emit_unary_response_to_stream_sink(
    sink: &crate::race_sink::StreamSink,
    resp: &OpenAIResponse,
) {
    if matches!(sink, crate::race_sink::StreamSink::Discard) {
        return;
    }
    use std::fmt::Write;
    let chunk_id = &resp.id;
    let created = resp.created;
    let model = &resp.model;

    for choice in &resp.choices {
        let mut delta = serde_json::Map::new();
        delta.insert("role".to_string(), serde_json::json!(choice.message.role));
        if let Some(ref content) = choice.message.content {
            delta.insert("content".to_string(), content.clone());
        }
        if let Some(ref tc) = choice.message.tool_calls {
            delta.insert("tool_calls".to_string(), serde_json::json!(tc));
        }
        for (k, v) in &choice.message.extra {
            delta.insert(k.clone(), v.clone());
        }

        let chunk = serde_json::json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": choice.index,
                "delta": delta,
                "finish_reason": null,
            }],
        });
        if let Ok(json_str) = serde_json::to_string(&chunk) {
            let mut buf = String::with_capacity(json_str.len() + 8);
            let _ = write!(&mut buf, "data: {json_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf)).await;
        }

        let finish_chunk = serde_json::json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": choice.index,
                "delta": {},
                "finish_reason": choice.finish_reason.as_deref().unwrap_or("stop"),
            }],
            "usage": resp.usage,
        });
        if let Ok(json_str) = serde_json::to_string(&finish_chunk) {
            let mut buf = String::with_capacity(json_str.len() + 8);
            let _ = write!(&mut buf, "data: {json_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf)).await;
        }
    }
    let _ = sink.send(crate::pipeline::SSE_DONE_BYTES).await;
}
