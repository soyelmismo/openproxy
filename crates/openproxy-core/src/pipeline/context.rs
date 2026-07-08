use crate::combos::{Combo, ComboTarget};
use crate::models::Model;
use crate::pipeline::{Pipeline, PipelineRequest};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct CustomProviderMeta {
    pub access_token: String,
    pub maybe_refresh: Option<String>,
    pub kiro_region: Option<String>,
    pub kiro_profile_arn: Option<String>,
    pub antigravity_project: Option<String>,
    pub codex_workspace_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub target: ComboTarget,
    pub model: Model,
    pub api_key: String,
    pub api_key_label: Option<String>,
    pub custom_meta: Option<CustomProviderMeta>,
}

/// The execution context passed through the pipeline stages.
#[derive(Clone)]
pub struct PipelineContext {
    pub req: Arc<PipelineRequest>,
    pub pipeline: Pipeline,

    // State populated by outer stages
    pub combo: Option<Combo>,
    pub targets: Vec<ResolvedTarget>,
    pub attempt: u8,
    pub combo_walk_log: Vec<String>,

    // State populated per-target (used by inner PipelineChain)
    pub current_target: Option<ResolvedTarget>,
    pub current_target_attempt: u8,
    pub race_size: u8,
    pub race_cancel: Option<crate::upstream::CancellationToken>,
    pub trace_id: String,
    pub started: Option<std::time::Instant>,

    // State passed between inner stages
    pub resolved_timeouts: Option<crate::timeouts::Timeouts>,
    pub target_format: Option<crate::models::TargetFormat>,
    pub body_bytes: Option<bytes::Bytes>,
}

impl PipelineContext {
    pub fn new(req: Arc<PipelineRequest>, pipeline: Pipeline) -> Self {
        let trace_id = req.trace_id.to_string();
        Self {
            req,
            pipeline,
            combo: None,
            targets: Vec::new(),
            attempt: 1,
            combo_walk_log: Vec::new(),
            current_target: None,
            current_target_attempt: 1,
            race_size: 1,
            race_cancel: None,
            trace_id,
            started: None,
            resolved_timeouts: None,
            target_format: None,
            body_bytes: None,
        }
    }
}
