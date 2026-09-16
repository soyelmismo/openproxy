use super::*;
use openproxy_types::ImageGenerationRequest;

mod prompt_tests;

#[test]
fn test_horde_metadata() {
    let a = HordeAdapter::new();
    let meta = a.metadata();
    assert!(meta.supports_quota);
    assert!(meta.quota_refresh_supported);
    assert!(meta.built_in);
    assert!(!meta.deletable);
}

#[test]
fn test_parse_horde_quota_authenticated() {
    let json = serde_json::json!({
        "username": "AIArtist",
        "kudos": 1520.4,
        "worker_count": 3,
        "records": {
            "usage": {
                "tokens": 4200
            }
        }
    });
    let quota = parse_horde_quota(&json, "1700000000");
    assert_eq!(quota.session_limit, Some(1520));
    assert_eq!(quota.session_used, Some(4200));
    assert_eq!(
        quota.plan_name,
        Some("AIArtist (Kudos: 1520, Workers: 3)".to_string())
    );
    assert_eq!(quota.last_fetched_at, "1700000000");
    assert!(quota.fetch_error.is_none());
}

#[test]
fn test_parse_horde_quota_anonymous_or_default() {
    let json = serde_json::json!({
        "username": "",
        "kudos": 0.0,
        "worker_count": 0
    });
    let quota = parse_horde_quota(&json, "1700000000");
    assert_eq!(quota.session_limit, Some(0));
    assert_eq!(quota.session_used, Some(0));
    assert_eq!(
        quota.plan_name,
        Some("Anonymous (Kudos: 0, Workers: 0)".to_string())
    );
    assert_eq!(quota.last_fetched_at, "1700000000");
    assert!(quota.fetch_error.is_none());
}

#[test]
fn test_parse_horde_quota_error_response() {
    let json = serde_json::json!({
        "message": "Invalid API Key"
    });
    let quota = parse_horde_quota(&json, "1700000000");
    assert_eq!(quota.fetch_error, Some("Invalid API Key".to_string()));
    assert!(quota.session_limit.is_none());
    assert!(quota.session_used.is_none());
    assert!(quota.plan_name.is_none());
    assert_eq!(quota.last_fetched_at, "1700000000");
}

#[test]
fn test_horde_adapter_config() {
    let a = HordeAdapter::new();
    assert_eq!(a.id().as_str(), "horde");
    assert_eq!(a.config().name, "AI Horde");
    assert_eq!(a.config().base_url, "https://aihorde.net/api/v2");
    assert!(a.config().anonymous_fallback);
    assert_eq!(
        a.build_chat_url(TargetFormat::Openai, &ModelId::new("any")),
        "https://oai.aihorde.net/v1/chat/completions"
    );
    assert_eq!(a.models_url().unwrap(), "https://oai.aihorde.net/v1/models");
}

#[test]
fn test_horde_auth_header_anonymous() {
    let a = HordeAdapter::new();
    let (name, val) = a.build_auth_header("").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(val, "Bearer 0000000000");

    let (name, val) = a.build_auth_header("my-custom-key").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(val, "Bearer my-custom-key");
}

#[test]
fn test_normalize_dimension_64() {
    for (inp, exp) in [
        (0, 64),
        (30, 64),
        (64, 64),
        (65, 64),
        (95, 64),
        (96, 128),
        (500, 512),
        (700, 704),
        (1024, 1024),
        (5000, 3072),
    ] {
        assert_eq!(normalize_dimension_64(inp), exp);
    }
}

#[test]
fn test_parse_dimensions_aspect_ratios() {
    let pairs = [
        ("16:9", (1024, 576)),
        ("9:16", (576, 1024)),
        ("3:2", (960, 640)),
        ("2:3", (640, 960)),
        ("4:3", (1024, 768)),
        ("3:4", (768, 1024)),
        ("21:9", (1344, 576)),
        ("9:21", (576, 1344)),
        ("1:1", (1024, 1024)),
    ];

    for (ar, expected) in pairs {
        let (w, h) = parse_dimensions(None, Some(ar));
        assert_eq!((w, h), expected, "aspect ratio {ar}");
        assert_eq!(w % 64, 0, "width not multiple of 64: {w}");
        assert_eq!(h % 64, 0, "height not multiple of 64: {h}");
    }

    let (w, h) = parse_dimensions(Some("700x700"), None);
    assert_eq!((w, h), (704, 704));
    assert_eq!(w % 64, 0);
    assert_eq!(h % 64, 0);
}

