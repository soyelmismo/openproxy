use crate::sse::SseParser;
use crate::streaming::{ChunkEvent, ChunkInterceptor};
use crate::streaming_state::{ChunkResult, StreamContext};
use openproxy_adapters::upstream::{UpstreamBodyStream, UpstreamError, UpstreamPhase};
use openproxy_types::error::CoreError;

pub(crate) async fn run_pipeline(
    ctx: &StreamContext<'_>,
    stream: &mut UpstreamBodyStream,
    mut sse_parser: SseParser,
    processor: &mut impl ChunkInterceptor,
) -> Result<ChunkResult, CoreError> {
    loop {
        if is_race_cancelled(ctx) {
            return Ok(ChunkResult::Break);
        }

        let bytes = match stream.next_chunk().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => match map_upstream_stream_error(e, ctx) {
                Some(err) => return Err(err),
                None => break,
            },
        };

        sse_parser.push(&bytes)?;

        while let Some(line_bytes) = sse_parser.next_line() {
            let event = ChunkEvent::Data(line_bytes.into());
            if let Some(res) = process_and_dispatch_event(ctx, stream, processor, event).await? {
                return Ok(res);
            }
        }
    }

    if !sse_parser.is_empty() {
        let bytes = sse_parser.remaining_bytes();
        let event = ChunkEvent::Data(bytes.to_vec().into());
        if let Some(res) = process_and_dispatch_event(ctx, stream, processor, event).await? {
            return Ok(res);
        }
    }

    Ok(ChunkResult::Break)
}

fn is_race_cancelled(ctx: &StreamContext<'_>) -> bool {
    ctx.req
        .race_cancel
        .as_ref()
        .is_some_and(openproxy_adapters::CancellationToken::is_cancelled)
}

fn map_upstream_stream_error(e: UpstreamError, ctx: &StreamContext<'_>) -> Option<CoreError> {
    match e {
        UpstreamError::Timeout(UpstreamPhase::Body) => Some(CoreError::UpstreamTimeout {
            phase: "idle_chunk".into(),
            ms: ctx.resolved_timeouts.idle_chunk.as_millis() as u64,
        }),
        UpstreamError::Timeout(UpstreamPhase::Total) => Some(CoreError::UpstreamTimeout {
            phase: "total".into(),
            ms: ctx.resolved_timeouts.total.as_millis() as u64,
        }),
        UpstreamError::Cancel => None,
        UpstreamError::Connection(msg)
        | UpstreamError::Tls(msg)
        | UpstreamError::Http(msg)
        | UpstreamError::Decode(msg)
        | UpstreamError::Invalid(msg) => {
            Some(CoreError::UpstreamConnection(format!("stream read: {msg}")))
        }
        UpstreamError::Timeout(_) => {
            Some(CoreError::UpstreamConnection(format!("stream read: {e}")))
        }
        _ => Some(CoreError::UpstreamConnection(format!("stream read: {e:?}"))),
    }
}

async fn handle_event_dispatch(
    event: ChunkEvent,
    ctx: &StreamContext<'_>,
) -> Result<Option<ChunkResult>, CoreError> {
    match event {
        ChunkEvent::Data(bytes) => {
            if let Err(crate::race_sink::StreamSinkError::Lost) = ctx.sink.send(bytes).await {
                return Err(CoreError::UpstreamConnection("sink lost".to_string()));
            }
            Ok(None)
        }
        ChunkEvent::Skip => Ok(None),
        ChunkEvent::Done => Ok(Some(ChunkResult::Break)),
        ChunkEvent::Return(r) => Ok(Some(ChunkResult::Return(r))),
    }
}

async fn process_and_dispatch_event(
    ctx: &StreamContext<'_>,
    stream: &mut UpstreamBodyStream,
    processor: &mut impl ChunkInterceptor,
    event: ChunkEvent,
) -> Result<Option<ChunkResult>, CoreError> {
    if is_race_cancelled(ctx) {
        return Ok(Some(ChunkResult::Break));
    }
    let processed = processor.process_chunk(ctx, stream, event).await?;
    if is_race_cancelled(ctx) {
        return Ok(Some(ChunkResult::Break));
    }
    handle_event_dispatch(processed, ctx).await
}
