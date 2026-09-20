use super::HORDE_CLIENT_AGENT;
use crate::CancellationToken;
use crate::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_types::{DiscoveredModel, ModelId, TargetFormat};
use std::sync::Arc;

pub(crate) fn infer_horde_family(model_name: &str) -> &'static str {
    const PATTERNS: &[(&str, &str)] = &[
        ("flux", "flux"),
        ("sdxl", "sdxl"),
        ("xl", "sdxl"),
        ("pony", "pony"),
        ("stable_diffusion", "sd15"),
        ("sd 1.5", "sd15"),
        ("sd15", "sd15"),
        ("dreamshaper", "dreamshaper"),
    ];

    let bytes = model_name.as_bytes();
    for &(pattern, family) in PATTERNS {
        let p_bytes = pattern.as_bytes();
        if bytes.len() >= p_bytes.len()
            && bytes
                .windows(p_bytes.len())
                .any(|w| w.eq_ignore_ascii_case(p_bytes))
        {
            return family;
        }
    }

    "diffusion"
}

fn map_horde_cluster_model(
    item: &serde_json::Value,
    is_image: bool,
) -> Option<(u64, u64, DiscoveredModel)> {
    let name = item.get("name")?.as_str()?.to_string();
    let count = item.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    let eta = item.get("eta").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);

    let (family, out_mods, m_type): (_, Box<[String]>, _) = if is_image {
        (
            Some(infer_horde_family(&name).to_string()),
            vec!["image".into()].into(),
            "image",
        )
    } else {
        (
            openproxy_types::capabilities::infer_family(&name).or_else(|| Some("instruct".into())),
            vec!["text".into()].into(),
            "chat",
        )
    };

    let display_name = if count > 0 && eta != u64::MAX {
        Some(format!("{name} ({count}w, ~{eta}s)"))
    } else if count > 0 {
        Some(format!("{name} ({count}w)"))
    } else {
        Some(name.clone())
    };

    Some((
        count,
        eta,
        DiscoveredModel {
            model_id: ModelId::new(name),
            display_name,
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: Some(vec!["text".into()].into()),
            output_modalities: Some(out_mods),
            model_type: Some(m_type.into()),
            family,
            capabilities: None,
        },
    ))
}

pub(crate) async fn fetch_horde_cluster_models(
    upstream_client: &Arc<UpstreamClient>,
    base_url: &str,
    header_refs: &[(&str, &str)],
    model_type: &'static str,
) -> Vec<DiscoveredModel> {
    let url = format!("{base_url}/status/models?type={model_type}");
    let Ok(json_val) = crate::adapters::upstream_get_json(upstream_client, &url, header_refs).await
    else {
        return Vec::new();
    };
    let Some(arr) = json_val.as_array() else {
        return Vec::new();
    };
    let is_image = model_type == "image";
    let mut models: Vec<(u64, u64, DiscoveredModel)> = arr
        .iter()
        .filter_map(|item| map_horde_cluster_model(item, is_image))
        .collect();

    models.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    models.into_iter().map(|(_, _, m)| m).collect()
}

pub(crate) async fn query_horde_user(
    upstream: &Arc<UpstreamClient>,
    base_url: &str,
    key: &str,
) -> std::result::Result<serde_json::Value, String> {
    let url = format!("{base_url}/find_user");
    let mut req = UpstreamRequest::get(url);
    if let Ok(v) = http::HeaderValue::from_str(key) {
        req.headers
            .insert(http::header::HeaderName::from_static("apikey"), v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("client-agent"),
        HORDE_CLIENT_AGENT.clone(),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );

    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| format!("network: {e}"))?;

    if !response.status.is_success() {
        let status = response.status.as_u16();
        let body = response.collect().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("HTTP {status}: {snippet}"));
    }

    let body = response
        .collect()
        .await
        .map_err(|e| format!("collect: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))
}

fn check_horde_quota_error(body: &serde_json::Value) -> Option<&str> {
    if let Some(msg) = body.get("message").and_then(|v| v.as_str())
        && body.get("kudos").is_none()
        && body.get("username").is_none()
    {
        return Some(msg);
    }
    None
}

pub fn parse_horde_quota(
    body: &serde_json::Value,
    last_fetched_at: &str,
) -> openproxy_types::AccountQuota {
    if let Some(msg) = check_horde_quota_error(body) {
        return openproxy_types::AccountQuota {
            last_fetched_at: last_fetched_at.to_string(),
            ..openproxy_types::AccountQuota::with_error(msg)
        };
    }

    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Anonymous");

    let kudos = body.get("kudos").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let worker_count = body
        .get("worker_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let tokens_used = body
        .get("records")
        .and_then(|r| r.get("usage"))
        .and_then(|u| u.get("tokens"))
        .and_then(|t| t.as_i64())
        .or_else(|| {
            body.get("usage")
                .and_then(|u| u.get("tokens"))
                .and_then(|t| t.as_i64())
        })
        .unwrap_or(0);

    let plan_name = format!("{username} (Kudos: {kudos:.0}, Workers: {worker_count})");
    let session_limit = kudos.max(0.0) as i64;

    openproxy_types::AccountQuota {
        session_used: Some(tokens_used),
        session_limit: Some(session_limit),
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: last_fetched_at.to_string(),
        fetch_error: None,
        model_details: None,
    }
}
