use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use crate::{FailureContext, PipelineRequest, PipelineResult, parse_retry_after_ms};
use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::upstream::{CancellationToken, UpstreamError, UpstreamRequest};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

use crate::think_extractor::extract_think_from_response;

/// Bundles the parameters shared by streaming failure methods
/// (`fail_stream_client_disconnected`, `fail_on_sink_send_error`).
/// Eliminates la anti-pattern de 14-15 argumentos posicionales.
pub(crate) struct DispatchContext<'a> {
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: Instant,
    pub(crate) model: &'a Model,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

impl<'a> DispatchContext<'a> {
    #[inline]
    pub(crate) fn fail_ctx_code<'e>(
        &self,
        err: &'e CoreError,
        connect_ms: Option<u64>,
        ttft_ms: Option<u64>,
        status_code: u16,
    ) -> crate::FailureContext<'e>
    where
        'a: 'e,
    {
        crate::FailureContext {
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
            attempt: self.attempt,
            race_size: self.race_size,
            err,
            started: self.started,
            model: Some(self.model),
            connect_ms,
            ttft_ms,
            status_code,
        }
    }
}

pub(crate) struct StreamFailureContext<'a> {
    pub(crate) req: PipelineRequest,
    pub(crate) combo: &'a Combo,
    pub(crate) target: &'a ComboTarget,
    pub(crate) attempt: u8,
    pub(crate) race_size: u8,
    pub(crate) started: std::time::Instant,
    pub(crate) model: &'a Model,
    pub(crate) connect_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) trace_id: String,
    pub(crate) acc: Option<&'a mut crate::sse_accumulator::ResponseAccumulator>,
    pub(crate) chunk_id: &'a str,
    pub(crate) created: u64,
    pub(crate) model_name: &'a str,
    pub(crate) proxy_url: Option<String>,
    pub(crate) proxy_status: Option<String>,
}

pub trait Dispatcher: Send + Sync {
    fn is_recording(&self) -> bool;
}

impl Dispatcher for UpstreamDispatcher {
    fn is_recording(&self) -> bool {
        self.record_bodies_and_headers()
    }
}

#[derive(Clone)]
pub struct UpstreamDispatcher {
    pub(crate) conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    pub(crate) config: crate::PipelineConfig,
    pub(crate) tracker: crate::usage_tracker::UsageTracker,
    pub(crate) record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

fn find_bad_proxy_id(
    provider: &openproxy_types::providers::Provider,
    conn: &rusqlite::Connection,
    override_proxy_id: Option<&str>,
    is_per_account: bool,
    account_id: Option<openproxy_types::ids::AccountId>,
) -> Option<String> {
    if let Some(pid) = override_proxy_id {
        Some(pid.to_string())
    } else if is_per_account {
        account_id.and_then(|acc_id| {
            openproxy_db::accounts::get_current_proxy_id(conn, acc_id).unwrap_or(None)
        })
    } else {
        provider
            .current_proxy_id
            .as_deref()
            .map(ToString::to_string)
    }
}

fn should_rotate_proxy(
    provider: &openproxy_types::providers::Provider,
    trigger: crate::upstream_dispatcher::ProxyRotationTrigger,
) -> bool {
    match trigger {
        crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited => true,
        crate::upstream_dispatcher::ProxyRotationTrigger::Status(sc) => {
            let sc_str = sc.to_string();
            provider
                .proxy_rotation_errors
                .split(',')
                .map(str::trim)
                .any(|e| e == sc_str)
        }
        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError => provider
            .proxy_rotation_errors
            .split(',')
            .map(str::trim)
            .any(|e| e == "connect_error" || e == "timeout"),
    }
}

struct ProxyRotationArgs<'a> {
    conn: &'a rusqlite::Connection,
    provider_id: &'a openproxy_types::ids::ProviderId,
    bad_proxy: &'a str,
    trigger: crate::upstream_dispatcher::ProxyRotationTrigger,
    is_per_account: bool,
    account_id: Option<openproxy_types::ids::AccountId>,
    cooldown_ms: Option<u64>,
}

fn apply_proxy_rotation(args: ProxyRotationArgs<'_>) -> bool {
    let cooldown_duration = args.cooldown_ms.map_or_else(
        || std::time::Duration::from_mins(15),
        std::time::Duration::from_millis,
    );

    if matches!(
        args.trigger,
        crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError
    ) {
        let _ = openproxy_db::free_proxies::update_proxy_status(
            args.conn,
            args.bad_proxy,
            "dead",
            None,
        );
    }

    let _ = openproxy_db::cooldowns::add_provider_proxy_cooldown(
        args.conn,
        args.provider_id.as_str(),
        args.bad_proxy,
        cooldown_duration,
    );
    if args.is_per_account {
        if let Some(acc_id) = args.account_id {
            let _ = openproxy_db::accounts::clear_current_proxy_id(args.conn, acc_id);
        }
    } else {
        let _ = openproxy_db::providers::update_current_proxy(args.conn, args.provider_id, None);
    }

    openproxy_db::free_proxies::get_candidate_proxies_for_provider(args.conn, args.provider_id, 1)
        .is_ok_and(|c| !c.is_empty())
}

impl UpstreamDispatcher {
    pub(crate) fn new(
        conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
        config: crate::PipelineConfig,
        tracker: crate::usage_tracker::UsageTracker,
        record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            conn,
            config,
            tracker,
            record_bodies_and_headers,
        }
    }

    pub(crate) fn record_bodies_and_headers(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) async fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &openproxy_types::ids::ProviderId,
        account_id: Option<openproxy_types::ids::AccountId>,
        override_proxy_id: Option<&str>,
        trigger: crate::upstream_dispatcher::ProxyRotationTrigger,
        cooldown_ms: Option<u64>,
    ) -> bool {
        let conn_clone = Arc::clone(&self.conn);
        let provider_id = provider_id.to_owned();
        let override_proxy_id = override_proxy_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let conn = conn_clone.lock();
            let Some(provider) = openproxy_db::providers::get(&conn, &provider_id).unwrap_or(None)
            else {
                return false;
            };
            if !provider.use_proxies {
                return false;
            }
            let is_per_account = provider.proxy_rotation_mode.as_ref() == "account";
            let bad_proxy_id = find_bad_proxy_id(
                &provider,
                &conn,
                override_proxy_id.as_deref(),
                is_per_account,
                account_id,
            );

            if should_rotate_proxy(&provider, trigger)
                && let Some(ref bad_proxy) = bad_proxy_id
            {
                tracing::warn!(
                    provider = %provider_id,
                    account_id = ?account_id,
                    proxy_id = %bad_proxy,
                    trigger = ?trigger,
                    "proxy rotation triggered: clearing binding and adding cooldown for provider"
                );
                return apply_proxy_rotation(ProxyRotationArgs {
                    conn: &conn,
                    provider_id: &provider_id,
                    bad_proxy,
                    trigger,
                    is_per_account,
                    account_id,
                    cooldown_ms,
                });
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) fn is_client_disconnected(
        rx: &mut watch::Receiver<Option<openproxy_types::CancelReason>>,
    ) -> Option<openproxy_types::CancelReason> {
        *rx.borrow_and_update()
    }

    pub(crate) fn record_and_fail(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
    ) -> PipelineResult {
        let trace_id = if ctx.attempt > 1 {
            {
                let mut s = String::with_capacity(48);
                use std::fmt::Write;
                let _ = write!(&mut s, "{}:retry{}", req.trace_id, ctx.attempt - 1);
                s
            }
        } else {
            req.trace_id.to_string()
        };
        self.record_and_fail_with_trace_id(req, combo, target, ctx, trace_id)
    }
}

