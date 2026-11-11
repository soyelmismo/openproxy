use serde::{Deserialize, Serialize};

use super::input::EmbeddingInput;

/// Request payload for embedding generation (`POST /v1/embeddings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Vector embedding representation: list of floats or base64 encoded string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Floats(Box<[f32]>),
    Base64(String),
}

impl_enum_from! {
    EmbeddingVector {
        Floats(Box<[f32]>),
        Floats(Vec<f32> => Vec::into_boxed_slice),
        Base64(String),
        Base64(&str => ToString::to_string),
    }
}

/// An individual embedding object in the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingObject {
    #[serde(default = "default_embedding_object")]
    pub object: String,
    pub embedding: EmbeddingVector,
    pub index: usize,
}

fn default_embedding_object() -> String {
    "embedding".to_string()
}

/// Usage statistics for an embedding request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

/// Response payload for embedding generation (`POST /v1/embeddings`).
///
/// Multimodal responses are shaped exactly like text ones — a list of vectors —
/// so nothing here changes when a request carries media.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    #[serde(default = "default_embedding_response_object")]
    pub object: String,
    pub data: Box<[EmbeddingObject]>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

fn default_embedding_response_object() -> String {
    "list".to_string()
}
