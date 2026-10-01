//! Object-safe scheduling boundary for synchronous repository operations.
//! Compound reads stay in one blocking task; no connection guard enters Tokio.

use super::PipelineRepository;
use openproxy_types::{CoreError, Result};
use std::{future::Future, pin::Pin, sync::Arc};

pub type RepositoryFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
pub type RepositoryOperation = Box<dyn FnOnce(&dyn PipelineRepository) + Send + 'static>;

pub trait AsyncPipelineRepository: Send + Sync {
    fn execute(&self, operation: RepositoryOperation) -> RepositoryFuture<Result<()>>;
}

impl dyn AsyncPipelineRepository {
    pub fn run<R, F>(&self, operation: F) -> RepositoryFuture<Result<R>>
    where
        R: Send + 'static,
        F: FnOnce(&dyn PipelineRepository) -> Result<R> + Send + 'static,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let execution = self.execute(Box::new(move |repository| {
            let _ = sender.send(operation(repository));
        }));
        Box::pin(async move {
            execution.await?;
            receiver.await.map_err(|error| {
                CoreError::Internal(format!("repository operation cancelled: {error}"))
            })?
        })
    }
}

#[derive(Clone)]
pub struct BlockingPipelineRepository {
    repository: Arc<dyn PipelineRepository>,
}

impl BlockingPipelineRepository {
    pub fn new(repository: Arc<dyn PipelineRepository>) -> Self {
        Self { repository }
    }
}

impl AsyncPipelineRepository for BlockingPipelineRepository {
    fn execute(&self, operation: RepositoryOperation) -> RepositoryFuture<Result<()>> {
        let repository = Arc::clone(&self.repository);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || operation(repository.as_ref())).await?;
            Ok(())
        })
    }
}
