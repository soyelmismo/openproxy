pub use crate::adapters::opencode_common::classify_opencode_target_format as classify_zen_target_format;

crate::define_opencode_adapter!(
    /// Adapter for <https://opencode.ai/zen/v1>.
    ///
    /// OpenCode Zen is mixed: some models speak OpenAI, others Anthropic, and
    /// the per-model choice is recorded in `models.target_format`. The adapter
    /// picks `/chat/completions` vs `/messages` based on that stored value, and
    /// the auth header flips between `Authorization: Bearer ...` and
    /// `x-api-key: ...` accordingly.
    OpenCodeZenAdapter,
    id: "opencode-zen",
    name: "OpenCode Zen",
    base_url: "https://opencode.ai/zen/v1",
    models_dev_canonical_ids: &["opencode"],
);
