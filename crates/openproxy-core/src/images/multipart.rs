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

pub use crate::unary::sanitize_header_token;
use crate::unary::{MultipartFilePart, call_unary_multipart_upstream};

#[must_use]
pub(crate) fn effective_delivered_images(requested: Option<u32>, actual_len: usize) -> Option<u32> {
    requested.map(|req_n| {
        let actual = u32::try_from(actual_len).unwrap_or(req_n);
        req_n.max(actual)
    })
}

pub async fn dispatch_image_multipart_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    body: &ParsedImageMultipartBody,
) -> Result<UpstreamResponse> {
    let parts: Vec<MultipartFilePart<'_>> = body
        .files
        .iter()
        .map(|f| MultipartFilePart {
            field_name: &f.name,
            file_name: &f.file_name,
            content_type: &f.content_type,
            bytes: &f.bytes,
        })
        .collect();
    call_unary_multipart_upstream(
        upstream_client,
        adapter,
        upstream_url,
        api_key,
        upstream_model_id,
        &body.form_fields,
        &parts,
        false,
    )
    .await
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
