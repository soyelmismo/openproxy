//! Anthropic SSE parsing and OpenAI translation.

pub mod accumulator;
pub mod events;
pub mod parser;
pub mod payload;
pub mod usage;

pub use accumulator::AnthropicToolUseAccumulator;
pub use events::translate_anthropic_sse_event;
pub use parser::parse_anthropic_sse_stream_line;
pub use payload::translate_anthropic_sse_payload;
pub(crate) use usage::merge_usage;

#[cfg(test)]
pub(crate) use payload::{build_anthropic_message_start_chunk, translate_anthropic_message_delta};

#[cfg(test)]
mod tests;