fn make_test_req(
    prompt: &str,
    size: Option<&str>,
    neg: Option<&str>,
    quality: Option<&str>,
    seed: Option<u64>,
    post: Option<Vec<String>>,
) -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: prompt.into(),
        model: "SDXL 1.0".into(),
        n: Some(1),
        quality: quality.map(Into::into),
        response_format: None,
        size: size.map(Into::into),
        style: None,
        user: None,
        aspect_ratio: None,
        seed,
        negative_prompt: neg.map(Into::into),
        post_processing: post.map(Into::into),
    }
}

#[test]
fn test_format_image_request_standard() {
    let req = make_test_req(
        "A beautiful sunset",
        Some("512x512"),
        Some("blurry, low quality"),
        None,
        Some(42),
        None,
    );
    let body = HordeAdapter::new()
        .format_image_request(&req, "SDXL 1.0")
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["prompt"], "A beautiful sunset ### blurry, low quality");
    assert_eq!(v["params"]["width"], 512);
    assert_eq!(v["params"]["height"], 512);
    assert_eq!(v["params"]["seed"], 42);
    assert_eq!(v["models"][0], "SDXL 1.0");
    assert!(v["params"]["karras"].as_bool().unwrap());
    assert!(v["params"]["hires_fix"].as_bool().unwrap());
    assert!(v["params"].get("post_processing").is_none());
    assert!(v.get("source_image").is_none() && v.get("source_processing").is_none());
}

#[test]
fn test_format_image_request_hd_quality() {
    let req = make_test_req(
        "Cyberpunk street",
        Some("1024x1024"),
        None,
        Some("hd"),
        None,
        None,
    );
    let body = HordeAdapter::new()
        .format_image_request(&req, "SDXL 1.0")
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let post = v["params"]["post_processing"].as_array().unwrap();
    assert_eq!(post, &["RealESRGAN_x4plus", "GFPGAN"]);
}

#[test]
fn test_format_image_request_cumulative_post_processing() {
    let req = make_test_req(
        "A portrait --post NMKD_Siax,GFPGAN",
        Some("1024x1024"),
        None,
        None,
        None,
        Some(vec!["RealESRGAN_x4plus".into(), "CodeFormers".into()]),
    );
    let body = HordeAdapter::new()
        .format_image_request(&req, "SDXL 1.0")
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let post = v["params"]["post_processing"].as_array().unwrap();
    assert_eq!(
        post,
        &["RealESRGAN_x4plus", "CodeFormers", "NMKD_Siax", "GFPGAN"]
    );
}

#[test]
fn test_format_img2img_request() {
    let req = make_test_req("Add sunglasses", Some("512x512"), None, None, None, None);
    let body = HordeAdapter::new()
        .build_horde_payload(
            &req,
            "SDXL 1.0",
            Some("aGVsbG8=".into()),
            None,
            None,
            Some(0.75),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["source_image"], "aGVsbG8=");
    assert_eq!(v["source_processing"], "img2img");
    assert_eq!(v["params"]["denoising_strength"], 0.75);
    assert!(!v["params"]["hires_fix"].as_bool().unwrap());
}

#[test]
fn test_format_inpainting_request() {
    let req = make_test_req(
        "Replace car with motorcycle",
        Some("512x512"),
        None,
        None,
        None,
        None,
    );
    let body = HordeAdapter::new()
        .build_horde_payload(
            &req,
            "SDXL 1.0",
            Some("aW1hZ2U=".into()),
            Some("bWFzaw==".into()),
            None,
            Some(1.5),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["source_image"], "aW1hZ2U=");
    assert_eq!(v["source_mask"], "bWFzaw==");
    assert_eq!(v["source_processing"], "inpainting");
    assert_eq!(v["params"]["denoising_strength"], 1.0);
}

