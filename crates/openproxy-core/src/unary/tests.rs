use super::*;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::config::CircuitBreakerConfig;

fn open_test_pool() -> (DbPool, std::path::PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-unary-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

#[test]
fn test_resolve_unary_targets_not_found() {
    let (pool, _path) = open_test_pool();
    let plan = RoutingPlan::NotFound {
        model: "nonexistent".to_string(),
        hint: Some("check model name".to_string()),
    };
    let err = resolve_unary_targets(
        &pool,
        plan,
        "nonexistent",
        EndpointKind::Image,
        None,
        Instant::now(),
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::ModelNotFound { .. }));
}

#[test]
fn test_is_target_available_healthy() {
    let (pool, _path) = open_test_pool();
    let cb = CircuitBreakerRegistry::new(&CircuitBreakerConfig::default());
    assert!(is_target_available(&pool, &cb, None, None));
}

#[test]
fn test_unary_usage_args_builder_and_from_target() {
    let (pool, _path) = open_test_pool();
    let provider = ProviderId::new("openai");
    let target = UnaryTarget {
        provider: provider.clone(),
        account_id: None,
        model_row_id: None,
        combo_target_id: None,
        upstream_model: "text-embedding-3-small".to_string(),
        combo_id: None,
    };

    let args = UnaryUsageArgs::from_target(&target, EndpointKind::Embedding, 200, 50)
        .with_tokens(Some(10), None);
    assert_eq!(args.provider_id, &provider);
    assert_eq!(args.upstream_model_id, "text-embedding-3-small");
    assert_eq!(args.endpoint_kind, EndpointKind::Embedding);
    assert_eq!(args.status_code, 200);
    assert_eq!(args.total_ms, 50);
    assert_eq!(args.prompt_tokens, Some(10));
    assert_eq!(args.completion_tokens, None);

    let built = UnaryUsageArgs::builder(&provider, "gpt-4o", EndpointKind::Chat)
        .status_code(200)
        .total_ms(120)
        .build();
    assert_eq!(built.provider_id, &provider);
    assert_eq!(built.upstream_model_id, "gpt-4o");
    assert_eq!(built.status_code, 200);
    assert_eq!(built.total_ms, 120);

    record_unary_usage(&pool, &args);
}

#[test]
fn test_map_upstream_status_error() {
    let err_429 = map_upstream_status_error(429, "openai", "gpt-4o", "rate limited");
    assert!(matches!(err_429, CoreError::RateLimited { .. }));

    let err_401 = map_upstream_status_error(401, "openai", "gpt-4o", "unauthorized");
    assert!(matches!(err_401, CoreError::Auth(_)));

    let err_403 = map_upstream_status_error(403, "openai", "gpt-4o", "forbidden");
    assert!(matches!(err_403, CoreError::Auth(_)));

    let err_400 = map_upstream_status_error(400, "openai", "gpt-4o", "bad request");
    assert!(matches!(err_400, CoreError::Validation(_)));

    let err_500 = map_upstream_status_error(500, "openai", "gpt-4o", "server error");
    assert!(matches!(
        err_500,
        CoreError::UpstreamError { status: 500, .. }
    ));

    let err_empty = map_upstream_status_error(502, "openai", "gpt-4o", "");
    match err_empty {
        CoreError::UpstreamError { status, body, .. } => {
            assert_eq!(status, 502);
            assert_eq!(body, "upstream status 502");
        }
        _ => panic!("expected UpstreamError"),
    }
}
