//! Wire types for the OpenAI Responses protocol surface that
//! openproxy exposes at `POST /v1/responses`.
//!
//! See `docs/specs/antigravity-gaps-p2.md` §3 (GAP-2) for the spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `POST /v1/responses` request body.
///
/// Mirrors the public OpenAI Responses API shape: a list of `input` items
/// (each tagged with its `type`), an optional `instructions` string
/// prepended as a system message, and pass-through `tools` / `tool_choice`
/// fields that share the OpenAI function-calling shape verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// Model name to route through the pipeline.
    pub model: String,
    /// Optional system instructions (prepended as a `system` message).
    #[serde(default)]
    pub instructions: Option<String>,
    /// Ordered list of input items.
    #[serde(default)]
    pub input: Vec<ResponsesInputItem>,
    /// Tools (pass-through; Responses and OpenAI share the
    /// `{type:"function", function:{...}}` shape).
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    /// Stored-response chain. Not implemented in MVP — we log a warning
    /// and proceed without it.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Unknown fields are preserved verbatim so the proxy can pass them
    /// to upstreams that understand them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One entry in the `input[]` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    /// A plain chat-style message (`role` + `content`).
    Message {
        role: String,
        content: ResponsesContent,
    },
    /// Assistant-side function call emission (re-injected into the
    /// conversation history to restore assistant tool-call state).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Tool-side function result (re-injected to restore tool results).
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    /// Forward-compatible: new item types the proxy doesn't know
    /// about are dropped (with a debug log).
    #[serde(other)]
    Unknown,
}

/// Message content can be a plain string OR an array of parts
/// (e.g. `[{type:"input_text", text:"hi"}, {type:"input_image", ...}]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Plain(String),
    Parts(Vec<Value>),
}