use openproxy_types::error::CoreError;

pub fn map_db_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> CoreError {
    CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    }
}

pub fn map_db_error_ctx<E: std::error::Error + Send + Sync + 'static>(
    ctx: impl Into<String>,
) -> impl FnOnce(E) -> CoreError {
    let c = ctx.into();
    move |e| CoreError::Database {
        message: format!("{c}: {e}"),
        source: Some(Box::new(e)),
    }
}
