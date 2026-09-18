mod payloads;
pub(crate) use payloads::*;

use super::{AccountId, ApiError, AppState, CoreError, Deserialize, ModelRowId, core_models};
use axum::{
    Json,
    extract::{Path, State},
};

use super::models::resolve_adapter;

/// `POST /admin/models/:id/test` input
#[derive(Debug, Default, Deserialize)]
pub struct TestModelInput {
    pub account_id: Option<i64>,
    pub proxy_id: Option<String>,
}

/// Maximum number of characters from a failing response body that we
/// surface back to the dashboard.
pub const TEST_ERROR_BODY_MAX_CHARS: usize = 512;

/// The outcome of a single test ping.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub row_id: i64,
    pub status: u16,
    pub elapsed_ms: u64,
    pub error_msg: Option<String>,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

impl TestResult {
    pub fn skipped(row_id: i64, reason: &str) -> Self {
        Self {
            row_id,
            status: 0,
            elapsed_ms: 0,
            error_msg: Some(reason.to_string()),
            skipped: true,
            skip_reason: Some(reason.to_string()),
        }
    }
}

/// Knobs that distinguish the per-row test path from the
/// per-combo fan-out.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestOptions {
    pub in_combo_fanout: bool,
}

pub async fn resolve_proxy_url_by_id(s: &AppState, pid: &str) -> Option<String> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let pid = pid.to_string();
    tokio::task::spawn_blocking(move || {
        let r = pool.try_reader_for(std::time::Duration::from_secs(5))?;
        let p = openproxy_core::free_proxies::get_proxy(&r, &pid)
            .ok()
            .flatten()?;
        Some(format!(
            "{}://{}:{}",
            p.r#type.to_lowercase(),
            p.host,
            p.port
        ))
    })
    .await
    .ok()
    .flatten()
}

pub async fn parse_test_model_params(
    s: &AppState,
    body_bytes: &[u8],
) -> Result<(Option<AccountId>, Option<String>), ApiError> {
    if body_bytes.is_empty() {
        return Ok((None, None));
    }
    let input = serde_json::from_slice::<TestModelInput>(body_bytes)
        .map_err(|e| ApiError(CoreError::Parse(format!("Invalid JSON: {e}"))))?;
    let aid = input.account_id.map(AccountId::new);
    let purl = match input.proxy_id.as_deref() {
        Some(pid) => resolve_proxy_url_by_id(s, pid).await,
        None => None,
    };
    Ok((aid, purl))
}

pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    body_bytes: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);
    let (account_id, proxy_url) = parse_test_model_params(&s, &body_bytes).await?;

    let (r, debug_payload) = run_test_for_model(
        &s,
        model_row_id,
        account_id,
        proxy_url,
        TestOptions::default(),
        cancel_rx,
    )
    .await;
    Ok(Json(serde_json::json!({
        "row_id": r.row_id,
        "status": r.status,
        "elapsed_ms": r.elapsed_ms,
        "error_msg": r.error_msg,
        "debug_payload": debug_payload,
    })))
}

