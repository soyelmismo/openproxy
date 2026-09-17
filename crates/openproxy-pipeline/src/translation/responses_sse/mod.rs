//! Streaming bridge from internal OpenAI chat completion SSE chunks to OpenAI Responses API SSE events.
//!
//! Conforms to OpenAI Responses API streaming specifications:
//! - `response.created`
//! - `response.output_item.added`
//! - `response.output_text.delta`
//! - `response.reasoning_text.delta`
//! - `response.function_call_arguments.delta`
//! - `response.function_call_arguments.done`
//! - `response.output_item.done`
//! - `response.completed`
//! - `data: [DONE]\n\n`

mod types;
pub use types::*;

#[cfg(test)]
mod tests;

use crate::sse::{MAX_SSE_LINE_BYTES, SseParser, parse_sse_data_line};
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::stream::Stream;
use openproxy_types::OpenAIUsage;
use serde_json::json;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

fn append_sse_event(out: &mut BytesMut, event_name: &str, payload: &serde_json::Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event_name.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    if serde_json::to_writer((&mut *out).writer(), payload).is_err() {
        out.extend_from_slice(
            br#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#,
        );
    }
    out.extend_from_slice(b"\n\n");
}

pub struct OpenAIToResponsesSseStream<S> {
    inner: S,
    response_id: String,
    model: String,
    has_started: bool,
    has_completed: bool,
    reasoning_output_index: Option<usize>,
    reasoning_done: bool,
    accumulated_reasoning: String,
    reasoning_id: String,
    text_output_index: Option<usize>,
    text_done: bool,
    accumulated_text: String,
    message_id: String,
    tool_calls: Vec<StreamToolCall>,
    next_output_index: usize,
    usage: Option<OpenAIUsage>,
    parser: SseParser,
    out_queue: VecDeque<Bytes>,
}

impl<S> OpenAIToResponsesSseStream<S> {
    pub fn new(inner: S, response_id: String, model: String) -> Self {
        let message_id = format!("msg_{response_id}");
        let reasoning_id = format!("rs_{response_id}");
        Self {
            inner,
            response_id,
            model,
            has_started: false,
            has_completed: false,
            reasoning_output_index: None,
            reasoning_done: false,
            accumulated_reasoning: String::new(),
            reasoning_id,
            text_output_index: None,
            text_done: false,
            accumulated_text: String::new(),
            message_id,
            tool_calls: Vec::new(),
            next_output_index: 0,
            usage: None,
            parser: SseParser::new(MAX_SSE_LINE_BYTES),
            out_queue: VecDeque::new(),
        }
    }

    fn ensure_started(&mut self) {
        if self.has_started {
            return;
        }
        self.has_started = true;
        let start_event = json!({
            "type": "response.created",
            "response": {
                "id": self.response_id,
                "object": "response",
                "status": "in_progress",
                "model": self.model,
                "output": []
            }
        });
        let mut b = BytesMut::with_capacity(256);
        append_sse_event(&mut b, "response.created", &start_event);
        self.out_queue.push_back(b.freeze());
    }

    fn close_reasoning(&mut self) {
        if let Some(r_idx) = self.reasoning_output_index
            && !self.reasoning_done
        {
            let done_event = json!({
                "type": "response.output_item.done",
                "output_index": r_idx,
                "item": {
                    "id": self.reasoning_id,
                    "type": "reasoning",
                    "summary": [],
                    "content": [{
                        "type": "reasoning_text",
                        "text": self.accumulated_reasoning,
                    }]
                }
            });
            let mut b = BytesMut::with_capacity(256);
            append_sse_event(&mut b, "response.output_item.done", &done_event);
            self.out_queue.push_back(b.freeze());
            self.reasoning_done = true;
        }
    }