pub(crate) struct DispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body_bytes: bytes::Bytes,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
}

pub(crate) struct NonStreamingSuccessArgs {
    pub status_code: u16,
    pub connect_and_send_ms: u64,
    pub ttft_ms: u64,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_body_raw: serde_json::Value,
    pub openai_response: OpenAIResponse,
}

pub(crate) struct StreamDispatchParams<'a> {
    pub target: &'a ComboTarget,
    pub combo: &'a Combo,
    pub req: PipelineRequest,
    pub model: &'a Model,
    pub target_format: openproxy_types::TargetFormat,
    pub resolved_timeouts: &'a Timeouts,
    pub started: Instant,
    pub attempt: u8,
    pub race_size: u8,
    pub trace_id: String,
    pub upstream_request: UpstreamRequest,
}

pub(crate) struct StreamingNon2xxArgs<'a> {
    pub response: openproxy_adapters::upstream::UpstreamResponse,
    pub status_code: u16,
    pub req: PipelineRequest,
    pub combo: &'a Combo,
    pub target: &'a ComboTarget,
    pub model: &'a Model,
    pub connect_and_send_ms: u64,
}

pub(crate) struct StreamingSuccessArgs<'a> {
    pub state: crate::streaming_state::StreamingState,
    pub chunk_id: &'a str,
    pub created: u64,
    pub model_name: &'a str,
    pub connect_and_send_ms: u64,
    pub status_code: u16,
}

impl UpstreamDispatcher {
    pub(crate) fn record_and_fail_with_trace_id(
        &self,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        ctx: FailureContext<'_>,
        trace_id: String,
    ) -> PipelineResult {
        self.tracker
            .record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx,
                trace_id,
                acc: None,
                chunk_id: None,
                created: 0,
                model_name: "",
            })
    }

    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        params: crate::PartialFailureParams<'_>,
    ) -> PipelineResult {
        self.tracker
            .record_and_fail_with_trace_id_and_partial(params)
    }

    pub(crate) async fn handle_upstream_error(
        &self,
        err: UpstreamError,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        dctx: &DispatchContext<'_>,
        connect_and_send_ms: u64,
    ) -> PipelineResult {
        if matches!(err, UpstreamError::Cancel) {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = connect_and_send_ms,
                "client cancelled during upstream send; aborting attempt"
            );
            let core_err = CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected);
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(
                    &core_err,
                    Some(connect_and_send_ms),
                    None,
                    core_err.http_status(),
                ),
            );
        }

        let (status, body) = match err {
            UpstreamError::Timeout(phase) => {
                let phase_label = phase.as_str();
                let config_hint = phase.config_hint();
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.id.0,
                    provider = %target.provider_id,
                    phase = %phase,
                    elapsed_ms = connect_and_send_ms,
                    config_hint = config_hint,
                    "upstream phase timed out; aborting attempt"
                );
                (
                    504,
                    format!(
                        "upstream phase `{phase_label}` timed out after {connect_and_send_ms}ms (config: {config_hint})"
                    ),
                )
            }
            UpstreamError::Connection(msg)
            | UpstreamError::Tls(msg)
            | UpstreamError::Http(msg)
            | UpstreamError::Decode(msg)
            | UpstreamError::Invalid(msg) => (502, format!("upstream connection error: {msg}")),
            _ => (502, "unknown upstream error".to_string()),
        };

        let is_proxy_rotated = self
            .check_and_trigger_proxy_rotation(
                &target.provider_id,
                target.account_id,
                req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                None,
            )
            .await;

        let core_err = CoreError::upstream_error(
            status,
            target.provider_id.to_string(),
            dctx.model.model_id.as_str().to_string(),
            body,
            is_proxy_rotated,
        );

        self.record_and_fail(
            req,
            combo,
            target,
            dctx.fail_ctx_code(
                &core_err,
                Some(connect_and_send_ms),
                None,
                core_err.http_status(),
            ),
        )
    }

    async fn broadcast_account_invalid_notification(
        &self,
        aid: openproxy_types::ids::AccountId,
        provider_id_str: String,
        model_id_str: String,
        status_code: u16,
    ) {
        let dedup_key = format!("account_invalid:{}", aid.0);
        let payload = serde_json::json!({
            "code": "account_invalid",
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
        let repo = Arc::clone(&self.tracker.repo);
        tokio::task::spawn_blocking(move || {
            let _ = repo.insert_and_broadcast_notification(
                "system",
                &payload,
                Some(&dedup_key),
                Some(&provider_id_str),
            );
        })
        .await
        .ok();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_non_2xx_response(
        &self,
        status_code: u16,
        retry_after_header: Option<&str>,
        body_str: String,
        req: PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        _model: &Model,
        dctx: &DispatchContext<'_>,
        connect_and_send_ms: u64,
        ttft_ms: Option<u64>,
    ) -> PipelineResult {
        let retry_after_ms = retry_after_header.and_then(parse_retry_after_ms);
        let is_rate_limited_status = status_code == 429 || status_code == 408 || status_code == 503;
        let retry_ms = retry_after_ms.unwrap_or(300_000);

        let trigger = if is_rate_limited_status {
            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited
        } else {
            crate::upstream_dispatcher::ProxyRotationTrigger::Status(status_code)
        };
        let is_proxy_rotated = self
            .check_and_trigger_proxy_rotation(
                &target.provider_id,
                target.account_id,
                req.proxy_override.as_ref().map(|(pid, _)| pid.as_str()),
                trigger,
                is_rate_limited_status.then_some(retry_ms),
            )
            .await;

        if (status_code == 401 || status_code == 403)
            && let Some(aid) = target.account_id
        {
            self.broadcast_account_invalid_notification(
                aid,
                target.provider_id.to_string(),
                dctx.model.model_id.as_str().to_string(),
                status_code,
            )
            .await;
        }

        let err = if is_rate_limited_status {
            // GAP-6: if the body says RESOURCE_EXHAUSTED, mark this
            // (account, model) pair as live-limited for 5 minutes.
            // Fire-and-forget; we don't want to block the dispatch
            // path on a SQLite write. The `conn_clone` follows the
            // existing pattern in `check_and_trigger_proxy_rotation`
            // (upstream_dispatcher.rs:215-219).
            //
            // Note: when GAP-4 lands and `UpstreamErrorClass` is wired
            // into the error construction, this can be tightened to
            // `class == UpstreamErrorClass::ResourceExhausted`. Until
            // then we match the body string directly (case-sensitive
            // substring, matching the antigravity wire format).
            if status_code == 429
                && body_str.contains("RESOURCE_EXHAUSTED")
                && let Some(aid) = target.account_id
            {
                let model_id = dctx.model.model_id.clone();
                let conn_clone = Arc::clone(&self.conn);
                let handle = tokio::task::spawn_blocking(move || {
                    let conn = conn_clone.lock();
                    let until = (chrono::Utc::now()
                        + chrono::Duration::minutes(5))
                    .to_rfc3339();
                    if let Err(e) = openproxy_db::live_limited::mark_limited(
                        &conn,
                        aid,
                        &model_id,
                        &until,
                        "RESOURCE_EXHAUSTED",
                    ) {
                        tracing::warn!(
                            account_id = aid.0,
                            model = %model_id.as_str(),
                            error = %e,
                            "failed to mark live_limited_models"
                        );
                    }
                });
                // Fire-and-forget; we don't want to block the dispatch
                // path on the SQLite write. `drop` the handle to make
                // the intent explicit (AGENTS.md §3.3 fire-and-forget
                // pattern + clippy::let_underscore_future).
                std::mem::drop(handle);
            }
            CoreError::RateLimited {
                provider: target.provider_id.to_string(),
                retry_after_ms: retry_ms,
                is_proxy_rotated,
            }
        } else {
            if status_code == 400 && body_str.contains("2013") {
                tracing::warn!(
                    status_code = status_code,
                    provider = %target.provider_id,
                    model = %dctx.model.model_id.as_str(),
                    error_body = %body_str,
                    openai_request_messages_count = req.openai_request.messages.len(),
                    openai_request_tools_count = req.openai_request.tools.as_ref().map_or(0, std::vec::Vec::len),
                    "MiniMax 2013 error: tool_call/tool_result mismatch."
                );
            }
            // GAP-4: classify the body and propagate the result so the
            // circuit breaker knows not to penalize request-shape errors
            // (see `error_classification::classify_upstream_error`).
            let class =
                crate::error_classification::classify_upstream_error(status_code, &body_str);
            let is_hard_skip = class.is_hard_skip();
            if is_hard_skip {
                tracing::debug!(
                    provider = %target.provider_id,
                    model = %dctx.model.model_id.as_str(),
                    status = status_code,
                    class = %class,
                    "non-account error class — will not penalize circuit breaker"
                );
            }
            CoreError::upstream_error_classified(
                status_code,
                target.provider_id.to_string(),
                dctx.model.model_id.as_str().to_string(),
                body_str,
                is_proxy_rotated,
                class,
            )
        };

        self.record_and_fail(
            req,
            combo,
            target,
            dctx.fail_ctx_code(&err, Some(connect_and_send_ms), ttft_ms, status_code),
        )
    }
}

fn is_horde_vision_request(target: &ComboTarget, model: &Model, req: &PipelineRequest) -> bool {
    target.provider_id.as_str() == "horde"
        && (openproxy_adapters::HordeAdapter::is_vision_model(model.model_id.as_str())
            || openproxy_adapters::HordeAdapter::extract_image_from_messages(
                &req.openai_request.messages,
            )
            .is_some())
}

fn populate_upstream_headers(upstream_request: &mut UpstreamRequest, headers: &[(String, String)]) {
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(v),
        ) {
            upstream_request.headers.insert(name, value);
        }
    }
}

