//! Group commit for last-used telemetry. Authentication still validates the
//! credential on every request; only the non-authoritative timestamp is batched.
use openproxy_db::DbPool;
use openproxy_types::{CoreError, Result, ids::ApiKeyId};
use std::sync::{Arc, OnceLock};
use tokio::sync::{mpsc, oneshot};
type Pending = (ApiKeyId, oneshot::Sender<Result<()>>);
#[derive(Default)]
pub(crate) struct LastUsedBatcher {
    sender: OnceLock<mpsc::Sender<Pending>>,
}
impl LastUsedBatcher {
    pub async fn touch(&self, pool: Arc<DbPool>, key: ApiKeyId) -> Result<()> {
        let sender = self.sender.get_or_init(|| {
            let (tx, mut rx) = mpsc::channel::<Pending>(2048);
            tokio::spawn(async move {
                while let Some(first) = rx.recv().await {
                    let mut batch = vec![first];
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(2);
                    while batch.len() < 512 {
                        match tokio::time::timeout_at(deadline, rx.recv()).await {
                            Ok(Some(next)) => batch.push(next),
                            _ => break,
                        }
                    }
                    let keys: std::collections::HashSet<i64> =
                        batch.iter().map(|p| p.0.0).collect();
                    let result = pool
                        .spawn_write(move |w| {
                            let tx = rusqlite::Transaction::new_unchecked(
                                w,
                                rusqlite::TransactionBehavior::Immediate,
                            )
                            .map_err(openproxy_db::error::map_db_error)?;
                            for key in keys {
                                openproxy_core::api_keys::touch_last_used(&tx, ApiKeyId(key))?;
                            }
                            tx.commit().map_err(openproxy_db::error::map_db_error)
                        })
                        .await;
                    for (_, reply) in batch {
                        let _ = reply.send(result.clone());
                    }
                }
            });
            tx
        });
        let (tx, rx) = oneshot::channel();
        sender
            .send((key, tx))
            .await
            .map_err(|_| CoreError::Internal("last-used worker stopped".into()))?;
        rx.await
            .map_err(|_| CoreError::Internal("last-used response lost".into()))?
    }
}
