//! IPC payload types and the per-model engine configuration.

use openproxy_types::error::CoreError;

use super::preprocess::{AudioSettings, ImageSettings};

/// Task prefix per EmbeddingGemma's `config_sentence_transformers.json`.
/// Queries and documents must be embedded with their own prompt, or
/// retrieval quality collapses. Prefixes apply to text only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedTask {
    /// `task: search result | query: ` — retrieval/search-side text.
    Query,
    /// `title: none | text: ` — corpus/document-side text.
    Document,
}

impl EmbedTask {
    pub fn as_prefix(self) -> &'static str {
        match self {
            EmbedTask::Query => "task: search result | query: ",
            EmbedTask::Document => "title: none | text: ",
        }
    }
}

/// One item of a multimodal batch. Images arrive as decoded RGB8 and audio as
/// mono 16 kHz PCM, so the worker never runs an image or audio decoder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmbedInput {
    Text {
        text: String,
    },
    Image {
        width: u32,
        height: u32,
        /// Row-major RGB, `width * height * 3` bytes.
        rgb: Vec<u8>,
    },
    Audio {
        /// Mono samples at 16 kHz, nominally in [-1, 1].
        samples: Vec<f32>,
    },
}

/// IPC request: a batch of inputs to embed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedRequest {
    pub inputs: Vec<EmbedInput>,
    pub task: EmbedTask,
    /// Truncate to this many dimensions (Matryoshka). 0 or absent = full.
    #[serde(default)]
    pub dimensions: usize,
}

/// IPC response: flat row-major embeddings plus the active dimension.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedResponse {
    pub embedding_dim: usize,
    pub embeddings: Vec<f32>,
}

/// Settings forwarded to the worker on spawn.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Settings {
    pub model: Option<String>,
    pub tokenizer: Option<String>,
    pub config: Option<String>,
    pub threads: Option<usize>,
}

/// Build an IPC request.
pub(crate) fn build_request(
    inputs: Vec<EmbedInput>,
    task: EmbedTask,
    dimensions: usize,
) -> EmbedRequest {
    EmbedRequest {
        inputs,
        task,
        dimensions,
    }
}

/// Map a flat embedding vector list back into per-input vectors.
pub(crate) fn split_rows(
    response: &EmbedResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let dim = response.embedding_dim;
    if dim == 0 || response.embeddings.len() != dim * expected {
        return Err(CoreError::Internal(format!(
            "embed engine returned {} floats for {expected} inputs at dim {dim}",
            response.embeddings.len()
        )));
    }
    Ok(response
        .embeddings
        .chunks_exact(dim)
        .map(<[f32]>::to_vec)
        .collect())
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct EmbedConfig {
    #[serde(default = "default_max_len")]
    pub max_len: usize,
    #[serde(default)]
    pub image: Option<ImageSettings>,
    #[serde(default)]
    pub audio: Option<AudioSettings>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            max_len: default_max_len(),
            image: None,
            audio: None,
        }
    }
}

fn default_max_len() -> usize {
    2048
}
