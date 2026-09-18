//! Streaming response body accumulator.
//!
//! Gathers chunks received during a streaming upstream turn and assembles
//! a single OpenAI-style `chat.completion` JSON value at the end, so the
//! persisted `usage.response_body_json` column is non-NULL for streaming
//! rows (matching the non-streaming behavior).
//!
//! Spec: docs/specs/gate-G1-streaming-response-body-persistence.md
//!
//! Cap: `MAX_ACCUMULATED_BYTES = 256 KiB` (262,144 bytes). When the accumulated text would
//! exceed this, `truncated` is set to `true` and the JSON's `extra` map
//! carries `{"truncated": true}`. This bounds heap usage under high
//! concurrency (50 concurrent streams × 256 KiB = 12.8 MiB worst case).

pub mod accumulator;
pub mod parser;
pub mod types;

#[cfg(test)]
mod tests;

pub use accumulator::ResponseAccumulator;
pub use parser::{
    decode_json_escape_into, extract_reasoning_content, normalize_nonstandard_reasoning_fields,
};
pub use types::{
    AccumulatedToolCall, AnthropicToolEvent, AnthropicToolOpen, MAX_ACCUMULATED_BYTES,
};
