//! Orquestador del dispatcher upstream. Compone los submódulos por
//! responsabilidad: `rotation`, `proxy`, `fail`, `stream`, `unary`, `horde`.
//! Esta fachada es la única ruta de entrada para `dispatch_upstream`,
//! usado por `stages/target.rs`.

use crate::PipelineResult;
use openproxy_adapters::upstream::UpstreamRequest;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub mod types;
pub(super) mod rotation;
pub(super) mod proxy;
pub(super) mod fail;
pub(super) mod stream;
pub(super) mod unary;
pub(super) mod horde;
#[cfg(test)]
mod tests;

pub(crate) use rotation::ProxyRotationTrigger;
pub(crate) use types::{
    DispatchContext, DispatchParams, NonStreamingSuccessArgs, StreamDispatchParams,
    StreamFailureContext, StreamingNon2xxArgs, StreamingSuccessArgs,
};

#[derive(Clone)]
pub struct UpstreamDispatcher {
    pub(crate) conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    pub(crate) config: crate::PipelineConfig,
    pub(crate) tracker: crate::usage_tracker::UsageTracker,
    pub(crate) record_bodies_and_headers: Arc<std::sync::atomic::AtomicBool>,
}

pub trait Dispatcher: Send + Sync {
    fn is_recording(&self) -> bool;
}

impl Dispatcher for UpstreamDispatcher {
    fn is_recording(&self) -> bool {
        self.record_bodies_and_headers()
    }
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
        self.record_bodies_and_headers.load(Ordering::Relaxed)
    }

    /// Construye el `UpstreamRequest` (URL, body, proxy asignado) y el
    /// `DispatchContext` asociado. Si la resolución de proxy falla,
    /// devuelve un `PipelineResult` de fallo en `Box`.
    async fn setup_upstream_request_and_context<'a>(
        &self,
        params: &types::DispatchParams<'a>,
    ) -> Result<(types::DispatchContext<'a>, UpstreamRequest), Box<PipelineResult>> {
        let mut dctx = types::DispatchContext {
            attempt: params.attempt,
            race_size: params.race_size,
            started: params.started,
            model: params.model,
            proxy_url: None,
            proxy_status: None,
        };

        let mut upstream_request =
            UpstreamRequest::post_json(params.url.to_string(), params.body_bytes.clone());
        match self.resolve_and_assign_proxy(&params.req, params.target).await {
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

    /// Bifurca entre streaming (si hay `stream_sink`) y non-streaming.
    async fn dispatch_by_stream_mode(
        &self,
        params: types::DispatchParams<'_>,
        dctx: types::DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        if params.req.stream_sink.is_some() {
            self.dispatch_upstream_streaming(types::StreamDispatchParams {
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

    /// Entry point público. Pasos:
    /// 1. `setup_upstream_request_and_context` (proxy + body).
    /// 2. Si `is_horde_vision_request` → `dispatch_horde_vision`.
    /// 3. Popula headers + `is_streaming=true`.
    /// 4. `dispatch_by_stream_mode`.
    pub(crate) async fn dispatch_upstream(
        &self,
        params: types::DispatchParams<'_>,
    ) -> PipelineResult {
        let (dctx, mut upstream_request) =
            match self.setup_upstream_request_and_context(&params).await {
                Ok(res) => res,
                Err(err_res) => return *err_res,
            };

        if horde::is_horde_vision_request(params.target, params.model, &params.req) {
            return self
                .dispatch_horde_vision(params, dctx, upstream_request)
                .await;
        }

        upstream_request.is_streaming = true;
        unary::populate_upstream_headers(&mut upstream_request, params.headers);

        self.dispatch_by_stream_mode(params, dctx, upstream_request)
            .await
    }
}