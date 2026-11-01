//! Multimodal embeddings over Google's `embedContent` / `batchEmbedContents`.
//!
//! The canonical [`EmbeddingRequest`] is provider-agnostic: one item is either
//! text, pre-tokenized ids, or an ordered list of parts. Gemini speaks none of
//! that — it takes a `content` with `parts`, a `taskType`, and an
//! `outputDimensionality`. This module is the only place that knows the mapping.
//!
//! Two decisions worth stating, because they are not obvious from the wire:
//!
//! * Every request goes out as `batchEmbedContents`, even a single item. One
//!   request shape, one response shape, one code path. Gemini accepts a
//!   one-element batch, and the response translator still understands the
//!   singular `embedding` envelope so a hand-rolled upstream cannot surprise us.
//! * `taskType` is derived from the model alias, mirroring the native engine:
//!   an alias containing `doc` embeds corpus, everything else embeds a query.
//!   Gemini uses this to pick the prompt prefix, so getting it wrong silently
//!   degrades retrieval quality rather than failing.

use super::types::{GeminiContent, GeminiPart};
use openproxy_types::embeddings::{
    EmbeddingObject, EmbeddingPart, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage,
    EmbeddingVector,
};
use openproxy_types::error::CoreError;

/// Gemini's task types, restricted to the two retrieval directions.
///
/// The other five (`SEMANTIC_SIMILARITY`, `CLASSIFICATION`, `CLUSTERING`, ...)
/// describe a use the caller cannot express through an OpenAI-shaped request,
/// and guessing one would be worse than defaulting to retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiEmbedTask {
    Query,
    Document,
}

impl GeminiEmbedTask {
    /// Picks the side from the upstream model alias.
    ///
    /// Same convention as the native engine: `*-doc` (or any alias containing
    /// `doc`) embeds corpus text, everything else embeds a query.
    #[must_use]
    pub fn from_model_alias(upstream_model: &str) -> Self {
        if upstream_model.contains("doc") {
            Self::Document
        } else {
            Self::Query
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "RETRIEVAL_QUERY",
            Self::Document => "RETRIEVAL_DOCUMENT",
        }
    }
}

/// One entry of the `requests` array Gemini expects.
#[derive(Debug, Clone, serde::Serialize)]
struct GeminiEmbedRequest {
    content: GeminiContent,
    #[serde(rename = "taskType")]
    task_type: &'static str,
    #[serde(
        rename = "outputDimensionality",
        skip_serializing_if = "Option::is_none"
    )]
    output_dimensionality: Option<u32>,
}

/// The `batchEmbedContents` envelope.
#[derive(Debug, Clone, serde::Serialize)]
struct GeminiEmbedBatch {
    requests: Vec<GeminiEmbedRequest>,
}

/// Splits a `data:` URL into the MIME type and the base64 payload.
///
/// Only `data:` URLs are accepted. An `https://` reference needs bytes fetched
/// and encoded, which is an async job the adapter cannot do — `format_request`
/// is synchronous. Returning an error here beats shipping a URL where Gemini
/// expects base64, because the failure is immediate and names the cause.
fn split_data_url(url: &str) -> std::result::Result<(&str, &str), CoreError> {
    let Some(stripped) = url.strip_prefix("data:") else {
        return Err(CoreError::Validation(format!(
            "gemini embeddings need inline base64 media, got a remote reference: {url}. \
             Fetch it and send a data: URL."
        )));
    };
    let Some((mime, rest)) = stripped.split_once(';') else {
        return Err(CoreError::Validation(format!(
            "gemini embeddings: malformed data URL, no MIME type: {url}"
        )));
    };
    let Some((_, data)) = rest.split_once(',') else {
        return Err(CoreError::Validation(format!(
            "gemini embeddings: malformed data URL, no payload: {url}"
        )));
    };
    Ok((mime, data))
}

