//! Manejo de fallos HTTP: clasificación de `UpstreamError` → `CoreError`,
//! respuesta a non-2xx, marcado de cuenta inválida y live-limited.
//! Concentra las llamadas a `self.record_and_fail*` (los 3 variantes:
//! sin trace_id, con trace_id, con partial-params).

use super::types::DispatchContext;
use super::UpstreamDispatcher;
use crate::{parse_retry_after_ms, FailureContext, PipelineRequest, PipelineResult};
use openproxy_adapters::upstream::UpstreamError;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::CancelReason;
use openproxy_types::Model;
use std::sync::Arc;
use tokio::sync::watch;

/// Lee el último valor publicado por el watchdog del cliente (sin avanzar
/// la versión). Devuelve `None` si no hay cancelación pendiente.
pub(super) fn is_client_disconnected(
    rx: &mut watch::Receiver<Option<CancelReason>>,
) -> Option<CancelReason> {
    *rx.borrow_and_update()
}

impl UpstreamDispatcher {
    /// Entrada principal para registrar un fallo. Construye el `trace_id`
    /// añadiendo `:retry{N}` si `attempt > 1`, y delega a la versión con
    /// `trace_id`.
    pub(super) fn record_and_fail(
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

    /// Variante con `trace_id` ya calculado. Pasa-through al partial.
    pub(super) fn record_and_fail_with_trace_id(
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

    /// Variante partial: reenvía los params al `UsageTracker`.
    ///
    /// Visibilidad `pub(crate)`: invocado por `streaming_state.rs`
    /// (cross-module).
    pub(crate) fn record_and_fail_with_trace_id_and_partial(
        &self,
        params: crate::PartialFailureParams<'_>,
    ) -> PipelineResult {
        self.tracker
            .record_and_fail_with_trace_id_and_partial(params)
    }

    /// Mapea `UpstreamError` → `CoreError` y dispara rotación de proxy si
    /// aplica. Diferencia `Cancel` (cliente canceló) del resto.
    pub(super) async fn handle_upstream_error(
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
            let core_err = CoreError::Cancelled(CancelReason::ClientDisconnected);
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
                super::rotation::ProxyRotationTrigger::ConnectError,
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

    /// Publica una notificación `account_invalid` en el bus, deduplicada
    /// por `account_invalid:{aid}`. Fire-and-forget: el `JoinHandle` se
    /// descarta explícitamente para no bloquear el dispatch path.
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

    /// Maneja respuestas non-2xx: dispara rotación de proxy (por status o
    /// rate-limit), broadcast de cuenta inválida si 401/403, marcado
    /// `live_limited` si `RESOURCE_EXHAUSTED` en body, y clasificación
    /// final con `is_hard_skip` para que el circuit breaker no penalice
    /// errores de forma del request.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_non_2xx_response(
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
            super::rotation::ProxyRotationTrigger::RateLimited
        } else {
            super::rotation::ProxyRotationTrigger::Status(status_code)
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
            // existing pattern in `check_and_trigger_proxy_rotation`.
            if status_code == 429
                && body_str.contains("RESOURCE_EXHAUSTED")
                && let Some(aid) = target.account_id
            {
                let model_id = dctx.model.model_id.clone();
                let conn_clone = Arc::clone(&self.conn);
                let handle = tokio::task::spawn_blocking(move || {
                    let conn = conn_clone.lock();
                    let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
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
                // Fire-and-forget; drop explícito (AGENTS.md §3.3
                // fire-and-forget pattern + clippy::let_underscore_future).
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