use crate::error::CoreError;
use crate::pipeline::streaming::{ChunkEvent, ChunkInterceptor};
use crate::pipeline::streaming_state::{ChunkResult, StreamContext};
use crate::sse::SseParser;
use crate::upstream::{UpstreamBodyStream, UpstreamError, UpstreamPhase};

pub(crate) async fn run_pipeline(
    ctx: &StreamContext<'_>,
    stream: &mut UpstreamBodyStream,
    mut sse_parser: SseParser,
    processor: &mut impl ChunkInterceptor,
) -> Result<ChunkResult, CoreError> {
    loop {
        if ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(|rc| rc.is_cancelled())
        {
            return Ok(ChunkResult::Break);
        }

        let bytes = match stream.next_chunk().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                let err = match e {
                    UpstreamError::Timeout(UpstreamPhase::Body) => CoreError::UpstreamTimeout {
                        phase: "idle_chunk".into(),
                        ms: ctx.resolved_timeouts.idle_chunk.as_millis() as u64,
                    },
                    UpstreamError::Timeout(UpstreamPhase::Total) => CoreError::UpstreamTimeout {
                        phase: "total".into(),
                        ms: ctx.resolved_timeouts.total.as_millis() as u64,
                    },
                    UpstreamError::Cancel => break,
                    UpstreamError::Connection(msg)
                    | UpstreamError::Tls(msg)
                    | UpstreamError::Http(msg)
                    | UpstreamError::Decode(msg)
                    | UpstreamError::Invalid(msg) => {
                        CoreError::UpstreamConnection(format!("stream read: {}", msg))
                    }
                    UpstreamError::Timeout(_) => {
                        CoreError::UpstreamConnection(format!("stream read: {}", e))
                    }
                };
                return Err(err);
            }
        };

        sse_parser.push(&bytes)?;

        while let Some(line_bytes) = sse_parser.next_line() {
            let mut event = ChunkEvent::Data(line_bytes.into());

            if ctx
                .req
                .race_cancel
                .as_ref()
                .is_some_and(|rc| rc.is_cancelled())
            {
                return Ok(ChunkResult::Break);
            }

            event = processor.process_chunk(ctx, stream, event).await?;

            if ctx
                .req
                .race_cancel
                .as_ref()
                .is_some_and(|rc| rc.is_cancelled())
            {
                return Ok(ChunkResult::Break);
            }

            match event {
                ChunkEvent::Data(bytes) => {
                    if let Err(crate::race_sink::StreamSinkError::Lost) = ctx.sink.send(bytes).await
                    {
                        return Err(CoreError::UpstreamConnection("sink lost".to_string()));
                    }
                }
                ChunkEvent::Skip => {}
                ChunkEvent::Done => return Ok(ChunkResult::Break),

                ChunkEvent::Return(r) => return Ok(ChunkResult::Return(r)),
            }
        }
    }

    if !sse_parser.is_empty() {
        let bytes = sse_parser.remaining_bytes();
        let mut event = ChunkEvent::Data(bytes.to_vec().into());

        if !ctx
            .req
            .race_cancel
            .as_ref()
            .is_some_and(|rc| rc.is_cancelled())
        {
            event = processor.process_chunk(ctx, stream, event).await?;
            if !ctx
                .req
                .race_cancel
                .as_ref()
                .is_some_and(|rc| rc.is_cancelled())
            {
                match event {
                    ChunkEvent::Data(bytes) => {
                        if let Err(crate::race_sink::StreamSinkError::Lost) =
                            ctx.sink.send(bytes).await
                        {
                            return Err(CoreError::UpstreamConnection("sink lost".to_string()));
                        }
                    }
                    ChunkEvent::Skip => {}
                    ChunkEvent::Done => return Ok(ChunkResult::Break),

                    ChunkEvent::Return(r) => return Ok(ChunkResult::Return(r)),
                }
            }
        }
    }

    Ok(ChunkResult::Break)
}
