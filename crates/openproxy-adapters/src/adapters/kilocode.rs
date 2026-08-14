// =====================================================================
// Kilocode
// =====================================================================

declare_openai_adapter!(
    /// Adapter for <https://api.kilo.ai/api/openrouter>.
    ///
    /// Kilocode is an OpenRouter gateway with its own auth. Chat goes through
    /// `/v1/chat/completions` but models are listed at `/models` (not
    /// `/v1/models`), so [`models_url`] overrides the default.
    KilocodeAdapter,
    id: "kilocode",
    name: "Kilocode",
    base_url: "https://api.kilo.ai/api/openrouter/v1",
    models_url: "https://api.kilo.ai/api/openrouter/models"
);
