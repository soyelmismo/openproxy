use super::processor::ChunkProcessor;
use super::{StreamContext, StreamingState};
use crate::streaming::{StreamAction, StreamingChunkStage};
use openproxy_types::error::CoreError;

pub(super) fn parse_translated_sse_line(
    state: &mut StreamingState,
    target_format: openproxy_types::TargetFormat,
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Result<Option<crate::sse::UpstreamSseChunk>, CoreError> {
    match target_format {
        openproxy_types::TargetFormat::Responses => crate::sse::parse_responses_sse_stream_line(
            line,
            chunk_id,
            created,
            model_name,
            &mut state.responses_sse_state,
        ),
        openproxy_types::TargetFormat::Openai => crate::sse::parse_openai_sse_line(line),
        openproxy_types::TargetFormat::CommandCodeGo => crate::sse::parse_commandcode_sse_line(
            line,
            chunk_id,
            created,
            model_name,
            &mut state.commandcode_sse_state,
        ),
        openproxy_types::TargetFormat::Atomesus => {
            crate::sse::parse_atomesus_sse_line(line, chunk_id, created, model_name)
        }
        openproxy_types::TargetFormat::Gemini => {
            crate::sse::parse_gemini_sse_line(line, chunk_id, created, model_name)
        }
        openproxy_types::TargetFormat::Anthropic => {
            let Some(payload) =
                crate::sse::parse_anthropic_sse_stream_line(line, &mut state.current_event_type)?
            else {
                return Ok(None);
            };
            crate::sse::translate_anthropic_sse_event(
                &payload,
                chunk_id,
                created,
                model_name,
                &mut state.tool_use_acc,
                &mut state.tool_call_index_counter,
            )
        }
        openproxy_types::TargetFormat::SystemOne => Ok(None),
    }
}

impl ChunkProcessor<'_> {
    /// Metadata-only: clasifica la línea SSE cruda y actualiza los contadores
    /// de razonamiento. Se llama **antes** del traductor para que el hidden
    /// thinking (que el traductor convierte en liveness y descarta) quede
    /// registrado.
    ///
    /// `line` es la línea SSE cruda que entrega `decode_and_record_line`, una
    /// por llamada (`event: X` y `data: {...}` por separado), NO la forma
    /// combinada `"X\ndata: {...}"` que fabrica el parser: esa se construye
    /// después, dentro de `parse_translated_sse_line`. Por eso
    /// [`ReasoningProgress::observe`] trackea el evento él mismo.
    ///
    /// Logs, todos metadata-only:
    /// * `debug` por delta clasificado (tipo, índice, estimación y
    ///   longitudes; nunca texto ni SSE crudo).
    /// * un único `info` de primera actividad de razonamiento por request,
    ///   para que el hidden thinking no sea invisible sin generar ruido por
    ///   token.
    ///
    /// No altera el chunk, el forwarding, el deadline ni el stop reason, y no
    /// emite `StageEvent`: un stage desconocido haría que
    /// `publish_stage_global` retrocediera el rank del intento en el
    /// `INFLIGHT_REGISTRY` compartido de `streaming` (3) a 0.
    pub(super) fn record_reasoning_progress(&mut self, ctx: &StreamContext<'_>, line: &str) {
        let elapsed_ms = ctx.started.elapsed().as_millis() as u64;
        let probe = self.state.reasoning_progress.observe(line);
        let Some(p) = probe.as_ref() else {
            return;
        };
        self.state.reasoning_progress.record(p, elapsed_ms);

        tracing::debug!(
            request_id = %ctx.req.request_id,
            elapsed_ms,
            delta_type = %p.delta_type,
            block_index = ?p.block_index,
            estimated_tokens = ?p.estimated_tokens,
            thinking_len = p.thinking_len,
            text_len = p.text_len,
            "anthropic reasoning delta observed (metadata-only)"
        );
        // Un único evento `info` por request, en la primera actividad de
        // razonamiento: evita la invisibilidad trace-only sin generar ruido
        // por token.
        if p.is_hidden_progress() && !self.state.reasoning_first_logged {
            self.state.reasoning_first_logged = true;
            tracing::info!(
                request_id = %ctx.req.request_id,
                trace_id = %ctx.trace_id,
                elapsed_ms,
                block_index = ?p.block_index,
                estimated_tokens = ?p.estimated_tokens,
                "anthropic hidden thinking in progress (metadata-only; estimate is not billed usage)"
            );
        }
    }

    /// Emite el contador agregado de razonamiento al cerrar el stream. Un
    /// solo `info` por request con los totales; nunca contenido. Se invoca en
    /// el cierre normal y en los caminos de error/retorno de translated.rs,
    /// para que un stream que muere con hidden thinking en curso deje rastro.
    pub(super) fn emit_reasoning_summary(&self, ctx: &StreamContext<'_>) {
        let rp = &self.state.reasoning_progress;
        if !rp.saw_reasoning() {
            return;
        }
        tracing::info!(
            request_id = %ctx.req.request_id,
            hidden_events = rp.hidden_events,
            visible_reasoning_events = rp.visible_reasoning_events,
            text_delta_events = rp.text_delta_events,
            estimated_tokens_sum = rp.estimated_tokens_sum,
            ping_events = rp.ping_events,
            first_activity_ms = ?rp.first_activity_ms,
            first_block_index = ?rp.first_block_index,
            "anthropic hidden-thinking metadata summary"
        );
    }
}

