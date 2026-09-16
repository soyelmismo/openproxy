//! Image multipart handling, edit, and variation operations.

use std::sync::Arc;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::{CoreError, ImageGenerationResponse, Result, ids::ApiKeyId};

use crate::images::multipart_exec::execute_image_multipart;
use crate::unary::apply_adapter_headers;

#[derive(Clone, Debug)]
pub struct MultipartFile {
    pub name: String,
    pub file_name: String,
    pub content_type: String,
    pub bytes: bytes::Bytes,
}

#[derive(Clone, Debug)]
pub struct ParsedImageMultipartBody {
    pub model_name: String,
    pub files: Vec<MultipartFile>,
    pub form_fields: Vec<(String, String)>,
}

pub struct ImageServiceContext<'a> {
    pub db_pool: &'a DbPool,
    pub adapters: &'a [ProviderAdapterEnum],
    pub upstream_client: &'a Arc<UpstreamClient>,
    pub circuit_breaker: &'a CircuitBreakerRegistry,
    pub master_key: &'a MasterKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMultipartKind {
    Edit,
    Variation,
}

pub async fn dispatch_image_multipart_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    body: &ParsedImageMultipartBody,
) -> Result<UpstreamResponse> {
    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    // model field
    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(upstream_model_id.as_bytes());
    payload.extend_from_slice(b"\r\n");

    // form fields
    for (k, v) in &body.form_fields {
        if k == "model" {
            continue;
        }
        payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        payload.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        payload.extend_from_slice(v.as_bytes());
        payload.extend_from_slice(b"\r\n");
    }

    // files
    for file in &body.files {
        payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        payload.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                file.name, file.file_name
            )
            .as_bytes(),
        );
        payload
            .extend_from_slice(format!("Content-Type: {}\r\n\r\n", file.content_type).as_bytes());
        payload.extend_from_slice(&file.bytes);
        payload.extend_from_slice(b"\r\n");
    }

    // end boundary
    payload.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let mut upstream_req =
        UpstreamRequest::post_multipart(upstream_url, &content_type, bytes::Bytes::from(payload));

    apply_adapter_headers(&mut upstream_req, adapter, api_key, upstream_model_id, true);

    let cancel = CancellationToken::new();
    upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
}

pub async fn execute_image_edit(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    body: ParsedImageMultipartBody,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let ctx = ImageServiceContext {
        db_pool,
        adapters,
        upstream_client,
        circuit_breaker,
        master_key,
    };
    execute_image_multipart(&ctx, body, ImageMultipartKind::Edit, api_key_id).await
}

pub async fn execute_image_variation(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    body: ParsedImageMultipartBody,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let ctx = ImageServiceContext {
        db_pool,
        adapters,
        upstream_client,
        circuit_breaker,
        master_key,
    };
    execute_image_multipart(&ctx, body, ImageMultipartKind::Variation, api_key_id).await
}
