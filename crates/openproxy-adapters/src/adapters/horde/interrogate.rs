use super::apply_horde_auth_headers;
use crate::CancellationToken;
use bytes::Bytes;
use openproxy_types::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeInterrogateForm {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeInterrogatePayload {
    pub forms: Vec<HordeInterrogateForm>,
    pub source_image: String,
}

#[derive(Debug, Deserialize)]
pub struct HordeInterrogateStatusResponse {
    pub id: Option<String>,
    pub state: Option<String>,
    pub forms: Option<Vec<HordeInterrogateFormStatus>>,
    pub message: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HordeInterrogateFormStatus {
    pub name: Option<String>,
    pub form: Option<String>,
    pub state: Option<String>,
    pub result: Option<serde_json::Value>,
}

pub fn is_vision_model(model_name: &str) -> bool {
    if model_name.eq_ignore_ascii_case("horde/vision") || model_name.eq_ignore_ascii_case("vision")
    {
        return true;
    }
    let bytes = model_name.as_bytes();
    bytes.len() >= 7 && bytes[bytes.len() - 7..].eq_ignore_ascii_case(b"/vision")
}

pub fn build_interrogate_payload(source_image: &str, forms: &[&str]) -> Result<Bytes> {
    let forms_vec: Vec<HordeInterrogateForm> = if forms.is_empty() {
        vec![HordeInterrogateForm {
            name: "caption".to_string(),
        }]
    } else {
        forms
            .iter()
            .map(|f| HordeInterrogateForm {
                name: f.to_string(),
            })
            .collect()
    };

    let payload = HordeInterrogatePayload {
        forms: forms_vec,
        source_image: clean_image_str(source_image).into_owned(),
    };

    let vec = serde_json::to_vec(&payload).map_err(|e| {
        CoreError::Parse(format!(
            "failed to serialize horde interrogate request: {e}"
        ))
    })?;
    Ok(Bytes::from(vec))
}

pub fn extract_image_from_messages(messages: &[openproxy_types::OpenAIMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter_map(|msg| msg.content.as_ref())
        .find_map(extract_image_from_content)
}

fn extract_caption_from_object(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    for key in [
        "caption",
        "text",
        "interrogation",
        "description",
        "summary",
        "result",
    ] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str())
            && !s.trim().is_empty()
        {
            return Some(s.trim().to_string());
        }
    }
    serde_json::to_string(obj).ok()
}

fn parse_form_result_caption(forms: &[serde_json::Value]) -> Option<String> {
    for form in forms {
        let Some(result) = form.get("result") else {
            continue;
        };
        if let Some(s) = result.as_str()
            && !s.trim().is_empty()
        {
            return Some(s.trim().to_string());
        }
        if let Some(obj) = result.as_object()
            && let Some(cap) = extract_caption_from_object(obj)
        {
            return Some(cap);
        }
    }
    None
}

fn parse_generations_caption(gens: &[serde_json::Value]) -> Option<String> {
    for item in gens {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            return Some(text.trim().to_string());
        }
        if let Some(img) = item.get("img").and_then(|v| v.as_str()) {
            return Some(img.trim().to_string());
        }
    }
    None
}

fn check_interrogate_forms_caption(status_json: &serde_json::Value) -> Option<String> {
    let forms = status_json.get("forms")?.as_array()?;
    parse_form_result_caption(forms)
}

fn check_interrogate_result_caption(status_json: &serde_json::Value) -> Option<String> {
    let result = status_json.get("result")?;
    if let Some(s) = result.as_str() {
        return Some(s.trim().to_string());
    }
    let caption = result.get("caption")?.as_str()?;
    Some(caption.trim().to_string())
}

pub fn parse_interrogate_status_caption(status_json: &serde_json::Value) -> Option<String> {
    if let Some(cap) = check_interrogate_forms_caption(status_json) {
        return Some(cap);
    }

    if let Some(caption) = status_json.get("caption").and_then(|v| v.as_str()) {
        return Some(caption.trim().to_string());
    }

    if let Some(cap) = check_interrogate_result_caption(status_json) {
        return Some(cap);
    }

    if let Some(gens) = status_json.get("generations").and_then(|v| v.as_array()) {
        return parse_generations_caption(gens);
    }

    None
}

fn any_form_done(status_json: &serde_json::Value) -> bool {
    let Some(arr) = status_json.get("forms").and_then(|v| v.as_array()) else {
        return false;
    };
    arr.iter().any(|f| {
        f.get("state")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("done"))
            || f.get("result").is_some()
    })
}

pub fn is_interrogate_done(status_json: &serde_json::Value) -> (bool, bool) {
    let state = status_json
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_done = state.eq_ignore_ascii_case("done")
        || status_json
            .get("done")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        || any_form_done(status_json);
    let is_faulted = state.eq_ignore_ascii_case("faulted")
        || status_json
            .get("faulted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    (is_done, is_faulted)
}

pub async fn submit_interrogate_job(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
    base_url: &str,
    api_key: &str,
    source_image: &str,
    cancel_token: CancellationToken,
) -> Result<String> {
    let payload = build_interrogate_payload(source_image, &["caption"])?;
    let async_url = format!("{base_url}/interrogate/async");
    let mut post_req = crate::upstream::UpstreamRequest::post_json(async_url, payload);
    apply_horde_auth_headers(&mut post_req, api_key, true);

    let resp = upstream_client
        .call(
            post_req,
            crate::upstream::TimeoutProfile::Chat,
            cancel_token,
        )
        .await
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("horde interrogate submit error: {e:?}"))
        })?;

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body = resp.collect().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body);
        return Err(CoreError::UpstreamConnection(format!(
            "horde interrogate submit failed (HTTP {status}): {snippet}"
        )));
    }

    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("read submit response: {e:?}")))?;

    let submit_json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("parse horde submit response: {e}")))?;

    let Some(job_id) = submit_json.get("id").and_then(|v| v.as_str()) else {
        return Err(CoreError::UpstreamConnection(format!(
            "horde interrogate did not return a job ID: {submit_json}"
        )));
    };
    Ok(job_id.to_string())
}

