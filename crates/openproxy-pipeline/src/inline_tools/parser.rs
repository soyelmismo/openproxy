use super::types::ParsedToolCall;

/// Trait for an inline tool call parser strategy.
///
/// Implementations recognize and parse specific syntax variations
/// (e.g. MiniMax XML, Hermes JSON, etc.).
pub trait InlineToolParser: Send + Sync {
    /// Attempts to parse tool calls from a recognized block of text.
    ///
    /// Returns `Some(vec)` if the block matches this parser's syntax and contains
    /// at least one valid tool call. Returns `None` if the syntax is not recognized.
    fn parse_block(&self, block: &str) -> Option<Vec<ParsedToolCall>>;
}
