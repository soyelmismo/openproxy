use openproxy_core::executor_antigravity::*;
use openproxy_core::models::*;
use serde_json::json;

fn main() {
    let req = OpenAIRequest {
        model: "gemini-3.1-pro-low".to_string(),
        messages: vec![
            OpenAIMessage {
                role: "system".to_string(),
                content: Some(OpenAIMessageContent::Text("# Hermeona".to_string())),
                ..Default::default()
            }
        ],
        ..Default::default()
    };
    // No pub access, but we can just reason about it.
}
