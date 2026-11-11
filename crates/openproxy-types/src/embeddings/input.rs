use serde::{Deserialize, Serialize};

use super::modality::{EmbeddingModalities, EmbeddingModality};
use super::part::{EmbeddingBatchItem, EmbeddingPart};

/// Input for embedding requests.
///
/// The first four variants are the OpenAI shapes and stay first so the
/// `untagged` deserializer keeps resolving them exactly as before. The two
/// multimodal variants are appended after them: an array of part objects fails
/// every earlier variant before reaching `Parts`, so no previously valid
/// request changes meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Array(Box<[String]>),
    Tokens(Box<[u32]>),
    TokenArrays(Box<[Box<[u32]>]>),
    /// One multimodal item: an ordered list of parts.
    Parts(Box<[EmbeddingPart]>),
    /// A batch of multimodal items, each of which may itself be a bare string.
    PartArrays(Box<[EmbeddingBatchItem]>),
}

/// One item of an embedding request, in the uniform shape consumers need.
///
/// Text and token variants normalize to their own arms so a caller never has to
/// special-case "is this the multimodal shape".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingItem<'a> {
    /// Plain text, already carrying its task prefix upstream of here.
    Text(&'a str),
    /// Pre-tokenized ids.
    Tokens(&'a [u32]),
    /// A multimodal item, parts in order.
    Parts(&'a [EmbeddingPart]),
}

impl EmbeddingInput {
    /// Every item of the request, in order.
    ///
    /// One small allocation per request, which is noise next to an inference.
    #[must_use]
    pub fn items(&self) -> Vec<EmbeddingItem<'_>> {
        match self {
            Self::Single(text) => vec![EmbeddingItem::Text(text)],
            Self::Array(texts) => texts.iter().map(|t| EmbeddingItem::Text(t)).collect(),
            Self::Tokens(ids) => vec![EmbeddingItem::Tokens(ids)],
            Self::TokenArrays(batches) => batches
                .iter()
                .map(|ids| EmbeddingItem::Tokens(ids))
                .collect(),
            Self::Parts(parts) => vec![EmbeddingItem::Parts(parts)],
            Self::PartArrays(batches) => batches
                .iter()
                .map(|item| EmbeddingItem::Parts(item.parts()))
                .collect(),
        }
    }

    /// How many embeddings this request asks for.
    #[must_use]
    pub fn item_count(&self) -> usize {
        match self {
            Self::Single(_) | Self::Tokens(_) | Self::Parts(_) => 1,
            Self::Array(texts) => texts.len(),
            Self::TokenArrays(batches) => batches.len(),
            Self::PartArrays(batches) => batches.len(),
        }
    }

    /// Every modality the request carries.
    ///
    /// Text and token items count as [`EmbeddingModality::Text`], so a plain
    /// string request reports text rather than nothing.
    #[must_use]
    pub fn modalities(&self) -> EmbeddingModalities {
        let mut set = EmbeddingModalities::empty();
        for item in self.items() {
            match item {
                EmbeddingItem::Text(_) | EmbeddingItem::Tokens(_) => {
                    set = set.with(EmbeddingModality::Text);
                }
                EmbeddingItem::Parts(parts) => {
                    for part in parts {
                        set = set.with(part.modality());
                    }
                }
            }
        }
        set
    }

    /// Whether the request carries no usable input.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Single(s) => s.is_empty(),
            Self::Array(arr) => arr.is_empty() || arr.iter().all(|s| s.is_empty()),
            Self::Tokens(t) => t.is_empty(),
            Self::TokenArrays(arr) => arr.is_empty() || arr.iter().all(|t| t.is_empty()),
            Self::Parts(parts) => parts.is_empty() || parts.iter().all(EmbeddingPart::is_empty),
            Self::PartArrays(batches) => {
                batches.is_empty()
                    || batches
                        .iter()
                        .all(|item| item.parts().iter().all(EmbeddingPart::is_empty))
            }
        }
    }
}

impl_enum_from! {
    EmbeddingInput {
        Single(String),
        Single(&str => ToString::to_string),
        Array(Box<[String]>),
        Array(Vec<String> => Vec::into_boxed_slice),
        Tokens(Box<[u32]>),
        Tokens(Vec<u32> => Vec::into_boxed_slice),
        TokenArrays(Box<[Box<[u32]>]>),
        TokenArrays(Vec<Vec<u32>> => |v: Vec<Vec<u32>>| v.into_iter().map(Vec::into_boxed_slice).collect::<Box<[_]>>()),
        Parts(Box<[EmbeddingPart]>),
        Parts(Vec<EmbeddingPart> => Vec::into_boxed_slice),
        PartArrays(Box<[EmbeddingBatchItem]>),
        PartArrays(Vec<EmbeddingBatchItem> => Vec::into_boxed_slice),
    }
}