    fn close_text(&mut self) {
        if let Some(t_idx) = self.text_output_index
            && !self.text_done
        {
            let done_event = json!({
                "type": "response.output_item.done",
                "output_index": t_idx,
                "item": {
                    "id": self.message_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": self.accumulated_text,
                    }]
                }
            });
            let mut b = BytesMut::with_capacity(256);
            append_sse_event(&mut b, "response.output_item.done", &done_event);
            self.out_queue.push_back(b.freeze());
            self.text_done = true;
        }
    }

    fn close_tool_calls(&mut self) {
        for tc in &mut self.tool_calls {
            if !tc.done_emitted {
                let mut b = BytesMut::with_capacity(256);
                let args_done = json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": tc.output_index,
                    "call_id": tc.call_id,
                    "arguments": tc.arguments,
                });
                append_sse_event(&mut b, "response.function_call_arguments.done", &args_done);

                let item_done = json!({
                    "type": "response.output_item.done",
                    "output_index": tc.output_index,
                    "item": {
                        "id": tc.call_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": tc.call_id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }
                });
                append_sse_event(&mut b, "response.output_item.done", &item_done);
                self.out_queue.push_back(b.freeze());
                tc.done_emitted = true;
            }
        }
    }

    fn finish(&mut self) {
        if self.has_completed {
            return;
        }
        self.ensure_started();
        self.close_reasoning();
        self.close_text();
        self.close_tool_calls();

        if self.reasoning_output_index.is_none()
            && self.text_output_index.is_none()
            && self.tool_calls.is_empty()
        {
            let mut b = BytesMut::with_capacity(256);
            let add_event = json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": self.message_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            });
            append_sse_event(&mut b, "response.output_item.added", &add_event);

            let done_event = json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": self.message_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": ""
                    }]
                }
            });
            append_sse_event(&mut b, "response.output_item.done", &done_event);
            self.out_queue.push_back(b.freeze());
            self.text_output_index = Some(0);
            self.text_done = true;
        }

        let mut output_items = Vec::new();
        if self.reasoning_output_index.is_some() {
            output_items.push(json!({
                "id": self.reasoning_id,
                "type": "reasoning",
                "summary": [],
                "content": [{
                    "type": "reasoning_text",
                    "text": self.accumulated_reasoning,
                }]
            }));
        }
        if self.text_output_index.is_some() {
            output_items.push(json!({
                "id": self.message_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.accumulated_text,
                }]
            }));
        }
        for tc in &self.tool_calls {
            output_items.push(json!({
                "id": tc.call_id,
                "type": "function_call",
                "status": "completed",
                "call_id": tc.call_id,
                "name": tc.name,
                "arguments": tc.arguments,
            }));
        }

        let (prompt_tokens, completion_tokens, total_tokens, cached_tokens) = match &self.usage {
            Some(u) => {
                let cached = u
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0);
                (u.prompt_tokens, u.completion_tokens, u.total_tokens, cached)
            }
            None => (0, 0, 0, 0),
        };

        let usage_val = json!({
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens,
            "total_tokens": total_tokens,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "input_tokens_details": {
                "cached_tokens": cached_tokens,
            },
            "output_tokens_details": {
                "reasoning_tokens": 0,
            }
        });

        let completed_event = json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "status": "completed",
                "model": self.model,
                "output": output_items,
                "usage": usage_val,
            }
        });

        let mut b = BytesMut::with_capacity(512);
        append_sse_event(&mut b, "response.completed", &completed_event);
        b.extend_from_slice(b"data: [DONE]\n\n");
        self.out_queue.push_back(b.freeze());
        self.has_completed = true;
    }

    fn process_incoming_chunk(&mut self, chunk: &[u8]) {
        if self.parser.push(chunk).is_err() {
            return;
        }

        while let Some(line_bytes) = self.parser.next_line() {
            let Ok(line) = std::str::from_utf8(&line_bytes) else {
                continue;
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed.starts_with(':') {
                let mut b = BytesMut::with_capacity(trimmed.len() + 2);
                b.extend_from_slice(trimmed.as_bytes());
                b.extend_from_slice(b"\n\n");
                self.out_queue.push_back(b.freeze());
                continue;
            }

            if trimmed.starts_with("event:") {
                continue;
            }

            let Some(payload) = parse_sse_data_line(trimmed) else {
                continue;
            };

            if payload == "[DONE]" {
                self.finish();
                continue;
            }

            self.process_payload(payload);
        }
    }

    fn process_payload(&mut self, payload: &str) {
        if let Ok(err_probe) = serde_json::from_str::<ErrorProbe<'_>>(payload)
            && (err_probe.error.is_some()
                || err_probe.r#type.as_deref() == Some("error")
                || err_probe.r#type.as_deref() == Some("response.failed"))
        {
            if err_probe.r#type.as_deref() == Some("response.failed") {
                let mut b = BytesMut::with_capacity(payload.len() + 32);
                b.extend_from_slice(b"event: response.failed\ndata: ");
                b.extend_from_slice(payload.as_bytes());
                b.extend_from_slice(b"\n\n");
                self.out_queue.push_back(b.freeze());
                self.has_completed = true;
                return;
            }
            let (code, msg) = match err_probe.error {
                Some(serde_json::Value::Object(map)) => {
                    let c = map
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("internal_error")
                        .to_string();
                    let m = map
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("upstream error")
                        .to_string();
                    (c, m)
                }
                Some(serde_json::Value::String(s)) => ("internal_error".to_string(), s),
                _ => {
                    let c = err_probe
                        .code
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .unwrap_or("internal_error")
                        .to_string();
                    let m = err_probe
                        .message
                        .as_deref()
                        .unwrap_or("upstream error")
                        .to_string();
                    (c, m)
                }
            };
            let fail_event = json!({
                "type": "response.failed",
                "response": {
                    "id": self.response_id,
                    "status": "failed",
                    "error": {
                        "code": code,
                        "message": msg,
                    }
                }
            });
            let mut b = BytesMut::with_capacity(256);
            append_sse_event(&mut b, "response.failed", &fail_event);
            self.out_queue.push_back(b.freeze());
            self.has_completed = true;
            return;
        }

        let Ok(probe) = serde_json::from_str::<ResponsesSseProbe<'_>>(payload) else {
            return;
        };

        if let Some(id) = probe.id
            && self.response_id.starts_with("resp_req_")
        {
            self.response_id = format!("resp_{id}");
        }

        if let Some(u) = probe.usage {
            self.usage = Some(u.to_openai_usage());
        }

        self.ensure_started();

        let Some(choices) = probe.choices else {
            return;
        };

        for c in choices {
            if let Some(delta) = c.delta {
                // Reasoning
                if let Some(reasoning) =
                    delta.reasoning_content.as_deref().filter(|s| !s.is_empty())
                {
                    let r_idx = match self.reasoning_output_index {
                        Some(idx) => idx,
                        None => {
                            let idx = self.next_output_index;
                            self.next_output_index += 1;
                            self.reasoning_output_index = Some(idx);
                            let add_event = json!({
                                "type": "response.output_item.added",
                                "output_index": idx,
                                "item": {
                                    "id": self.reasoning_id,
                                    "type": "reasoning",
                                }
                            });
                            let mut b = BytesMut::with_capacity(256);
                            append_sse_event(&mut b, "response.output_item.added", &add_event);
                            self.out_queue.push_back(b.freeze());
                            idx
                        }
                    };
                    let delta_event = json!({
                        "type": "response.reasoning_text.delta",
                        "output_index": r_idx,
                        "delta": reasoning,
                    });
                    let mut b = BytesMut::with_capacity(256);
                    append_sse_event(&mut b, "response.reasoning_text.delta", &delta_event);
                    self.out_queue.push_back(b.freeze());
                    self.accumulated_reasoning.push_str(reasoning);
                }

                // Text
                if let Some(text) = delta.content.as_deref().filter(|s| !s.is_empty()) {
                    self.close_reasoning();
                    let t_idx = match self.text_output_index {
                        Some(idx) => idx,
                        None => {
                            let idx = self.next_output_index;
                            self.next_output_index += 1;
                            self.text_output_index = Some(idx);
                            let add_event = json!({
                                "type": "response.output_item.added",
                                "output_index": idx,
                                "item": {
                                    "id": self.message_id,
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": []
                                }
                            });
                            let mut b = BytesMut::with_capacity(256);
                            append_sse_event(&mut b, "response.output_item.added", &add_event);
                            self.out_queue.push_back(b.freeze());
                            idx
                        }
                    };
                    let delta_event = json!({
                        "type": "response.output_text.delta",
                        "output_index": t_idx,
                        "content_index": 0,
                        "delta": text,
                    });
                    let mut b = BytesMut::with_capacity(256);
                    append_sse_event(&mut b, "response.output_text.delta", &delta_event);
                    self.out_queue.push_back(b.freeze());
                    self.accumulated_text.push_str(text);
                }

                // Tool calls
                if let Some(tool_calls) = delta.tool_calls {
                    self.close_reasoning();
                    self.close_text();

                    for tc in tool_calls {
                        let tc_idx = tc.index.unwrap_or(0);
                        let pos = self.tool_calls.iter().position(|t| t.tool_index == tc_idx);
                        let (out_idx, call_id) = match pos {
                            Some(p) => (
                                self.tool_calls[p].output_index,
                                self.tool_calls[p].call_id.clone(),
                            ),
                            None => {
                                let out_idx = self.next_output_index;
                                self.next_output_index += 1;
                                let id = tc.id.as_deref().unwrap_or("").to_string();
                                let call_id = if id.is_empty() {
                                    format!("call_{out_idx}")
                                } else {
                                    id
                                };
                                let name = tc
                                    .function
                                    .as_ref()
                                    .and_then(|f| f.name.as_deref())
                                    .unwrap_or("")
                                    .to_string();

                                let add_event = json!({
                                    "type": "response.output_item.added",
                                    "output_index": out_idx,
                                    "item": {
                                        "id": call_id,
                                        "type": "function_call",
                                        "status": "in_progress",
                                        "call_id": call_id,
                                        "name": name,
                                        "arguments": ""
                                    }
                                });
                                let mut b = BytesMut::with_capacity(256);
                                append_sse_event(&mut b, "response.output_item.added", &add_event);
                                self.out_queue.push_back(b.freeze());

                                self.tool_calls.push(StreamToolCall {
                                    tool_index: tc_idx,
                                    output_index: out_idx,
                                    call_id: call_id.clone(),
                                    name,
                                    arguments: String::new(),
                                    done_emitted: false,
                                });
                                (out_idx, call_id)
                            }
                        };

                        if let Some(func) = tc.function
                            && let Some(args) = func.arguments.as_deref().filter(|s| !s.is_empty())
                        {
                            if let Some(t) =
                                self.tool_calls.iter_mut().find(|t| t.tool_index == tc_idx)
                            {
                                t.arguments.push_str(args);
                            }
                            let delta_event = json!({
                                "type": "response.function_call_arguments.delta",
                                "output_index": out_idx,
                                "call_id": call_id,
                                "delta": args,
                            });
                            let mut b = BytesMut::with_capacity(256);
                            append_sse_event(
                                &mut b,
                                "response.function_call_arguments.delta",
                                &delta_event,
                            );
                            self.out_queue.push_back(b.freeze());
                        }
                    }
                }
            }

            if c.finish_reason.is_some() {
                self.close_reasoning();
                self.close_text();
                self.close_tool_calls();
            }
        }
    }
}

impl<S: Stream<Item = Bytes> + Unpin> Stream for OpenAIToResponsesSseStream<S> {
    type Item = std::result::Result<Bytes, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(chunk) = this.out_queue.pop_front() {
                return Poll::Ready(Some(Ok(chunk)));
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    this.process_incoming_chunk(&chunk);
                }
                Poll::Ready(None) => {
                    if !this.has_completed {
                        this.finish();
                        if let Some(chunk) = this.out_queue.pop_front() {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