pub async fn run_test_for_model(
    s: &AppState,
    model_row_id: i64,
    account_id: Option<AccountId>,
    proxy_url: Option<String>,
    opts: TestOptions,
    cancel_rx: Option<tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>>,
) -> (TestResult, Option<serde_json::Value>) {
    let row_id = ModelRowId(model_row_id);
    let start = std::time::Instant::now();

    // 1. Load model row
    let model = match load_model_for_test(s, model_row_id) {
        Ok(m) => m,
        Err(err_res) => return (err_res, None),
    };

    if !model.active && opts.in_combo_fanout {
        return (TestResult::skipped(model_row_id, "model is inactive"), None);
    }

    // 2. Resolve adapter
    let adapter = match resolve_adapter(s, &model.provider_id, s.adapters().as_slice()) {
        Ok(a) => a,
        Err(err) => {
            return (
                test_error_result(model_row_id, err.http_status(), &err.to_string()),
                None,
            );
        }
    };

    // 3. Resolve account & credentials
    let (_account_id_opt, account_label, api_key, raw_account_opt) =
        match resolve_test_credentials(s, &model, model_row_id, account_id, start).await {
            Ok(creds) => creds,
            Err(err_res) => return (err_res, None),
        };

    // 4. Build request
    let openai_req = build_test_openai_request(model.model_id.as_str());
    let effective_target_format =
        resolve_effective_target_format(adapter.format(), model.target_format);
    let inferred_type = openproxy_types::capabilities::infer_model_type(model.model_id.as_str());
    let effective_type = openproxy_types::capabilities::resolve_effective_model_type(
        &model.model_type,
        model.custom,
        inferred_type,
    );
    let is_audio = effective_type == "audio";
    let is_stt = is_audio
        && (openproxy_types::capabilities::is_stt_model(model.model_id.as_str())
            || !model.model_id.as_str().contains("tts"));
    let is_tts = is_audio && !is_stt;
    let is_embedding = effective_type == "embedding";
    let is_image = effective_type == "image";

    let (status, error_msg, elapsed_ms, debug_payload) = if is_stt
        || is_embedding
        || is_image
        || is_tts
    {
        let (url, body_value, multipart_opt): (
            String,
            serde_json::Value,
            Option<(String, bytes::Bytes)>,
        ) = if is_stt {
            build_stt_test_payload(&adapter, &model)
        } else {
            let (u, v) = build_audio_or_specialized_payload(
                &adapter,
                &model,
                is_embedding,
                is_image,
                is_tts,
            );
            (u, v, None)
        };

        let custom_meta = build_custom_provider_meta(
            model.provider_id.as_str(),
            raw_account_opt.as_ref(),
            &api_key,
        );
        let headers = adapter.build_headers(&api_key, effective_target_format, &model.model_id);

        let dummy_target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ids::ComboTargetId(0),
                combo_id: openproxy_types::ids::ComboId(0),
                provider_id: openproxy_types::ids::ProviderId::new(model.provider_id.as_str()),
                account_id: _account_id_opt,
                model_row_id: Some(model.row_id),
                sub_combo_id: None,
                priority_order: 0,
                weight: 1,
                active: true,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                thinking_effort: None,
            },
            model: model.clone(),
            api_key: api_key.clone(),
            api_key_label: Some(account_label.clone()),
            custom_meta,
        };

        let mut req = if let Some((content_type, body_bytes)) = multipart_opt {
            openproxy_adapters::upstream::UpstreamRequest::post_multipart(
                &url,
                &content_type,
                body_bytes,
            )
        } else {
            let wrapped_res = serde_json::to_vec(&body_value)
                .map_err(|e| format!("failed to serialize request: {e}"))
                .and_then(|b| {
                    adapter
                        .wrap_request_body(
                            bytes::Bytes::from(b),
                            effective_target_format,
                            &dummy_target.model.model_id,
                            &dummy_target,
                        )
                        .map_err(|e| format!("failed to wrap request: {e}"))
                });

            match wrapped_res {
                Ok(wrapped) => {
                    openproxy_adapters::upstream::UpstreamRequest::post_json(&url, wrapped)
                }
                Err(err_msg) => {
                    return (test_error_result(model_row_id, 500, &err_msg), None);
                }
            }
        };
        let effective_proxy = if let Some(ref purl) = proxy_url {
            Some(purl.clone())
        } else {
            let pool = std::sync::Arc::clone(s.db_pool());
            let pid = model.provider_id.clone();
            let aid = _account_id_opt;
            tokio::task::spawn_blocking(move || {
                let r = pool.try_reader_for(std::time::Duration::from_secs(5))?;
                openproxy_core::free_proxies::get_or_assign_provider_proxy(&r, &pid, aid.as_ref())
                    .ok()
                    .flatten()
            })
            .await
            .ok()
            .flatten()
        };
        req.proxy = effective_proxy;
        for (k, v) in &headers {
            if is_stt && k.eq_ignore_ascii_case("content-type") {
                continue;
            }
            if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
                && let Ok(hv) = axum::http::HeaderValue::from_str(v)
            {
                req.headers.insert(hn, hv);
            }
        }

        let request_headers_map = if opts.in_combo_fanout {
            None
        } else {
            Some(
                req.headers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        };

        let cancel = openproxy_adapters::upstream::CancellationToken::new();
        if let Some(mut rx) = cancel_rx {
            let rx_cancel = openproxy_adapters::upstream::CancellationToken::clone(&cancel);
            tokio::spawn(async move {
                if rx.borrow().is_some() {
                    rx_cancel.cancel();
                    return;
                }
                while rx.changed().await.is_ok() {
                    if rx.borrow().is_some() {
                        rx_cancel.cancel();
                        return;
                    }
                }
            });
        }

        let profile = openproxy_adapters::upstream::TimeoutProfile::Custom(
            openproxy_adapters::upstream::ResolvedTimeouts {
                dns_ms: 2000,
                dial_ms: 5000,
                tls_ms: 5000,
                write_ms: 5000,
                headers_ms: 15000,
                body_chunk_ms: 5000,
                total_ms: 15000,
            },
        );

        let start_req = std::time::Instant::now();
        let result = s.upstream_client().call(req, profile, cancel).await;
        let elapsed_ms = start_req.elapsed().as_millis() as u64;

        let mut debug_payload = request_headers_map.map(|req_headers| {
            serde_json::json!({
                "request_headers": req_headers,
                "request_url": url,
                "request_body": body_value,
            })
        });

        let (status, error_msg) = match result {
            Ok(response) => {
                let status = response.status.as_u16();
                if status >= 400 {
                    let body = response.collect().await.unwrap_or_default();
                    let text = String::from_utf8_lossy(&body);
                    if let Some(dp) = debug_payload.as_mut() {
                        dp["response_body"] = serde_json::from_str(&text)
                            .unwrap_or_else(|_| serde_json::json!(text.to_string()));
                    }
                    let truncated: String = text.chars().take(TEST_ERROR_BODY_MAX_CHARS).collect();
                    (status, Some(truncated))
                } else {
                    (status, None)
                }
            }
            Err(e) => (0, Some(format!("{e:?}"))),
        };

        (status, error_msg, elapsed_ms, debug_payload)
    } else {
        let custom_meta = build_custom_provider_meta(
            model.provider_id.as_str(),
            raw_account_opt.as_ref(),
            &api_key,
        );

        let dummy_target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ids::ComboTargetId(0),
                combo_id: openproxy_types::ids::ComboId(0),
                provider_id: openproxy_types::ids::ProviderId::new(model.provider_id.as_str()),
                account_id: _account_id_opt,
                model_row_id: Some(model.row_id),
                sub_combo_id: None,
                priority_order: 0,
                weight: 1,
                active: true,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                thinking_effort: None,
            },
            model: model.clone(),
            api_key: api_key.clone(),
            api_key_label: Some(account_label.clone()),
            custom_meta,
        };

        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("user-agent".to_string(), "openproxy-tester".to_string());

        let (_dummy_tx, dummy_rx) = tokio::sync::watch::channel(None);
        let client_disconnected = cancel_rx.unwrap_or(dummy_rx);

        let pipeline_req = openproxy_pipeline::PipelineRequest {
            request_id: openproxy_types::ids::RequestId::new(),
            trace_id: openproxy_types::ids::TraceId::new(),
            combo_id: openproxy_types::ids::ComboId(0),
            openai_request: std::sync::Arc::new(openai_req.clone()),
            client_disconnected,
            stream_sink: None,
            api_key_id: None,
            race_cancel: None,
            combo_override: None,
            targets_override: None,
            request_headers: req_headers.clone(),
            request_body_json: None,
            race_cancelled: false,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
            pii_session: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            compression_stats: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            proxy_override: proxy_url.map(|purl| ("manual".to_string(), purl)),
        };

        let start_req = std::time::Instant::now();
        let pipeline = crate::services::pipeline_runner::PipelineRunner::build_pipeline(s);
        let pipeline_res = pipeline
            .execute_test_target(pipeline_req, dummy_target)
            .await;
        let elapsed_ms = start_req.elapsed().as_millis() as u64;

        let (status, error_msg) = if matches!(pipeline_res.error, Some(CoreError::Cancelled(_))) {
            (0, Some("Cancel".to_string()))
        } else if let Some(ref err) = pipeline_res.error {
            let status = err.http_status();
            let err_str = err.to_string();
            let truncated: String = err_str.chars().take(TEST_ERROR_BODY_MAX_CHARS).collect();
            (status, Some(truncated))
        } else {
            (pipeline_res.status_code, None)
        };

        let debug_payload = if opts.in_combo_fanout {
            None
        } else {
            let response_body = if let Some(ref resp) = pipeline_res.final_response {
                serde_json::to_value(resp).unwrap_or_default()
            } else if let Some(ref err) = pipeline_res.error {
                serde_json::json!({ "error": err.to_string() })
            } else {
                serde_json::Value::Null
            };
            Some(serde_json::json!({
                "request_headers": req_headers,
                "request_url": adapter.build_chat_url_for_account(
                    effective_target_format,
                    &model.model_id,
                    &account_label,
                ),
                "request_body": openai_req,
                "response_body": response_body,
            }))
        };

        (status, error_msg, elapsed_ms, debug_payload)
    };

    if !opts.in_combo_fanout {
        let status_i32 = i32::from(status);
        let pool = std::sync::Arc::clone(s.db_pool());
        let set_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let w = pool
                .try_writer_for(std::time::Duration::from_secs(5))
                .ok_or_else(|| "writer lock timeout".to_string())?;
            core_models::set_test_status(&w, row_id, status_i32).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("spawn failed: {e}")));
        if let Err(msg) = set_result {
            let mut err_res = test_error_result(model_row_id, 500, &msg);
            err_res.elapsed_ms = elapsed_ms;
            return (err_res, None);
        }
    }

    (
        TestResult {
            row_id: model_row_id,
            status,
            elapsed_ms,
            error_msg,
            skipped: false,
            skip_reason: None,
        },
        debug_payload,
    )
}
