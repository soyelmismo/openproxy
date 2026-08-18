pub use crate::adapters::opencode_common::classify_opencode_target_format as classify_go_target_format;

crate::define_opencode_adapter!(
    /// Adapter for <https://opencode.ai/zen/go/v1>.
    OpenCodeGoAdapter,
    id: "opencode-go",
    name: "OpenCode Go",
    base_url: "https://opencode.ai/zen/go/v1",
    models_dev_canonical_ids: &["opencode-go"],
);