impl ChunkProcessor<'_> {
    pub(super) async fn handle_translated_done(
        &mut self,
        ctx: &StreamContext<'_>,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        Self::sync_chunk_metadata_to_accumulator(
            self.state,
            chunk.usage.take(),
            chunk.stop_reason.take(),
            true,
        );
        if let Some(residual) = self.state.normalizer.finalize() {
            let sse_bytes = crate::sse::build_sse_frame(&residual);
            let _ = self.send_to_sink(ctx, sse_bytes).await;
        }

        let payload_is_null = chunk.payload.is_null();
        let json_str = chunk.into_json_string();

        let norm_action = self.state.normalizer.process_chunk(&json_str);
        let normalized_json = match &norm_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => &json_str,
        };

        if let Some(a) = self.state.acc.as_mut() {
            a.append_openai_raw(normalized_json);
        }

        if let Some(cancel) = self.check_race_cancelled(ctx).await {
            return Ok(cancel);
        }

        if !payload_is_null {
            let deliver_res = self
                .deliver_payload_through_pii(ctx, normalized_json)
                .await?;
            if let crate::streaming::ChunkEvent::Return(_) = deliver_res {
                return Ok(deliver_res);
            }
        }

        self.emit_reasoning_summary(ctx);
        self.finish_chat_stream(ctx).await
    }

    pub(super) async fn handle_translated_chunk(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        mut chunk: crate::sse::UpstreamSseChunk,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        // Hidden generation progress: the upstream reported that it is
        // generating (Anthropic `thinking_delta` with an empty `thinking`
        // and a positive `estimated_tokens`) but nothing the client can see.
        // Nothing is forwarded, accumulated or normalised — the only effect
        // is telling the body stream that the request is alive, which moves
        // the wait off the TTFT deadline onto the idle-chunk budget.
        if chunk.is_liveness() {
            stream.note_progress_chunk();
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        Self::sync_chunk_metadata_to_accumulator(
            self.state,
            chunk.usage.take(),
            chunk.stop_reason.take(),
            false,
        );
        // Map cloaked tool names back before the chunk is serialized, so the
        // accumulator and the client both see the names the client declared.
        if let Some(map) = self.state.tool_name_restore.as_ref() {
            crate::claude_tool_names::restore_chunk_tool_names(&mut chunk.payload, map);
        }
        let delta_reasoning = chunk.delta_reasoning.take();
        let _delta_tool_calls = std::mem::take(&mut chunk.delta_tool_calls);
        let chunk_has_content = chunk.has_content;
        let json_str = chunk.into_json_string();

        let norm_action = self.state.normalizer.process_chunk(&json_str);
        if matches!(norm_action, StreamAction::Skip) {
            if chunk_has_content {
                stream.note_content_chunk();
            }
            return Ok(crate::streaming::ChunkEvent::Skip);
        }
        let normalized_json = match &norm_action {
            StreamAction::Mutate(s) => s.as_str(),
            _ => &json_str,
        };

        if let Some(a) = self.state.acc.as_mut() {
            if let Some(dr) = &delta_reasoning
                && !dr.is_empty()
            {
                a.append_reasoning(dr);
            }
            a.append_openai_raw(normalized_json);
        }

        if chunk_has_content {
            stream.note_content_chunk();
        }
        self.deliver_payload_through_pii(ctx, normalized_json).await
    }

    /// Gemini / Anthropic SSE handler. Translates to OpenAI shape, then forwards.
    pub(super) async fn process_translated_format(
        &mut self,
        ctx: &StreamContext<'_>,
        stream: &mut openproxy_adapters::upstream::UpstreamBodyStream,
        line: &str,
    ) -> Result<crate::streaming::ChunkEvent, CoreError> {
        let line_payload = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
            .unwrap_or(line)
            .trim();
        if let Some(ret) = self
            .check_and_handle_inline_upstream_error(ctx, line_payload)
            .await
        {
            // Un error inline del upstream corta el stream: el resumen
            // metadata-only debe quedar antes del retorno para que un
            // thinking en curso no se pierda.
            self.emit_reasoning_summary(ctx);
            return Ok(ret);
        }

        // Metadata-only: clasificar la línea ANTES del traductor, para que
        // el hidden thinking (que el traductor convierte en liveness y
        // descarta) quede registrado. No altera el chunk ni el forwarding.
        self.record_reasoning_progress(ctx, line);

        let parsed = parse_translated_sse_line(
            self.state,
            ctx.target_format,
            line,
            ctx.chunk_id,
            ctx.created,
            ctx.model_name,
        );

        match parsed {
            Ok(Some(chunk)) => {
                if chunk.done {
                    self.handle_translated_done(ctx, chunk).await
                } else {
                    self.handle_translated_chunk(ctx, stream, chunk).await
                }
            }
            Ok(None) => Ok(crate::streaming::ChunkEvent::Skip),
            Err(e) => {
                tracing::warn!(
                    chunk_id = %ctx.chunk_id,
                    error = %e,
                    "failed to parse SSE line from upstream"
                );
                self.emit_reasoning_summary(ctx);
                let acc_ref = self.state.acc.as_mut().map(|a| {
                    a.mark_partial();
                    &*a
                });
                Ok(crate::streaming::ChunkEvent::Return(Box::new(
                    self.dispatcher
                        .record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                            req: ctx.req.to_owned(),
                            combo: ctx.combo,
                            target: ctx.target,
                            ctx: crate::FailureContext {
                                proxy_url: ctx.proxy_url.clone(),
                                proxy_status: ctx.proxy_status.clone(),
                                attempt: ctx.attempt,
                                race_size: ctx.race_size,
                                err: &e,
                                started: ctx.started,
                                model: Some(ctx.model),
                                connect_ms: Some(ctx.connect_and_send_ms),
                                ttft_ms: self.state.ttft_ms,
                                status_code: e.http_status(),
                            },
                            trace_id: ctx.trace_id.to_string(),
                            acc: acc_ref,
                            chunk_id: Some(ctx.chunk_id),
                            created: ctx.created,
                            model_name: ctx.model_name,
                        })
                        .await,
                )))
            }
        }
    }
}
