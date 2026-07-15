use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    #[default]
    Chat,
    Audio,
    Image,
    Embedding,
    Video,
}

impl EndpointKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Embedding => "embedding",
            Self::Video => "video",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "chat" => Ok(Self::Chat),
            "audio" => Ok(Self::Audio),
            "image" => Ok(Self::Image),
            "embedding" => Ok(Self::Embedding),
            "video" => Ok(Self::Video),
            other => Err(format!("invalid endpoint kind: {}", other)),
        }
    }
}

impl std::fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