fn translate_simple_text_response(
    response_body_raw: &serde_json::Value,
    model_name: String,
) -> OpenAIResponse {
    let text = response_body_raw
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .or_else(|| response_body_raw.get("content").and_then(|s| s.as_str()))
        .unwrap_or("");

    let mut id = String::with_capacity(48);
    use std::fmt::Write;
    let _ = write!(&mut id, "chatcmpl_{}", uuid::Uuid::new_v4());

    OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        model: model_name,
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(text.to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    }
}

fn translate_non_streaming_body(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &PipelineRequest,
) -> Result<OpenAIResponse, CoreError> {
    match target_format {
        openproxy_types::TargetFormat::Responses => {
            unreachable!("Responses format is handled natively before dispatcher")
        }
        openproxy_types::TargetFormat::Openai => {
            <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .map_err(|e| CoreError::Parse(format!("parse openai response: {e}")))
        }
        openproxy_types::TargetFormat::Anthropic => {
            let anthropic_resp: crate::translation::AnthropicResponse =
                <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                    response_body_raw,
                )
                .map_err(|e| CoreError::Parse(format!("parse anthropic response: {e}")))?;
            Ok(crate::translation::anthropic_to_openai(&anthropic_resp))
        }
        openproxy_types::TargetFormat::Gemini => {
            let adapter = openproxy_adapters::GeminiAdapter::new();
            adapter.translate_non_streaming_response(target_format, response_body_raw.clone())
        }
        openproxy_types::TargetFormat::Atomesus | openproxy_types::TargetFormat::Fx => Ok(
            translate_simple_text_response(response_body_raw, req.openai_request.model.clone()),
        ),
    }
}

fn is_empty_response(resp: &OpenAIResponse) -> bool {
    resp.choices.first().is_some_and(|c| {
        let msg = &c.message;
        let content_empty = msg
            .content
            .as_ref()
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty));
        let no_tool_calls = msg.tool_calls.as_ref().is_none_or(std::vec::Vec::is_empty);
        let no_reasoning = !msg.extra.contains_key("reasoning_content");
        let no_finish = c
            .finish_reason
            .as_ref()
            .is_none_or(|f| f == "null" || f.is_empty());
        content_empty && no_tool_calls && no_reasoning && no_finish
    })
}

impl UpstreamDispatcher {
    async fn fetch_proxy_status(&self, proxy_url: Option<&str>) -> Option<String> {
        let url = proxy_url?.to_string();
        let repo = Arc::clone(&self.tracker.repo);
        tokio::task::spawn_blocking(move || repo.get_proxy_status_by_url(&url))
            .await
            .unwrap_or(None)
    }

