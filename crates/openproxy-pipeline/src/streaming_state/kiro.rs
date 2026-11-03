use super::{ChunkProcessor, StreamContext};
use crate::sse::kiro as kiro_sse;
use crate::streaming::ChunkEvent;
use openproxy_adapters::adapters::kiro::{KiroEventType, KiroFrameEvent};
use openproxy_types::error::CoreError;

impl ChunkProcessor<'_> {
    pub(super) async fn process_kiro_frame(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        frame_bytes: &[u8],
    ) -> Result<ChunkEvent, CoreError> {
        let Some(pos) = frame_bytes.iter().position(|&b| b == 0) else {
            return Ok(ChunkEvent::Skip);
        };

        let event_type_str = std::str::from_utf8(&frame_bytes[..pos]).unwrap_or("");
        let payload = bytes::Bytes::copy_from_slice(&frame_bytes[pos + 1..]);

        let event_type = match event_type_str {
            "assistantResponseEvent" => KiroEventType::AssistantResponse,
            "toolUseEvent" => KiroEventType::ToolUse,
            "messageStopEvent" => KiroEventType::MessageStop,
            "contextUsageEvent" => KiroEventType::ContextUsage,
            _ => KiroEventType::Other(event_type_str.to_string()),
        };

        let frame = KiroFrameEvent {
            event_type,
            payload,
        };

        super::processor::record_ttft_if_first(ctx, self.state);

        match kiro_sse::kiro_frame_to_chunk(
            &mut self.state.kiro,
            &frame,
            ctx.chunk_id,
            ctx.created,
            ctx.model_name,
        ) {
            Some(chunk) => self.handle_translated_chunk(ctx, stream, chunk).await,
            None => Ok(ChunkEvent::Skip),
        }
    }
}
