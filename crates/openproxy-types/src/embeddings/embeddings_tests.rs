use super::*;

#[test]
fn test_embedding_input_single_serde() {
    let json_str = r#"{"model":"text-embedding-3-small","input":"hello world"}"#;
    let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(req.model, "text-embedding-3-small");
    assert_eq!(req.input, EmbeddingInput::Single("hello world".to_string()));
    assert!(!req.input.is_empty());
    assert_eq!(req.encoding_format, None);
    assert_eq!(req.dimensions, None);
    assert_eq!(req.user, None);

    let serialized = serde_json::to_string(&req).expect("serialize");
    assert!(serialized.contains(r#""input":"hello world""#));
}

#[test]
fn test_embedding_input_array_serde() {
    let json_str = r#"{
            "model":"text-embedding-3-large",
            "input":["hello", "world"],
            "dimensions": 1536,
            "encoding_format": "float",
            "user": "user-123"
        }"#;
    let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(req.model, "text-embedding-3-large");
    assert_eq!(
        req.input,
        EmbeddingInput::Array(vec!["hello".to_string(), "world".to_string()].into_boxed_slice())
    );
    assert!(!req.input.is_empty());
    assert_eq!(req.dimensions, Some(1536));
    assert_eq!(req.encoding_format.as_deref(), Some("float"));
    assert_eq!(req.user.as_deref(), Some("user-123"));
}

#[test]
fn test_embedding_input_tokens_serde() {
    let json_str = r#"{"model":"text-embedding-3-small","input":[101, 2054, 2003, 102]}"#;
    let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(req.model, "text-embedding-3-small");
    assert_eq!(
        req.input,
        EmbeddingInput::Tokens(vec![101, 2054, 2003, 102].into_boxed_slice())
    );
    assert!(!req.input.is_empty());

    let serialized = serde_json::to_string(&req).expect("serialize");
    assert!(serialized.contains(r#""input":[101,2054,2003,102]"#));
}

#[test]
fn test_embedding_input_token_arrays_serde() {
    let json_str = r#"{"model":"text-embedding-3-small","input":[[101, 2054], [2003, 102]]}"#;
    let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(req.model, "text-embedding-3-small");
    assert_eq!(
        req.input,
        EmbeddingInput::TokenArrays(
            vec![
                vec![101, 2054].into_boxed_slice(),
                vec![2003, 102].into_boxed_slice()
            ]
            .into_boxed_slice()
        )
    );
    assert!(!req.input.is_empty());
}

#[test]
fn test_embedding_input_is_empty() {
    let empty_single = EmbeddingInput::Single(String::new());
    assert!(empty_single.is_empty());

    let empty_arr = EmbeddingInput::Array(vec![].into_boxed_slice());
    assert!(empty_arr.is_empty());

    let empty_arr_strings =
        EmbeddingInput::Array(vec![String::new(), String::new()].into_boxed_slice());
    assert!(empty_arr_strings.is_empty());

    let non_empty =
        EmbeddingInput::Array(vec![String::new(), "foo".to_string()].into_boxed_slice());
    assert!(!non_empty.is_empty());

    let empty_tokens = EmbeddingInput::Tokens(vec![].into_boxed_slice());
    assert!(empty_tokens.is_empty());

    let non_empty_tokens = EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice());
    assert!(!non_empty_tokens.is_empty());

    let empty_token_arrays = EmbeddingInput::TokenArrays(vec![].into_boxed_slice());
    assert!(empty_token_arrays.is_empty());

    let empty_token_arrays_inner = EmbeddingInput::TokenArrays(
        vec![vec![].into_boxed_slice(), vec![].into_boxed_slice()].into_boxed_slice(),
    );
    assert!(empty_token_arrays_inner.is_empty());

    let non_empty_token_arrays = EmbeddingInput::TokenArrays(
        vec![vec![1].into_boxed_slice(), vec![].into_boxed_slice()].into_boxed_slice(),
    );
    assert!(!non_empty_token_arrays.is_empty());

    let empty_parts = EmbeddingInput::Parts(vec![].into_boxed_slice());
    assert!(empty_parts.is_empty());

    let empty_text_parts = EmbeddingInput::Parts(
        vec![EmbeddingPart::Text {
            text: String::new(),
        }]
        .into_boxed_slice(),
    );
    assert!(empty_text_parts.is_empty());

    let media_parts = EmbeddingInput::Parts(
        vec![EmbeddingPart::Image {
            url: "data:image/png;base64,AAAA".to_string(),
        }]
        .into_boxed_slice(),
    );
    assert!(!media_parts.is_empty());

    let empty_part_arrays = EmbeddingInput::PartArrays(vec![].into_boxed_slice());
    assert!(empty_part_arrays.is_empty());
}

#[test]
fn test_embedding_response_floats_serde() {
    let json_str = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2, 0.3],
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            }
        }"#;
    let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(res.object, "list");
    assert_eq!(res.model, "text-embedding-3-small");
    assert_eq!(res.data.len(), 1);
    assert_eq!(res.data[0].object, "embedding");
    assert_eq!(
        res.data[0].embedding,
        EmbeddingVector::Floats(vec![0.1, 0.2, 0.3].into_boxed_slice())
    );
    assert_eq!(res.data[0].index, 0);
    assert_eq!(res.usage.prompt_tokens, 5);
    assert_eq!(res.usage.total_tokens, 5);

    let serialized = serde_json::to_string(&res).expect("serialize");
    assert!(serialized.contains(r#""embedding":[0.1,0.2,0.3]"#));
}

#[test]
fn test_embedding_response_base64_serde() {
    let json_str = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": "eJwrycxNLSoBAAhFAw0=",
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 3,
                "total_tokens": 3
            }
        }"#;
    let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(res.object, "list");
    assert_eq!(res.model, "text-embedding-3-small");
    assert_eq!(res.data.len(), 1);
    assert_eq!(res.data[0].object, "embedding");
    assert_eq!(
        res.data[0].embedding,
        EmbeddingVector::Base64("eJwrycxNLSoBAAhFAw0=".to_string())
    );

    let serialized = serde_json::to_string(&res).expect("serialize");
    assert!(serialized.contains(r#""embedding":"eJwrycxNLSoBAAhFAw0=""#));
}

#[test]
fn test_embedding_response_defaults() {
    let json_str = r#"{
            "data": [
                {
                    "embedding": [0.5, 0.6],
                    "index": 0
                }
            ],
            "model": "text-embedding-ada-002",
            "usage": {
                "prompt_tokens": 2,
                "total_tokens": 2
            }
        }"#;
    let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
    assert_eq!(res.object, "list");
    assert_eq!(res.data[0].object, "embedding");
    assert_eq!(
        res.data[0].embedding,
        EmbeddingVector::Floats(vec![0.5, 0.6].into_boxed_slice())
    );
}

#[test]
fn test_embedding_input_from_conversions() {
    let from_str: EmbeddingInput = "hello".into();
    assert_eq!(from_str, EmbeddingInput::Single("hello".to_string()));

    let from_string: EmbeddingInput = String::from("world").into();
    assert_eq!(from_string, EmbeddingInput::Single("world".to_string()));

    let from_vec_str: EmbeddingInput = vec!["a".to_string(), "b".to_string()].into();
    assert_eq!(
        from_vec_str,
        EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice())
    );

    let from_tokens: EmbeddingInput = vec![1u32, 2, 3].into();
    assert_eq!(
        from_tokens,
        EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice())
    );

    let from_token_arrays: EmbeddingInput = vec![vec![1u32, 2], vec![3u32, 4]].into();
    assert_eq!(
        from_token_arrays,
        EmbeddingInput::TokenArrays(
            vec![vec![1, 2].into_boxed_slice(), vec![3, 4].into_boxed_slice()].into_boxed_slice()
        )
    );
}