    async fn resolve_and_assign_proxy(
        &self,
        req: &PipelineRequest,
        target: &ComboTarget,
    ) -> Result<(Option<String>, Option<String>), CoreError> {
        let proxy_url = if let Some((_, ref purl)) = req.proxy_override {
            Some(purl.clone())
        } else {
            let repo = Arc::clone(&self.tracker.repo);
            let provider_id = target.provider_id.clone();
            let account_id = target.account_id;
            tokio::task::spawn_blocking(move || {
                repo.get_or_assign_provider_proxy(&provider_id, account_id)
            })
            .await??
        };

        let proxy_status = self.fetch_proxy_status(proxy_url.as_deref()).await;

        tracing::info!(
            proxy_used = ?proxy_url,
            proxy_status = %proxy_status.as_ref().unwrap_or(&"none".to_string()),
            "assigned proxy for upstream request"
        );

        Ok((proxy_url, proxy_status))
    }

    async fn dispatch_horde_vision(
        &self,
        params: DispatchParams<'_>,
        dctx: DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        let DispatchParams {
            target,
            combo,
            req,
            model,
            headers,
            started,
            attempt,
            race_size,
            trace_id,
            ..
        } = params;

        let Some(source_image) = openproxy_adapters::HordeAdapter::extract_image_from_messages(
            &req.openai_request.messages,
        ) else {
            let err = CoreError::Validation(
                "No image found in request messages for Horde vision interrogation".into(),
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, None, None, 400),
            );
        };