#[test]
fn test_is_vision_model() {
    for (m, exp) in [
        ("horde/vision", true),
        ("vision", true),
        ("HORDE/VISION", true),
        ("custom/vision", true),
        ("horde/sdxl", false),
        ("gpt-4o", false),
    ] {
        assert_eq!(HordeAdapter::is_vision_model(m), exp);
    }
}

#[test]
fn test_build_interrogate_payload() {
    let bytes =
        HordeAdapter::build_interrogate_payload("data:image/png;base64,iVBORw0KGgo=", &[]).unwrap();
    let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(val["source_image"], "iVBORw0KGgo=");
    assert_eq!(val["forms"][0]["name"], "caption");

    let bytes_custom = HordeAdapter::build_interrogate_payload(
        "https://example.com/cat.jpg",
        &["caption", "nsfw"],
    )
    .unwrap();
    let val_custom: serde_json::Value = serde_json::from_slice(&bytes_custom).unwrap();
    assert_eq!(val_custom["source_image"], "https://example.com/cat.jpg");
    assert_eq!(val_custom["forms"].as_array().unwrap().len(), 2);
    assert_eq!(val_custom["forms"][0]["name"], "caption");
    assert_eq!(val_custom["forms"][1]["name"], "nsfw");
}

#[test]
fn test_extract_image_from_messages() {
    use openproxy_types::OpenAIMessage;
    let user_msg = |c: serde_json::Value| OpenAIMessage {
        role: "user".into(),
        content: Some(c),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    };
    let msg1 = user_msg(serde_json::json!([
        {"type": "text", "text": "What is in this picture?"},
        {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,dGVzdGltYWdl"}}
    ]));
    assert_eq!(
        HordeAdapter::extract_image_from_messages(&[msg1]),
        Some("dGVzdGltYWdl".into())
    );
    let msg2 = user_msg(
        serde_json::json!([{"type": "image_url", "image_url": "https://example.com/dog.png"}]),
    );
    assert_eq!(
        HordeAdapter::extract_image_from_messages(&[msg2]),
        Some("https://example.com/dog.png".into())
    );
    let msg3 = user_msg(
        serde_json::json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "YW50aHJvcGlj"}}]),
    );
    assert_eq!(
        HordeAdapter::extract_image_from_messages(&[msg3]),
        Some("YW50aHJvcGlj".into())
    );
    let msg4 = user_msg(serde_json::Value::String(
        "data:image/png;base64,c3RyaW5nZGF0YQ==".into(),
    ));
    assert_eq!(
        HordeAdapter::extract_image_from_messages(&[msg4]),
        Some("c3RyaW5nZGF0YQ==".into())
    );
    let msg5 = user_msg(serde_json::Value::String("Just plain text".into()));
    assert_eq!(HordeAdapter::extract_image_from_messages(&[msg5]), None);
}

#[test]
fn test_parse_interrogate_status_caption() {
    let s1 = serde_json::json!({"id": "1", "state": "done", "forms": [{"name": "caption", "state": "done", "result": {"caption": "cat"}}]});
    assert_eq!(
        HordeAdapter::parse_interrogate_status_caption(&s1),
        Some("cat".into())
    );
    let s2 = serde_json::json!({"id": "2", "state": "done", "forms": [{"name": "caption", "state": "done", "result": "dog"}]});
    assert_eq!(
        HordeAdapter::parse_interrogate_status_caption(&s2),
        Some("dog".into())
    );
    let s3 = serde_json::json!({"id": "3", "state": "done", "caption": "beach"});
    assert_eq!(
        HordeAdapter::parse_interrogate_status_caption(&s3),
        Some("beach".into())
    );
}

#[test]
fn test_is_interrogate_done() {
    for (state, exp) in [
        ("done", (true, false)),
        ("faulted", (false, true)),
        ("waiting", (false, false)),
    ] {
        assert_eq!(
            HordeAdapter::is_interrogate_done(&serde_json::json!({"state": state})),
            exp
        );
    }
}
