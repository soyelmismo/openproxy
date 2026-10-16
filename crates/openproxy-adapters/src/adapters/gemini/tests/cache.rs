use super::*;

#[test]
fn gemini_cache_lee_usage_nativo_y_legacy_con_y_sin_wrapper() {
    for wrapped in [false, true] {
        for native in [false, true] {
            for cached in [None, Some(0), Some(7983)] {
                let mut usage = if native {
                    json!({"promptTokenCount": 10026, "candidatesTokenCount": 3, "totalTokenCount": 10029})
                } else {
                    json!({"prompt_token_count": 10026, "candidates_token_count": 3, "total_token_count": 10029})
                };
                if let Some(value) = cached {
                    usage["cachedContentTokenCount"] = json!(value);
                }
                let mut body = json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "CACHE_OK"}]}, "finishReason": "STOP"}]});
                body[if native {
                    "usageMetadata"
                } else {
                    "usage_metadata"
                }] = usage;
                if wrapped {
                    body = json!({"response": body});
                }
                let response = deserialize_gemini_response(&body).unwrap();
                let usage = response
                    .usage
                    .expect("El usage del proveedor no debe perderse");
                assert_eq!(usage.prompt_tokens, 10026);
                assert_eq!(usage.completion_tokens, 3);
                assert_eq!(usage.total_tokens, 10029);
                assert_eq!(
                    usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
                    cached
                );
            }
        }
    }
}
