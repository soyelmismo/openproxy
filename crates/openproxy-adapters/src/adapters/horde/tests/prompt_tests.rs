use super::super::*;
use openproxy_types::ImageGenerationRequest;

#[test]
fn test_parse_prompt_directives_loras() {
    let prompt =
        "A girl in armor <lora:detail:0.8> <lora:face_v2:0.9:0.7> <lora:style> <LoRA:cyber:-0.5>";
    let parsed = parse_prompt_directives(prompt);
    assert_eq!(parsed.clean_prompt, "A girl in armor");
    let exp = [
        ("detail", 0.8, 0.8),
        ("face_v2", 0.9, 0.7),
        ("style", 1.0, 1.0),
        ("cyber", -0.5, -0.5),
    ];
    assert_eq!(parsed.loras.len(), exp.len());
    for (i, (n, m, c)) in exp.into_iter().enumerate() {
        assert_eq!(
            (
                parsed.loras[i].name.as_str(),
                parsed.loras[i].model,
                parsed.loras[i].clip
            ),
            (n, m, c)
        );
    }
}

#[test]
fn test_parse_prompt_directives_tis_and_embeddings() {
    let prompt =
        "sunset landscape <ti:bad_hands:0.9> <ti:easynegative> <emb:deepneg:0.6> <EMB:fastneg>";
    let parsed = parse_prompt_directives(prompt);
    assert_eq!(parsed.clean_prompt, "sunset landscape");
    let exp = [
        ("bad_hands", 0.9),
        ("easynegative", 1.0),
        ("deepneg", 0.6),
        ("fastneg", 1.0),
    ];
    assert_eq!(parsed.tis.len(), exp.len());
    for (i, (n, s)) in exp.into_iter().enumerate() {
        assert_eq!(
            (parsed.tis[i].name.as_str(), parsed.tis[i].strength),
            (n, s)
        );
    }
}

#[test]
fn test_parse_prompt_directives_flags() {
    let prompt = "cyberpunk city --sampler k_dpmpp_2m --steps 35 --cfg 7.5 --clip_skip 2 --hires --hires_denoising 0.4 --seed 123456 --control canny --post GFPGAN --upscale RealESRGAN_x4plus --allow_slow --any_worker --worker node_77 --no blurry, low res";
    let parsed = parse_prompt_directives(prompt);
    assert_eq!(parsed.clean_prompt, "cyberpunk city");
    assert_eq!(parsed.sampler_name.as_deref(), Some("k_dpmpp_2m"));
    assert_eq!(parsed.steps, Some(35));
    assert_eq!(parsed.cfg_scale, Some(7.5));
    assert_eq!(parsed.clip_skip, Some(2));
    assert_eq!(parsed.hires_fix, Some(true));
    assert_eq!(parsed.hires_fix_denoising_strength, Some(0.4));
    assert_eq!(parsed.seed, Some(123456));
    assert_eq!(parsed.control_type.as_deref(), Some("canny"));
    assert_eq!(parsed.post_processing, vec!["GFPGAN", "RealESRGAN_x4plus"]);
    assert_eq!(parsed.slow_workers, Some(true));
    assert_eq!(parsed.trusted_workers, Some(false));
    assert_eq!(parsed.workers, vec!["node_77"]);
    assert_eq!(parsed.negative_prompts, vec!["blurry, low res"]);
}

#[test]
fn test_parse_prompt_directives_clamping() {
    let parsed =
        parse_prompt_directives("test --steps 5 --cfg 0.2 --clip_skip 0 --hires_denoising 2.5");
    assert_eq!(
        (
            parsed.steps,
            parsed.cfg_scale,
            parsed.clip_skip,
            parsed.hires_fix_denoising_strength
        ),
        (Some(10), Some(1.0), Some(1), Some(1.0))
    );
    let parsed_high = parse_prompt_directives("test --steps 500 --cfg 100.0 --clip_skip 20");
    assert_eq!(
        (
            parsed_high.steps,
            parsed_high.cfg_scale,
            parsed_high.clip_skip
        ),
        (Some(100), Some(30.0), Some(12))
    );
}

