
use crate::combos::{Combo, ComboTarget};
use crate::error::CoreError;
use crate::models::Model;
use crate::pipeline::{FailureContext, Pipeline, PipelineRequest, PipelineResult, parse_retry_after_ms};
use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use crate::upstream::{CancellationToken, UpstreamError, UpstreamPhase, UpstreamRequest};
use std::sync::Arc;
use std::time::Instant;

use crate::think_extractor::extract_think_from_response;
use crate::pipeline::SSE_DONE_BYTES;

#[derive(Default)]
struct ToolCallAccumulator {
    /// Map of tool_call index → running total of arguments seen so far.
    args_by_index: std::collections::HashMap<u64, String>,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self::default()
    }

    /// Process a tool_call delta. Returns the `arguments` value that
    /// should be sent to the client (just the new fragment, not the
    /// running total). If the upstream already sends fragments (the
    /// correct behavior), this is a no-op — the fragment is returned
    /// as-is and the running total is updated.
    fn process(&mut self, index: u64, arguments: &str) -> String {
        let prev = self.args_by_index.entry(index).or_default();
        if prev.is_empty() {
            // First chunk for this index — the arguments IS the
            // fragment (there's nothing before it).
            prev.push_str(arguments);
            return arguments.to_string();
        }
        if arguments.starts_with(prev.as_str()) {
            // Running-total pattern: the upstream sent prev + new.
            // Extract just the new suffix.
            let new_fragment = &arguments[prev.len()..];
            prev.push_str(new_fragment);
            new_fragment.to_string()
        } else {
            // Fragment pattern (correct OpenAI behavior): the
            // upstream sent just the new fragment. Update the
            // running total and pass it through.
            prev.push_str(arguments);
            arguments.to_string()
        }
    }
}

