#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_types::OpenAIMessage;
use serde_json::json;

fn msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content: Some(json!(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }
}

#[path = "tier1_features_a.rs"]
mod tier1_features_a;
#[path = "tier1_features_b.rs"]
mod tier1_features_b;
#[path = "tier1_features_c.rs"]
mod tier1_features_c;
