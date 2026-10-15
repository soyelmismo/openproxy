use super::*;

#[test]
fn gemini_cache_preserva_contador_en_stream_y_acumulador() {
    for wrapped in [false, true] {
        for cached in [None, Some(0), Some(8192)] {
            let mut body = json!({
                "candidates": [{"content": {"parts": [{"text": "OK"}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10000, "candidatesTokenCount": 2, "totalTokenCount": 10002}
            });
            if let Some(value) = cached {
                body["usageMetadata"]["cachedContentTokenCount"] = json!(value);
            }
            if wrapped {
                body = json!({"response": body});
            }
            let chunk = parse_gemini_sse_line(&format!("data: {body}"), "cache-test", 0, "gemini")
                .unwrap()
                .unwrap();
            let usage = chunk.usage.as_ref().unwrap();
            assert_eq!(usage.prompt_tokens, 10000);
            assert_eq!(
                usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens),
                cached
            );
            let mut acc = crate::sse_accumulator::ResponseAccumulator::new();
            acc.set_usage(usage.clone());
            let response = acc.finish("cache-test", 0, "gemini");
            assert_eq!(
                response["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64(),
                cached.map(u64::from)
            );
            assert_eq!(
                chunk.payload["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64(),
                cached.map(u64::from)
            );
        }
    }
}
