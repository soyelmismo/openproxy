//! Horde async submission polling logic.

use std::sync::Arc;
use std::time::Instant;

use http::HeaderValue;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::images::{HordeAsyncSubmitResponse, HordeCheckResponse, HordeStatusResponse};
use openproxy_types::{CoreError, ImageData, ImageGenerationResponse, Result};

pub(crate) struct HordePollContext<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) trace_id: &'a str,
    pub(crate) started: Instant,
    pub(crate) response_format: Option<&'a str>,
    pub(crate) has_alternatives: bool,
}

pub(crate) async fn cancel_horde_job(
    upstream_client: &Arc<UpstreamClient>,
    base_url: &str,
    job_id: &str,
    auth_headers: &[(String, String)],
) {
    let cancel_url = format!("{base_url}/generate/status/{job_id}");
    let mut del_req = UpstreamRequest::delete(&cancel_url);
    for (k, v) in auth_headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            del_req.headers.insert(name, val);
        }
    }
    let _ = upstream_client
        .call(del_req, TimeoutProfile::Chat, CancellationToken::new())
        .await;
}

pub(crate) async fn poll_horde_image_generation(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    api_key: &str,
    initial_body: &[u8],
    ctx: HordePollContext<'_>,
) -> Result<ImageGenerationResponse> {
    let submit_resp: HordeAsyncSubmitResponse = serde_json::from_slice(initial_body)
        .map_err(|e| CoreError::Parse(format!("failed to parse horde submit response: {e}")))?;

    let Some(job_id) = submit_resp.id else {
        let msg = submit_resp
            .message
            .or(submit_resp.error)
            .unwrap_or_else(|| "unknown horde submission error".to_string());
        return Err(CoreError::Validation(format!(
            "horde submission failed: {msg}"
        )));
    };

    let base_url = adapter.config().base_url.as_str();
    let check_url = format!("{base_url}/generate/check/{job_id}");
    let status_url = format!("{base_url}/generate/status/{job_id}");

    let auth_headers = adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(""),
    );

    // If there are non-Horde fallback targets available, failover after 45s; otherwise wait up to 120s
    let timeout = if ctx.has_alternatives {
        std::time::Duration::from_secs(45)
    } else {
        std::time::Duration::from_secs(120)
    };
    let start = Instant::now();

    while start.elapsed() < timeout {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let mut req = UpstreamRequest::get(&check_url);
        for (k, v) in &auth_headers {
            if let (Ok(name), Ok(val)) = (
                http::header::HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                req.headers.insert(name, val);
            }
        }

        let resp = upstream_client
            .call(req, TimeoutProfile::Chat, CancellationToken::new())
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("horde check error: {e:?}")))?;

        if resp.status.as_u16() != 200 {
            continue;
        }

        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("horde check body error: {e:?}")))?;

        let check: HordeCheckResponse = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("horde check parse error: {e}")))?;

        // Emit in-flight stage event to live logs
        openproxy_types::emit_stage_event!(
            request_id: ctx.request_id,
            trace_id: ctx.trace_id,
            stage: "waiting_upstream",
            elapsed_ms: ctx.started.elapsed().as_millis() as u64,
            provider_id: "horde",
            status_code: 202,
            endpoint_kind: openproxy_types::EndpointKind::Image,
        );

        if check.faulted == Some(true) {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::UpstreamConnection(
                "horde job faulted or worker unavailable".into(),
            ));
        }

        if check.done == Some(true) || check.finished.unwrap_or(0) > 0 {
            break;
        }
    }

    if start.elapsed() >= timeout {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamTimeout {
            phase: "horde_polling".into(),
            ms: timeout.as_millis() as u64,
        });
    }

    // Fetch final status and images
    let mut req = UpstreamRequest::get(&status_url);
    for (k, v) in &auth_headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            req.headers.insert(name, val);
        }
    }

    let resp = upstream_client
        .call(req, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("horde status error: {e:?}")))?;

    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("horde status body error: {e:?}")))?;

    let status: HordeStatusResponse = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("horde status parse error: {e}")))?;

    let generations = status.generations.unwrap_or_default();
    if generations.is_empty() {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamConnection(
            "horde returned no generations".into(),
        ));
    }

    let mut data = Vec::with_capacity(generations.len());
    for gen_item in generations {
        if gen_item.censored == Some(true) || gen_item.state.as_deref() == Some("censored") {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::Validation(format!(
                "generation censored by worker {}",
                gen_item.worker_name.as_deref().unwrap_or("unknown")
            )));
        }
        if gen_item.state.as_deref() == Some("csam") {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::Validation(
                "generation rejected: content safety violation (csam)".into(),
            ));
        }

        let Some(img) = gen_item.img else {
            continue;
        };

        if img.starts_with("http://") || img.starts_with("https://") {
            if ctx.response_format == Some("b64_json") {
                let dl_req = UpstreamRequest::get(&img);
                let dl_resp = upstream_client
                    .call(dl_req, TimeoutProfile::Chat, CancellationToken::new())
                    .await
                    .map_err(|e| {
                        CoreError::UpstreamConnection(format!("download image error: {e:?}"))
                    })?;
                let dl_bytes = dl_resp.collect().await.map_err(|e| {
                    CoreError::UpstreamConnection(format!("download image body error: {e:?}"))
                })?;
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&dl_bytes);
                data.push(ImageData {
                    url: None,
                    b64_json: Some(b64),
                    revised_prompt: None,
                });
            } else {
                data.push(ImageData {
                    url: Some(img),
                    b64_json: None,
                    revised_prompt: None,
                });
            }
        } else if ctx.response_format == Some("url") {
            data.push(ImageData {
                url: Some(format!("data:image/webp;base64,{img}")),
                b64_json: None,
                revised_prompt: None,
            });
        } else {
            data.push(ImageData {
                url: None,
                b64_json: Some(img),
                revised_prompt: None,
            });
        }
    }

    if data.is_empty() {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamConnection(
            "horde returned no valid images".into(),
        ));
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;

    Ok(ImageGenerationResponse {
        created,
        data: data.into_boxed_slice(),
    })
}