#[test]
fn test_clean_residual_prompt() {
    for (inp, exp) in [
        ("  A  cat , , on a tree , ", "A cat, on a tree"),
        (
            "A mountain, , highly detailed, ",
            "A mountain, highly detailed",
        ),
        (", leading comma, ", "leading comma"),
    ] {
        assert_eq!(clean_residual_prompt(inp), exp);
    }
}

#[test]
fn bench_clean_residual_prompt_baseline() {
    let sample =
        "  A  cat , , on a tree , , highly detailed, , , masterpiece , , high resolution, , ";
    for _ in 0..100 {
        std::hint::black_box(clean_residual_prompt(sample));
    }
}

#[test]
fn test_parse_prompt_directives_multibyte_safe() {
    let parsed = parse_prompt_directives(
        "A prompt with <🦀> and <🎨:1.0> and <lora:🦀_style:0.8> <ti:✨:0.5>",
    );
    assert_eq!(
        (parsed.loras.len(), parsed.loras[0].name.as_str()),
        (1, "🦀_style")
    );
    assert_eq!((parsed.tis.len(), parsed.tis[0].name.as_str()), (1, "✨"));
    assert!(parsed.clean_prompt.contains("<🦀>") && parsed.clean_prompt.contains("<🎨:1.0>"));
}

fn make_img_req(prompt: &str, neg: Option<&str>, size: Option<&str>) -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: prompt.into(),
        model: "SDXL 1.0".into(),
        n: Some(1),
        quality: None,
        response_format: None,
        size: size.map(Into::into),
        style: None,
        user: None,
        aspect_ratio: None,
        seed: None,
        negative_prompt: neg.map(Into::into),
        post_processing: None,
    }
}

#[test]
fn test_build_horde_payload_with_all_directives() {
    let req = make_img_req(
        "A fantasy warrior <lora:armor:0.85:0.7> <ti:bad_hands:0.95> --sampler k_euler_a --steps 40 --cfg 8.0 --clip_skip 2 --hires --hires_denoising 0.35 --seed 9999 --control depth --post CodeFormers --worker fast_gpu_1 --no bad anatomy, extra limbs",
        Some("blurry"),
        Some("1024x1024"),
    );
    let body = HordeAdapter::new()
        .format_image_request(&req, "SDXL 1.0")
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["prompt"],
        "A fantasy warrior ### blurry, bad anatomy, extra limbs"
    );
    let p = &v["params"];
    assert_eq!(p["width"], 1024);
    assert_eq!(p["height"], 1024);
    assert_eq!(p["steps"], 40);
    assert_eq!(p["sampler_name"], "k_euler_a");
    assert_eq!(p["cfg_scale"], 8.0);
    assert_eq!(p["clip_skip"], 2);
    assert_eq!(p["seed"], 9999);
    assert!(p["hires_fix"].as_bool().unwrap());
    assert_eq!(p["hires_fix_denoising_strength"], 0.35);
    assert_eq!(p["control_type"], "depth");
    assert_eq!(p["post_processing"][0], "CodeFormers");
    assert_eq!(p["loras"][0]["name"], "armor");
    assert_eq!(p["loras"][0]["model"], 0.85);
    assert_eq!(p["loras"][0]["clip"], 0.7);
    assert_eq!(p["tis"][0]["name"], "bad_hands");
    assert_eq!(p["tis"][0]["strength"], 0.95);
    assert_eq!(p["workers"][0], "fast_gpu_1");
    assert_eq!(v["slow_workers"], false);
    assert_eq!(v["trusted_workers"], true);
}

#[test]
fn test_build_horde_payload_negative_prompt_deduplication() {
    let req = make_img_req(
        "A beautiful landscape --no blurry --no dark --no blurry --no low_quality --no dark",
        Some("blurry"),
        None,
    );
    let body = HordeAdapter::new()
        .format_image_request(&req, "SDXL 1.0")
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["prompt"],
        "A beautiful landscape ### blurry, dark, low_quality"
    );
}
