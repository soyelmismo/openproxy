use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::models::Model;
use crate::{Pipeline, PipelineRequest};

pub use openproxy_types::context::{CustomProviderMeta, ResolvedTarget};

/// The execution context passed through the pipeline stages.
#[derive(Clone)]
pub struct PipelineContext {
    pub req: PipelineRequest,
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
    pub race_cancel: Option<openproxy_adapters::upstream::CancellationToken>,
    pub trace_id: String,
    pub started: Option<std::time::Instant>,

    // State passed between inner stages
    pub resolved_timeouts: Option<crate::timeouts::Timeouts>,
    pub target_format: Option<openproxy_types::TargetFormat>,
    pub body_bytes: Option<bytes::Bytes>,
}

impl PipelineContext {
    pub fn new(req: PipelineRequest, pipeline: Pipeline) -> Self {
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
