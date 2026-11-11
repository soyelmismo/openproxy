use super::*;
use crate::error::CoreError;

// --- multimodal schema -------------------------------------------------

#[test]
fn test_part_accepts_every_dialect() {
    let cases: &[(&str, EmbeddingPart)] = &[
        // OpenAI chat / embeddings dialect, nested and flattened.
        (
            r#"{"type":"image_url","image_url":{"url":"https://x/a.png"}}"#,
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ),
        (
            r#"{"type":"image_url","image_url":"https://x/a.png"}"#,
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ),
        // OpenAI Responses dialect.
        (
            r#"{"type":"input_image","image_url":"https://x/a.png"}"#,
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ),
        // Anthropic dialect, url source.
        (
            r#"{"type":"image","source":{"type":"url","url":"https://x/a.png"}}"#,
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ),
        // Anthropic dialect, base64 source.
        (
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}"#,
            EmbeddingPart::Image {
                url: "data:image/png;base64,QUJD".to_string(),
            },
        ),
        // Gemini dialect, which carries no `type` at all.
        (
            r#"{"inline_data":{"mime_type":"image/png","data":"QUJD"}}"#,
            EmbeddingPart::Image {
                url: "data:image/png;base64,QUJD".to_string(),
            },
        ),
        // Simple flat form.
        (
            r#"{"type":"image","url":"https://x/a.png"}"#,
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ),
        // Text dialects.
        (r#"{"type":"text","text":"hi"}"#, text_part("hi")),
        (r#"{"text":"hi"}"#, text_part("hi")),
        (r#"{"type":"text","content":"hi"}"#, text_part("hi")),
        ("\"hi\"", text_part("hi")),
        // Audio dialects.
        (
            r#"{"type":"input_audio","input_audio":{"data":"QUJD","format":"wav"}}"#,
            EmbeddingPart::Audio {
                url: "data:audio/wav;base64,QUJD".to_string(),
            },
        ),
        (
            r#"{"type":"audio","url":"https://x/a.wav"}"#,
            EmbeddingPart::Audio {
                url: "https://x/a.wav".to_string(),
            },
        ),
        (
            r#"{"inline_data":{"mime_type":"audio/wav","data":"QUJD"}}"#,
            EmbeddingPart::Audio {
                url: "data:audio/wav;base64,QUJD".to_string(),
            },
        ),
        // Video dialects.
        (
            r#"{"type":"video_url","video_url":{"url":"https://x/a.mp4"}}"#,
            EmbeddingPart::Video {
                url: "https://x/a.mp4".to_string(),
            },
        ),
        (
            r#"{"type":"input_video","video_url":"https://x/a.mp4"}"#,
            EmbeddingPart::Video {
                url: "https://x/a.mp4".to_string(),
            },
        ),
        (
            r#"{"inline_data":{"mime_type":"video/mp4","data":"QUJD"}}"#,
            EmbeddingPart::Video {
                url: "data:video/mp4;base64,QUJD".to_string(),
            },
        ),
    ];

    for (json, expected) in cases {
        let part: EmbeddingPart =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} should parse: {e}"));
        assert_eq!(&part, expected, "dialect {json}");
    }
}

#[test]
fn test_part_rejects_unusable_shapes() {
    let cases = &[
        // A number is neither a string nor an object.
        "42",
        // Unknown type.
        r#"{"type":"document","source":{}}"#,
        // Media with no reference at all.
        r#"{"type":"image"}"#,
        // Empty reference.
        r#"{"type":"image","url":""}"#,
        // Base64 without a MIME type to build a data URL from.
        r#"{"type":"image","source":{"type":"base64","data":"QUJD"}}"#,
        // inline_data whose MIME names no embeddable modality.
        r#"{"inline_data":{"mime_type":"application/zip","data":"QUJD"}}"#,
        // An object with nothing recognizable in it.
        r#"{"foo":"bar"}"#,
    ];

    for json in cases {
        let result: std::result::Result<EmbeddingPart, _> = serde_json::from_str(json);
        assert!(result.is_err(), "{json} should not parse");
    }
}

#[test]
fn a_pdf_part_parses_as_a_document() {
    // Gemini reads PDFs through the same `inline_data` slot as images, so
    // the modality has to come from the MIME type, not the field name.
    let part: EmbeddingPart =
        serde_json::from_str(r#"{"inline_data":{"mime_type":"application/pdf","data":"QUJD"}}"#)
            .expect("pdf parses");
    assert_eq!(part.modality(), EmbeddingModality::Document);
    let EmbeddingPart::Document { url } = &part else {
        panic!("expected a document part, got {part:?}");
    };
    assert_eq!(url, "data:application/pdf;base64,QUJD");
}

#[test]
fn an_unrelated_application_mime_is_still_rejected() {
    let result: std::result::Result<EmbeddingPart, _> =
        serde_json::from_str(r#"{"inline_data":{"mime_type":"application/zip","data":"QUJD"}}"#);
    assert!(result.is_err(), "a zip is not an embeddable document");
}

#[test]
fn test_part_round_trips_through_canonical_form() {
    let part = EmbeddingPart::Image {
        url: "data:image/png;base64,QUJD".to_string(),
    };
    let json = serde_json::to_string(&part).expect("serialize");
    assert_eq!(
        json,
        r#"{"type":"image","url":"data:image/png;base64,QUJD"}"#
    );

    let back: EmbeddingPart = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, part);
}

#[test]
fn test_multimodal_input_variants_resolve_after_text_ones() {
    // A single multimodal item.
    let req: EmbeddingRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"type":"text","text":"a cat"},{"type":"image_url","image_url":{"url":"https://x/a.png"}}]}"#,
        )
        .expect("deserialize");
    assert_eq!(req.input.item_count(), 1);
    assert_eq!(
        req.input.modalities(),
        EmbeddingModalities::single(EmbeddingModality::Text).with(EmbeddingModality::Image)
    );

    // A batch of multimodal items.
    let batch: EmbeddingRequest = serde_json::from_str(
            r#"{"model":"m","input":[[{"type":"text","text":"a"}],[{"type":"text","text":"b"},{"type":"audio","url":"https://x/a.wav"}]]}"#,
        )
        .expect("deserialize");
    assert_eq!(batch.input.item_count(), 2);
    assert_eq!(
        batch.input.modalities(),
        EmbeddingModalities::single(EmbeddingModality::Text).with(EmbeddingModality::Audio)
    );
}

