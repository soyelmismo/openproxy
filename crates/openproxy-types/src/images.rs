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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_string_or_vec_opt"
    )]
    pub post_processing: Option<Box<[String]>>,
}

fn deserialize_string_or_vec_opt<'de, D>(deserializer: D) -> Result<Option<Box<[String]>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrVecVisitor;

    impl<'de> serde::de::Visitor<'de> for StringOrVecVisitor {
        type Value = Option<Box<[String]>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, a list of strings, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let list: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Ok((!list.is_empty()).then(|| list.into_boxed_slice()))
        }

        fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let list = collect_non_empty_seq(seq)?;
            Ok((!list.is_empty()).then_some(list))
        }
    }

    deserializer.deserialize_option(StringOrVecVisitor)
}

fn collect_non_empty_seq<'de, A>(mut seq: A) -> Result<Box<[String]>, A::Error>
where
    A: serde::de::SeqAccess<'de>,
{
    let mut list = Vec::new();
    while let Some(elem) = seq.next_element::<String>()? {
        let trimmed = elem.trim();
        if !trimmed.is_empty() {
            list.push(trimmed.to_string());
        }
    }
    Ok(list.into_boxed_slice())
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_string_or_vec_opt"
    )]
    pub post_processing: Option<Box<[String]>>,
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
    pub data: Box<[ImageData]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_generation_request_serde() {
        let json_str =
            r#"{"prompt":"a cute cat","model":"dall-e-3","n":1,"size":"1024x1024","quality":"hd"}"#;
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

    #[test]
    fn test_image_post_processing_serde() {
        // Array format
        let json_arr = r#"{"prompt":"dragon","post_processing":["RealESRGAN_x4plus","GFPGAN"]}"#;
        let req_arr: ImageGenerationRequest =
            serde_json::from_str(json_arr).expect("deserialize array");
        assert_eq!(
            req_arr.post_processing,
            Some(vec!["RealESRGAN_x4plus".to_string(), "GFPGAN".to_string()].into_boxed_slice())
        );

        // Comma-separated string format
        let json_str =
            r#"{"prompt":"dragon","post_processing":"RealESRGAN_x4plus, GFPGAN, CodeFormers"}"#;
        let req_str: ImageGenerationRequest =
            serde_json::from_str(json_str).expect("deserialize string");
        assert_eq!(
            req_str.post_processing,
            Some(
                vec![
                    "RealESRGAN_x4plus".to_string(),
                    "GFPGAN".to_string(),
                    "CodeFormers".to_string()
                ]
                .into_boxed_slice()
            )
        );

        // Null / omitted
        let json_none = r#"{"prompt":"dragon"}"#;
        let req_none: ImageGenerationRequest =
            serde_json::from_str(json_none).expect("deserialize none");
        assert_eq!(req_none.post_processing, None);
    }
}
