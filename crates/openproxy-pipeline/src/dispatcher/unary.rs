//! Dispatch no-streaming: ciclo de vida de una respuesta unary completa.

use crate::translation::OpenAIResponse;
use openproxy_adapters::upstream::UpstreamRequest;

/// Inserta los pares (k, v) que parseen como `HeaderName`/`HeaderValue`.
/// Los malformados se descartan.
///
/// Security (audit H-6): los nombres de cabecera configurables por el
/// operador (los `extra_headers` de un proveedor custom llegan aquí vía
/// `params.headers`) se filtran contra una denylist de cabeceras de framing
/// y hop-by-hop. Un `content-length` o `transfer-encoding` ajeno desincroniza
/// el pool HTTP/1.1 keep-alive hacia el upstream (request smuggling de primer
/// orden contra el propio gateway); un `host` ajeno al SNI habilita confusión
/// de virtual-host en CDNs. `hyper 1.x` revalida parte del framing, pero la
/// denegación en la frontera es la corrección de raíz.
const FORBIDDEN_UPSTREAM_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
    "host",
];

pub(crate) fn populate_upstream_headers(
    upstream_request: &mut UpstreamRequest,
    headers: &[(String, String)],
) {
    upstream_request.headers.reserve(headers.len());
    for (k, v) in headers {
        if k.to_ascii_lowercase().starts_with("x-openproxy-") {
            continue;
        }
        let lower = k.to_ascii_lowercase();
        if FORBIDDEN_UPSTREAM_HEADERS.contains(&lower.as_str()) {
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

        let estimated = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages,
                ..Default::default()
            },
        );
        assert_eq!(estimated, (true, true));

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

        let estimated = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages,
                ..Default::default()
            },
        );
        assert_eq!(estimated, (false, false));

        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 42);
        assert_eq!(u.completion_tokens, 18);
        assert_eq!(u.total_tokens, 60);
    }
}

#[cfg(test)]
mod header_denylist_tests {
    use super::*;

    /// Security (audit H-6): framing / hop-by-hop headers configured as
    /// provider `extra_headers` must never reach the upstream request.
    #[test]
    fn populate_upstream_headers_drops_hop_by_hop_and_framing() {
        let mut req = UpstreamRequest::post_json("http://upstream.test/", bytes::Bytes::new());
        populate_upstream_headers(
            &mut req,
            &[
                ("content-length".to_string(), "0".to_string()),
                ("transfer-encoding".to_string(), "chunked".to_string()),
                ("connection".to_string(), "close".to_string()),
                ("host".to_string(), "evil.cdn.example".to_string()),
                ("keep-alive".to_string(), "timeout=1".to_string()),
                ("te".to_string(), "trailers".to_string()),
                ("upgrade".to_string(), "websocket".to_string()),
                // Case-insensitive rejection.
                ("Content-Length".to_string(), "999".to_string()),
                // Legitimate custom headers still pass through.
                ("x-custom-header".to_string(), "value".to_string()),
                ("authorization".to_string(), "Bearer tok".to_string()),
            ],
        );
        assert!(req.headers.get("content-length").is_none());
        assert!(req.headers.get("transfer-encoding").is_none());
        assert!(req.headers.get("connection").is_none());
        assert!(req.headers.get("host").is_none());
        assert!(req.headers.get("keep-alive").is_none());
        assert!(req.headers.get("te").is_none());
        assert!(req.headers.get("upgrade").is_none());
        assert_eq!(
            req.headers
                .get("x-custom-header")
                .and_then(|v| v.to_str().ok()),
            Some("value")
        );
        assert_eq!(
            req.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok")
        );
    }
}
