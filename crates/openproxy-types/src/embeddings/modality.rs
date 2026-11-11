impl_string_enum! {
    /// A modality an embedding request or model can carry.
    ///
    /// Four values, matching the input slots the embedding graph exposes
    /// (`input_ids`, `image_features`, `video_features`, `audio_features`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum EmbeddingModality {
        Text => "text",
        Image => "image",
        Audio => "audio",
        Video => "video",
        Document => "document",
    }
    core_error: "embedding_modality"
}

/// Which modalities a request or a model touches.
///
/// Five modalities fit in a byte, so the set is `Copy` and never allocates.
/// This is what lets a request be gated against what the target model actually
/// accepts, instead of failing deep inside an upstream call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct EmbeddingModalities(u8);

impl EmbeddingModalities {
    const TEXT: u8 = 1 << 0;
    const IMAGE: u8 = 1 << 1;
    const AUDIO: u8 = 1 << 2;
    const VIDEO: u8 = 1 << 3;
    const DOCUMENT: u8 = 1 << 4;

    const fn bit(modality: EmbeddingModality) -> u8 {
        match modality {
            EmbeddingModality::Text => Self::TEXT,
            EmbeddingModality::Image => Self::IMAGE,
            EmbeddingModality::Audio => Self::AUDIO,
            EmbeddingModality::Video => Self::VIDEO,
            EmbeddingModality::Document => Self::DOCUMENT,
        }
    }

    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// A set holding exactly one modality.
    #[must_use]
    pub const fn single(modality: EmbeddingModality) -> Self {
        Self(Self::bit(modality))
    }

    /// The set widened by one modality.
    #[must_use]
    pub const fn with(self, modality: EmbeddingModality) -> Self {
        Self(self.0 | Self::bit(modality))
    }

    /// Whether the set holds a modality.
    #[must_use]
    pub const fn contains(self, modality: EmbeddingModality) -> bool {
        self.0 & Self::bit(modality) != 0
    }

    /// Whether the set holds every modality of another set.
    #[must_use]
    pub const fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modality was ever added.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every modality in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = EmbeddingModality> {
        [
            EmbeddingModality::Text,
            EmbeddingModality::Image,
            EmbeddingModality::Audio,
            EmbeddingModality::Video,
            EmbeddingModality::Document,
        ]
        .into_iter()
        .filter(move |modality| self.contains(*modality))
    }
}

impl From<EmbeddingModality> for EmbeddingModalities {
    #[inline]
    fn from(modality: EmbeddingModality) -> Self {
        Self::single(modality)
    }
}
