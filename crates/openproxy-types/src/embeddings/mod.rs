mod input;
mod modality;
mod part;
mod payload;

pub use input::{EmbeddingInput, EmbeddingItem};
pub use modality::{EmbeddingModalities, EmbeddingModality};
pub use part::{EmbeddingBatchItem, EmbeddingPart};
pub use payload::{
    EmbeddingObject, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage, EmbeddingVector,
};

#[cfg(test)]
#[path = "embeddings_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "embeddings_multimodal_tests.rs"]
mod multimodal_tests;
