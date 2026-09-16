//! Anthropic tool use stateful accumulator.

use openproxy_types::error::{CoreError, Result};

/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious input_json_delta fragments.
pub(crate) const MAX_TOOL_ARGUMENTS_BYTES: usize = 1_048_576; // 1 MiB

/// Maximum allowed length for tool call ID string.
pub(crate) const MAX_TOOL_ID_BYTES: usize = 256;

/// Maximum allowed length for tool call name string.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 256;

#[derive(Debug, Default, Clone)]
pub struct AnthropicToolUseAccumulator {
    /// Index of the tool call within the assistant message's `tool_calls` array.
    pub index: u32,
    /// Anthropic `id` (e.g. "toolu_01ABC"). Emitted once at start.
    pub id: String,
    /// Function name (e.g. "get_weather"). Emitted once at start.
    pub name: String,
    /// Accumulated partial JSON fragments from input_json_delta.
    pub arguments: String,
}

impl AnthropicToolUseAccumulator {
    /// Create a new accumulator with bounds checking.
    pub fn new_with_bounds(index: u32, id: String, name: String) -> Result<Self> {
        if id.len() > MAX_TOOL_ID_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use id exceeds maximum length of {MAX_TOOL_ID_BYTES} bytes"
            )));
        }
        if name.len() > MAX_TOOL_NAME_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use name exceeds maximum length of {MAX_TOOL_NAME_BYTES} bytes"
            )));
        }
        Ok(Self {
            index,
            id,
            name,
            arguments: String::new(),
        })
    }

    /// Append to arguments with bounds checking.
    pub fn push_arguments(&mut self, fragment: &str) -> Result<()> {
        if self.arguments.len() + fragment.len() > MAX_TOOL_ARGUMENTS_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use arguments exceeds maximum length of {MAX_TOOL_ARGUMENTS_BYTES} bytes"
            )));
        }
        self.arguments.push_str(fragment);
        Ok(())
    }
}
