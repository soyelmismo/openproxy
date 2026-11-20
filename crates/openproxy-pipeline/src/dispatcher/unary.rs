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
#[cfg(test)]
pub(crate) use super::translation::translate_non_streaming_body_with_presence;

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

    /// Probe regression: upstream unary bodies may omit `completion_tokens`.
    /// The strict `OpenAIUsage` deserializer must not reject the whole
    /// response before presence/estimation can run.
    #[test]
    fn openai_unary_usage_without_completion_tokens_deserializes() {
        let body = serde_json::json!({
            "id": "c1",
            "object": "chat.completion",
            "created": 1,
            "model": "m",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 17, "total_tokens": 17}
        });
        let resp: OpenAIResponse = serde_json::from_value(body).expect("usage must parse");
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 17);
    }

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

        let fill = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages,
                ..Default::default()
            },
            None,
            crate::dispatcher::translation::UsagePresence::none(),
            openproxy_compression::token_estimate::PromptRail::Translated,
        );
        assert_eq!(fill.estimated, (true, true));
        assert!(fill.observed.is_none());

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

        let fill = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages,
                ..Default::default()
            },
            None,
            crate::dispatcher::translation::UsagePresence::both(),
            openproxy_compression::token_estimate::PromptRail::Translated,
        );
        assert_eq!(fill.estimated, (false, false));
        assert_eq!(
            fill.observed,
            Some(crate::dispatcher::translation::ObservedUsage {
                prompt: Some(42),
                completion: Some(18),
                total: None,
                cached: None,
            })
        );

        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 42);
        assert_eq!(u.completion_tokens, 18);
        assert_eq!(u.total_tokens, 60);
    }

    #[test]
    fn test_ensure_usage_explicit_zero_reaches_the_wire_unchanged() {
        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String("Hello".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 7,
                total_tokens: 7,
                prompt_tokens_details: None,
            }),
        };

        let fill = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages: vec![OpenAIMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::Value::String("Hello world".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                }],
                ..Default::default()
            },
            None,
            crate::dispatcher::translation::UsagePresence::both(),
            openproxy_compression::token_estimate::PromptRail::Translated,
        );

        // The explicit zero is a valid observation and must reach the tracker
        // as zero, not as an estimate.
        assert_eq!(
            fill.observed,
            Some(crate::dispatcher::translation::ObservedUsage {
                prompt: Some(0),
                completion: Some(7),
                total: None,
                cached: None,
            })
        );
        assert_eq!(fill.estimated, (false, false));
        // No documented client contract justifies raising an observed zero, so
        // the wire keeps the upstream value and only absent fields are
        // estimated.
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[test]
    fn test_ensure_usage_missing_field_estimates_only_that_field() {
        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String("Hello".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 7,
                total_tokens: 7,
                prompt_tokens_details: None,
            }),
        };

        let fill = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest {
                messages: vec![OpenAIMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::Value::String("Hello world".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                }],
                ..Default::default()
            },
            None,
            crate::dispatcher::translation::UsagePresence {
                prompt: false,
                completion: true,
                total: false,
                cached: false,
            },
            openproxy_compression::token_estimate::PromptRail::Translated,
        );

        assert_eq!(fill.estimated, (true, false));
        assert_eq!(
            fill.observed,
            Some(crate::dispatcher::translation::ObservedUsage {
                prompt: None,
                completion: Some(7),
                total: None,
                cached: None,
            })
        );
        let usage = resp.usage.unwrap();
        assert!(usage.prompt_tokens >= 1);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[test]
    fn test_usage_presence_is_per_field_and_dialect_aware() {
        use crate::dispatcher::translation::{
            UsagePresence, openai_usage_presence, upstream_usage_presence,
        };

        let openai = serde_json::json!({
            "usage": {"prompt_tokens": 0, "completion_tokens": 12}
        });
        assert_eq!(
            openai_usage_presence(&openai),
            UsagePresence {
                prompt: true,
                completion: true,
                total: false,
                cached: false,
            }
        );

        let anthropic = serde_json::json!({
            "usage": {"input_tokens": 0, "output_tokens": 9}
        });
        assert_eq!(
            upstream_usage_presence(openproxy_types::TargetFormat::Anthropic, &anthropic),
            UsagePresence {
                prompt: true,
                completion: true,
                total: false,
                cached: false,
            }
        );

        let gemini = serde_json::json!({
            "usageMetadata": {"promptTokenCount": 0, "candidatesTokenCount": 4}
        });
        assert_eq!(
            upstream_usage_presence(openproxy_types::TargetFormat::Gemini, &gemini),
            UsagePresence {
                prompt: true,
                completion: true,
                total: false,
                cached: false,
            }
        );

        // Gemini `response` wrapper plus snake_case aliases, the shapes the
        // adapter already accepts. Cached tokens are reported per field.
        let gemini_wrapped = serde_json::json!({
            "response": {
                "usage_metadata": {
                    "prompt_token_count": 40,
                    "candidates_token_count": 3,
                    "total_token_count": 43,
                    "cached_content_token_count": 32
                }
            }
        });
        assert_eq!(
            upstream_usage_presence(openproxy_types::TargetFormat::Gemini, &gemini_wrapped),
            UsagePresence {
                prompt: true,
                completion: true,
                total: true,
                cached: true,
            }
        );

        // Unary Gemini metadata is deserialized into a zero-filled struct, so
        // presence stays per field: the prompt is observed, the missing
        // completion is estimated. (The SSE translator drops partial metadata
        // instead, and its own test covers that difference.)
        let gemini_partial = serde_json::json!({
            "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 5}
        });
        assert_eq!(
            upstream_usage_presence(openproxy_types::TargetFormat::Gemini, &gemini_partial),
            UsagePresence {
                prompt: true,
                completion: false,
                total: true,
                cached: false,
            }
        );

        // Anthropic cache fields count as cached even when the prompt field
        // arrives through the cache-specific aliases.
        let anthropic_cached = serde_json::json!({
            "usage": {
                "input_tokens": 10,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 7,
                "output_tokens": 2
            }
        });
        assert_eq!(
            upstream_usage_presence(openproxy_types::TargetFormat::Anthropic, &anthropic_cached),
            UsagePresence {
                prompt: true,
                completion: true,
                total: false,
                cached: true,
            }
        );
    }

    #[test]
    fn partial_upstream_usage_keeps_the_observed_prompt() {
        // Probe case: prompt_tokens=17 and total_tokens=17 with no
        // completion_tokens. The prompt is observed; only completion is
        // estimated, and the total is recomputed because the body was adjusted.
        let mut resp = OpenAIResponse {
            id: "c1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "m".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String("ok".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 17,
                completion_tokens: 0,
                total_tokens: 17,
                prompt_tokens_details: None,
            }),
        };
        let presence = crate::dispatcher::translation::UsagePresence {
            prompt: true,
            completion: false,
            total: true,
            cached: false,
        };
        let fill = ensure_usage_populated(
            &mut resp,
            &openproxy_types::OpenAIRequest::default(),
            Some("gpt-4"),
            presence,
            openproxy_compression::token_estimate::PromptRail::Translated,
        );
        assert_eq!(fill.estimated, (false, true));
        let observed = fill.observed.expect("observed snapshot");
        assert_eq!(observed.prompt, Some(17));
        assert_eq!(observed.completion, None);
        let usage = resp.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert!(usage.completion_tokens >= 1);
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens
        );
    }

    #[test]
    fn recorded_snapshot_survives_the_client_prefix_discount() {
        // The tracker persists `recorded_usage`; the discount only mutates the
        // client wire afterwards. Snapshot-before-discount keeps them apart.
        let mut usage = OpenAIUsage {
            prompt_tokens: 120,
            completion_tokens: 8,
            total_tokens: 128,
            prompt_tokens_details: None,
        };
        let recorded = usage.clone();
        crate::dispatcher::translation::discount_client_prefix(&mut usage, 100);
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.total_tokens, 28);
        assert_eq!(recorded.prompt_tokens, 120);
        assert_eq!(recorded.total_tokens, 128);
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
