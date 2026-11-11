use crate::adapters::{DiscoveredModel, ModelId, TargetFormat};

pub fn claude_code_builtin_models() -> Vec<DiscoveredModel> {
    let models = [
        (
            "claude-sonnet-5-5",
            "Claude Sonnet 5.5",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-5-5",
            "Claude Opus 5.5",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-fable-5-1",
            "Claude Fable 5.1",
            1_000_000,
            128_000,
            true,
        ),
        ("claude-opus-5", "Claude Opus 5", 1_000_000, 128_000, true),
        (
            "claude-sonnet-5",
            "Claude Sonnet 5",
            1_000_000,
            128_000,
            true,
        ),
        ("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000, true),
        (
            "claude-opus-4-8",
            "Claude Opus 4.8",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-7",
            "Claude Opus 4.7",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-6",
            "Claude Opus 4.6",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-5-20251101",
            "Claude Opus 4.5",
            200_000,
            64_000,
            true,
        ),
        (
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            200_000,
            64_000,
            false,
        ),
        (
            "claude-sonnet-4-5-20250929",
            "Claude Sonnet 4.5",
            1_000_000,
            64_000,
            false,
        ),
    ];

    models
        .into_iter()
        .map(|(id, name, ctx, max_out, thinking)| {
            let caps = openproxy_types::ModelCapabilities {
                vision: Some(true),
                tool_calling: Some(true),
                reasoning: Some(thinking),
                thinking: Some(thinking),
                streaming: Some(true),
                ..Default::default()
            };
            DiscoveredModel {
                model_id: ModelId::new(id),
                display_name: Some(name.to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(ctx),
                max_output_tokens: Some(max_out),
                input_modalities: Some(
                    vec!["text".to_string(), "image".to_string()].into_boxed_slice(),
                ),
                output_modalities: Some(vec!["text".to_string()].into_boxed_slice()),
                model_type: Some("chat".to_string()),
                family: Some("claude".to_string()),
                capabilities: Some(caps),
            }
        })
        .collect()
}

pub fn parse_claude_models_response(body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
    let items = body.get("data")?.as_array()?;
    let mut discovered = Vec::with_capacity(items.len());

    for item in items {
        let id = item.get("id").and_then(|v| v.as_str())?;
        let name = item
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        let ctx = item
            .get("max_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(200_000);
        let max_out = item
            .get("max_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(8_192);

        let reasoning = item
            .get("capabilities")
            .and_then(|c| c.get("effort"))
            .and_then(|e| e.get("supported"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| {
                id.contains("thinking")
                    || id.contains("sonnet")
                    || id.contains("opus")
                    || id.contains("fable")
            });

        let caps = openproxy_types::ModelCapabilities {
            vision: Some(true),
            tool_calling: Some(true),
            reasoning: Some(reasoning),
            thinking: Some(reasoning),
            streaming: Some(true),
            ..Default::default()
        };

        discovered.push(DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Anthropic,
            context_length: Some(ctx),
            max_output_tokens: Some(max_out),
            input_modalities: Some(
                vec!["text".to_string(), "image".to_string()].into_boxed_slice(),
            ),
            output_modalities: Some(vec!["text".to_string()].into_boxed_slice()),
            model_type: Some("chat".to_string()),
            family: Some("claude".to_string()),
            capabilities: Some(caps),
        });
    }

    if discovered.is_empty() {
        None
    } else {
        Some(discovered)
    }
}

pub fn merge_claude_models(base: &mut Vec<DiscoveredModel>, dynamic: Vec<DiscoveredModel>) {
    for dm in dynamic {
        if let Some(existing) = base.iter_mut().find(|m| m.model_id == dm.model_id) {
            existing.display_name = dm.display_name;
            existing.context_length = dm.context_length;
            existing.max_output_tokens = dm.max_output_tokens;
            existing.capabilities = dm.capabilities;
        } else {
            base.push(dm);
        }
    }
}
