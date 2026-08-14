// =====================================================================
// Nous Research
// =====================================================================

declare_openai_adapter!(
    /// Adapter for <https://inference-api.nousresearch.com>.
    ///
    /// Nous Research speaks OpenAI-compatible `/v1/chat/completions` with
    /// Bearer auth. Free-tier models include Hermes-4-405B and Hermes-4-70B.
    NousResearchAdapter,
    id: "nous-research",
    name: "Nous Research",
    base_url: "https://inference-api.nousresearch.com/v1"
);