/// Converts one canonical part into the Gemini part that carries it.
fn part_to_gemini(part: &EmbeddingPart) -> std::result::Result<GeminiPart, CoreError> {
    match part {
        EmbeddingPart::Text { text } => Ok(GeminiPart {
            text: Some(text.clone()),
            ..Default::default()
        }),
        EmbeddingPart::Image { url }
        | EmbeddingPart::Audio { url }
        | EmbeddingPart::Video { url }
        | EmbeddingPart::Document { url } => {
            let (mime, data) = split_data_url(url)?;
            Ok(GeminiPart {
                inline_data: Some(super::types::GeminiInlineData {
                    mime_type: mime.to_string(),
                    data: data.to_string(),
                }),
                ..Default::default()
            })
        }
    }
}

/// Builds the `content` for one item.
///
/// Empty text parts are dropped: Gemini rejects an empty string, and a part
/// that carries nothing is noise in the payload. An item whose every part is
/// empty is an error, because there is nothing left to embed.
fn item_to_content(parts: &[EmbeddingPart]) -> std::result::Result<GeminiContent, CoreError> {
    let mut converted = Vec::with_capacity(parts.len());
    for part in parts {
        if part.is_empty() {
            continue;
        }
        converted.push(part_to_gemini(part)?);
    }
    if converted.is_empty() {
        return Err(CoreError::Validation(
            "embedding item carries no usable parts".into(),
        ));
    }
    Ok(GeminiContent {
        role: "user".to_string(),
        parts: converted,
    })
}

/// Translates the canonical request into the `batchEmbedContents` body.
///
/// # Errors
/// A media part that is not a `data:` URL, or an item with no usable parts.
pub fn serialize_gemini_embedding_request(
    req: &EmbeddingRequest,
    upstream_model: &str,
) -> std::result::Result<bytes::Bytes, CoreError> {
    let task = GeminiEmbedTask::from_model_alias(upstream_model);
    let output_dimensionality = req.dimensions;

    let mut requests = Vec::with_capacity(req.input.items().len());
    for item in req.input.items() {
        // Text and pre-tokenized ids both normalize into parts, so the request
        // builder only ever sees one shape. Ids are the exception: Gemini has no
        // way to accept them, and inventing a detokenizer would be worse than
        // refusing.
        let parts: Vec<EmbeddingPart> = match item {
            openproxy_types::embeddings::EmbeddingItem::Text(text) => {
                vec![EmbeddingPart::Text {
                    text: text.to_string(),
                }]
            }
            openproxy_types::embeddings::EmbeddingItem::Parts(parts) => parts.to_vec(),
            openproxy_types::embeddings::EmbeddingItem::Tokens(_) => {
                return Err(CoreError::Validation(
                    "gemini embeddings accept text and media parts, not pre-tokenized ids".into(),
                ));
            }
        };
        requests.push(GeminiEmbedRequest {
            content: item_to_content(&parts)?,
            task_type: task.as_str(),
            output_dimensionality,
        });
    }

    serde_json::to_vec(&GeminiEmbedBatch { requests })
        .map(bytes::Bytes::from)
        .map_err(|e| CoreError::Validation(format!("gemini embeddings serialize failed: {e}")))
}

/// Reads one vector out of either Gemini envelope.
///
/// `batchEmbedContents` answers `{"embeddings":[{"values":[...]}, ...]}` and
/// `embedContent` answers `{"embedding":{"values":[...]}}`. Both are accepted so
/// the translator does not depend on which URL happened to be called.
fn read_vector(entry: &serde_json::Value) -> std::result::Result<Vec<f32>, CoreError> {
    let values = entry
        .get("values")
        .or_else(|| entry.get("embedding").and_then(|e| e.get("values")))
        .ok_or_else(|| {
            CoreError::Validation("gemini embeddings: response carries no values".into())
        })?;
    serde_json::from_value(values.clone())
        .map_err(|e| CoreError::Validation(format!("gemini embeddings: bad values: {e}")))
}

