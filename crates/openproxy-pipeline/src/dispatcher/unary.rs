//! Dispatch no-streaming: ciclo de vida de una respuesta unary completa.

use crate::translation::OpenAIResponse;
use openproxy_adapters::upstream::UpstreamRequest;

/// Inserta los pares (k, v) que parseen como `HeaderName`/`HeaderValue`.
/// Los malformados se descartan.
pub(crate) fn populate_upstream_headers(
    upstream_request: &mut UpstreamRequest,
    headers: &[(String, String)],
) {
    upstream_request.headers.reserve(headers.len());
    for (k, v) in headers {
        if k.to_ascii_lowercase().starts_with("x-openproxy-") {
            continue;
        }
        if let (Ok(name), Ok(mut value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(v),
        ) {
            // `HeaderValue::from_str` crea un valor no sensible; marcar el
            // bearer de nuevo para que Debug/redaction no filtren el token.
            if name == http::header::AUTHORIZATION {
                value.set_sensitive(true);
            }
            upstream_request.headers.insert(name, value);
        }
    }
}

/// Anuncia `Accept: text/event-stream` en requests de inferencia streaming
/// cuando el adapter no declaró su propio Accept. Algunos upstreams (p.ej. el
/// endpoint mavis de MiniMax) condicionan el flush incremental de chunks a
/// este header: sin él bufferizan el cuerpo completo aunque el JSON diga
/// `stream:true`. Los adapters que ya fijan Accept (p.ej. atomesus) se
/// respetan. En no-streaming no se toca.
pub(crate) fn ensure_streaming_accept(upstream_request: &mut UpstreamRequest) {
    if upstream_request.is_streaming && !upstream_request.headers.contains_key(http::header::ACCEPT)
    {
        upstream_request.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("text/event-stream"),
        );
    }
}

pub(crate) use super::translation::ensure_usage_populated;
#[cfg(test)]
pub(crate) use super::translation::translate_non_streaming_body;

/// `true` cuando la respuesta 200 no trae contenido útil. Se trata como
/// error para forzar el retry.
pub(crate) fn is_empty_response(resp: &OpenAIResponse) -> bool {
    resp.choices.first().is_some_and(|c| {
        let msg = &c.message;
        let content_empty = msg
            .content
            .as_ref()
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty));
        let no_tool_calls = msg.tool_calls.as_ref().is_none_or(std::vec::Vec::is_empty);
        let no_reasoning = !msg.extra.contains_key("reasoning_content");
        let no_finish = c
            .finish_reason
            .as_ref()
            .is_none_or(|f| f == "null" || f.is_empty());
        content_empty && no_tool_calls && no_reasoning && no_finish
    })
}

pub(super) mod lifecycle;
pub(super) mod send;
pub(super) mod sink;

#[cfg(test)]
mod ensure_usage_tests {
    use super::*;
    use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse, OpenAIUsage};

    #[test]
    fn test_ensure_usage_populates_missing() {
        let messages = vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(
                "Hello world from unit test".to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }];

        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(
                        "Hi! This is a test response.".to_string(),
                    )),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        ensure_usage_populated(&mut resp, &messages);

        assert!(resp.usage.is_some());
        let u = resp.usage.unwrap();
        assert!(
            u.prompt_tokens >= 4,
            "expected at least 4 prompt tokens, got {}",
            u.prompt_tokens
        );
        assert!(
            u.completion_tokens >= 4,
            "expected at least 4 completion tokens, got {}",
            u.completion_tokens
        );
        assert_eq!(u.total_tokens, u.prompt_tokens + u.completion_tokens);
    }

    #[test]
    fn test_ensure_usage_preserves_existing() {
        let messages = vec![];
        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![],
            usage: Some(OpenAIUsage {
                prompt_tokens: 42,
                completion_tokens: 18,
                total_tokens: 60,
                prompt_tokens_details: None,
            }),
        };

        ensure_usage_populated(&mut resp, &messages);

        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 42);
        assert_eq!(u.completion_tokens, 18);
        assert_eq!(u.total_tokens, 60);
    }
}