        let send_start = Instant::now();
        let cancel_token = CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
            &req.client_disconnected,
        ));
        let api_key = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("apikey"))
            .map_or("", |(_, v)| v.as_str());
        let base_url = "https://aihorde.net/api/v2";

        let caption_res = openproxy_adapters::HordeAdapter::execute_interrogate(
            &self.config.upstream_client,
            base_url,
            api_key,
            &source_image,
            cancel_token,
        )
        .await;

        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        let caption = match caption_res {
            Ok(c) => c,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, Some(connect_and_send_ms), None, e.http_status()),
                );
            }
        };

        let mut response_id = String::with_capacity(48);
        use std::fmt::Write;
        let _ = write!(
            &mut response_id,
            "chatcmpl_{}",
            uuid::Uuid::new_v4().simple()
        );

        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let completion_tokens = (caption.len() / 4).max(1) as u32;
        let prompt_tokens = 10u32;
        let openai_response = OpenAIResponse {
            id: response_id.clone(),
            object: "chat.completion".to_string(),
            created,
            model: req.openai_request.model.clone(),
            choices: vec![openproxy_types::OpenAIChoice {
                index: 0,
                message: openproxy_types::OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(caption.clone())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(openproxy_types::OpenAIUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                prompt_tokens_details: None,
            }),
        };

        if let Some(sink) = req.stream_sink.as_ref() {
            let chunk1 = serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": req.openai_request.model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": caption
                    },
                    "finish_reason": null
                }]
            });
            let chunk2 = serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": req.openai_request.model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            });
            let chunk1_str = serde_json::to_string(&chunk1).unwrap_or_default();
            let mut buf1 = String::with_capacity(chunk1_str.len() + 8);
            use std::fmt::Write;
            let _ = write!(&mut buf1, "data: {chunk1_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf1)).await;

            let chunk2_str = serde_json::to_string(&chunk2).unwrap_or_default();
            let mut buf2 = String::with_capacity(chunk2_str.len() + 8);
            let _ = write!(&mut buf2, "data: {chunk2_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf2)).await;
            let _ = sink.send(crate::pipeline::SSE_DONE_BYTES).await;
        }

        let total_ms_now = started.elapsed().as_millis() as u64;
        let request_headers_btm = if self.tracker.is_recording() {
            Some(crate::redact::redact_btreemap_sensitive(
                headers
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<std::collections::BTreeMap<String, String>>(),
            ))
        } else {
            None
        };
        let resp_json = serde_json::to_value(&openai_response).unwrap_or_default();
        let usage_tuple =
            match crate::usage_tracker::UsageRecordBuilder::new(&self.tracker, req, combo, target)
                .proxy_url(upstream_request.proxy)
                .proxy_status(upstream_request.proxy_status)
                .model_opt(Some(model))
                .err_opt(None)
                .connect_ms_opt(Some(connect_and_send_ms))
                .ttft_ms_opt(Some(connect_and_send_ms))
                .total_ms(total_ms_now)
                .status_code(200)
                .attempt(attempt)
                .race_size(race_size)
                .trace_id(trace_id.clone())
                .prompt_tokens_opt(Some(prompt_tokens))
                .completion_tokens_opt(Some(completion_tokens))
                .cached_tokens(None)
                .response_body_json(Some(resp_json))
                .request_headers(request_headers_btm)
                .response_headers(None)
                .is_streaming(false)
                .stream_complete(true)
                .stop_reason(None)
                .record()
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                    None
                }
            };

        PipelineResult {
            status_code: 200,
            error: None,
            final_response: Some(openai_response),
            attempts: attempt,
            usage_tuple,
        }
    }

    async fn collect_non_streaming_body(
        &self,
        response: openproxy_adapters::upstream::UpstreamResponse,
        status_code: u16,
        params: &DispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        connect_and_send_ms: u64,
        ttft_ms: u64,
    ) -> Result<bytes::Bytes, Box<PipelineResult>> {
        let non_streaming_body_deadline = params.started
            + std::time::Duration::from_millis(params.resolved_timeouts.total.as_millis() as u64);
        let mut remaining = non_streaming_body_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(std::time::Duration::ZERO);

        if !(200..300).contains(&status_code) {
            remaining = std::cmp::min(remaining, std::time::Duration::from_secs(5));
        }

        match tokio::time::timeout(remaining, response.collect()).await {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(UpstreamError::Cancel)) => {
                tracing::warn!(
                    combo_id = params.combo.id.0,
                    target_id = params.target.id.0,
                    provider = %params.target.provider_id,
                    elapsed_ms = params.started.elapsed().as_millis() as u64,
                    "client cancelled during upstream body read; aborting attempt"
                );
                let err = CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected);
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
            Ok(Err(UpstreamError::Timeout(phase))) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: params.started.elapsed().as_millis() as u64,
                };
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
            Ok(Err(e)) => {
                self.check_and_trigger_proxy_rotation(
                    &params.target.provider_id,
                    params.target.account_id,
                    params
                        .req
                        .proxy_override
                        .as_ref()
                        .map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let err = CoreError::UpstreamConnection(format!("read upstream body: {e}"));
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
            Err(_elapsed) => {
                self.check_and_trigger_proxy_rotation(
                    &params.target.provider_id,
                    params.target.account_id,
                    params
                        .req
                        .proxy_override
                        .as_ref()
                        .map(|(pid, _)| pid.as_str()),
                    crate::upstream_dispatcher::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let elapsed = params.started.elapsed().as_millis() as u64;
                let err = CoreError::UpstreamTimeout {
                    phase: "total (config: total_ms)".to_string(),
                    ms: elapsed,
                };
                tracing::warn!(
                    combo_id = params.combo.id.0,
                    target_id = params.target.id.0,
                    provider = %params.target.provider_id,
                    elapsed_ms = elapsed,
                    "non-streaming body read exceeded total_ms; aborting attempt"
                );
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
        }
    }

    fn record_non_streaming_success(
        &self,
        params: DispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        args: NonStreamingSuccessArgs,
    ) -> PipelineResult {
        let prompt_tokens = args.openai_response.usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = args
            .openai_response
            .usage
            .as_ref()
            .map(|u| u.completion_tokens);
        let cached_tokens = args
            .openai_response
            .usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);

        let total_ms_now = params.started.elapsed().as_millis() as u64;
        let request_headers_btm = if self.tracker.is_recording() {
            Some(crate::redact::redact_btreemap_sensitive(
                params
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<std::collections::BTreeMap<String, String>>(),
            ))
        } else {
            None
        };
        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            params.req,
            params.combo,
            params.target,
        )
        .proxy_url(dctx.proxy_url.clone())
        .proxy_status(dctx.proxy_status.clone())
        .model_opt(Some(params.model))
        .err_opt(None)
        .connect_ms_opt(Some(args.connect_and_send_ms))
        .ttft_ms_opt(Some(args.ttft_ms))
        .total_ms(total_ms_now)
        .status_code(args.status_code)
        .attempt(params.attempt)
        .race_size(params.race_size)
        .trace_id(params.trace_id)
        .prompt_tokens_opt(prompt_tokens)
        .completion_tokens_opt(completion_tokens)
        .cached_tokens(cached_tokens)
        .response_body_json(Some(args.response_body_raw))
        .request_headers(request_headers_btm)
        .response_headers(args.response_headers)
        .is_streaming(false)
        .stream_complete(true)
        .stop_reason(None)
        .record()
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code: args.status_code,
            error: None,
            final_response: Some(args.openai_response),
            attempts: params.attempt,
            usage_tuple,
        }
    }

    async fn dispatch_upstream_non_streaming(
        &self,
        params: DispatchParams<'_>,
        dctx: DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        let send_start = Instant::now();
        let reason = *params.req.client_disconnected.borrow();
        if let Some(reason) = reason {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = params.combo.id.0,
                target_id = params.target.id.0,
                provider = %params.target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream send; aborting attempt"
            );
            return self.record_and_fail(
                params.req,
                params.combo,
                params.target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            );
        }

        let cancel_token = CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
            &params.req.client_disconnected,
        ));
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                openproxy_adapters::upstream::TimeoutProfile::Custom(
                    params.resolved_timeouts.as_resolved(),
                ),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        let response = match result {
            Ok(r) => r,
            Err(err) => {
                return self
                    .handle_upstream_error(
                        err,
                        params.req,
                        params.combo,
                        params.target,
                        &dctx,
                        connect_and_send_ms,
                    )
                    .await;
            }
        };

        let status_code = response.status.as_u16();
        let response_headers = self.is_recording().then(|| {
            response
                .headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<std::collections::BTreeMap<String, String>>()
        });

        let ttft_ms = params.started.elapsed().as_millis() as u64;
        if (200..300).contains(&status_code) {
            openproxy_types::emit_stage_event!(
                request_id: params.req.request_id,
                trace_id: params.trace_id,
                stage: "waiting_ttft",
                elapsed_ms: ttft_ms,
                connect_ms: connect_and_send_ms,
                status_code: status_code,
            );
        }

        let body_bytes = match self
            .collect_non_streaming_body(
                response,
                status_code,
                &params,
                &dctx,
                connect_and_send_ms,
                ttft_ms,
            )
            .await
        {
            Ok(b) => b,
            Err(fail_res) => return *fail_res,
        };

        if !(200..300).contains(&status_code) {
            let retry_after_header = response_headers
                .as_ref()
                .and_then(|h: &std::collections::BTreeMap<String, String>| {
                    h.get("retry-after").or_else(|| h.get("Retry-After"))
                })
                .map(String::as_str);
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            return self
                .handle_non_2xx_response(
                    status_code,
                    retry_after_header,
                    body_str,
                    params.req,
                    params.combo,
                    params.target,
                    params.model,
                    &dctx,
                    connect_and_send_ms,
                    Some(ttft_ms),
                )
                .await;
        }

        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("invalid json in upstream response: {e}"));
                return self.record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        let openai_response = match translate_non_streaming_body(
            params.target_format,
            &response_body_raw,
            &params.req,
        ) {
            Ok(r) => extract_think_from_response(r),
            Err(err) => {
                return self.record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        if is_empty_response(&openai_response) {
            let err = CoreError::UpstreamConnection(
                "upstream returned 200 but response is empty (content=null, finish_reason=null, no tool_calls, no reasoning) — treating as error for retry".to_string(),
            );
            return self.record_and_fail(
                params.req,
                params.combo,
                params.target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), 502),
            );
        }

        self.record_non_streaming_success(
            params,
            &dctx,
            NonStreamingSuccessArgs {
                status_code,
                connect_and_send_ms,
                ttft_ms,
                response_headers,
                response_body_raw,
                openai_response,
            },
        )
    }

    async fn setup_upstream_request_and_context<'a>(
        &self,
        params: &DispatchParams<'a>,
    ) -> Result<(DispatchContext<'a>, UpstreamRequest), Box<PipelineResult>> {
        let mut dctx = DispatchContext {
            attempt: params.attempt,
            race_size: params.race_size,
            started: params.started,
            model: params.model,
            proxy_url: None,
            proxy_status: None,
        };

        let mut upstream_request =
            UpstreamRequest::post_json(params.url.to_string(), params.body_bytes.clone());
        match self
            .resolve_and_assign_proxy(&params.req, params.target)
            .await
        {
            Ok((proxy_url, proxy_status)) => {
                upstream_request.proxy = proxy_url.clone();
                upstream_request.proxy_status = proxy_status.clone();
                dctx.proxy_url = proxy_url;
                dctx.proxy_status = proxy_status;
                Ok((dctx, upstream_request))
            }
            Err(e) => {
                let fail_result = self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(&e, None, None, e.http_status()),
                );
                Err(Box::new(fail_result))
            }
        }
    }

    async fn dispatch_by_stream_mode(
        &self,
        params: DispatchParams<'_>,
        dctx: DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        if params.req.stream_sink.is_some() {
            self.dispatch_upstream_streaming(StreamDispatchParams {
                target: params.target,
                combo: params.combo,
                req: params.req,
                model: params.model,
                target_format: params.target_format,
                resolved_timeouts: params.resolved_timeouts,
                started: params.started,
                attempt: params.attempt,
                race_size: params.race_size,
                trace_id: params.trace_id,
                upstream_request,
            })
            .await
        } else {
            self.dispatch_upstream_non_streaming(params, dctx, upstream_request)
                .await
        }
    }

    pub(crate) async fn dispatch_upstream(&self, params: DispatchParams<'_>) -> PipelineResult {
        let (dctx, mut upstream_request) =
            match self.setup_upstream_request_and_context(&params).await {
                Ok(res) => res,
                Err(err_res) => return *err_res,
            };

        if is_horde_vision_request(params.target, params.model, &params.req) {
            return self
                .dispatch_horde_vision(params, dctx, upstream_request)
                .await;
        }

        upstream_request.is_streaming = true;
        populate_upstream_headers(&mut upstream_request, params.headers);

        self.dispatch_by_stream_mode(params, dctx, upstream_request)
            .await
    }

    fn fail_stream_with_error(
        &self,
        err: CoreError,
        mut fctx: StreamFailureContext<'_>,
        status_override: Option<u16>,
    ) -> PipelineResult {
        let dctx = DispatchContext {
            attempt: fctx.attempt,
            race_size: fctx.race_size,
            started: fctx.started,
            model: fctx.model,
            proxy_url: fctx.proxy_url,
            proxy_status: fctx.proxy_status,
        };
        if let Some(a) = fctx.acc.as_deref_mut() {
            a.mark_partial();
        }
        let status = status_override.unwrap_or_else(|| err.http_status());
        let fail_ctx = dctx.fail_ctx_code(&err, Some(fctx.connect_ms), None, status);
        self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
            req: fctx.req,
            combo: fctx.combo,
            target: fctx.target,
            ctx: fail_ctx,
            trace_id: fctx.trace_id,
            acc: fctx.acc.as_deref(),
            chunk_id: Some(fctx.chunk_id),
            created: fctx.created,
            model_name: fctx.model_name,
        })
    }

    pub(crate) fn fail_stream_client_disconnected(
        &self,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        let inline_err = fctx
            .acc
            .as_ref()
            .and_then(|a| a.extract_upstream_error_from_raw());
        if let Some((code, message)) = inline_err {
            tracing::warn!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                provider = %fctx.target.provider_id,
                model = %fctx.model.model_id.as_str(),
                inline_error_code = code,
                inline_error_message = %message,
                "client disconnected but upstream had sent inline SSE error (code={code}); attributing to upstream error",
            );
            let err = CoreError::upstream_error(
                code,
                fctx.target.provider_id.to_string(),
                fctx.model_name,
                message,
                false,
            );
            return self.fail_stream_with_error(err, fctx, Some(code));
        }

        let has_partial_content = fctx.acc.as_deref().is_some_and(|a| !a.is_empty());
        let err = if has_partial_content {
            CoreError::UpstreamConnection(
                "stream interrupted — client disconnected after receiving partial content".into(),
            )
        } else {
            CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
        };
        self.fail_stream_with_error(err, fctx, Some(499))
    }

    pub(crate) fn fail_on_sink_send_error(
        &self,
        e: crate::race_sink::StreamSinkError,
        fctx: StreamFailureContext<'_>,
    ) -> PipelineResult {
        if matches!(e, crate::race_sink::StreamSinkError::Lost) {
            tracing::debug!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                "sink send failed: Lost (another race lane won)"
            );
            return self.fail_stream_with_error(CoreError::RaceLost, fctx, None);
        }

        let elapsed = fctx.started.elapsed().as_millis() as u64;
        let inline_err = fctx
            .acc
            .as_ref()
            .and_then(|a| a.extract_upstream_error_from_raw());
        if let Some((code, message)) = inline_err {
            tracing::warn!(
                combo_id = fctx.combo.id.0,
                target_id = fctx.target.id.0,
                provider = %fctx.target.provider_id,
                model = %fctx.model.model_id.as_str(),
                elapsed_ms = elapsed,
                inline_error_code = code,
                inline_error_message = %message,
                "sink closed after upstream sent inline SSE error (code={code}, elapsed={elapsed}ms)",
            );
            let err = CoreError::upstream_error(
                code,
                fctx.target.provider_id.to_string(),
                fctx.model_name,
                message,
                false,
            );
            return self.fail_stream_with_error(err, fctx, Some(code));
        }

        let is_watchdog_fired = fctx.req.client_disconnected.borrow().is_some();
        tracing::warn!(
            combo_id = fctx.combo.id.0,
            target_id = fctx.target.id.0,
            provider = %fctx.target.provider_id,
            model = %fctx.model.model_id.as_str(),
            elapsed_ms = elapsed,
            connect_ms = fctx.connect_ms,
            ttft_ms = ?fctx.ttft_ms,
            watchdog_fired = is_watchdog_fired,
            "sink send failed: Closed — client/proxy disconnected"
        );
        let err = CoreError::UpstreamConnection(format!(
            "client disconnected (elapsed={elapsed}ms, connect={}ms, ttft={:?}) — likely proxy idle timeout or client HTTP library timeout",
            fctx.connect_ms, fctx.ttft_ms
        ));
        self.fail_stream_with_error(err, fctx, None)
    }

    fn check_preflight_stream_disconnect(
        &self,
        req: &PipelineRequest,
        combo: &Combo,
        target: &ComboTarget,
        dctx: &DispatchContext<'_>,
        send_start: Instant,
    ) -> Option<PipelineResult> {
        let client_disconnected = *req.client_disconnected.borrow();
        if let Some(reason) = client_disconnected {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream streaming send; aborting attempt"
            );
            return Some(self.record_and_fail(
                req.clone(),
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            ));
        }
        None
    }

    async fn handle_streaming_non_2xx(
        &self,
        args: StreamingNon2xxArgs<'_>,
        dctx: &DispatchContext<'_>,
    ) -> PipelineResult {
        let retry_after_header = args
            .response
            .headers
            .get("retry-after")
            .or_else(|| args.response.headers.get("Retry-After"))
            .and_then(|v| v.to_str().ok());
        let body_str = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            args.response.body.collect_all(),
        )
        .await
        {
            Ok(Ok(b)) => String::from_utf8_lossy(&b).to_string(),
            _ => String::new(),
        };
        self.handle_non_2xx_response(
            args.status_code,
            retry_after_header,
            body_str,
            args.req,
            args.combo,
            args.target,
            args.model,
            dctx,
            args.connect_and_send_ms,
            None,
        )
        .await
    }

    fn record_streaming_success(
        &self,
        params: StreamDispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        args: StreamingSuccessArgs<'_>,
    ) -> PipelineResult {
        let usage = args.state.usage;
        let acc = args.state.acc;
        let ttft_ms = args.state.ttft_ms;
        let stop_reason = args.state.stop_reason;
        let done_sent = args.state.done_sent;
        let total_ms = params.started.elapsed().as_millis() as u64;

        let prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = usage.as_ref().map(|u| u.completion_tokens);
        let cached_tokens = usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);

        let response_body_json: Option<serde_json::Value> = acc
            .as_ref()
            .map(|a| a.finish(args.chunk_id, args.created, args.model_name));
        let final_response = if matches!(
            params.req.stream_sink.as_ref(),
            Some(crate::race_sink::StreamSink::Discard)
        ) {
            response_body_json
                .as_ref()
                .and_then(|v| serde::Deserialize::deserialize(v).ok())
        } else {
            None
        };

        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            params.req,
            params.combo,
            params.target,
        )
        .proxy_url(dctx.proxy_url.clone())
        .proxy_status(dctx.proxy_status.clone())
        .model_opt(Some(params.model))
        .err_opt(None)
        .connect_ms_opt(Some(args.connect_and_send_ms))
        .ttft_ms_opt(ttft_ms)
        .total_ms(total_ms)
        .status_code(args.status_code)
        .attempt(params.attempt)
        .race_size(params.race_size)
        .trace_id(params.trace_id)
        .prompt_tokens_opt(prompt_tokens)
        .completion_tokens_opt(completion_tokens)
        .cached_tokens(cached_tokens)
        .response_body_json(response_body_json)
        .request_headers(None)
        .response_headers(None)
        .is_streaming(true)
        .stream_complete(done_sent)
        .stop_reason(stop_reason)
        .record()
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code: args.status_code,
            error: None,
            final_response,
            attempts: params.attempt,
            usage_tuple,
        }
    }

    pub(crate) async fn dispatch_upstream_streaming(
        &self,
        params: StreamDispatchParams<'_>,
    ) -> PipelineResult {
        let StreamDispatchParams {
            target,
            combo,
            req,
            model,
            target_format,
            resolved_timeouts,
            started,
            attempt,
            race_size,
            trace_id,
            upstream_request,
        } = params;
        let dctx = DispatchContext {
            attempt,
            race_size,
            started,
            model,
            proxy_url: upstream_request.proxy.clone(),
            proxy_status: upstream_request.proxy_status.clone(),
        };

        let Some(sink) = req.stream_sink.as_ref() else {
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(
                    &CoreError::Internal(
                        "dispatch_upstream_streaming called without stream_sink".into(),
                    ),
                    None,
                    None,
                    500,
                ),
            );
        };

        let send_start = Instant::now();
        if let Some(fail_res) =
            self.check_preflight_stream_disconnect(&req, combo, target, &dctx, send_start)
        {
            return fail_res;
        }

        let cancel_token = if let Some(rc) = req.race_cancel.as_ref() {
            CancellationToken::from_watch_and_token(
                tokio::sync::watch::Receiver::clone(&req.client_disconnected),
                rc,
            )
        } else {
            CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
                &req.client_disconnected,
            ))
        };
        let req_proxy_url = upstream_request.proxy.clone();
        let req_proxy_status = upstream_request.proxy_status.clone();
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                openproxy_adapters::upstream::TimeoutProfile::Custom(
                    resolved_timeouts.as_resolved(),
                ),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        let response = match result {
            Ok(r) => r,
            Err(err) => {
                return self
                    .handle_upstream_error(err, req, combo, target, &dctx, connect_and_send_ms)
                    .await;
            }
        };

        let status_code = response.status.as_u16();
        if !(200..300).contains(&status_code) {
            return self
                .handle_streaming_non_2xx(
                    StreamingNon2xxArgs {
                        response,
                        status_code,
                        req,
                        combo,
                        target,
                        model,
                        connect_and_send_ms,
                    },
                    &dctx,
                )
                .await;
        }

        let mut chunk_id = String::with_capacity(48);
        use std::fmt::Write;
        let _ = write!(&mut chunk_id, "chatcmpl-{}", uuid::Uuid::new_v4());

        let created = chrono::Utc::now().timestamp() as u64;
        let model_name = model.model_id.as_str().to_string();

        openproxy_types::emit_stage_event!(
            request_id: req.request_id,
            trace_id: trace_id,
            stage: "waiting_ttft",
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: connect_and_send_ms,
            status_code: status_code,
        );

        let mut stream = response.body;
        let mut state = crate::streaming_state::StreamingState::new(true);

        let ctx = crate::streaming_state::StreamContext {
            req: &req,
            combo,
            target,
            model,
            target_format,
            sink,
            trace_id: &trace_id,
            chunk_id: &chunk_id,
            model_name: &model_name,
            started,
            attempt,
            race_size,
            created,
            connect_and_send_ms,
            resolved_timeouts,
            proxy_url: dctx.proxy_url.clone(),
            proxy_status: dctx.proxy_status.clone(),
        };

        match state.run_stream_loop(&ctx, self, &mut stream).await {
            Ok(crate::streaming_state::ChunkResult::Return(r)) => return *r,
            Ok(crate::streaming_state::ChunkResult::Break) => {}
            Err(e) => {
                return self.record_and_fail_with_trace_id(
                    req.clone(),
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, Some(connect_and_send_ms), state.ttft_ms, 502),
                    trace_id.clone(),
                );
            }
        }

        let client_disconnected = if state.done_sent {
            None
        } else {
            let mut rx = tokio::sync::watch::Receiver::clone(&req.client_disconnected);
            Self::is_client_disconnected(&mut rx)
        };

        if let Some(_reason) = client_disconnected {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.id.0,
                provider = %target.provider_id,
                "client cancelled during SSE stream; aborting attempt"
            );
            return self.fail_stream_client_disconnected(StreamFailureContext {
                proxy_url: req_proxy_url.clone(),
                proxy_status: req_proxy_status.clone(),
                req: req.clone(),
                combo,
                target,
                attempt,
                race_size,
                started,
                model,
                connect_ms: connect_and_send_ms,
                ttft_ms: state.ttft_ms,
                trace_id: trace_id.clone(),
                acc: state.acc.as_mut(),
                chunk_id: &chunk_id,
                created,
                model_name: &model_name,
            });
        }

        let is_empty_stream = state
            .acc
            .as_ref()
            .is_some_and(super::sse_accumulator::ResponseAccumulator::is_empty)
            && state.stop_reason.is_none();
        if is_empty_stream {
            let err = CoreError::UpstreamConnection(
                "streaming response was empty (no content, no reasoning, no tool_calls) — treating as error for retry".to_string(),
            );
            let mut acc = state.acc;
            if let Some(a) = acc.as_mut() {
                a.mark_partial();
            }
            return self.record_and_fail_with_trace_id_and_partial(crate::PartialFailureParams {
                req,
                combo,
                target,
                ctx: dctx.fail_ctx_code(&err, Some(connect_and_send_ms), None, 502),
                trace_id,
                acc: acc.as_ref(),
                chunk_id: Some(&chunk_id),
                created,
                model_name: &model_name,
            });
        }

        self.record_streaming_success(
            StreamDispatchParams {
                target,
                combo,
                req,
                model,
                target_format,
                resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
                upstream_request: UpstreamRequest::post_json(String::new(), bytes::Bytes::new()),
            },
            &dctx,
            StreamingSuccessArgs {
                state,
                chunk_id: &chunk_id,
                created,
                model_name: &model_name,
                connect_and_send_ms,
                status_code,
            },
        )
    }
}

