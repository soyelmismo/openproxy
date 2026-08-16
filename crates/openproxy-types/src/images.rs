use serde::{Deserialize, Serialize};

/// Request payload for image generation (`POST /v1/images/generations`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    #[serde(default = "default_image_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
}

fn default_image_model() -> String {
    "dall-e-2".to_string()
}

/// Request payload for image edits / variations (`POST /v1/images/edits`, `POST /v1/images/variations`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImageEditRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default = "default_image_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// An individual generated image in the response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// Response payload for image generation (`POST /v1/images/generations`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageGenerationResponse {
    pub created: i64,
    pub data: Vec<ImageData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_generation_request_serde() {
        let json_str = r#"{"prompt":"a cute cat","model":"dall-e-3","n":1,"size":"1024x1024","quality":"hd"}"#;
        let req: ImageGenerationRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.prompt, "a cute cat");
        assert_eq!(req.model, "dall-e-3");
        assert_eq!(req.n, Some(1));
        assert_eq!(req.size.as_deref(), Some("1024x1024"));
        assert_eq!(req.quality.as_deref(), Some("hd"));
        assert_eq!(req.response_format, None);

        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(serialized.contains(r#""prompt":"a cute cat""#));
        assert!(serialized.contains(r#""model":"dall-e-3""#));
    }

    #[test]
    fn test_image_generation_extended_fields_serde() {
        let json_str = r#"{
            "prompt": "a futuristic city",
            "model": "flux-1",
            "aspect_ratio": "16:9",
            "seed": 42,
            "negative_prompt": "blurry, low quality"
        }"#;
        let req: ImageGenerationRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.prompt, "a futuristic city");
        assert_eq!(req.model, "flux-1");
        assert_eq!(req.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(req.seed, Some(42));
        assert_eq!(req.negative_prompt.as_deref(), Some("blurry, low quality"));

        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(serialized.contains(r#""aspect_ratio":"16:9""#));
        assert!(serialized.contains(r#""seed":42"#));
        assert!(serialized.contains(r#""negative_prompt":"blurry, low quality""#));
    }

    #[test]
    fn test_image_edit_request_serde() {
        let json_str = r#"{
            "prompt": "add a hat",
            "model": "dall-e-2",
            "n": 2,
            "size": "512x512",
            "response_format": "b64_json"
        }"#;
        let req: ImageEditRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.prompt.as_deref(), Some("add a hat"));
        assert_eq!(req.model, "dall-e-2");
        assert_eq!(req.n, Some(2));
        assert_eq!(req.size.as_deref(), Some("512x512"));
        assert_eq!(req.response_format.as_deref(), Some("b64_json"));
    }

    #[test]
    fn test_image_generation_request_defaults() {
        let json_str = r#"{"prompt":"a dog"}"#;
        let req: ImageGenerationRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.prompt, "a dog");
        assert_eq!(req.model, "dall-e-2");
        assert_eq!(req.n, None);
    }

    #[test]
    fn test_image_generation_response_serde() {
        let json_str = r#"{
            "created": 1589478378,
            "data": [
                {
                    "url": "https://example.com/image.png",
                    "revised_prompt": "a detailed cute cat"
                }
            ]
        }"#;
        let res: ImageGenerationResponse = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(res.created, 1589478378);
        assert_eq!(res.data.len(), 1);
        assert_eq!(
            res.data[0].url.as_deref(),
            Some("https://example.com/image.png")
        );
        assert_eq!(
            res.data[0].revised_prompt.as_deref(),
            Some("a detailed cute cat")
        );
        assert_eq!(res.data[0].b64_json, None);
    }
}