/// Translates a Gemini embeddings response into the canonical shape.
///
/// # Errors
/// The body carries neither envelope, or a vector is not an array of numbers.
pub fn deserialize_gemini_embedding_response(
    response_body: serde_json::Value,
) -> std::result::Result<EmbeddingResponse, CoreError> {
    let entries: Vec<&serde_json::Value> =
        if let Some(list) = response_body.get("embeddings").and_then(|v| v.as_array()) {
            list.iter().collect()
        } else if let Some(single) = response_body.get("embedding") {
            vec![single]
        } else {
            return Err(CoreError::Validation(
                "gemini embeddings: response has neither `embeddings` nor `embedding`".into(),
            ));
        };

    let data = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            read_vector(entry).map(|values| EmbeddingObject {
                object: "embedding".to_string(),
                embedding: EmbeddingVector::Floats(values.into_boxed_slice()),
                index,
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(EmbeddingResponse {
        object: "list".to_string(),
        data: data.into(),
        // Gemini does not echo the model, and the caller asked for the alias it
        // sent, not the upstream name it was routed to.
        model: String::new(),
        usage: EmbeddingUsage::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::embeddings::{EmbeddingBatchItem, EmbeddingInput};

    fn request(input: EmbeddingInput, dimensions: Option<u32>) -> EmbeddingRequest {
        EmbeddingRequest {
            model: "gemini-embedding-2".into(),
            input,
            encoding_format: None,
            dimensions,
            user: None,
        }
    }

    fn text_part(text: &str) -> EmbeddingPart {
        EmbeddingPart::Text {
            text: text.to_string(),
        }
    }

    fn image_part(url: &str) -> EmbeddingPart {
        EmbeddingPart::Image {
            url: url.to_string(),
        }
    }

    #[test]
    fn task_side_follows_the_model_alias() {
        assert_eq!(
            GeminiEmbedTask::from_model_alias("gemini-embedding-2"),
            GeminiEmbedTask::Query
        );
        assert_eq!(
            GeminiEmbedTask::from_model_alias("gemini-embedding-2-doc"),
            GeminiEmbedTask::Document
        );
        // Same fragile-by-design convention the native engine uses.
        assert_eq!(
            GeminiEmbedTask::from_model_alias("my-doc-model"),
            GeminiEmbedTask::Document
        );
    }

    #[test]
    fn text_becomes_a_batch_request_with_the_query_task() {
        let req = request(EmbeddingInput::Single("hola mundo".into()), Some(768));
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let requests = val["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["taskType"], "RETRIEVAL_QUERY");
        assert_eq!(requests[0]["outputDimensionality"], 768);
        assert_eq!(requests[0]["content"]["role"], "user");
        assert_eq!(requests[0]["content"]["parts"][0]["text"], "hola mundo");
    }

    #[test]
    fn doc_alias_switches_the_task_type() {
        let req = request(EmbeddingInput::Single("corpus".into()), None);
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2-doc").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["requests"][0]["taskType"], "RETRIEVAL_DOCUMENT");
        // Omitted rather than sent as null, which Gemini rejects.
        assert!(val["requests"][0].get("outputDimensionality").is_none());
    }

    #[test]
    fn mixed_parts_keep_their_order_in_one_item() {
        let req = request(
            EmbeddingInput::Parts(Box::new([
                text_part("describe this"),
                image_part("data:image/png;base64,QUJD"),
            ])),
            None,
        );
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let parts = val["requests"][0]["content"]["parts"].as_array().unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "describe this");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "QUJD");
    }

    #[test]
    fn plain_text_input_is_not_rejected() {
        // Regression: the first cut only accepted `Parts`, so every existing
        // text-only caller failed. Text is the common case and must survive.
        let req = request(EmbeddingInput::Single("hola mundo".into()), None);
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            val["requests"][0]["content"]["parts"][0]["text"],
            "hola mundo"
        );
    }

    #[test]
    fn an_array_of_strings_becomes_one_request_each() {
        let req = request(
            EmbeddingInput::Array(Box::new(["one".to_string(), "two".to_string()])),
            None,
        );
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let requests = val["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["content"]["parts"][0]["text"], "one");
        assert_eq!(requests[1]["content"]["parts"][0]["text"], "two");
    }

    #[test]
    fn a_document_part_travels_as_inline_data() {
        // Gemini reads a PDF through the same inline_data slot as an image, so
        // the only difference is the MIME type the canonical part carries.
        let req = request(
            EmbeddingInput::Parts(Box::new([EmbeddingPart::Document {
                url: "data:application/pdf;base64,JVBERi0xLjQK".to_string(),
            }])),
            None,
        );
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let part = &val["requests"][0]["content"]["parts"][0];
        assert_eq!(part["inlineData"]["mimeType"], "application/pdf");
        assert_eq!(part["inlineData"]["data"], "JVBERi0xLjQK");
    }

    #[test]
    fn each_batch_item_becomes_its_own_request() {
        // A bare string and an array of parts in the same batch: the shape a
        // corpus with both plain paragraphs and scanned pages produces.
        let items: Box<[EmbeddingBatchItem]> = Box::new([
            EmbeddingBatchItem::from_str_item("one"),
            EmbeddingBatchItem::from_parts(vec![
                text_part("two"),
                image_part("data:image/png;base64,QUJD"),
            ]),
        ]);
        let req = request(EmbeddingInput::PartArrays(items), None);
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let requests = val["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["content"]["parts"][0]["text"], "one");
        assert_eq!(requests[1]["content"]["parts"][0]["text"], "two");
        assert_eq!(
            requests[1]["content"]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
    }

    #[test]
    fn a_remote_url_is_rejected_not_silently_mangled() {
        let req = request(
            EmbeddingInput::Parts(Box::new([image_part("https://example.com/a.png")])),
            None,
        );
        let err = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("remote reference"), "{msg}");
        assert!(msg.contains("data: URL"), "{msg}");
    }

    #[test]
    fn empty_text_parts_are_dropped_but_the_item_survives() {
        let req = request(
            EmbeddingInput::Parts(Box::new([
                text_part(""),
                image_part("data:image/png;base64,QUJD"),
            ])),
            None,
        );
        let body = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let parts = val["requests"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].get("inlineData").is_some());
    }

    #[test]
    fn an_item_with_only_empty_parts_is_an_error() {
        let req = request(EmbeddingInput::Parts(Box::new([text_part("")])), None);
        let err = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap_err();
        assert!(err.to_string().contains("no usable parts"), "{err}");
    }

    #[test]
    fn pretokenized_ids_are_rejected() {
        let req = request(EmbeddingInput::Tokens(Box::new([1, 2, 3])), None);
        let err = serialize_gemini_embedding_request(&req, "gemini-embedding-2").unwrap_err();
        assert!(err.to_string().contains("pre-tokenized"), "{err}");
    }

    #[test]
    fn batch_response_becomes_the_canonical_list() {
        let body = serde_json::json!({
            "embeddings": [
                {"values": [0.1, 0.2]},
                {"values": [0.3, 0.4]},
            ]
        });
        let resp = deserialize_gemini_embedding_response(body).unwrap();
        assert_eq!(resp.object, "list");
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[1].index, 1);
        let openproxy_types::embeddings::EmbeddingVector::Floats(v) = &resp.data[0].embedding
        else {
            panic!("expected floats");
        };
        assert_eq!(&**v, &[0.1, 0.2]);
    }

    #[test]
    fn singular_response_is_also_understood() {
        let body = serde_json::json!({"embedding": {"values": [1.0, 2.0, 3.0]}});
        let resp = deserialize_gemini_embedding_response(body).unwrap();
        assert_eq!(resp.data.len(), 1);
        let openproxy_types::embeddings::EmbeddingVector::Floats(v) = &resp.data[0].embedding
        else {
            panic!("expected floats");
        };
        assert_eq!(&**v, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_response_with_no_vector_is_an_error() {
        let body = serde_json::json!({"error": {"code": 400}});
        let err = deserialize_gemini_embedding_response(body).unwrap_err();
        assert!(
            err.to_string()
                .contains("neither `embeddings` nor `embedding`"),
            "{err}"
        );
    }
}