#[test]
fn test_text_shapes_keep_their_previous_meaning() {
    // The regression guard for the untagged ordering: every shape that was
    // valid before the multimodal variants existed must still resolve to
    // the same variant.
    let single: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"m","input":"hi"}"#).expect("deserialize");
    assert_eq!(single.input, EmbeddingInput::Single("hi".to_string()));

    let array: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"m","input":["a","b"]}"#).expect("deserialize");
    assert_eq!(
        array.input,
        EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice())
    );

    let tokens: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"m","input":[1,2,3]}"#).expect("deserialize");
    assert_eq!(
        tokens.input,
        EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice())
    );

    let token_arrays: EmbeddingRequest =
        serde_json::from_str(r#"{"model":"m","input":[[1,2],[3,4]]}"#).expect("deserialize");
    assert_eq!(
        token_arrays.input,
        EmbeddingInput::TokenArrays(
            vec![vec![1, 2].into_boxed_slice(), vec![3, 4].into_boxed_slice()].into_boxed_slice()
        )
    );
}

#[test]
fn test_items_normalizes_every_shape() {
    let single = EmbeddingInput::Single("hi".to_string());
    assert_eq!(single.items(), vec![EmbeddingItem::Text("hi")]);

    let array = EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice());
    assert_eq!(
        array.items(),
        vec![EmbeddingItem::Text("a"), EmbeddingItem::Text("b")]
    );

    let tokens = EmbeddingInput::Tokens(vec![1, 2].into_boxed_slice());
    assert_eq!(tokens.items(), vec![EmbeddingItem::Tokens(&[1, 2])]);

    let parts = EmbeddingInput::Parts(
        vec![
            EmbeddingPart::Text {
                text: "a".to_string(),
            },
            EmbeddingPart::Image {
                url: "https://x/a.png".to_string(),
            },
        ]
        .into_boxed_slice(),
    );
    assert_eq!(parts.items().len(), 1);
    match &parts.items()[0] {
        EmbeddingItem::Parts(slice) => assert_eq!(slice.len(), 2),
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn test_modalities_of_plain_text_is_text() {
    // A plain string request reports text rather than an empty set, so a
    // capability gate never mistakes it for "carries nothing".
    let input = EmbeddingInput::Single("hi".to_string());
    assert_eq!(
        input.modalities(),
        EmbeddingModalities::single(EmbeddingModality::Text)
    );

    let tokens = EmbeddingInput::Tokens(vec![1].into_boxed_slice());
    assert_eq!(
        tokens.modalities(),
        EmbeddingModalities::single(EmbeddingModality::Text)
    );
}

#[test]
fn test_modalities_set_operations() {
    let set = EmbeddingModalities::empty();
    assert!(set.is_empty());
    assert!(!set.contains(EmbeddingModality::Text));

    let set = set
        .with(EmbeddingModality::Text)
        .with(EmbeddingModality::Image);
    assert!(!set.is_empty());
    assert!(set.contains(EmbeddingModality::Text));
    assert!(set.contains(EmbeddingModality::Image));
    assert!(!set.contains(EmbeddingModality::Audio));

    assert!(set.contains_all(EmbeddingModalities::single(EmbeddingModality::Text)));
    assert!(!set.contains_all(EmbeddingModalities::single(EmbeddingModality::Audio)));

    let collected: Vec<_> = set.iter().collect();
    assert_eq!(
        collected,
        vec![EmbeddingModality::Text, EmbeddingModality::Image]
    );
}

#[test]
fn test_modality_parse_and_display() {
    use std::str::FromStr;

    assert_eq!(EmbeddingModality::Text.as_str(), "text");
    assert_eq!(EmbeddingModality::Image.as_str(), "image");
    assert_eq!(EmbeddingModality::Audio.as_str(), "audio");
    assert_eq!(EmbeddingModality::Video.as_str(), "video");
    assert_eq!(EmbeddingModality::Document.as_str(), "document");

    assert_eq!(
        EmbeddingModality::from_str("image").expect("parse"),
        EmbeddingModality::Image
    );
    assert_eq!(
        EmbeddingModality::from_str("document").expect("parse"),
        EmbeddingModality::Document
    );
    // `core_error:` builds a CoreError, which is not PartialEq, so the
    // failure is matched rather than compared.
    match EmbeddingModality::from_str("spreadsheet") {
        Err(CoreError::Validation(message)) => {
            assert!(message.contains("embedding_modality"), "{message}");
        }
        other => panic!("expected a validation error, got {other:?}"),
    }
}

fn text_part(text: &str) -> EmbeddingPart {
    EmbeddingPart::Text {
        text: text.to_string(),
    }
}
