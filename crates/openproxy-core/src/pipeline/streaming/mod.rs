pub mod pipeline;

use crate::error::CoreError;
use bytes::Bytes;

/// Represents an event in the streaming pipeline.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ChunkEvent {
    /// A data chunk, typically representing an SSE payload or raw bytes.
    Data(Bytes),
    /// Skip sending data (already handled).
    Skip,
    /// The end of the stream (e.g., [DONE] received or EOF reached).
    Done,
    /// Early return with a complete PipelineResult.
    Return(crate::pipeline::PipelineResult),
}

use crate::pipeline::streaming_state::StreamContext;

pub(crate) trait ChunkInterceptor: Send + Sync {
    /// Processes a chunk event, optionally mutating it or emitting a new event.
    /// Returning an Error will abort the pipeline.
    async fn process_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut crate::upstream::UpstreamBodyStream,
        event: ChunkEvent,
    ) -> Result<ChunkEvent, CoreError>;
}
