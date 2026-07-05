//! Endpoint kind enumeration for multi-endpoint support.
//!
//! Each OpenAI-compatible API endpoint (chat, audio, image, etc.)
//! is identified by an EndpointKind. The pipeline uses this to
//! dispatch to the correct executor, select the correct pricing
//! model, and record the correct usage shape.

use serde::{Deserialize, Serialize};

/// The kind of API endpoint a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    /// `/v1/chat/completions` — the original endpoint.
    #[default]
    Chat,
    /// `/v1/audio/transcriptions` and `/v1/audio/translations`.
    Audio,
    /// `/v1/images/generations`.
    Image,
    /// `/v1/embeddings`.
    Embedding,
    /// `/v1/video/generations` (future).
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_endpoint_kind_as_str() {
        assert_eq!(EndpointKind::Chat.as_str(), "chat");
        assert_eq!(EndpointKind::Audio.as_str(), "audio");
    }

    #[test]
    fn test_endpoint_kind_display() {
        assert_eq!(format!("{}", EndpointKind::Chat), "chat");
    }

    #[test]
    fn test_endpoint_kind_serialization() {
        assert_eq!(serde_json::to_string(&EndpointKind::Chat).unwrap(), "\"chat\"");
        assert_eq!(serde_json::to_string(&EndpointKind::Audio).unwrap(), "\"audio\"");
    }

    #[test]
    fn test_endpoint_kind_deserialization() {
        let kind: EndpointKind = serde_json::from_str("\"chat\"").unwrap();
        assert_eq!(kind, EndpointKind::Chat);
    }

    #[test]
    fn test_endpoint_request_kind() {
        let chat_req = EndpointRequest::Chat(crate::translation::OpenAIRequest {
            model: "gpt-4".to_string(),
            ..Default::default()
        });
        assert_eq!(chat_req.kind(), EndpointKind::Chat);
        assert_eq!(chat_req.model(), "gpt-4");

        let audio_req = EndpointRequest::Audio {
            model: "whisper-1".to_string(),
            file_bytes: bytes::Bytes::new(),
            file_name: "test.mp3".to_string(),
            file_content_type: "audio/mpeg".to_string(),
            form_fields: vec![],
        };
        assert_eq!(audio_req.kind(), EndpointKind::Audio);
        assert_eq!(audio_req.model(), "whisper-1");
    }

    #[test]
    fn test_endpoint_request_model_json() {
        let emb_req = EndpointRequest::Embedding(json!({"model": "text-embedding-3"}));
        assert_eq!(emb_req.model(), "text-embedding-3");

        let img_req = EndpointRequest::Image(json!({"model": "dall-e-3"}));
        assert_eq!(img_req.model(), "dall-e-3");

        let vid_req = EndpointRequest::Video(json!({"model": "sora"}));
        assert_eq!(vid_req.model(), "sora");
    }
}

impl std::fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tagged request enum. Each variant wraps the endpoint-specific
/// request type. The pipeline dispatches on this to select the
/// correct executor, body serializer, and URL builder.
#[derive(Debug, Clone)]
pub enum EndpointRequest {
    /// Chat completions request.
    Chat(crate::translation::OpenAIRequest),
    /// Audio transcription/translation request.
    /// The inner type is the raw multipart body (bytes + content-type)
    /// since multipart doesn't have a typed struct.
    Audio {
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        file_content_type: String,
        form_fields: Vec<(String, String)>,
    },
    /// Embeddings request (future).
    Embedding(serde_json::Value),
    /// Image generation request (future).
    Image(serde_json::Value),
    /// Video generation request (future).
    Video(serde_json::Value),
}

/// Tagged response enum. Each variant wraps the endpoint-specific
/// response type.
#[derive(Debug, Clone)]
pub enum EndpointResponse {
    /// Chat completions response.
    Chat(crate::translation::OpenAIResponse),
    /// Audio transcription response (raw bytes from upstream —
    /// could be JSON, text, srt, vtt depending on response_format).
    Audio(bytes::Bytes),
    /// Embeddings response (future).
    Embedding(serde_json::Value),
    /// Image generation response (future).
    Image(serde_json::Value),
    /// Video generation response (future).
    Video(serde_json::Value),
}

impl EndpointRequest {
    /// The endpoint kind for this request.
    pub fn kind(&self) -> EndpointKind {
        match self {
            Self::Chat(_) => EndpointKind::Chat,
            Self::Audio { .. } => EndpointKind::Audio,
            Self::Embedding(_) => EndpointKind::Embedding,
            Self::Image(_) => EndpointKind::Image,
            Self::Video(_) => EndpointKind::Video,
        }
    }

    /// The model name from the request.
    pub fn model(&self) -> &str {
        match self {
            Self::Chat(req) => &req.model,
            Self::Audio { model, .. } => model,
            Self::Embedding(v) => v.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            Self::Image(v) => v.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            Self::Video(v) => v.get("model").and_then(|m| m.as_str()).unwrap_or(""),
        }
    }
}
