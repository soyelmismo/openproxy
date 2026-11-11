use super::*;

#[test]
fn task_prefixes_match_sentence_transformers_config() {
    assert_eq!(
        EmbedTask::Query.as_prefix(),
        "task: search result | query: "
    );
    assert_eq!(EmbedTask::Document.as_prefix(), "title: none | text: ");
}

#[test]
fn split_rows_rejects_shape_mismatch() {
    let response = EmbedResponse {
        embedding_dim: 3,
        embeddings: vec![1.0, 2.0],
    };
    assert!(split_rows(&response, 1).is_err());
    let ok = EmbedResponse {
        embedding_dim: 2,
        embeddings: vec![1.0, 2.0, 3.0, 4.0],
    };
    let rows = split_rows(&ok, 2).unwrap();
    assert_eq!(rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn build_request_roundtrips_through_json() {
    let request = build_request(
        vec![EmbedInput::Text {
            text: "hola".into(),
        }],
        EmbedTask::Query,
        256,
    );
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("\"kind\":\"text\""));
    let parsed: EmbedRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.dimensions, 256);
    assert_eq!(parsed.inputs.len(), 1);
}

#[test]
fn image_and_audio_inputs_roundtrip_through_json() {
    let request = build_request(
        vec![
            EmbedInput::Image {
                width: 2,
                height: 2,
                rgb: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            },
            EmbedInput::Audio {
                samples: vec![0.0, 0.5, -0.5],
            },
        ],
        EmbedTask::Document,
        0,
    );
    let json = serde_json::to_string(&request).unwrap();
    let parsed: EmbedRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.inputs.len(), 2);
    match &parsed.inputs[0] {
        EmbedInput::Image { width, height, rgb } => {
            assert_eq!((*width, *height), (2, 2));
            assert_eq!(rgb.len(), 12);
        }
        other => panic!("expected image, got {other:?}"),
    }
    match &parsed.inputs[1] {
        EmbedInput::Audio { samples } => assert_eq!(samples.len(), 3),
        other => panic!("expected audio, got {other:?}"),
    }
}

#[test]
fn config_defaults_and_validation_bounds() {
    let config = EmbedConfig::default();
    assert_eq!(config.max_len, 2048);
    let parsed: EmbedConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(parsed.max_len, 2048);
    assert!(parsed.image.is_none());
    assert!(parsed.audio.is_none());
}

#[test]
fn vision_and_audio_paths_default_to_the_cache_root() {
    assert!(resolve_vision_path(None).ends_with("vision_encoder_q4f16.onnx"));
    assert!(resolve_audio_path(None).ends_with("audio_encoder_q4f16.onnx"));
}

#[test]
fn towers_resolve_beside_an_explicit_text_model_not_to_it() {
    // A caller naming the text graph must not make the towers load the same
    // file: that leaves the tower session without its modality input and
    // the first Run dies with "Invalid input name".
    let dir = std::env::temp_dir().join("openproxy_tower_resolution_test");
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.join("model_q4f16.onnx");
    std::fs::write(&text, b"stub").unwrap();
    for tower in ["vision_encoder_q4f16.onnx", "audio_encoder_q4f16.onnx"] {
        std::fs::write(dir.join(tower), b"stub").unwrap();
    }

    let explicit = text.to_str().unwrap();
    let vision = resolve_vision_path(Some(explicit));
    let audio = resolve_audio_path(Some(explicit));
    assert_eq!(
        vision,
        dir.join("vision_encoder_q4f16.onnx").to_str().unwrap()
    );
    assert_eq!(
        audio,
        dir.join("audio_encoder_q4f16.onnx").to_str().unwrap()
    );
    assert_ne!(vision, explicit);
    assert_ne!(audio, explicit);

    std::fs::remove_dir_all(&dir).ok();
}
