mod diagnostics;
mod identity;
mod request;
mod response;
mod reverse;
#[cfg(test)]
mod tests;

pub use identity::{
    CLAUDE_AGENT_SDK_IDENTITY, CLAUDE_CODE_CLI_IDENTITY, normalize_claude_client_identity,
};
pub use request::openai_to_anthropic;
pub use response::{anthropic_to_openai, map_finish_reason};
pub use reverse::{anthropic_request_to_openai, openai_response_to_anthropic};
