//! Embedding service: routing resolution, multi-target dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{UpstreamClient, UpstreamResponse};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, EmbeddingRequest, EmbeddingResponse, EndpointKind, Result, ResultExt,
    ids::ApiKeyId,
};

use crate::routing::{self, RoutingPlan};

pub use crate::unary::{
    UnaryTarget as EmbeddingTargets, UnaryTarget, UnaryUsageArgs,
    is_target_available, map_upstream_status_error, record_unary_target_success_async,
    record_unary_usage, record_unary_usage_async, resolve_api_key, resolve_unary_targets,
};

pub type EmbeddingUsageArgs<'a> = UnaryUsageArgs<'a>;

pub fn resolve_embedding_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<EmbeddingTargets>> {
    resolve_unary_targets(
        db_pool,
        routing_plan,
        req_model,
        EndpointKind::Embedding,
        api_key_id,
        started,
    )
}

pub async fn dispatch_embedding_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    req: &EmbeddingRequest,
) -> Result<UpstreamResponse> {
    let payload = adapter.format_embedding_request(req, upstream_model_id)?;
    crate::unary::call_unary_upstream(
        upstream_client,
        adapter,
        upstream_url,
        api_key,
        upstream_model_id,
        payload,
        false,
    )
    .await
}

async fn dispatch_single_embedding(
    db_pool: &DbPool,
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    adapter: &ProviderAdapterEnum,
    target: &UnaryTarget,
    req: &EmbeddingRequest,
) -> Result<(EmbeddingResponse, u16)> {
    let upstream_url = adapter.build_embeddings_url();
    let api_key = crate::unary::resolve_api_key_async(
        db_pool,
        master_key,
        target.account_id,
        &target.provider,
    )
    .await?;

    let response = dispatch_embedding_request(
        upstream_client,
        adapter,
        &upstream_url,
        &api_key,
        &target.upstream_model,
        req,
    )
    .await
    .map_err(|e| {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        tracing::warn!(
            "Embedding target failed (connection error): provider={}, error={:?}",
            target.provider,
            e
        );
        e
    })?;

    let status_code = response.status.as_u16();
    let body_bytes = response.collect().await.map_err(|e| {
        let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        tracing::warn!(
            "Embedding target body read failed: provider={}, error={:?}",
            target.provider,
            err
        );
        err
    })?;

    if status_code >= 400 {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        let err_text = String::from_utf8_lossy(&body_bytes);
        tracing::warn!(
            "Embedding target returned error status: provider={}, status={}, body={}",
            target.provider,
            status_code,
            err_text
        );
        return Err(map_upstream_status_error(
            status_code,
            target.provider.as_str(),
            &target.upstream_model,
            &err_text,
        ));
    }

    let parsed_response: EmbeddingResponse = serde_json::from_slice(&body_bytes)
        .ctx_parse("failed to parse embedding response")?;

    if let Some(account_id) = target.account_id {
        circuit_breaker.record_success(CircuitBreakerKey::Account(account_id));
    }

    Ok((parsed_response, status_code))
}

pub async fn execute_embeddings(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    req: EmbeddingRequest,
    api_key_id: Option<ApiKeyId>,
) -> Result<EmbeddingResponse> {
    let started = Instant::now();

    let routing_plan = routing::resolve_routing(db_pool, &req.model).await?;

    let targets = crate::unary::resolve_unary_targets_async(
        db_pool,
        routing_plan,
        &req.model,
        EndpointKind::Embedding,
        api_key_id,
        started,
    )
    .await?;

    let mut last_error = None;
    let mut attempt = 0;

    for target in targets {
        attempt += 1;

        crate::guarded_unary_target!(check_async: db_pool, circuit_breaker, target);

        let Some(adapter) = adapters.iter().find(|a| a.id() == &target.provider) else {
            last_error = Some(CoreError::Internal(format!(
                "no adapter registered for provider '{}'",
                target.provider
            )));
            continue;
        };

        match dispatch_single_embedding(
            db_pool,
            upstream_client,
            circuit_breaker,
            master_key,
            adapter,
            &target,
            &req,
        )
        .await
        {
            Ok((parsed_response, status_code)) => {
                let total_ms = started.elapsed().as_millis() as u64;
                record_unary_target_success_async(
                    db_pool,
                    &target,
                    EndpointKind::Embedding,
                    api_key_id,
                    status_code,
                    total_ms,
                    (Some(parsed_response.usage.prompt_tokens), None),
                )
                .await;

                tracing::info!("Embedding request succeeded after {attempt} attempts");
                return Ok(parsed_response);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db as core_db;
    use openproxy_types::ids::ProviderId;
    use std::path::PathBuf;

    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        let pool =
            core_db::DbPool::test_pool_with_prefix("openproxy-embedding-test").expect("open pool");
        let path = pool.path().to_path_buf();
        (pool, path)
    }

    #[test]
    fn test_resolve_embedding_targets_not_found() {
        let (pool, _dir) = fresh_pool();
        let plan = RoutingPlan::NotFound {
            model: "nonexistent-model".into(),
            hint: None,
        };
        let res = resolve_embedding_targets(&pool, plan, "nonexistent-model", None, Instant::now());
        assert!(matches!(res, Err(CoreError::ModelNotFound { .. })));
    }

    #[test]
    fn test_record_embedding_usage_row() {
        let (pool, _dir) = fresh_pool();
        let provider = ProviderId::new("openai");
        record_unary_usage(
            &pool,
            &UnaryUsageArgs::builder(&provider, "text-embedding-3-small", EndpointKind::Embedding)
                .prompt_tokens(Some(8))
                .status_code(200)
                .total_ms(85)
                .build(),
        );

        let r = pool.reader();
        let count: i64 = r
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE endpoint_kind = 'embedding' AND prompt_tokens = 8",
                [],
                |row| row.get(0),
            )
            .expect("query usage");
        assert_eq!(count, 1);
    }
}
