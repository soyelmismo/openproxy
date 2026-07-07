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
pub struct PipelineContext {
    pub req: Arc<PipelineRequest>,
    pub pipeline: Pipeline,

    // State populated by stages
    pub combo: Option<Combo>,
    pub targets: Vec<ResolvedTarget>,
    pub attempt: u8,
    pub combo_walk_log: Vec<String>,
}

impl PipelineContext {
    pub fn new(req: Arc<PipelineRequest>, pipeline: Pipeline) -> Self {
        Self {
            req,
            pipeline,
            combo: None,
            targets: Vec::new(),
            attempt: 1,
            combo_walk_log: Vec::new(),
        }
    }
}
