//! Embedding service: routing resolution, multi-target dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, EmbeddingRequest, EmbeddingResponse, EndpointKind, Result,
    ids::{ApiKeyId, RequestId},
};

use crate::routing::{self, RoutingPlan};

pub use crate::unary::{
    UnaryTarget as EmbeddingTargets, UnaryTarget, UnaryUsageArgs, apply_adapter_headers,
    is_target_available, map_upstream_status_error, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
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
    let mut upstream_req = UpstreamRequest::post_json(upstream_url, payload);

    apply_adapter_headers(
        &mut upstream_req,
        adapter,
        api_key,
        upstream_model_id,
        false,
    );

    let cancel = CancellationToken::new();
    upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
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

    // 1. Resolve routing plan.
    let routing_plan = {
        let r = db_pool.reader();
        routing::resolve(&r, &req.model)?
    };

    // 2. Resolve embedding targets.
    let targets =
        resolve_embedding_targets(db_pool, routing_plan, &req.model, api_key_id, started)?;

    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in targets {
        attempt += 1;

        crate::guarded_unary_target!(check: db_pool, circuit_breaker, target);

        // Adapter resolution.
        let Some(adapter) = adapters
            .iter()
            .find(|a| a.id() == &target.provider)
            .cloned()
        else {
            last_error = Some(CoreError::Internal(format!(
                "no adapter registered for provider '{}'",
                target.provider
            )));
            continue;
        };
        let upstream_url = adapter.build_embeddings_url();

        // Credentials decryption via master key.
        let api_key =
            match resolve_api_key(db_pool, master_key, target.account_id, &target.provider) {
                Ok(k) => k,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

        // Dispatch upstream.
        let response = match dispatch_embedding_request(
            upstream_client,
            &adapter,
            &upstream_url,
            &api_key,
            &target.upstream_model,
            &req,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                crate::guarded_unary_target!(record_failure: circuit_breaker, target);
                tracing::warn!(
                    "Embedding target failed (connection error): provider={}, error={:?}",
                    target.provider,
                    e
                );
                last_error = Some(e);
                continue;
            }
        };

        let status_code = response.status.as_u16();
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
                crate::guarded_unary_target!(record_failure: circuit_breaker, target);
                tracing::warn!(
                    "Embedding target body read failed: provider={}, error={:?}",
                    target.provider,
                    err
                );
                last_error = Some(err);
                continue;
            }
        };

        if status_code >= 400 {
            crate::guarded_unary_target!(record_failure: circuit_breaker, target);
            let err_text = String::from_utf8_lossy(&body_bytes);
            tracing::warn!(
                "Embedding target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            let err = map_upstream_status_error(
                status_code,
                target.provider.as_str(),
                &target.upstream_model,
                &err_text,
            );
            last_error = Some(err);
            continue;
        }

        // Parse upstream response into standard EmbeddingResponse.
        let parsed_response: EmbeddingResponse = match serde_json::from_slice(&body_bytes) {
            Ok(res) => res,
            Err(e) => {
                let err = CoreError::Parse(format!("failed to parse embedding response: {e}"));
                last_error = Some(err);
                continue;
            }
        };

        if let Some(account_id) = target.account_id {
            circuit_breaker.record_success(CircuitBreakerKey::Account(account_id));
        }

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_unary_usage(
            db_pool,
            &UnaryUsageArgs {
                request_id: RequestId::new(),
                api_key_id,
                provider_id: &target.provider,
                account_id: target.account_id,
                combo_id: target.combo_id,
                combo_target_id: target.combo_target_id,
                model_row_id: target.model_row_id,
                upstream_model_id: &target.upstream_model,
                prompt_tokens: Some(parsed_response.usage.prompt_tokens),
                completion_tokens: None,
                status_code,
                error_msg: None,
                total_ms,
                endpoint_kind: EndpointKind::Embedding,
            },
        );

        tracing::info!("Embedding request succeeded after {attempt} attempts");
        return Ok(parsed_response);
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
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-embedding-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("state.db");
        let pool = core_db::DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrate");
        }
        (pool, dir)
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
            &UnaryUsageArgs {
                request_id: RequestId::new(),
                api_key_id: None,
                provider_id: &provider,
                account_id: None,
                combo_id: None,
                combo_target_id: None,
                model_row_id: None,
                upstream_model_id: "text-embedding-3-small",
                prompt_tokens: Some(8),
                completion_tokens: None,
                status_code: 200,
                error_msg: None,
                total_ms: 85,
                endpoint_kind: EndpointKind::Embedding,
            },
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
