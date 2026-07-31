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


impl std::str::FromStr for EndpointKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl std::fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        assert_eq!(EndpointKind::parse("chat").unwrap(), EndpointKind::Chat);
        assert_eq!(EndpointKind::parse("audio").unwrap(), EndpointKind::Audio);
        assert_eq!(EndpointKind::parse("image").unwrap(), EndpointKind::Image);
        assert_eq!(
            EndpointKind::parse("embedding").unwrap(),
            EndpointKind::Embedding
        );
        assert_eq!(EndpointKind::parse("video").unwrap(), EndpointKind::Video);
        assert!(EndpointKind::parse("unknown").is_err());
    }

    #[test]
    fn test_as_str() {
        assert_eq!(EndpointKind::Chat.as_str(), "chat");
        assert_eq!(EndpointKind::Audio.as_str(), "audio");
        assert_eq!(EndpointKind::Image.as_str(), "image");
        assert_eq!(EndpointKind::Embedding.as_str(), "embedding");
        assert_eq!(EndpointKind::Video.as_str(), "video");
    }

    #[test]
    fn test_display() {
        assert_eq!(EndpointKind::Chat.to_string(), "chat");
        assert_eq!(EndpointKind::Audio.to_string(), "audio");
        assert_eq!(EndpointKind::Image.to_string(), "image");
        assert_eq!(EndpointKind::Embedding.to_string(), "embedding");
        assert_eq!(EndpointKind::Video.to_string(), "video");
    }
}