// ==========
// Audit fix #1 regression test: handle_non_2xx_response must propagate
// is_hard_skip=true for request-shaped errors so the circuit breaker
// doesn't penalize the account. See
// `docs/specs/adversarial-findings.md` BUG findings GAP-4 wiring.
// ==========
#[cfg(test)]
mod wiring_tests {
    use super::*;
    use openproxy_adapters::UpstreamClient;
    use openproxy_db::MasterKey;
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::providers::RateLimitScope;
    use std::sync::atomic::AtomicU64;

    /// Build a minimal in-memory-ish DB+pool pair compatible with
    /// `UpstreamDispatcher::new`. Mirrors the helper that previously
    /// lived in `tests.rs` (removed because that file was never
    /// included as a module from `lib.rs`).
    fn fresh_pool() -> (
        openproxy_db::DbPool,
        std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
        std::path::PathBuf,
    ) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "openproxy-wiring-test-{pid}-{nanos}-{n}"
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("wiring.db");
        let pool = openproxy_db::DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        let extra = pool.open_connection().expect("open extra connection");
        let conn = std::sync::Arc::new(parking_lot::Mutex::new(extra));
        (pool, conn, path)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_non_2xx_response_wires_is_hard_skip_for_validation_required() {
        let (_pool, conn_arc, _path) = fresh_pool();

        // Seed a provider so the proxy-rotation branch (which is
        // a no-op when `use_proxies=0` — the SQL default) succeeds.
        let provider_id = "wired-test";
        let pid = openproxy_types::ids::ProviderId::new(provider_id);
        {
            let conn = conn_arc.lock();
            openproxy_db::providers::create(
                &conn,
                openproxy_db::providers::NewProvider {
                    id: &pid,
                    name: provider_id,
                    base_url: "https://example.com",
                    auth_type: openproxy_types::providers::AuthType::Bearer,
                    format: openproxy_types::providers::ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                    rate_limit_scope: RateLimitScope::Account,
                },
            )
            .expect("seed provider");
        }

        let model = openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: pid.clone(),
            model_id: openproxy_types::ids::ModelId::new("g-2.5"),
            display_name: None,
            discovered_at: "2024-01-01".into(),
            expires_at: None,
            timeout_overrides_json: None,
            last_test_at: None,
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: "test".into(),
            input_modalities_json: None,
            output_modalities_json: None,
            last_test_status: None,
            target_format: openproxy_types::TargetFormat::Openai,
            active: true,
            custom: false,
        };

