use serde::{Deserialize, Serialize};

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
    #[serde(rename_all = "lowercase")]
    pub enum EndpointKind {
        #[default]
        Chat => "chat",
        Audio => "audio",
        Image => "image",
        Embedding => "embedding",
        Video => "video",
    }
    error: "endpoint kind"
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