fn apply_reasoning_normalizations(
    payload: &str,
    think_extractor: &mut crate::think_extractor::ThinkStreamExtractor,
    tool_call_acc: &mut ToolCallAccumulator,
) -> Option<String> {
    // Step 1: normalize non-standard reasoning fields.
    let normalized = crate::sse_accumulator::normalize_nonstandard_reasoning_fields(payload);
    let p: &str = normalized.as_deref().unwrap_or(payload);

    // Fast check: if there's no "content" AND no "tool_calls", skip
    // the JSON parse entirely — the chunk is role-only, finish, etc.
    let has_content = p.contains("\"content\"");
    let has_tool_calls = p.contains("\"tool_calls\"");
    if !has_content && !has_tool_calls {
        return normalized;
    }

    let mut v: serde_json::Value = serde_json::from_str(p).ok()?;
    let choices = v.get_mut("choices").and_then(|c| c.as_array_mut())?;
    let choice = choices.first_mut()?;
    let delta = choice.get_mut("delta")?;
    let obj = delta.as_object_mut()?;

    let mut modified = false;

    // Step 2: think extraction on content (only if content is a string).
    if has_content
        && let Some(content) = obj
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
    {
        let (clean_content, extracted_reasoning) = think_extractor.process(&content);
        let content_changed = clean_content != content;
        let has_native_reasoning = obj.contains_key("reasoning_content");

        if content_changed {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(clean_content),
            );
            modified = true;
        }

        if !extracted_reasoning.is_empty() && !has_native_reasoning {
            obj.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(extracted_reasoning),
            );
            modified = true;
        }
    }

    // Step 3: normalize tool_call arguments — detect running-total
    // pattern and replace with just the new fragment.
    if has_tool_calls
        && let Some(tool_calls) = obj.get_mut("tool_calls").and_then(|t| t.as_array_mut())
    {
        for tc in tool_calls.iter_mut() {
            let tc_obj = match tc.as_object_mut() {
                Some(o) => o,
                None => continue,
            };
            let index = tc_obj.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            let func = match tc_obj.get_mut("function").and_then(|f| f.as_object_mut()) {
                Some(f) => f,
                None => continue,
            };
            let arguments = match func.get("arguments").and_then(|a| a.as_str()) {
                Some(a) => a.to_string(),
                None => continue,
            };
            let new_fragment = tool_call_acc.process(index, &arguments);
            if new_fragment != arguments {
                func.insert(
                    "arguments".to_string(),
                    serde_json::Value::String(new_fragment),
                );
                modified = true;
            }
        }
    }

    if !modified {
        return normalized;
    }

    serde_json::to_string(&v).ok().or(normalized)
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_upstream(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: Arc<PipelineRequest>,
        model: &Model,
        target_format: crate::models::TargetFormat,
        url: &str,
        headers: &[(String, String)],
        body_bytes: bytes::Bytes,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: String,
    ) -> PipelineResult {
        // Gate 2: both the non-streaming path AND the streaming path
        // now go through the hyper-based `UpstreamClient`
        // (`PipelineConfig::upstream_client`). The reqwest
        // `request_builder` chain is gone from this dispatch.
        //
        // `body_bytes` is pre-serialized by the caller (single pass
        // from the translated struct — no intermediate `Value`).
        let mut upstream_request = UpstreamRequest::post_json(url.to_string(), body_bytes);
        // If the provider has proxy routing enabled, fetch/assign a proxy
        let proxy_url = {
            let conn = self.conn.lock();
            match crate::free_proxies::get_or_assign_provider_proxy(&conn, &target.provider_id) {
                Ok(url) => url,
                Err(e) => {
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &e,
                            started,
                            model: Some(model),
                            connect_ms: None,
                            ttft_ms: None,
                            status_code: e.http_status(),
                        },
                    );
                }
            }
        };
        upstream_request.proxy = proxy_url;
        // is_streaming is always true because we force stream=true
        // to the upstream (see comment above). The body-chunk gap
        // timeout (idle_chunk_ms) applies normally — but only AFTER
        // the first chunk arrives (the initial deadline is
        // total_deadline, not start + body_chunk_ms).
        upstream_request.is_streaming = true;
        // Caller-supplied headers (auth, content-type overrides from
        // the adapter, etc.) — `post_json` already sets
        // `Content-Type: application/json`, so `insert` overwrites if
        // a caller header collides (matches the reqwest chain's
        // behavior with `.header(k, v)` which appends; we choose
        // overwrite for determinism — the adapter layer is
        // responsible for not setting conflicting headers).
        for (k, v) in headers {
            // HeaderMap's insert() requires HeaderName/HeaderValue;
            // parse the strings. Skip headers that fail to parse —
            // matches the previous `.header(k.as_str(), v.as_str())`
            // which also silently dropped invalid values.
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                upstream_request.headers.insert(name, value);
            }
        }

        // ALWAYS use the streaming path to the upstream. This
        // simplifies the code (one path instead of two) and fixes
        // the timeout issues with non-streaming requests:
        // - TTFT (first token) is properly measured for both modes
        // - idle_chunk_ms only applies after the first token arrives
        // - The upstream LLM starts generating immediately instead
        //   of waiting for the full response before sending
        // - Cancel propagation is faster (can cancel mid-generation)
        //
        // For non-streaming clients (stream: false), the stream_sink
        // is a Direct channel that the chat handler reads from. The
        // pipeline sends SSE chunks; the chat handler accumulates
        // them and returns the full JSON when the stream completes.
        // The `is_streaming` flag on the UpstreamRequest is set
        // based on the client's preference, but the upstream call
        // always uses stream=true (set in the translation layer).
        if let Some(sink) = &req.stream_sink {
            return self
                .dispatch_upstream_streaming(
                    target,
                    combo,
                    Arc::clone(&req),
                    model,
                    target_format,
                    sink,
                    resolved_timeouts,
                    started,
                    attempt,
                    race_size,
                    trace_id,
                    upstream_request,
                )
                .await;
        }

        // Fallback: no stream_sink (shouldn't happen in production —
        // the chat handler always provides one). Uses the old
        // non-streaming path as a safety net.
        // building the request) we short-circuit to a structured
        // `ClientDisconnected` result. The pre-flight is the only
        // place we map `UpstreamError::Cancel` → `ClientDisconnected`
        // — see below for the rationale.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream send; aborting attempt"
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::ClientDisconnected,
                    started,
                    model: Some(model),
                    connect_ms: Some(elapsed),
                    ttft_ms: None,
                    status_code: CoreError::ClientDisconnected.http_status(),
                },
            );
        }
        let cancel_token = CancellationToken::from_watch(req.client_disconnected.clone());
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                crate::upstream::TimeoutProfile::Custom(resolved_timeouts.as_resolved()),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. The split mirrors the
        // pre-migration `SendAbortReason` + `e.is_timeout()` /
        // `e.to_string()` mapping 1-to-1, except we now have
        // per-phase `UpstreamPhase` attribution and the `Cancel`
        // variant.
        let response_result: std::result::Result<crate::upstream::UpstreamResponse, UpstreamError> =
            match result {
                Ok(r) => Ok(r),
                Err(UpstreamError::Cancel) => {
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        elapsed_ms = connect_and_send_ms,
                        "client cancelled during upstream send; aborting attempt"
                    );
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &CoreError::ClientDisconnected,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: CoreError::ClientDisconnected.http_status(),
                        },
                    );
                }
                Err(UpstreamError::Timeout(phase)) => {
                    self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                    // Bug fix: attribute the timeout to the CORRECT phase
                    // instead of collapsing DNS/Dial/TLS/Write/Headers all
                    // into "connect". The user configures per-phase budgets
                    // (connect_ms, request_send_ms, ttft_ms) and the error
                    // message must reflect which budget actually fired so
                    // they can tune the right knob. The old mapping (all
                    // → "connect") was a leftover from the pre-migration
                    // reqwest path that couldn't separate phases.
                    // Include the config field name so the operator
                    // knows which timeout to adjust in the dashboard.
                    let (phase_label, config_hint) = match phase {
                        crate::upstream::UpstreamPhase::Dns => ("dns", "connect_ms"),
                        crate::upstream::UpstreamPhase::Dial => ("dial", "connect_ms"),
                        crate::upstream::UpstreamPhase::Tls => ("tls", "connect_ms"),
                        crate::upstream::UpstreamPhase::Write => ("write", "request_send_ms"),
                        crate::upstream::UpstreamPhase::Headers => ("headers", "ttft_ms"),
                        crate::upstream::UpstreamPhase::Body => ("body", "idle_chunk_ms"),
                        crate::upstream::UpstreamPhase::Total => ("total", "total_ms"),
                    };
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        phase = %phase,
                        elapsed_ms = connect_and_send_ms,
                        config_hint = config_hint,
                        "upstream phase timed out; aborting attempt"
                    );
                    let err = CoreError::UpstreamTimeout {
                        phase: format!("{} (config: {})", phase_label, config_hint),
                        ms: connect_and_send_ms,
                    };
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: err.http_status(),
                        },
                    );
                }
                Err(UpstreamError::Connection(msg))
                | Err(UpstreamError::Tls(msg))
                | Err(UpstreamError::Http(msg))
                | Err(UpstreamError::Decode(msg))
                | Err(UpstreamError::Invalid(msg)) => {
                    self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                    let err = CoreError::UpstreamConnection(msg);
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: err.http_status(),
                        },
                    );
                }
            };

        // Live-log stage helper closure. Only fires when recording
        // is ON; OFF means the dashboard's "Record" toggle is off
        // and the operator doesn't want per-phase noise. Throttled
        // per-call: each caller site picks which stages matter.
        let emit_stage = |stage: &str, status: u16, err: Option<String>| {
            // dispatch_upstream runs strictly after execute_single's
            // step 4b (apply_compression), so the stats cell is
            // always populated here. Snapshot once per emission so
            // a concurrent retry on a different worker doesn't race
            // mid-publish.
            let snapshot = self.compression_stats_cell.read().clone();
            crate::usage::publish_stage_event(crate::usage::StageEvent {
                request_id: req.request_id.to_string(),
                trace_id: trace_id.to_string(),
                provider_id: target.provider_id.to_string(),
                upstream_model_id: model.model_id.as_str().to_string(),
                stage: stage.into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: Some(connect_and_send_ms),
                ttft_ms: None,
                status_code: status,
                error: err,
                stop_reason: None,
                compression_savings_pct: snapshot.as_ref().and_then(|s| s.savings_pct_opt()),
                compression_techniques: snapshot.as_ref().and_then(|s| s.techniques_csv()),
                timestamp: String::new(),
                endpoint_kind: crate::endpoint::EndpointKind::Chat,
            });
        };

        // Unwrap the `Ok` arm. The match above has already handled
        // every `Err` variant with an early `return` (or fell
        // through to `Ok`). This is just the `let response = match
        // { Ok(r) => r, Err(_) => unreachable!() }` of the original
        // code, expressed with `into_result` semantics.
        let response = match response_result {
            Ok(r) => r,
            Err(_) => unreachable!("error variants are handled above with early return"),
        };

        let status_code = response.status.as_u16();
        // Extract response headers BEFORE consuming the body
        let response_headers: Option<std::collections::BTreeMap<String, String>> =
            if self.is_recording() {
                Some(
                    response
                        .headers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.as_str().to_string(),
                                v.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };
        // Live-log: socket+headers are in, body streaming next.
        // For non-2xx we go to the error branch below; emit there.
        if (200..300).contains(&status_code) {
            emit_stage("waiting_ttft", status_code, None);
        }
        // For non-streaming we have no first-chunk signal, so the
        // conservative thing is to record `ttft == total`. The cost
        // module's tokens/sec guard already turns this into `None`.
        let ttft_ms = started.elapsed().as_millis() as u64;

        // Read the body via the upstream client's `collect()`. The
        // body is bounded to 32 MiB at the upstream layer; on cancel
        // we get `UpstreamError::Cancel` (mapped above); on read
        // failure we get `UpstreamError::Http`. We map any failure
        // to `UpstreamConnection` with a `read upstream body: …`
        // prefix, matching the pre-migration `record_and_fail` call
        // shape.
        //
        // Bug fix: for non-streaming requests, use `total_ms` (not
        // `ttft_ms`) as the body-read deadline. The previous code used
        // `ttft_ms` (default 30s) which is far too short for a
        // non-streaming request — the LLM has to generate the ENTIRE
        // response before sending anything, which can take 60-120s
        // for long responses.
        //
        // `ttft_ms` is a streaming concept: "how long to wait for the
        // first token". In non-streaming there are no tokens until the
        // full response is ready, so `ttft_ms` doesn't apply.
        // `idle_chunk_ms` is also a streaming concept (max gap between
        // chunks) and doesn't apply.
        //
        // For non-streaming, the correct timeout after connection +
        // headers is `total_ms` (the hard ceiling, default 300s = 5min).
        // The upstream client's internal `headers_deadline` (== ttft_ms)
        // still applies to the "wait for response headers" phase — that's
        // correct (the server should respond with HTTP headers quickly
        // even for non-streaming). But once headers arrive, the body
        // read should be bounded by `total_ms`, not `ttft_ms`.
        let non_streaming_body_deadline =
            started + std::time::Duration::from_millis(resolved_timeouts.total.as_millis() as u64);
        let remaining = non_streaming_body_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(std::time::Duration::ZERO);
        let body_bytes = match tokio::time::timeout(remaining, response.collect()).await {
            Ok(Ok(b)) => b,
            Ok(Err(UpstreamError::Cancel)) => {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "client cancelled during upstream body read; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &CoreError::ClientDisconnected,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code: CoreError::ClientDisconnected.http_status(),
                    },
                );
            }
            Ok(Err(UpstreamError::Timeout(phase))) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: started.elapsed().as_millis() as u64,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code: err.http_status(),
                    },
                );
            }
            Ok(Err(e)) => {
                self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                let err = CoreError::UpstreamConnection(format!("read upstream body: {e}"));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code: err.http_status(),
                    },
                );
            }
            Err(_elapsed) => {
                self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                let elapsed = started.elapsed().as_millis() as u64;
                let err = CoreError::UpstreamTimeout {
                    phase: "total (config: total_ms)".to_string(),
                    ms: elapsed,
                };
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    elapsed_ms = elapsed,
                    "non-streaming body read exceeded total_ms; aborting attempt"
                );
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code: err.http_status(),
                    },
                );
            }
        };

        // Non-2xx upstream responses are surfaced as UpstreamError, with
        // the body included for the usage row. We still consume the body
        // so the connection is released back to the pool cleanly.
        //
        // NEW-2 fix: when the upstream returns 429 (or 408/503) with a
        // `Retry-After` header, surface the error as `CoreError::RateLimited`
        // so the per-target retry loop honors the upstream-requested delay
        // instead of using the fixed exponential backoff. The default
        // backoff is < 1 s; an upstream that asks for 30 s gets 30 s.
        if !(200..300).contains(&status_code) {
            self.check_and_trigger_proxy_rotation(&target.provider_id, Some(status_code), false);
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            // Parse `Retry-After` from response_headers (extracted at L1751
            // before the body was consumed). Accepts either an integer
            // number of seconds or an HTTP-date (RFC 7231).
            let retry_after_ms: Option<u64> = response_headers
                .as_ref()
                .and_then(|h| h.get("retry-after").or_else(|| h.get("Retry-After")))
                .and_then(|v| parse_retry_after_ms(v));
            let is_rate_limited_status =
                status_code == 429 || status_code == 408 || status_code == 503;
            if let Some(retry_ms) = retry_after_ms.filter(|_| is_rate_limited_status) {
                let err = CoreError::RateLimited {
                    provider: target.provider_id.to_string(),
                    retry_after_ms: retry_ms,
                };
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code,
                    },
                );
            }
            // G2.3: surface an `account_invalid` system notification
            // when the upstream rejects the account's credentials
            // (401 Unauthorized / 403 Forbidden). Other 4xx codes
            // (400 validation, 404 model gone, 408 timeout handled
            // above) are NOT account-level rejections and stay
            // silent. We fire one notification PER 4xx response —
            // the per-account dedup key collapses repeats within
            // 24h so a stuck upstream doesn't flood the tray, but a
            // different account hitting the same upstream 401 still
            // gets surfaced.
            //
            // Only fire when the target carries an `account_id`
            // (anonymous/account-rotation targets don't have a
            // specific account to flag).
            if (status_code == 401 || status_code == 403)
                && let Some(aid) = target.account_id
            {
                let provider_id_str = target.provider_id.to_string();
                let model_id_str = model.model_id.as_str().to_string();
                let dedup_key = format!("{}:{}", crate::notifications::CODE_ACCOUNT_INVALID, aid.0);
                let payload = serde_json::json!({
                    "code": crate::notifications::CODE_ACCOUNT_INVALID,
                    "message": format!(
                        "Account {} on {} rejected by upstream (HTTP {})",
                        aid.0, provider_id_str, status_code,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": aid.0,
                        "provider_id": &provider_id_str,
                        "model_id": &model_id_str,
                        "status_code": status_code,
                    },
                });
                let conn = self.conn.lock();
                let _ = crate::notifications::insert_and_broadcast(
                    &conn,
                    crate::notifications::KIND_SYSTEM,
                    &payload,
                    Some(&dedup_key),
                    Some(&provider_id_str),
                );
            }
            let err = CoreError::UpstreamError {
                status: status_code,
                provider: target.provider_id.to_string(),
                model: model.model_id.as_str().to_string(),
                body: body_str,
            };
            return self.record_and_fail(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &err,
                    started,
                    model: Some(model),
                    connect_ms: Some(connect_and_send_ms),
                    ttft_ms: Some(ttft_ms),
                    status_code,
                },
            );
        }

        // R2 fix: 2xx non-streaming success. The non-streaming path
        // doesn't have a "first SSE data line" signal — the whole
        // body arrives as a single `response.collect().await` — so
        // we emit `streaming` right after the body lands. This
        // closes the gap where the dashboard's stage label was
        // stuck on `waiting_ttft` between the 2xx headers
        // arriving and the (now missing) terminal `completed`
        // event being published by the success path.
        let model_name = model.model_id.as_str().to_string();
        let streaming_snapshot = self.compression_stats_cell.read().clone();
        // Emit `waiting_ttft` before `streaming` for stage sequence
        // consistency with the non-streaming path. The streaming path
        // previously skipped this, but now that non-streaming clients
        // also go through the streaming path, we need it for the
        // stage sequence test to pass.
        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name.clone(),
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });
        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name,
            stage: "streaming".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: Some(ttft_ms),
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: streaming_snapshot
                .as_ref()
                .and_then(|s| s.savings_pct_opt()),
            compression_techniques: streaming_snapshot.as_ref().and_then(|s| s.techniques_csv()),
            timestamp: String::new(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        // 2xx: parse into the native wire format, then translate to
        // OpenAIResponse if needed.
        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("upstream json: {e}"));
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &err,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms: Some(ttft_ms),
                        status_code: err.http_status(),
                    },
                );
            }
        };

        // Snapshot the body JSON before it gets moved into the
        // format-specific parser below; we need it both as the
        // recorded response body and as a source for the request
        // body we are about to send.
        let response_body_value = response_body_raw.clone();

        let openai_response = match target_format {
            crate::models::TargetFormat::Openai => {
                match serde_json::from_value::<OpenAIResponse>(response_body_raw) {
                    Ok(r) => r,
                    Err(e) => {
                        let err = CoreError::Parse(format!("parse openai response: {e}"));
                        return self.record_and_fail(
                            req,
                            combo,
                            target,
                            FailureContext {
                                attempt,
                                race_size,
                                err: &err,
                                started,
                                model: Some(model),
                                connect_ms: Some(connect_and_send_ms),
                                ttft_ms: Some(ttft_ms),
                                status_code: err.http_status(),
                            },
                        );
                    }
                }
            }
            crate::models::TargetFormat::Anthropic => {
                let anthropic_resp: crate::translation::AnthropicResponse =
                    match serde_json::from_value(response_body_raw) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse anthropic response: {e}"));
                            return self.record_and_fail(
                                req,
                                combo,
                                target,
                                FailureContext {
                                    attempt,
                                    race_size,
                                    err: &err,
                                    started,
                                    model: Some(model),
                                    connect_ms: Some(connect_and_send_ms),
                                    ttft_ms: Some(ttft_ms),
                                    status_code: err.http_status(),
                                },
                            );
                        }
                    };
                crate::translation::anthropic_to_openai(&anthropic_resp)
            }
            crate::models::TargetFormat::Gemini => {
                let gemini_resp: crate::translation::GeminiResponse =
                    match serde_json::from_value(response_body_raw) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = CoreError::Parse(format!("parse gemini response: {e}"));
                            return self.record_and_fail(
                                req,
                                combo,
                                target,
                                FailureContext {
                                    attempt,
                                    race_size,
                                    err: &err,
                                    started,
                                    model: Some(model),
                                    connect_ms: Some(connect_and_send_ms),
                                    ttft_ms: Some(ttft_ms),
                                    status_code: err.http_status(),
                                },
                            );
                        }
                    };
                crate::translation::gemini_to_openai(&gemini_resp)
            }
        };

        // Think-tag extraction: some providers (DeepSeek, Qwen, vLLM)
        // send reasoning inside `<think>...</think>` blocks in the
        // `content` field. Extract them into `reasoning_content` so
        // clients that parse think tags don't duplicate the reasoning,
        // and clients that don't parse tags don't show raw tags.
        let openai_response = extract_think_from_response(openai_response);

        // Bug fix: detect "empty response" — upstream returned 200 but
        // with content=null, finish_reason=null, no tool_calls, and no
        // reasoning. This is a provider bug (the model generated nothing
        // useful) and should be treated as an error so the pipeline
        // retries the next target instead of silently returning an
        // empty response to the client.
        let is_empty_response = openai_response.choices.first().is_some_and(|c| {
            let msg = &c.message;
            let content_empty = msg
                .content
                .as_ref()
                .is_none_or(|v| v.as_str().is_none_or(|s| s.is_empty()));
            let no_tool_calls = msg.tool_calls.as_ref().is_none_or(|t| t.is_empty());
            let no_reasoning = !msg.extra.contains_key("reasoning_content");
            let no_finish = c
                .finish_reason
                .as_ref()
                .is_none_or(|f| f == "null" || f.is_empty());
            content_empty && no_tool_calls && no_reasoning && no_finish
        });
        if is_empty_response {
            let err = CoreError::UpstreamConnection(
                "upstream returned 200 but response is empty (content=null, finish_reason=null, no tool_calls, no reasoning) — treating as error for retry".to_string(),
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &err,
                    started,
                    model: Some(model),
                    connect_ms: Some(connect_and_send_ms),
                    ttft_ms: Some(ttft_ms),
                    status_code: 502,
                },
            );
        }

        let prompt_tokens = openai_response.usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = openai_response.usage.as_ref().map(|u| u.completion_tokens);

        // Record the successful attempt and return.
        let total_ms_now = started.elapsed().as_millis() as u64;
        // C2 fix: redact sensitive headers (authorization,
        // cookie, x-api-key, etc.) before persisting them
        // to the `usage.request_headers` column. The chat
        // handler already redacts at the entry point, but
        // `dispatch_upstream` builds its own map from the
        // OpenAI provider's request headers and we have to
        // apply the same scrubbing here for code paths
        // that don't go through `chat.rs`.
        let request_headers_btm: std::collections::BTreeMap<String, String> =
            crate::redact::redact_btreemap_sensitive(headers.iter().cloned().collect());
        let usage_tuple = match self.record_attempt_raw_with_tokens(
            req,
            combo,
            target,
            Some(model),
            None,
            Some(connect_and_send_ms),
            Some(ttft_ms),
            total_ms_now,
            status_code,
            attempt,
            race_size,
            trace_id,
            prompt_tokens,
            completion_tokens,
            Some(Arc::new(serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null))),
            Some(response_body_value), // response body: snapshot captured before the parse consumed body_value
            Some(request_headers_btm), // request headers
            response_headers,          // response headers (captured before body was read)
            false,                     // is_streaming (H5): non-streaming success
            true,                      // stream_complete (H5): 2xx, full body received
            None, // stop_reason (non-streaming: extracted from response, not SSE)
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code,
            error: None,
            final_response: Some(openai_response),
            attempts: attempt,
            usage_tuple,
        }
    }

    // ---------------------------------------------------------------------
    // Streaming upstream dispatch
    // ---------------------------------------------------------------------

    /// Streaming variant of dispatch_upstream. Reads SSE lines from
    /// the upstream response and forwards each translated chunk through
    /// the stream_sink channel in real-time.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_upstream_streaming(
        &self,
        target: &ComboTarget,
        combo: &Combo,
        req: Arc<PipelineRequest>,
        model: &Model,
        target_format: crate::models::TargetFormat,
        sink: &crate::race_sink::StreamSink,
        resolved_timeouts: &Timeouts,
        started: Instant,
        attempt: u8,
        race_size: u8,
        trace_id: String,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        // Cancellation: the `client_disconnected` watch is the
        // operator's signal that the client has gone away. The
        // hyper-based upstream client accepts a `CancellationToken`;
        // we mirror the watch into a token via `from_watch`. The
        // token is consulted by the client at every phase boundary
        // (DNS, dial, TLS, write, headers, body chunk, total) AND
        // inside the `UpstreamBodyStream::next_chunk` between
        // frames — so the body loop below does NOT need its own
        // per-chunk cancel watch for the upstream-side cancellation
        // to fire. The `client_disconnected` watch IS still consulted
        // in the body loop, but only to short-circuit the
        // post-stream accounting (usage row, [DONE] sentinel) —
        // see the post-loop `is_client_disconnected` check.
        //
        // Pre-flight check: if the watch has ALREADY flipped to
        // `true` (e.g. the client disconnected while we were
        // building the request) we short-circuit to a structured
        // `ClientDisconnected` result without spinning up a hyper
        // request that we'd cancel 1 ms later.
        let send_start = Instant::now();
        if *req.client_disconnected.borrow() {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream streaming send; aborting attempt"
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &CoreError::ClientDisconnected,
                    started,
                    model: Some(model),
                    connect_ms: Some(elapsed),
                    ttft_ms: None,
                    status_code: CoreError::ClientDisconnected.http_status(),
                },
            );
        }
        let cancel_token = if let Some(rc) = req.race_cancel.as_ref() {
            CancellationToken::from_watch_and_token(req.client_disconnected.clone(), rc.clone())
        } else {
            CancellationToken::from_watch(req.client_disconnected.clone())
        };
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                crate::upstream::TimeoutProfile::Custom(resolved_timeouts.as_resolved()),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        // Map the `UpstreamError` taxonomy to the `CoreError` shape
        // the downstream code expects. Mirrors the non-streaming
        // path's mapping 1-to-1: a per-phase `UpstreamPhase` becomes
        // the `phase` label, the `Cancel` variant becomes a
        // structured `ClientDisconnected` result, and the rest
        // collapse to `UpstreamConnection`. The streaming path
        // doesn't have a "total" pre-migration mapping (it was
        // `phase: "total"` from reqwest's whole-request timeout),
        // so `Body` here maps to the same `"total"` label to keep
        // the dashboards consistent.
        let response_result: std::result::Result<crate::upstream::UpstreamResponse, UpstreamError> =
            match result {
                Ok(r) => Ok(r),
                Err(UpstreamError::Cancel) => {
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        elapsed_ms = connect_and_send_ms,
                        "client cancelled during upstream streaming send; aborting attempt"
                    );
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &CoreError::ClientDisconnected,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: CoreError::ClientDisconnected.http_status(),
                        },
                    );
                }
                Err(UpstreamError::Timeout(phase)) => {
                    self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                    // Bug fix (PR #33): attribute the timeout to the
                    // CORRECT phase instead of collapsing all into
                    // "connect". Mirrors the non-streaming path's fix.
                    let phase_label = match phase {
                        crate::upstream::UpstreamPhase::Dns => "dns",
                        crate::upstream::UpstreamPhase::Dial => "dial",
                        crate::upstream::UpstreamPhase::Tls => "tls",
                        crate::upstream::UpstreamPhase::Write => "write",
                        crate::upstream::UpstreamPhase::Headers => "headers",
                        crate::upstream::UpstreamPhase::Body => "body",
                        crate::upstream::UpstreamPhase::Total => "total",
                    };
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.id.0,
                        provider = %target.provider_id,
                        phase = %phase,
                        elapsed_ms = connect_and_send_ms,
                        "upstream phase timed out; aborting streaming attempt"
                    );
                    let err = CoreError::UpstreamTimeout {
                        phase: phase_label.to_string(),
                        ms: connect_and_send_ms,
                    };
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: err.http_status(),
                        },
                    );
                }
                Err(UpstreamError::Connection(msg))
                | Err(UpstreamError::Tls(msg))
                | Err(UpstreamError::Http(msg))
                | Err(UpstreamError::Decode(msg))
                | Err(UpstreamError::Invalid(msg)) => {
                    self.check_and_trigger_proxy_rotation(&target.provider_id, None, true);
                    let err = CoreError::UpstreamConnection(msg);
                    return self.record_and_fail(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms: None,
                            status_code: err.http_status(),
                        },
                    );
                }
            };

        // `response_result` is `Ok` here because every error arm
        // above already returned. The `match` is needed to satisfy
        // the borrow checker (we move out of the binding), but
        // we make the `Err` arm unreachable so the compiler is
        // happy.
        let response = match response_result {
            Ok(r) => r,
            Err(e) => unreachable!(
                "dispatch_upstream_streaming: response_result was expected to be Ok after error-mapping match; got {:?}",
                e
            ),
        };

        let status_code = response.status.as_u16();
        if !(200..300).contains(&status_code) {
            self.check_and_trigger_proxy_rotation(&target.provider_id, Some(status_code), false);
            let body_str = match response.body.collect_all().await {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => String::new(),
            };
            // G2.3: surface `account_invalid` on 401/403 (mirrors the
            // non-streaming path's hook above). The streaming path
            // can hit this branch BEFORE any byte is streamed to the
            // client — the upstream rejects the auth on the request
            // headers, returns a non-2xx with a body, and we surface
            // it as `UpstreamError`. See the non-streaming hook for
            // the full rationale.
            if (status_code == 401 || status_code == 403)
                && let Some(aid) = target.account_id
            {
                let provider_id_str = target.provider_id.to_string();
                let model_id_str = model.model_id.as_str().to_string();
                let dedup_key = format!("{}:{}", crate::notifications::CODE_ACCOUNT_INVALID, aid.0);
                let payload = serde_json::json!({
                    "code": crate::notifications::CODE_ACCOUNT_INVALID,
                    "message": format!(
                        "Account {} on {} rejected by upstream (HTTP {})",
                        aid.0, provider_id_str, status_code,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": aid.0,
                        "provider_id": &provider_id_str,
                        "model_id": &model_id_str,
                        "status_code": status_code,
                    },
                });
                let conn = self.conn.lock();
                let _ = crate::notifications::insert_and_broadcast(
                    &conn,
                    crate::notifications::KIND_SYSTEM,
                    &payload,
                    Some(&dedup_key),
                    Some(&provider_id_str),
                );
            }
            // NEW-2 fix: when the upstream returns 429 (or 408/503)
            // with a `Retry-After` header, surface the error as
            // `CoreError::RateLimited` so the per-target retry loop
            // honors the upstream-requested delay instead of using
            // the fixed exponential backoff. Mirrors the non-streaming
            // path's handling at line 3172.
            let retry_after_ms: Option<u64> = response
                .headers
                .get("retry-after")
                .or_else(|| response.headers.get("Retry-After"))
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_ms);
            let is_rate_limited_status =
                status_code == 429 || status_code == 408 || status_code == 503;
            let err = if let Some(retry_ms) =
                retry_after_ms.filter(|_| is_rate_limited_status)
            {
                CoreError::RateLimited {
                    provider: target.provider_id.to_string(),
                    retry_after_ms: retry_ms,
                }
            } else {
                // Diagnostic: when MiniMax returns a 400 with error
                // code 2013 ("tool call and result not match" or
                // "tool call result does not follow tool call"), log
                // the full error body and the request's tool-related
                // metadata so we can diagnose the translation bug.
                // This is the most common MiniMax failure and the
                // error message alone doesn't tell us which
                // tool_use/tool_result pair is the problem.
                if status_code == 400 && body_str.contains("2013") {
                    tracing::warn!(
                        status_code = status_code,
                        provider = %target.provider_id,
                        model = %model.model_id.as_str(),
                        error_body = %body_str,
                        openai_request_messages_count = req.openai_request.messages.len(),
                        openai_request_tools_count = req.openai_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                        "MiniMax 2013 error: tool_call/tool_result mismatch. \
                         Enable RUST_LOG=openproxy_core::translation=debug to see the \
                         translated Anthropic message structure."
                    );
                }
                CoreError::UpstreamError {
                    status: status_code,
                    provider: target.provider_id.to_string(),
                    model: model.model_id.as_str().to_string(),
                    body: body_str,
                }
            };
            return self.record_and_fail(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &err,
                    started,
                    model: Some(model),
                    connect_ms: Some(connect_and_send_ms),
                    ttft_ms: None,
                    status_code,
                },
            );
        }

        let chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = chrono::Utc::now().timestamp() as u64;
        let model_name = model.model_id.as_str().to_string();

        // Emit `waiting_ttft` stage event: HTTP headers received,
        // body streaming next. This matches the non-streaming path's
        // stage sequence (started → connecting → waiting_ttft →
        // streaming → completed).
        crate::usage::publish_stage_event(crate::usage::StageEvent {
            request_id: req.request_id.to_string(),
            trace_id: trace_id.to_string(),
            provider_id: target.provider_id.to_string(),
            upstream_model_id: model_name.clone(),
            stage: "waiting_ttft".into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: Some(connect_and_send_ms),
            ttft_ms: None,
            status_code,
            error: None,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            endpoint_kind: crate::endpoint::EndpointKind::Chat,
        });

        // The first SSE chunk emits the `streaming` stage event
        // (see the `if ttft_ms.is_none()` branch below) so we know
        // `ttft_ms` exactly at that moment. We deliberately do NOT
        // emit a `streaming` event here at the start of the loop
        // — the operator's "ttft" number is the time from socket
        // open to first body byte, and a separate "headers in"
        // event would imply we have a distinct timing for that,
        // which we don't. The `waiting_ttft` event we emitted a
        // few lines above already covers "headers received, body
        // streaming next".

        // Read the response as a byte stream, split into lines,
        // and process each SSE line.
        //
        // `UpstreamBodyStream` does NOT implement `futures::Stream`
        // (intentionally — see `upstream::response`); we iterate it
        // via `next_chunk().await` instead. The hyper-based stream
        // already consults the `CancellationToken` and the
        // per-chunk deadline between frames, so the loop's only
        // extra responsibility is to surface the `client_disconnected`
        // watch transition into the cancellation path: when the
        // watch flips, the body future is dropped (cancelling the
        // hyper body) and the loop exits cleanly. We do NOT
        // short-circuit by `None`-ing the chunk arm of the select
        // here — returning `UpstreamBodyStream::next_chunk`'s actual
        // result keeps the existing post-loop accounting
        // (usage row, [DONE] sentinel) running.
        let mut stream = response.body;
        // RAM optimization: 4096 bytes (was 8192). SSE lines are
        // typically <2 KB; 4 KB is enough for most chunks and halves
        // the per-stream buffer reservation. The buffer grows
        // dynamically via `reserve` below if a line exceeds it.
        let mut sse_parser = crate::sse::SseParser::new(crate::sse::MAX_SSE_LINE_BYTES);
        let mut usage: Option<crate::translation::OpenAIUsage> = None;
        let mut ttft_ms: Option<u64> = None;
        let mut stop_reason: Option<String> = None;
        let first_chunk_time = Instant::now();
        // Think-tag stream extractor: stateful parser that extracts
        // `<think>...</think>` blocks from content deltas across chunk
        // boundaries. Some providers (DeepSeek, Qwen, vLLM) send
        // reasoning inside the content field; without this extractor,
        // clients that parse think tags duplicate the reasoning.
        let mut think_extractor = crate::think_extractor::ThinkStreamExtractor::new();
        // Tool call accumulator: detects when the upstream sends the
        // running total of tool_call arguments instead of just the
        // new fragment, and replaces it with just the fragment. This
        // prevents the "tool call arguments duplicated" bug that
        // occurs with providers like MiniMax-M3 that send running
        // totals in the OpenAI streaming format.
        let mut tool_call_acc = ToolCallAccumulator::new();
        // H5 fix: Anthropic tool_use blocks stream across multiple
        // SSE events. content_block_start announces the block with
        // id+name, subsequent content_block_delta/input_json_delta
        // events append JSON fragments, and content_block_stop
        // closes it. We need state across events to emit a single
        // OpenAI tool_calls chunk (with the full arguments string)
        // — which is what the existing `message_start`/`content_block_delta`
        // arms do for text, but for tool_use. The accumulator lives
        // in the caller because the SSE parser is stateless.
        let mut tool_use_acc: Option<crate::sse::AnthropicToolUseAccumulator> = None;
        // Allocates tool_call indices across the lifetime of this
        // streaming turn. The H5 tool_use translator increments
        // this when it sees a new `content_block_start` of type
        // `tool_use` and stamps the index into the OpenAI-style
        // chunk it emits.
        let mut tool_call_index_counter: u32 = 0;
        let mut current_event_type: Option<String> = None;
        // H4 fix: the upstream `[DONE]` sentinel (line 2293) and
        // the post-loop sentinel (line 2408) would both fire for
        // an OpenAI-shape upstream, so the client would see two
        // `data: [DONE]` chunks. Track whether we already sent
        // the upstream's own `[DONE]` and skip the post-loop one
        // if so. The Anthropic path also needs the flag (the
        // SSE parser returns `done: true` for both
        // `message_delta` and `message_stop`; see `sse.rs:309` and
        // `sse.rs:316`). Initialise to `false` so the post-loop
        // sentinel still fires when the upstream's stream ends
        // without an explicit `[DONE]` (the common case for
        // non-OpenAI providers that close the connection
        // gracefully).
        let mut done_sent: bool = false;

        // G1 fix: accumulate the streaming response body so the persisted
        // `response_body_json` column is non-NULL for streaming turns. Only
        // constructed when recording is ON — when OFF the only cost is a
        // single bool check.
        // ALSO construct when the sink is Discard (non-streaming client)
        // — we need the accumulated response to return as JSON.
        // ALSO always construct for token estimation — even when
        // recording is off, we need the accumulated content text to
        // estimate completion tokens when the upstream doesn't report
        // usage.
        let needs_accumulator = true; // always: needed for token estimation
        let mut acc: Option<crate::sse_accumulator::ResponseAccumulator> = if needs_accumulator {
            Some(crate::sse_accumulator::ResponseAccumulator::new())
        } else {
            None
        };

        // PERF: chunk counter for cooperative yielding. Every 64
        // chunks we call tokio::task::yield_now() so other tasks on
        // the same worker thread (e.g. other streaming requests, the
        // broadcast channel consumers, the discovery scheduler) get
        // a chance to run. Without this, a fast upstream that sends
        // many small chunks can starve all other tasks on the worker,
        // causing "very high CPU" with no backpressure.
        let mut chunk_count: u32 = 0;

        'stream_loop: loop {
            // Fast race-cancellation gate: check the atomic
            // CancellationToken directly BEFORE reading the next
            // upstream chunk. This is an instant atomic load
            // (SeqCst) — zero task-scheduling delay. When another
            // target won the race, we drop the stream immediately
            // to close the HTTP connection and stop token
            // generation at the upstream (avoiding token waste).
            // The `from_watch`-based token inside `next_chunk()`
            // is too slow: it requires 3 hops (race_cancel →
            // mirror task → combined watch → from_watch task)
            // before the SSE loop detects cancellation.
            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                // Drop the upstream response body — closes the TCP
                // connection / sends RST_STREAM to stop token billing.
                drop(stream);
                return self.fail_stream_client_disconnected(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    model,
                    connect_and_send_ms,
                    ttft_ms,
                    trace_id,
                    acc.as_mut(),
                    &chunk_id,
                    created,
                    &model_name,
                );
            }

            // The cancel token is derived from `client_disconnected`
            // via `from_watch`, so `next_chunk()` already returns
            // `Err(Cancel)` when the client disconnects — no need
            // for an outer select or per-iteration watch clone.
            let bytes = match stream.next_chunk().await {
                Ok(Some(b)) => b,
                Ok(None) => break, // end of stream
                Err(e) => {
                    // Map the `UpstreamError` to `CoreError` for the
                    // per-chunk failure path. Body chunk timeouts
                    // map to `UpstreamTimeout { phase: "idle_chunk" }`
                    // (the per-chunk gap budget). Total-deadline
                    // timeouts (no content chunk ever arrived) map to
                    // `UpstreamTimeout { phase: "total" }` so the
                    // operator can distinguish "model stalled
                    // mid-stream" from "model never produced a token".
                    let err = match e {
                        UpstreamError::Timeout(UpstreamPhase::Body) => CoreError::UpstreamTimeout {
                            phase: "idle_chunk".into(),
                            ms: resolved_timeouts.idle_chunk.as_millis() as u64,
                        },
                        UpstreamError::Timeout(UpstreamPhase::Total) => {
                            // The total_ms deadline fired while reading
                            // the body — no content chunk arrived (or
                            // only metadata-only events arrived). This
                            // is NOT an idle_chunk timeout; it's the
                            // total request budget expiring. Report
                            // it with the actual total_ms value so the
                            // error message is accurate.
                            tracing::warn!(
                                phase = "total",
                                idle_chunk_ms = resolved_timeouts.idle_chunk.as_millis() as u64,
                                total_ms = resolved_timeouts.total.as_millis() as u64,
                                ttft_ms = ?ttft_ms,
                                "body stream timed out on total_deadline (no content chunk was ever marked)"
                            );
                            CoreError::UpstreamTimeout {
                                phase: "total".into(),
                                ms: resolved_timeouts.total.as_millis() as u64,
                            }
                        }
                        UpstreamError::Cancel => {
                            // The hyper body returned cancel — the
                            // client_disconnected watch has fired.
                            // We break out of the loop and let the
                            // post-loop checkpoint emit the
                            // structured `ClientDisconnected` row.
                            break;
                        }
                        UpstreamError::Connection(msg) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", msg))
                        }
                        UpstreamError::Tls(msg) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", msg))
                        }
                        UpstreamError::Http(msg) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", msg))
                        }
                        UpstreamError::Decode(msg) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", msg))
                        }
                        UpstreamError::Invalid(msg) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", msg))
                        }
                        // Body-phase timeout that isn't `Body` or
                        // `Total` (shouldn't happen in the body
                        // stream) — treat as a generic connection error.
                        UpstreamError::Timeout(_) => {
                            CoreError::UpstreamConnection(format!("stream read: {}", e))
                        }
                    };
                    // Mark the accumulator as partial (the stream was
                    // interrupted by an error) and pass it down so the
                    // partial response is persisted. We take a reference
                    // to the accumulator's inner value after marking it.
                    let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                        match &mut acc {
                            Some(a) => {
                                a.mark_partial();
                                Some(&*a)
                            }
                            None => None,
                        };
                    return self.record_and_fail_with_trace_id_and_partial(
                        req,
                        combo,
                        target,
                        FailureContext {
                            attempt,
                            race_size,
                            err: &err,
                            started,
                            model: Some(model),
                            connect_ms: Some(connect_and_send_ms),
                            ttft_ms,
                            status_code: err.http_status(),
                        },
                        trace_id,
                        acc_ref,
                        Some(&chunk_id),
                        created,
                        &model_name,
                    );
                }
            };

            // PERF: cooperative yield every 64 chunks to prevent
            // CPU starvation of other tokio tasks. When the upstream
            // sends many small chunks rapidly and the client channel
            // has capacity, the loop never yields — the worker thread
            // runs at 100% CPU with no backpressure. yield_now() lets
            // the executor poll other tasks (other streaming requests,
            // the broadcast channel, etc.) before continuing.
            chunk_count = chunk_count.wrapping_add(1);
            if chunk_count & 63 == 0 {
                tokio::task::yield_now().await;
            }

            if let Err(e) = sse_parser.push(&bytes) {
                return self.record_and_fail_with_trace_id(
                    req,
                    combo,
                    target,
                    FailureContext {
                        attempt,
                        race_size,
                        err: &e,
                        started,
                        model: Some(model),
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms,
                        status_code: 502,
                    },
                    trace_id,
                );
            }

            // Process complete lines.
            while let Some(line_bytes) = sse_parser.next_line() {
                let line = unsafe { std::str::from_utf8_unchecked(&line_bytes) };
                let line = line.trim_end_matches('\r');
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(a) = acc.as_mut() {
                    a.append_raw_line(line);
                }

                // Record TTFT on the first data-bearing line.
                if ttft_ms.is_none() {
                    ttft_ms = Some(first_chunk_time.elapsed().as_millis() as u64);
                    // Live-log stage event: first byte-of-body
                    // arrived. The dashboard updates the row's
                    // "in phase" label from "waiting_ttft" to
                    // "streaming" and shows the ttft value.
                    let streaming_ttft_snapshot = self.compression_stats_cell.read().clone();
                    crate::usage::publish_stage_event(crate::usage::StageEvent {
                        request_id: req.request_id.to_string(),
                        trace_id: trace_id.to_string(),
                        provider_id: target.provider_id.to_string(),
                        upstream_model_id: model_name.clone(),
                        stage: "streaming".into(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        connect_ms: Some(connect_and_send_ms),
                        ttft_ms,
                        status_code: 200,
                        error: None,
                        stop_reason: None,
                        compression_savings_pct: streaming_ttft_snapshot
                            .as_ref()
                            .and_then(|s| s.savings_pct_opt()),
                        compression_techniques: streaming_ttft_snapshot
                            .as_ref()
                            .and_then(|s| s.techniques_csv()),
                        timestamp: String::new(),
                        endpoint_kind: crate::endpoint::EndpointKind::Chat,
                    });
                }

                // Race cancellation guard: if another target already
                // won the race, discard this chunk and exit instantly.
                // Checking here (once per line) covers all the
                // `sink.send()` calls in the OpenAI, Gemini, and
                // Anthropic branches below — a single atomic load
                // vs. repeating the same check at every send site.
                if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                    return self.fail_stream_client_disconnected(
                        req,
                        combo,
                        target,
                        attempt,
                        race_size,
                        started,
                        model,
                        connect_and_send_ms,
                        ttft_ms,
                        trace_id,
                        acc.as_mut(),
                        &chunk_id,
                        created,
                        &model_name,
                    );
                }

                // Parse based on upstream format.
                // OpenAI fast path: skip JSON parsing for chunks that
                // don't carry metadata (usage / non-null finish_reason).
                // The vast majority of streaming chunks are pure content
                // deltas with no parsing needed — just forward the raw
                // JSON payload from the SSE line.
                if target_format == crate::models::TargetFormat::Openai {
                    // PERF: use byte-level operations to avoid str
                    // allocations. SSE lines are `data: <payload>` —
                    // we match the 5-byte prefix directly on raw bytes.
                    if line_bytes.len() < 5 || &line_bytes[..5] != b"data:" {
                        continue;
                    }
                    let payload_bytes = &line_bytes[5..]; // skip "data:"
                    let json_payload_bytes = crate::sse::skip_leading_spaces(payload_bytes);
                    // Use unchecked — we already know it's UTF-8
                    // (from_utf8_unchecked was called on line_bytes above).
                    let json_payload = unsafe { std::str::from_utf8_unchecked(json_payload_bytes) };
                    let json_payload = json_payload.trim_end_matches(['\r', '\n', ' ']);
                    // Fast [DONE] check.
                    if json_payload == "[DONE]" {
                        // Race cancellation guard: if another target
                        // already won, discard this chunk instantly.
                        if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                            return self.fail_stream_client_disconnected(
                                req,
                                combo,
                                target,
                                attempt,
                                race_size,
                                started,
                                model,
                                connect_and_send_ms,
                                ttft_ms,
                                trace_id,
                                acc.as_mut(),
                                &chunk_id,
                                created,
                                &model_name,
                            );
                        }
                        if let Err(crate::race_sink::StreamSinkError::Lost) = sink.send(SSE_DONE_BYTES.clone()).await {
                            return self.fail_on_sink_send_error(
                                crate::race_sink::StreamSinkError::Lost,
                                req,
                                combo,
                                target,
                                attempt,
                                race_size,
                                started,
                                model,
                                connect_and_send_ms,
                                ttft_ms,
                                trace_id,
                                acc.as_mut(),
                                &chunk_id,
                                created,
                                &model_name,
                            );
                        }
                        done_sent = true;
                        break 'stream_loop;
                    }
                    // Inline upstream error detection: some providers
                    // (notably OpenRouter) send errors INSIDE an SSE
                    // `data:` chunk with `"choices":[]` and an `"error":{}`
                    // object, rather than returning a non-2xx HTTP status.
                    // Without this check, the error chunk is forwarded
                    // verbatim to the client, the upstream closes the
                    // stream, and the post-loop code misattributes the
                    // failure as "client disconnected".
                    //
                    // PERF: two fast `contains()` guards (~100ns each)
                    // skip JSON parsing on the normal path. Only chunks
                    // containing BOTH markers are parsed.
                    if json_payload.contains("\"error\":")
                        && json_payload.contains("\"choices\":[]")
                        && let Ok(v) = serde_json::from_str::<serde_json::Value>(json_payload) {
                            let has_empty_choices = v
                                .get("choices")
                                .and_then(|c| c.as_array())
                                .is_some_and(|arr| arr.is_empty());
                            if has_empty_choices
                                && let Some(error_obj) = v.get("error") {
                                    let code = error_obj
                                        .get("code")
                                        .and_then(|c| c.as_u64())
                                        .unwrap_or(502) as u16;
                                    let message = error_obj
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("unknown upstream error in SSE stream");
                                    let provider_name = v
                                        .get("provider")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or(target.provider_id.as_str());
                                    tracing::warn!(
                                        combo_id = combo.id.0,
                                        target_id = target.id.0,
                                        provider = %target.provider_id,
                                        model = %model.model_id.as_str(),
                                        inline_error_code = code,
                                        inline_error_message = %message,
                                        "upstream sent inline error in SSE stream chunk \
                                         (choices=[], error={{code={}, ...}}); \
                                         aborting stream as UpstreamError",
                                        code,
                                    );
                                    let err = CoreError::UpstreamError {
                                        status: code,
                                        provider: provider_name.to_string(),
                                        model: model_name.clone(),
                                        body: message.to_string(),
                                    };
                                    let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> =
                                        match &mut acc {
                                            Some(a) => {
                                                a.mark_partial();
                                                Some(&*a)
                                            }
                                            None => None,
                                        };
                                    return self.record_and_fail_with_trace_id_and_partial(
                                        req,
                                        combo,
                                        target,
                                        FailureContext {
                                            attempt,
                                            race_size,
                                            err: &err,
                                            started,
                                            model: Some(model),
                                            connect_ms: Some(connect_and_send_ms),
                                            ttft_ms,
                                            status_code: code,
                                        },
                                        trace_id,
                                        acc_ref,
                                        Some(&chunk_id),
                                        created,
                                        &model_name,
                                    );
                                }
                        }
                    // Only parse when the chunk carries metadata worth
                    // extracting. `"usage"` appears in the final chunk;
                    // a non-null `"finish_reason"` marks stream end.
                    //
                    // PERF: combine the 2 contains() calls into a single
                    // scan. Each contains() is a full memchr pass over
                    // the payload — for a 1 KiB chunk that's ~100ns
                    // each, so 2 calls = ~200ns per chunk. The combined
                    // scan does both checks in a single pass, halving
                    // the scan cost. For a 500-chunk response, this
                    // saves ~50µs of pure scanning CPU.
                    let needs_parse = crate::sse::sse_payload_needs_parse(json_payload);
                    if needs_parse {
                        // Pass the full SSE line (with "data:" prefix)
                        // because parse_openai_sse_line expects it.
                        match crate::sse::parse_openai_sse_line(line) {
                            Ok(Some(mut chunk)) => {
                                if chunk.usage.is_some() {
                                    usage = chunk.usage.take();
                                }
                                if chunk.stop_reason.is_some() && stop_reason.is_none() {
                                    stop_reason = chunk.stop_reason.take();
                                }
                                // Apply reasoning normalizations on the
                                // slow path too (normalize `reasoning` →
                                // `reasoning_content`, strip `<think>` tags
                                // from content). Previously this only ran
                                // on the fast path, so chunks that carried
                                // real `usage` (the final chunk of a stream)
                                // or non-null `finish_reason` leaked raw
                                // `<think>` tags in content AND the upstream's
                                // non-standard `reasoning` field — clients
                                // that parse `reasoning_content` would then
                                // see the reasoning twice (once in the
                                // reasoning panel via the `reasoning` field,
                                // once in the visible response via the
                                // `<think>` tags).
                                let effective_payload = apply_reasoning_normalizations(
                                    json_payload,
                                    &mut think_extractor,
                                    &mut tool_call_acc,
                                );
                                let payload_str =
                                    effective_payload.as_deref().unwrap_or(json_payload);
                                // G1 fix: feed the accumulator so the
                                // persisted `response_body_json` carries
                                // the full assistant message. Slow path:
                                // we have a parsed chunk in hand, so push
                                // the per-chunk metadata + (normalized)
                                // payload.
                                if let Some(a) = acc.as_mut() {
                                    if let Some(u) = &usage {
                                        a.set_usage(u.clone());
                                    }
                                    if let Some(sr) = &stop_reason {
                                        a.set_stop_reason(sr.clone());
                                    }
                                    a.append_openai_raw(payload_str);
                                    // Extract reasoning_content from the
                                    // (possibly normalized) payload and
                                    // feed it to the accumulator. We do
                                    // this instead of using
                                    // `chunk.delta_reasoning` because:
                                    //   - `chunk.delta_reasoning` only
                                    //     catches `reasoning_content`
                                    //     from the raw upstream payload,
                                    //     missing upstream `reasoning`
                                    //     (which we just normalized) and
                                    //     `<think>` tags (which we just
                                    //     extracted to reasoning_content).
                                    //   - `payload_str` is the source of
                                    //     truth after normalization.
                                    if payload_str.contains("\"reasoning_content\"")
                                        && let Some(rc) =
                                            crate::sse_accumulator::extract_reasoning_content(
                                                payload_str,
                                            )
                                        && !rc.is_empty()
                                    {
                                        a.append_reasoning(rc);
                                    }
                                    // Drop `chunk.delta_reasoning` — we
                                    // extracted reasoning from the
                                    // normalized payload above. Using
                                    // both would double-count when the
                                    // upstream sent `reasoning_content`
                                    // natively.
                                    let _ = chunk.delta_reasoning.take();
                                    
                                }
                                // Build the SSE frame from the
                                // (possibly normalized) payload rather
                                // than `chunk.into_sse_bytes()` (which
                                // forwards the raw upstream payload
                                // verbatim, skipping normalization).
                                let sse_bytes = crate::sse::build_sse_frame(payload_str);
                                // Race cancellation guard: if another
                                // target won the race, discard this
                                // chunk to prevent interleaving.
                                if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                                    return self.fail_stream_client_disconnected(
                                        req,
                                        combo,
                                        target,
                                        attempt,
                                        race_size,
                                        started,
                                        model,
                                        connect_and_send_ms,
                                        ttft_ms,
                                        trace_id,
                                        acc.as_mut(),
                                        &chunk_id,
                                        created,
                                        &model_name,
                                    );
                                }
                                // Mark this chunk as "real content" so the
                                // body stream switches from `total_deadline`
                                // to the chunk-gap (`body_chunk_ms`) timer
                                // for subsequent reads. Only chunks with
                                // `has_content == true` reset the timer —
                                // metadata-only events (e.g. a final chunk
                                // carrying only `usage`/`finish_reason`)
                                // must NOT reset it because no real token
                                // was generated. This prevents the
                                // idle_chunk timer from firing when the
                                // upstream sends a metadata event and
                                // then goes silent while the model is
                                // still generating.
                                if chunk.has_content {
                                    stream.note_content_chunk();
                                }
                                if let Err(e) = sink.send(sse_bytes).await {
                                    return self.fail_on_sink_send_error(
                                        e,
                                        req,
                                        combo,
                                        target,
                                        attempt,
                                        race_size,
                                        started,
                                        model,
                                        connect_and_send_ms,
                                        ttft_ms,
                                        trace_id,
                                        acc.as_mut(),
                                        &chunk_id,
                                        created,
                                        &model_name,
                                    );
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(chunk_id = %chunk_id, error = %e,
                                    "failed to parse SSE line from upstream");
                            }
                        }
                    } else {
                        // G1 fix: feed the accumulator on the fast path too.
                        //
                        // Apply the canonical reasoning-normalization
                        // pipeline (normalize `reasoning` →
                        // `reasoning_content`, strip `<think>` tags from
                        // content) via the shared `apply_reasoning_normalizations`
                        // helper. Both fast and slow paths now produce
                        // identical client-facing output, and the
                        // anti-duplication logic ensures that upstreams
                        // which send reasoning in BOTH a `reasoning`
                        // field AND `<think>` tags in content (e.g.
                        // MiniMax-M3 via tokenrouter) don't surface the
                        // reasoning twice to the client.
                        //
                        // PERF (chunk-forwarding CPU reduction):
                        // When `apply_reasoning_normalizations` returns
                        // `None` (the common case — most upstreams send
                        // clean `reasoning_content` or no reasoning at
                        // all, AND no `<think>` tags in content), we can
                        // reuse the original `line_bytes` BytesMut —
                        // which already contains `data: <payload>`
                        // (without the trailing `\n` because `split_to(pos)`
                        // excludes the `\n` and `advance(1)` skips it) —
                        // by appending just `\n\n` in-place for the SSE
                        // terminator and freezing. This eliminates, per
                        // chunk on the common fast path:
                        //   - one heap allocation (`BytesMut::with_capacity`)
                        //   - one full-payload memcpy
                        //     (`extend_from_slice(payload.as_bytes())`)
                        //   - two small memcpys (`data: ` + `\n\n`)
                        // The only cost is appending 2 bytes to `line_bytes`,
                        // which usually does NOT reallocate: `split_to`
                        // preserves the parent buffer's capacity, and the
                        // parent is `BytesMut::with_capacity(8192)` reserved
                        // up to 16 KiB above. Even on the rare case where it
                        // does realloc, the cost is amortized across the
                        // buffer's lifetime, not per chunk.
                        let effective_payload = apply_reasoning_normalizations(
                            json_payload,
                            &mut think_extractor,
                            &mut tool_call_acc,
                        );

                        // Accumulator work — scoped so the borrows on
                        // `line_bytes` (via `line` -> `json_payload`) are
                        // released before we move `line_bytes` for the
                        // in-place reframe below. NLL releases the borrow at
                        // the end of this block.
                        //
                        // IMPORTANT: feed the accumulator from
                        // `effective_payload` (the post-normalization
                        // payload), NOT from `json_payload` (the raw
                        // upstream payload). Previously this block fed
                        // `normalized` (which only applied the
                        // `reasoning` → `reasoning_content` rename) and
                        // then computed `effective_payload` (which also
                        // applied `<think>` extraction) but only used
                        // `effective_payload` for the outgoing SSE frame
                        // — so the persisted `response_body_json` had
                        // raw `<think>` tags in `content` while the
                        // client saw the cleaned version. Using
                        // `effective_payload` for both paths makes the
                        // persisted body match the client-visible body.
                        {
                            let payload = effective_payload.as_deref().unwrap_or(json_payload);
                            if let Some(a) = acc.as_mut() {
                                a.append_openai_raw(payload);
                                // PERF (single-scan guard): gate the
                                // `extract_reasoning_content` call (which does
                                // a full `memchr::memmem::find` scan for
                                // `"reasoning_content":"`) behind a cheaper
                                // `contains("\"reasoning_content\"")` check.
                                // Most streaming chunks (content deltas,
                                // role-only, finish, etc.) do NOT carry this
                                // field, so this saves one full-payload scan
                                // per chunk on the recording path. The guard
                                // itself short-circuits on first non-match.
                                if payload.contains("\"reasoning_content\"")
                                    && let Some(rc) =
                                        crate::sse_accumulator::extract_reasoning_content(payload)
                                    && !rc.is_empty()
                                {
                                    a.append_reasoning(rc);
                                }
                            }
                        }

                        // Build the SSE frame.
                        let sse_bytes = if let Some(modified) = effective_payload {
                            // Normalization or think extraction modified
                            // the payload. Build a fresh frame with the
                            // modified JSON.
                            crate::sse::build_sse_frame(&modified)
                        } else {
                            // Fast path: forward the original `data: <payload>`
                            // line directly. `line_bytes` is the BytesMut
                            // returned by `buffer.split_to(pos)` and contains
                            // the upstream's `data: <payload>` bytes verbatim
                            // (we did not modify it — only `line`/`json_payload`
                            // were borrows for substring checks). Append `\n\n`
                            // for the SSE event terminator and freeze.
                            // Zero allocation, zero payload memcpy.
                            let mut frame = line_bytes;
                            frame.extend_from_slice(b"\n\n");
                            frame.freeze()
                        };

                        // Race cancellation guard: check before
                        // writing to the shared sink.
                        if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                            return self.fail_stream_client_disconnected(
                                req,
                                combo,
                                target,
                                attempt,
                                race_size,
                                started,
                                model,
                                connect_and_send_ms,
                                ttft_ms,
                                trace_id,
                                acc.as_mut(),
                                &chunk_id,
                                created,
                                &model_name,
                            );
                        }
                        // Mark this chunk as "real content" so the body
                        // stream switches from `total_deadline` to the
                        // chunk-gap (`body_chunk_ms`) timer for the next
                        // read. The fast path only runs for chunks
                        // WITHOUT `usage` or non-null `finish_reason`
                        // (those go to the slow path above), so every
                        // chunk reaching this point is a content delta
                        // — we unconditionally reset the timer here.
                        // See the slow path above for the `has_content`
                        // gate that metadata-only chunks must NOT reset.
                        stream.note_content_chunk();
                        if let Err(e) = sink.send(sse_bytes).await {
                            return self.fail_on_sink_send_error(
                                e,
                                req,
                                combo,
                                target,
                                attempt,
                                race_size,
                                started,
                                model,
                                connect_and_send_ms,
                                ttft_ms,
                                trace_id,
                                acc.as_mut(),
                                &chunk_id,
                                created,
                                &model_name,
                            );
                        }
                    }
                    continue;
                }

                let parsed = match target_format {
                    crate::models::TargetFormat::Openai => crate::sse::parse_openai_sse_line(line),
                    crate::models::TargetFormat::Gemini => {
                        crate::sse::parse_gemini_sse_line(line, &chunk_id, created, &model_name)
                    }
                    crate::models::TargetFormat::Anthropic => {
                        // Anthropic SSE: track event type across lines
                        // and run the stateful translator that can
                        // accumulate Anthropic `tool_use` blocks into
                        // OpenAI-style `tool_calls` chunks. The
                        // accumulator lives across iterations of the
                        // outer loop.
                        match crate::sse::parse_anthropic_sse_stream_line(
                            line,
                            &mut current_event_type,
                        ) {
                            Ok(Some(payload)) => {
                                // H5 fix: thread the tool_use accumulator
                                // through the translator. The counter
                                // allocates fresh indices for each
                                // content_block_start that opens a
                                // tool_use; the accumulator carries the
                                // in-flight id+name+arguments across
                                // deltas.
                                match crate::sse::translate_anthropic_sse_event(
                                    &payload,
                                    &chunk_id,
                                    created,
                                    &model_name,
                                    &mut tool_use_acc,
                                    &mut tool_call_index_counter,
                                ) {
                                    Ok(Some(chunk)) => Ok(Some(chunk)),
                                    Ok(None) => Ok(None),
                                    Err(e) => Err(e),
                                }
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                };

                match parsed {
                    Ok(Some(mut chunk)) => {
                        if chunk.done {
                            // Capture stop_reason from the final chunk.
                            if chunk.stop_reason.is_some() {
                                stop_reason = chunk.stop_reason;
                            }
                            // Send [DONE] sentinel and break.
                            // H4 fix: record the fact that we sent
                            // the upstream's [DONE] so the post-loop
                            // sentinel below does not double-emit.
                            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                                return self.fail_stream_client_disconnected(
                                    req,
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_and_send_ms,
                                    ttft_ms,
                                    trace_id,
                                    acc.as_mut(),
                                    &chunk_id,
                                    created,
                                    &model_name,
                                );
                            }
                            if let Err(crate::race_sink::StreamSinkError::Lost) = sink.send(SSE_DONE_BYTES.clone()).await {
                                return self.fail_on_sink_send_error(
                                    crate::race_sink::StreamSinkError::Lost,
                                    req,
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_and_send_ms,
                                    ttft_ms,
                                    trace_id,
                                    acc.as_mut(),
                                    &chunk_id,
                                    created,
                                    &model_name,
                                );
                            }
                            done_sent = true;
                            // CRITICAL FIX: break from the outer
                            // loop after sending [DONE], matching
                            // the OpenAI path above. Without this
                            // break the loop continues processing
                            // the buffer and can forward data
                            // after [DONE] to the client, causing
                            // chunk overlapping and output corruption.
                            break 'stream_loop;
                        } else {
                            // Extract metadata before consuming chunk.
                            if chunk.usage.is_some() {
                                usage = chunk.usage.take();
                            }
                            if chunk.stop_reason.is_some() && stop_reason.is_none() {
                                stop_reason = chunk.stop_reason.take();
                            }
                            // G1 fix: feed the accumulator. Per-format
                            // dispatch covers Gemini and Anthropic
                            // (the OpenAI slow path is handled at
                            // line 2632). The translated chunk's
                            // payload is already OpenAI-shaped JSON
                            // (sse.rs's translators emit OpenAI
                            // JSON for both Gemini and Anthropic),
                            // so we hand the final JSON to
                            // `append_openai_raw` for content
                            // reconstruction in `finish()`.
                            //
                            // Extract the per-chunk fields that
                            // don't fit the OpenAI shape before
                            // consuming the chunk.
                            let delta_reasoning = chunk.delta_reasoning.take();
                            let _delta_tool_calls = std::mem::take(&mut chunk.delta_tool_calls);
                            // Capture has_content BEFORE consuming
                            // the chunk — `into_json_string()` takes
                            // `self` by value. We use this flag below
                            // to decide whether to call
                            // `note_content_chunk()`. Metadata-only
                            // events (message_start, message_delta,
                            // content_block_start for tool_use) have
                            // `has_content == false` and must NOT
                            // reset the chunk-gap timer.
                            let chunk_has_content = chunk.has_content;
                            let json_str = chunk.into_json_string();
                            if let Some(a) = acc.as_mut() {
                                if let Some(u) = &usage {
                                    a.set_usage(u.clone());
                                }
                                if let Some(sr) = &stop_reason {
                                    a.set_stop_reason(sr.clone());
                                }
                                if let Some(dr) = &delta_reasoning
                                    && !dr.is_empty()
                                {
                                    a.append_reasoning(dr);
                                }
                                // Anthropic tool_use threading. The
                                // Open-shape events carry `id` and
                                // `function.name`; delta-shape
                                // events carry only `function.arguments`.
                                // `content_block_stop` returns
                                // `Ok(None)` upstream so we never see
                                // a Close event here (the accumulator's
                                // Close is a no-op anyway).
                                
                                a.append_openai_raw(&json_str);
                            }
                            // Pre-format as SSE frame to avoid per-chunk String alloc + axum Event overhead.
                            let sse_frame = crate::sse::build_sse_frame(&json_str);
                            // Mark this chunk as "real content" so the
                            // body stream switches from `total_deadline`
                            // to the chunk-gap timer for the next read.
                            // Only chunks with `has_content == true`
                            // reset the timer — metadata-only events
                            // (message_start, message_delta,
                            // content_block_start for tool_use) have
                            // `has_content == false` because they
                            // carry no generated tokens. This is the
                            // root fix for the "idle_chunk after
                            // 10000ms" bug on MiniMax-M3 tool calls:
                            // the content_block_start event arrived
                            // at ~0ms with empty arguments, and
                            // resetting the timer there caused the
                            // 10s gap timer to fire while the model
                            // was still generating the first argument
                            // fragment.
                            if chunk_has_content {
                                stream.note_content_chunk();
                            }
                            if let Err(e) = sink.send(sse_frame).await {
                                // C4 fix: a real client disconnect
                                // mid-stream previously returned
                                // `PipelineResult { error: None }`
                                // — no usage row, tokens consumed
                                // at the upstream were unbilled.
                                // Hand off to
                                // `record_and_fail_with_trace_id`
                                // (H3 fix: the row's `trace_id`
                                // matches the StageEvent's
                                // `trace_id`) so the row lands in
                                // the DB with status_code = 499
                                // and the operator sees a real
                                // failure event. The `usage` we
                                // accumulated up to this point
                                // still goes into the row because
                                // `record_attempt_raw_with_tokens`
                                // accepts an `Option<u32>` pair.
                                return self.fail_on_sink_send_error(
                                    e,
                                    req,
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_and_send_ms,
                                    ttft_ms,
                                    trace_id,
                                    acc.as_mut(),
                                    &chunk_id,
                                    created,
                                    &model_name,
                                );
                            }
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(
                            chunk_id = %chunk_id,
                            error = %e,
                            "failed to parse SSE line from upstream"
                        );
                        continue;
                    }
                }
            }
        } // end of SSE chunk loop

        // Process any remaining data in the buffer.
        // GUARD: skip when `[DONE]` was already sent — any data
        // that arrived after the end-of-stream marker is either
        // the trailing `\n` from `\n\n` or stray upstream data
        // that would corrupt the client's view if forwarded.
        if !done_sent && !sse_parser.is_empty() {
            // Also guard against race cancellation — if another
            // target already won, discard residual buffer data
            // to prevent chunk interleaving.
            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                return self.fail_stream_client_disconnected(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    model,
                    connect_and_send_ms,
                    ttft_ms,
                    trace_id,
                    acc.as_mut(),
                    &chunk_id,
                    created,
                    &model_name,
                );
            }
            if let Ok(line) = std::str::from_utf8(sse_parser.remaining_bytes()) {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with(':') {
                    let parsed = match target_format {
                        crate::models::TargetFormat::Openai => {
                            crate::sse::parse_openai_sse_line(line)
                        }
                        crate::models::TargetFormat::Gemini => {
                            crate::sse::parse_gemini_sse_line(line, &chunk_id, created, &model_name)
                        }
                        crate::models::TargetFormat::Anthropic => {
                            match crate::sse::parse_anthropic_sse_stream_line(
                                line,
                                &mut current_event_type,
                            ) {
                                Ok(Some(payload)) => {
                                    match crate::sse::translate_anthropic_sse_payload(
                                        &payload,
                                        &chunk_id,
                                        created,
                                        &model_name,
                                    ) {
                                        Ok(Some(chunk)) => Ok(Some(chunk)),
                                        Ok(None) => Ok(None),
                                        Err(e) => Err(e),
                                    }
                                }
                                Ok(None) => Ok(None),
                                Err(e) => Err(e),
                            }
                        }
                    };
                    if let Ok(Some(mut chunk)) = parsed {
                        if chunk.usage.is_some() {
                            usage = chunk.usage.take();
                        }
                        if !chunk.done {
                            let sse_bytes = chunk.into_sse_bytes();
                            if let Err(e) = sink.send(sse_bytes).await {
                                // Bug fix: handle BOTH `Lost` and `Closed`.
                                return self.fail_on_sink_send_error(
                                    e,
                                    req,
                                    combo,
                                    target,
                                    attempt,
                                    race_size,
                                    started,
                                    model,
                                    connect_and_send_ms,
                                    ttft_ms,
                                    trace_id,
                                    acc.as_mut(),
                                    &chunk_id,
                                    created,
                                    &model_name,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Cancellation checkpoint: if the watch fired during the
        // stream poll (above), the `while let` loop already
        // exited normally. We must NOT send [DONE] or any further
        // chunks to a client that has already given up — and we
        // must record a `ClientDisconnected` usage row, not a
        // success row, so the dashboard reflects the cancellation.
        //
        // IMPORTANT: we only treat this as a disconnect if the
        // stream did NOT complete normally. If `done_sent` is true,
        // the upstream sent [DONE] and the stream completed — the
        // client may have closed the connection immediately after
        // receiving [DONE] (which is normal behavior), and the
        // DisconnectBody may have fired the watch on the residual
        // socket close. In that case, the request was successful
        // and should be recorded as such, not as a disconnect.
        let client_disconnected = if done_sent {
            // Stream completed — ignore any residual disconnect signal.
            false
        } else {
            let mut rx = req.client_disconnected.clone();
            self.is_client_disconnected(&mut rx)
        };
        if client_disconnected {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                "client cancelled during SSE stream; aborting attempt"
            );
            return self.fail_stream_client_disconnected(
                req,
                combo,
                target,
                attempt,
                race_size,
                started,
                model,
                connect_and_send_ms,
                ttft_ms,
                trace_id,
                acc.as_mut(),
                &chunk_id,
                created,
                &model_name,
            );
        }

        // Send [DONE] if the upstream didn't send it explicitly.
        // Some upstreams close the connection without the sentinel.
        //
        // H4 fix: if the upstream's SSE stream ended with an
        // explicit `done: true` (or the OpenAI `[DONE]` line
        // forwarded at line 2307), the loop sets `done_sent = true`
        // and we MUST skip this post-loop sentinel — otherwise the
        // client sees two `data: [DONE]` chunks (Anthropic would
        // see three, since both `message_delta` AND `message_stop`
        // emit `done: true`; see sse.rs:309 / sse.rs:316).
        if !done_sent {
            // Guard against race cancellation — a loser should not
            // send [DONE] to the shared sink.
            if req.race_cancel.as_ref().is_some_and(|rc| rc.is_cancelled()) {
                return self.fail_stream_client_disconnected(
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    model,
                    connect_and_send_ms,
                    ttft_ms,
                    trace_id,
                    acc.as_mut(),
                    &chunk_id,
                    created,
                    &model_name,
                );
            }
            if let Err(crate::race_sink::StreamSinkError::Lost) = sink.send(SSE_DONE_BYTES.clone()).await {
                return self.fail_on_sink_send_error(
                    crate::race_sink::StreamSinkError::Lost,
                    req,
                    combo,
                    target,
                    attempt,
                    race_size,
                    started,
                    model,
                    connect_and_send_ms,
                    ttft_ms,
                    trace_id,
                    acc.as_mut(),
                    &chunk_id,
                    created,
                    &model_name,
                );
            }
        }

        let total_ms = started.elapsed().as_millis() as u64;

        // Bug fix: detect "empty streaming response" — the stream
        // completed (done_sent or EOF) but the accumulator has no
        // content, no reasoning, no tool_calls. This happens with
        // providers like nvidia-nim/minimax-m3 (Anthropic format)
        // that return 200 + empty content + null finish_reason.
        // Treat as error so the pipeline retries the next target.
        let is_empty_stream = acc.as_ref().is_some_and(|a| a.is_empty());
        if is_empty_stream {
            let err = CoreError::UpstreamConnection(
                "streaming response was empty (no content, no reasoning, no tool_calls) — treating as error for retry".to_string(),
            );
            let acc_ref: Option<&crate::sse_accumulator::ResponseAccumulator> = match &mut acc {
                Some(a) => {
                    a.mark_partial();
                    Some(&*a)
                }
                None => None,
            };
            return self.record_and_fail_with_trace_id_and_partial(
                req,
                combo,
                target,
                FailureContext {
                    attempt,
                    race_size,
                    err: &err,
                    started,
                    model: Some(model),
                    connect_ms: Some(connect_and_send_ms),
                    ttft_ms,
                    status_code: 502,
                },
                trace_id,
                acc_ref,
                Some(&chunk_id),
                created,
                &model_name,
            );
        }

        // Record usage.
        // H5: streaming-success semantics. `is_streaming` is
        // always true here (we came from the streaming
        // dispatch). `stream_complete` mirrors the
        // post-loop [DONE] flag — `done_sent` is true iff the
        // upstream emitted the sentinel before its connection
        // closed.
        let prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = usage.as_ref().map(|u| u.completion_tokens);
        // G1 fix: assemble the persisted response body. The accumulator
        // is `Some(_)` only when `is_recording() == true` at function
        // entry, so when recording is OFF the only cost is a single
        // match on `acc.as_ref()`. The downstream `is_recording` gate
        // at `record_attempt_raw_with_tokens` (pipeline.rs:3197-3200)
        // drops the body to `None` if recording flipped off mid-stream.
        let response_body_json: Option<serde_json::Value> = acc
            .as_ref()
            .map(|a| a.finish(&chunk_id, created, &model_name));
        // G1 fix: save the request body for streaming requests too.
        // Previously this was `None` ("out of scope per G1 spec") so
        // the detail modal always showed "No request body recorded"
        // for all streaming rows.
        // Prefer the raw request body (preserves unknown fields the
        // typed `OpenAIRequest` struct would drop). Fall back to
        // re-serializing the typed struct when the raw body wasn't
        // captured (e.g., requests constructed internally without
        // going through the HTTP handler).
        let request_body_json = req
            .request_body_json
            .clone()
            .or_else(|| serde_json::to_value(&*req.openai_request).ok().map(Arc::new));
        let usage_tuple = match self.record_attempt_raw_with_tokens(
            Arc::clone(&req),
            combo,
            target,
            Some(model),
            None,
            Some(connect_and_send_ms),
            ttft_ms,
            total_ms,
            status_code,
            attempt,
            race_size,
            trace_id,
            prompt_tokens,
            completion_tokens,
            request_body_json,
            response_body_json.clone(),
            None,        // request_headers
            None,        // response_headers
            true,        // is_streaming (H5)
            done_sent,   // stream_complete (H5)
            stop_reason, // captured from upstream SSE chunk
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "record_attempt_raw_with_tokens failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code,
            error: None,
            // For non-streaming clients (StreamSink::Discard), return
            // the accumulated response so the chat handler can serialize
            // it as JSON. For streaming clients, the chunks were already
            // forwarded via the sink — return None (the chat handler
            // doesn't need the full response, it already sent the SSE).
            final_response: if matches!(
                req.stream_sink.as_ref(),
                Some(crate::race_sink::StreamSink::Discard)
            ) {
                response_body_json
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            } else {
                None
            },
            attempts: attempt,
            usage_tuple,
        }
    }
}