async fn poll_interrogate_step(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
    status_url: &str,
    api_key: &str,
    cancel_token: &CancellationToken,
) -> Option<Result<String>> {
    let mut req = crate::upstream::UpstreamRequest::get(status_url);
    apply_horde_auth_headers(&mut req, api_key, false);

    let resp = match upstream_client
        .call(
            req,
            crate::upstream::TimeoutProfile::Chat,
            cancel_token.clone(),
        )
        .await
    {
        Ok(r) if r.status.as_u16() == 200 => r,
        Ok(_) => return None,
        Err(e) => {
            tracing::warn!("Horde interrogate status polling network error: {e:?}");
            return None;
        }
    };

    let body = resp.collect().await.ok()?;
    let status_json = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
    evaluate_interrogate_status(&status_json)
}

fn evaluate_interrogate_status(status_json: &serde_json::Value) -> Option<Result<String>> {
    let (is_done, is_faulted) = is_interrogate_done(status_json);
    if is_faulted {
        return Some(Err(CoreError::UpstreamConnection(
            "horde interrogation job faulted or worker unavailable".into(),
        )));
    }
    if (is_done || status_json.get("forms").is_some())
        && let Some(caption) = parse_interrogate_status_caption(status_json)
    {
        return Some(Ok(caption));
    }
    None
}

pub async fn poll_interrogate_job(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
    base_url: &str,
    api_key: &str,
    job_id: &str,
    cancel_token: CancellationToken,
) -> Result<String> {
    let status_url = format!("{base_url}/interrogate/status/{job_id}");
    let timeout = std::time::Duration::from_secs(120);
    let start = std::time::Instant::now();

    while start.elapsed() < timeout {
        if cancel_token.is_cancelled() {
            cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
            return Err(CoreError::Cancelled(
                openproxy_types::CancelReason::ClientDisconnected,
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        if let Some(res) =
            poll_interrogate_step(upstream_client, &status_url, api_key, &cancel_token).await
        {
            if res.is_err() {
                cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
            }
            return res;
        }
    }

    cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
    Err(CoreError::UpstreamTimeout {
        phase: "horde_interrogate_poll".into(),
        ms: 120_000,
    })
}

pub async fn execute_interrogate(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
    base_url: &str,
    api_key: &str,
    source_image: &str,
    cancel_token: CancellationToken,
) -> Result<String> {
    let job_id = submit_interrogate_job(
        upstream_client,
        base_url,
        api_key,
        source_image,
        cancel_token.clone(),
    )
    .await?;
    poll_interrogate_job(upstream_client, base_url, api_key, &job_id, cancel_token).await
}

pub async fn cancel_interrogate_job(
    upstream_client: &Arc<crate::upstream::UpstreamClient>,
    base_url: &str,
    job_id: &str,
    api_key: &str,
) {
    let cancel_url = format!("{base_url}/interrogate/status/{job_id}");
    let mut del_req = crate::upstream::UpstreamRequest::delete(&cancel_url);
    apply_horde_auth_headers(&mut del_req, api_key, false);
    let _ = upstream_client
        .call(
            del_req,
            crate::upstream::TimeoutProfile::Chat,
            CancellationToken::new(),
        )
        .await;
}

fn is_image_url_or_data(s: &str) -> bool {
    s.starts_with("data:image/") || s.starts_with("http://") || s.starts_with("https://")
}

fn extract_image_from_content(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::Array(parts) => parts.iter().find_map(extract_image_from_part),
        serde_json::Value::Object(map) => extract_image_from_json_map(map),
        serde_json::Value::String(s) if is_image_url_or_data(s) => {
            Some(clean_image_str(s).into_owned())
        }
        _ => None,
    }
}

fn extract_image_from_part(part: &serde_json::Value) -> Option<String> {
    let obj = part.as_object()?;
    extract_image_from_json_map(obj)
}

fn extract_image_from_json_map(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(img_url_val) = map.get("image_url") {
        if let Some(url_str) = img_url_val.as_str() {
            return Some(clean_image_str(url_str).into_owned());
        }
        if let Some(url_obj) = img_url_val.as_object()
            && let Some(url_str) = url_obj.get("url").and_then(|v| v.as_str())
        {
            return Some(clean_image_str(url_str).into_owned());
        }
    }

    if let Some(source) = map.get("source").and_then(|v| v.as_object())
        && let Some(data) = source.get("data").and_then(|v| v.as_str())
    {
        return Some(clean_image_str(data).into_owned());
    }

    if let Some(img) = map.get("image").and_then(|v| v.as_str()) {
        return Some(clean_image_str(img).into_owned());
    }
    if let Some(img) = map.get("source_image").and_then(|v| v.as_str()) {
        return Some(clean_image_str(img).into_owned());
    }

    None
}

pub(crate) fn clean_image_str(s: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = s.trim();
    if trimmed.starts_with("data:image/")
        && let Some((_header, data)) = trimmed.split_once(',')
    {
        std::borrow::Cow::Owned(data.trim().to_string())
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    }
}
