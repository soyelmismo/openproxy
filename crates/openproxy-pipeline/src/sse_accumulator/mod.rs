//! Streaming response body accumulator.
//!
//! Assembles a single OpenAI-style `chat.completion` JSON value from a
//! streaming upstream turn, so `usage.response_body_json` is non-NULL for
//! streaming rows as it is for unary ones.
//!
//! Spec: docs/specs/gate-G1-streaming-response-body-persistence.md
//!
//! Past `MAX_ACCUMULATED_BYTES` the accumulator sets `truncated` and the
//! JSON's `extra` map carries `{"truncated": true}`.

pub mod accumulator;
pub mod parser;
pub mod types;

#[cfg(test)]
mod tests;

pub use accumulator::ResponseAccumulator;
#[cfg(test)]
pub(crate) use parser::extract_delta_fields;
pub use parser::{
    decode_json_escape_into, extract_reasoning_content, normalize_nonstandard_reasoning_fields,
};
pub use types::{
    AccumulatedToolCall, AnthropicToolEvent, AnthropicToolOpen, MAX_ACCUMULATED_BYTES,
};
