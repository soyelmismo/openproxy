use crate::error::CoreError;
use crate::pipeline::is_upstream_health_issue;

#[test]
fn test_is_upstream_health_issue() {
    assert!(is_upstream_health_issue(&CoreError::UpstreamTimeout { phase: "connect".to_string(), ms: 1000 }));
    assert!(!is_upstream_health_issue(&CoreError::UpstreamTimeout { phase: "idle_chunk".to_string(), ms: 1000 }));
    assert!(is_upstream_health_issue(&CoreError::UpstreamConnection("err".to_string())));
    assert!(is_upstream_health_issue(&CoreError::RateLimited { provider: "test".to_string(), retry_after_ms: 100, is_proxy_rotated: false }));
    assert!(is_upstream_health_issue(&CoreError::UpstreamError { status: 500, provider: "p".to_string(), model: "m".to_string(), body: "b".to_string(), is_proxy_rotated: false }));
    assert!(!is_upstream_health_issue(&CoreError::UpstreamError { status: 400, provider: "p".to_string(), model: "m".to_string(), body: "b".to_string(), is_proxy_rotated: false }));
    assert!(!is_upstream_health_issue(&CoreError::ClientDisconnected));
}
