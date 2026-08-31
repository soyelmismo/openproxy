use std::hash::{DefaultHasher, Hash, Hasher};

pub fn compute_error_fingerprint(err: &openproxy_types::CoreError) -> u64 {
    let mut hasher = DefaultHasher::new();
    err.http_status().hash(&mut hasher);
    match err {
        openproxy_types::CoreError::UpstreamError { status, body, .. } => {
            status.hash(&mut hasher);
            let limit = body.floor_char_boundary(64);
            let prefix = &body[..limit];
            prefix.hash(&mut hasher);
        }
        openproxy_types::CoreError::UpstreamConnection(msg) => {
            let limit = msg.floor_char_boundary(64);
            let prefix = &msg[..limit];
            prefix.hash(&mut hasher);
        }
        openproxy_types::CoreError::UpstreamTimeout { phase, .. } => {
            phase.hash(&mut hasher);
        }
        _ => {}
    }
    hasher.finish()
}
