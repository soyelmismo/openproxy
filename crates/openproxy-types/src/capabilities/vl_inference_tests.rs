use super::*;

#[test]
fn nemotron_vl_is_detected_as_vision() {
    let caps = infer_capabilities("nvidia/nemotron-3.5-super-vl-120b-a12b");
    assert_eq!(caps.vision, Some(true));
}

#[test]
fn vl_suffix_marks_a_model_as_vision() {
    for id in ["qwen3-vl-8b", "Qwen/Qwen3-VL-32B", "some-vlm-model"] {
        assert_eq!(
            infer_capabilities(id).vision,
            Some(true),
            "{id} should be vision"
        );
    }
}

#[test]
fn plain_text_models_stay_text_only() {
    for id in ["llama-3.3-70b", "qwen3-27b", "deepseek-chat"] {
        assert_ne!(
            infer_capabilities(id).vision,
            Some(true),
            "{id} must not be vision"
        );
    }
}

#[test]
fn inferred_modalities_include_image_for_a_vl_model() {
    let caps = infer_capabilities("nvidia/nemotron-3.5-super-vl-120b-a12b");
    let mods = infer_input_modalities_for_model("nvidia/nemotron-3.5-super-vl-120b-a12b", &caps);
    assert!(mods.contains(&"image"), "modalities: {mods:?}");
}
