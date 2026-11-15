//! Coalesce identical reads only while a response is in flight. No TTL or
//! retained response cache: a later request always reads a fresh DB snapshot.
use crate::error::ApiError;
use axum::{
    Json,
    body::Bytes,
    http::header,
    response::{IntoResponse, Response},
};
use openproxy_types::CoreError;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::OnceCell;

type Cell = OnceCell<Result<Bytes, CoreError>>;
#[derive(Default)]
pub(crate) struct ReadFlights {
    entries: parking_lot::Mutex<HashMap<String, Weak<Cell>>>,
}
impl ReadFlights {
    pub async fn json<T, F>(&self, key: String, query: F) -> Result<Response, ApiError>
    where
        T: serde::Serialize + Send + 'static,
        F: FnOnce() -> Result<Json<T>, ApiError> + Send + 'static,
    {
        let cell = {
            let mut entries = self.entries.lock();
            if let Some(cell) = entries.get(&key).and_then(Weak::upgrade) {
                cell
            } else {
                entries.retain(|_, value| value.strong_count() > 0);
                let cell = Arc::new(Cell::new());
                // Bound bookkeeping. Excess unique requests still run normally.
                if entries.len() < 1024 {
                    entries.insert(key, Arc::downgrade(&cell));
                }
                cell
            }
        };
        let bytes = cell
            .get_or_init(|| async move {
                crate::error::run_bulk_read_blocking(move || {
                    let value = query()?;
                    serde_json::to_vec(&value.0)
                        .map(Bytes::from)
                        .map_err(|e| ApiError(CoreError::Internal(format!("read JSON: {e}"))))
                })
                .await
                .map_err(|e| e.0)
            })
            .await
            .as_ref()
            .map_err(|e| ApiError(e.clone()))?
            .clone();
        Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coalesces_overlap_but_never_retains_completed_results() {
        let flights = Arc::new(ReadFlights::default());
        let count = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let f = Arc::clone(&flights);
            let n = Arc::clone(&count);
            tasks.push(tokio::spawn(async move {
                f.json("tenant1:query".into(), move || {
                    n.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    Ok(Json(serde_json::json!({"value":1})))
                })
                .await
                .unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let n = Arc::clone(&count);
        flights
            .json("tenant1:query".into(), move || {
                n.fetch_add(1, Ordering::SeqCst);
                Ok(Json(2))
            })
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        flights
            .json("tenant2:query".into(), || Ok(Json(3)))
            .await
            .unwrap();
    }
}
