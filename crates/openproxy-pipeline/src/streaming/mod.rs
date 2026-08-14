pub mod pipeline;

use bytes::Bytes;
use openproxy_types::error::CoreError;

/// Action resulting from processing an SSE chunk in a `StreamingChunkStage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAction {
    /// Pass through the chunk unchanged.
    Passthrough,
    /// Mutate the chunk payload into new content.
    Mutate(String),
    /// Skip/drop this chunk from being forwarded.
    Skip,
    /// Stream reached normal termination ([DONE]).
    Done,
}

/// Modular stage/middleware trait for transforming or observing streaming SSE chunks.
pub trait StreamingChunkStage: Send + Sync {
    /// Process a streaming payload (e.g. JSON string within `data: ...`).
    /// Returns `StreamAction` indicating how to handle the result.
    fn process_chunk(&mut self, payload: &str) -> StreamAction;

    /// Finalize any remaining buffered state when the stream ends.
    fn finalize(&mut self) -> Option<String> {
        None
    }
}

/// A pipeline of `StreamingChunkStage`s executed sequentially.
#[derive(Default)]
pub struct StreamingStagePipeline {
    stages: Vec<Box<dyn StreamingChunkStage>>,
}

impl StreamingStagePipeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_stage<S: StreamingChunkStage + 'static>(&mut self, stage: S) {
        self.stages.push(Box::new(stage));
    }
}

impl StreamingChunkStage for StreamingStagePipeline {
    fn process_chunk(&mut self, payload: &str) -> StreamAction {
        let mut current_payload: Option<String> = None;
        for stage in &mut self.stages {
            let target = current_payload.as_deref().unwrap_or(payload);
            match stage.process_chunk(target) {
                StreamAction::Passthrough => {}
                StreamAction::Mutate(new_payload) => {
                    current_payload = Some(new_payload);
                }
                StreamAction::Skip => return StreamAction::Skip,
                StreamAction::Done => return StreamAction::Done,
            }
        }
        match current_payload {
            Some(s) => StreamAction::Mutate(s),
            None => StreamAction::Passthrough,
        }
    }

    fn finalize(&mut self) -> Option<String> {
        for stage in &mut self.stages {
            if let Some(s) = stage.finalize() {
                return Some(s);
            }
        }
        None
    }
}

/// Represents an event in the streaming pipeline.
pub(crate) enum ChunkEvent {
    /// A data chunk, typically representing an SSE payload or raw bytes.
    Data(Bytes),
    /// Skip sending data (already handled).
    Skip,
    /// The end of the stream (e.g., [DONE] received or EOF reached).
    Done,
    /// Early return with a complete PipelineResult.
    Return(Box<crate::PipelineResult>),
}

use crate::streaming_state::StreamContext;

pub(crate) trait ChunkInterceptor: Send + Sync {
    /// Processes a chunk event, optionally mutating it or emitting a new event.
    /// Returning an Error will abort the pipeline.
    async fn process_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        event: ChunkEvent,
    ) -> Result<ChunkEvent, CoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PrefixStage(&'static str);
    impl StreamingChunkStage for PrefixStage {
        fn process_chunk(&mut self, payload: &str) -> StreamAction {
            StreamAction::Mutate(format!("{}{}", self.0, payload))
        }
    }

    struct SuffixStage(&'static str);
    impl StreamingChunkStage for SuffixStage {
        fn process_chunk(&mut self, payload: &str) -> StreamAction {
            StreamAction::Mutate(format!("{}{}", payload, self.0))
        }
    }

    #[test]
    fn test_stage_pipeline_sequential_mutations() {
        let mut pipeline = StreamingStagePipeline::new();
        pipeline.add_stage(PrefixStage("[START]"));
        pipeline.add_stage(SuffixStage("[END]"));

        let action = pipeline.process_chunk("DATA");
        assert_eq!(action, StreamAction::Mutate("[START]DATA[END]".to_string()));
    }

    #[test]
    fn test_stage_passthrough() {
        struct NoopStage;
        impl StreamingChunkStage for NoopStage {
            fn process_chunk(&mut self, _payload: &str) -> StreamAction {
                StreamAction::Passthrough
            }
        }

        let mut pipeline = StreamingStagePipeline::new();
        pipeline.add_stage(NoopStage);

        let action = pipeline.process_chunk("RAW");
        assert_eq!(action, StreamAction::Passthrough);
    }
}
