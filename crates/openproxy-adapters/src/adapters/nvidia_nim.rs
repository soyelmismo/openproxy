// =====================================================================
// NVIDIA NIM
// =====================================================================

declare_openai_adapter!(
    /// Adapter for <https://integrate.api.nvidia.com>.
    ///
    /// NVIDIA NIM speaks OpenAI-compatible `/v1/chat/completions` with
    /// Bearer auth. Free tier offers 70+ models at ~40 RPM.
    NvidiaNimAdapter,
    id: "nvidia-nim",
    name: "NVIDIA NIM",
    base_url: "https://integrate.api.nvidia.com/v1",
    models_dev_canonical_ids: &["nvidia"]
);
