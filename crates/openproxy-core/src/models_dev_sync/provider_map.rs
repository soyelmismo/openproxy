//! Provider mapping between models.dev and OpenProxy internal provider IDs.

use std::collections::HashMap;
use std::sync::LazyLock;

/// models.dev API source URL.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Provider mapping: models.dev provider id → our internal IDs.
///
/// Static fallback/supplemental table for mapping models.dev provider IDs to
/// internal OpenProxy provider IDs when adapters are not directly registered
/// or for aliases (e.g. `minimax-cn`).
pub const PROVIDER_MAP: &[(&str, &[&str])] = &[
    ("openai", &["openrouter"]),
    ("anthropic", &["openrouter"]),
    ("google", &["gemini"]),
    ("meta", &["openrouter"]),
    ("mistral", &["openrouter"]),
    ("deepseek", &["openrouter"]),
    ("qwen", &["openrouter"]),
    ("nvidia", &["nvidia-nim"]),
    ("minimax", &["minimax", "minimax-cn"]),
    ("amazon", &["openrouter"]),
    ("cohere", &["openrouter"]),
    ("opencode", &["opencode-zen"]),
    ("opencode-go", &["opencode-go"]),
    ("perplexity", &["openrouter"]),
    ("groq", &["openrouter"]),
    ("together", &["openrouter"]),
    ("fireworks", &["openrouter"]),
    ("deepinfra", &["openrouter"]),
    ("xai", &["openrouter"]),
];

fn ingest_adapter_mappings(map: &mut HashMap<String, Vec<String>>) {
    for adapter in openproxy_adapters::adapters::builtin_adapters() {
        let internal_id = adapter.id().as_str().to_string();
        for &canon_id in adapter.models_dev_canonical_ids() {
            let entry = map.entry(canon_id.to_string()).or_default();
            if !entry.contains(&internal_id) {
                entry.push(internal_id.clone());
            }
        }
    }
}

fn ingest_static_provider_map(map: &mut HashMap<String, Vec<String>>) {
    for &(canon_id, internal_ids) in PROVIDER_MAP {
        let entry = map.entry(canon_id.to_string()).or_default();
        for &id in internal_ids {
            let id_str = id.to_string();
            if !entry.contains(&id_str) {
                entry.push(id_str);
            }
        }
    }
}

pub type ProviderTargetMap = HashMap<Box<str>, Box<[Box<str>]>>;

/// Builds the mapping of canonical models.dev provider ID -> OpenProxy internal provider IDs.
pub fn build_provider_mapping() -> ProviderTargetMap {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    ingest_adapter_mappings(&mut map);
    ingest_static_provider_map(&mut map);
    map.into_iter()
        .map(|(k, v)| {
            let boxed_v: Box<[Box<str>]> = v.into_iter().map(String::into_boxed_str).collect();
            (k.into_boxed_str(), boxed_v)
        })
        .collect()
}

/// Pre-indexed provider map built from adapter metadata + static fallback table.
pub static RESOLVED_PROVIDER_MAP: LazyLock<ProviderTargetMap> =
    LazyLock::new(build_provider_mapping);

pub fn resolve_provider_target_ids(ext_id: &str) -> Vec<&str> {
    let mapped_ids = RESOLVED_PROVIDER_MAP
        .get(ext_id)
        .map_or(&[][..], |v| v.as_ref());

    let mut all_ids: Vec<&str> = Vec::with_capacity(1 + mapped_ids.len());
    all_ids.push(ext_id);
    for id in mapped_ids {
        let id_str: &str = id.as_ref();
        if !all_ids.contains(&id_str) {
            all_ids.push(id_str);
        }
    }
    all_ids
}