        let target = ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: pid.clone(),
            // account_id = None so the 401/403 broadcast branch is skipped.
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            rate_limit_scope: RateLimitScope::Account,
        };

        let combo = Combo {
            id: openproxy_types::ids::ComboId(1),
            name: "wired".into(),
            strategy: Strategy::Priority,
            priority_mode: PriorityMode::Strict,
            race_size: 1,
            created_at: "2024-01-01".into(),
            context_window: None,
            cooldown_mode: openproxy_types::config::CooldownMode::None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: None,
            selection_window_secs: Some(3600),
            preventive_rate_limit: false,
        };

        // Build the dispatcher.
        let repo = std::sync::Arc::new(crate::repository::SqlitePipelineRepository::new(
            std::sync::Arc::clone(&conn_arc),
        ));
        let tracker = crate::usage_tracker::UsageTracker {
            conn: std::sync::Arc::clone(&conn_arc),
            background_tx: tokio::sync::mpsc::channel(1).0,
            record_bodies_and_headers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            compression_stats_cell: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            selection_registry: std::sync::Arc::new(openproxy_types::SelectionRegistry::new()),
            cooldown_secs: 60,
            cooldown_max_secs: 3600,
            cooldown_factor: 2,
            repo: std::sync::Arc::clone(&repo) as std::sync::Arc<dyn crate::repository::PipelineRepository>,
        };
        let cfg = crate::PipelineConfig {
            defaults: crate::timeouts::Timeouts::from_config(
                &openproxy_types::config::TimeoutsConfig::default(),
            ),
            racing: openproxy_types::config::RacingConfig::default(),
            retries: openproxy_types::config::RetriesConfig::default(),
            max_attempts: 1,
            master_key: std::sync::Arc::new(MasterKey::generate()),
            adapters: std::sync::Arc::new(Vec::new()),
            cooldown_secs: 60,
            cooldown_max_secs: 3600,
            cooldown_factor: 2,
            upstream_client: UpstreamClient::new(),
            oauth_provider_registry: None,
            compression_mode: openproxy_compression::CompressionMode::Off,
            idle_chunk_retryable: true,
            quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
            background_tx: tokio::sync::mpsc::channel(1).0,
        };
        let dispatcher = UpstreamDispatcher::new(
            std::sync::Arc::clone(&conn_arc),
            cfg,
            tracker,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        let started = std::time::Instant::now();
        let dctx = DispatchContext {
            attempt: 1,
            race_size: 1,
            started,
            model: &model,
            proxy_url: None,
            proxy_status: None,
        };

        // Build a PipelineRequest.
        let (_tx, rx) = tokio::sync::watch::channel::<Option<openproxy_types::CancelReason>>(None);
        let req = PipelineRequest {
            request_id: openproxy_types::ids::RequestId::new(),
            trace_id: openproxy_types::ids::TraceId::new(),
            combo_id: openproxy_types::ids::ComboId(1),
            openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
                model: "g-2.5".into(),
                messages: vec![openproxy_types::OpenAIMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String("hi".into())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                }],
                stream: false,
                temperature: None,
                max_tokens: None,
                top_p: None,
                stop: None,
                tools: None,
                tool_choice: None,
                top_k: None,
                user: None,
                extra: serde_json::Map::new(),
            }),
            client_disconnected: rx,
            stream_sink: None,
            api_key_id: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
            request_body_json: None,
            race_cancelled: false,
            race_cancel: None,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
            proxy_override: None,
        };

        // 403 + VALIDATION_REQUIRED body → must produce is_hard_skip=true.
        let result = dispatcher
            .handle_non_2xx_response(
                403,
                None,
                r#"{"error":{"code":"VALIDATION_REQUIRED"}}"#.to_string(),
                req,
                &combo,
                &target,
                &model,
                &dctx,
                42,
                Some(10),
            )
            .await;

        let err = result
            .error
            .expect("non-2xx response must produce an error");
        assert!(
            err.is_hard_skip(),
            "audit #1: 403 VALIDATION_REQUIRED must yield is_hard_skip=true, got error={err:?}"
        );
        assert_eq!(
            err.upstream_error_class(),
            Some(openproxy_types::UpstreamErrorClass::ValidationRequired)
        );
    }
}
