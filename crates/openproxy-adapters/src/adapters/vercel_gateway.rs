// =====================================================================
// Vercel AI Gateway
// =====================================================================

declare_openai_adapter!(
    /// Adapter for Vercel AI Gateway (<https://ai-gateway.vercel.sh>).
    ///
    /// Vercel AI Gateway speaks OpenAI-compatible `/v1/chat/completions` with
    /// Bearer auth and routes to various frontier and open-source models.
    VercelGatewayAdapter,
    id: "vercel-gateway",
    name: "Vercel Gateway",
    base_url: "https://ai-gateway.vercel.sh/v1",
    models_dev_canonical_ids: &["vercel", "vercel-gateway", "zai"]
);
