use super::*;
use std::sync::Arc;

fn async_repository(repository: SqlitePipelineRepository) -> Arc<dyn AsyncPipelineRepository> {
    Arc::new(BlockingPipelineRepository::new(Arc::new(repository)))
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_for_legacy_connection_does_not_block_tokio() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let repository = async_repository(SqlitePipelineRepository::new(Arc::clone(&conn)));
    let (locked_sender, locked_receiver) = tokio::sync::oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = conn.lock();
        locked_sender.send(()).unwrap();
        release_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
    });
    locked_receiver.await.unwrap();
    let query = tokio::spawn(repository.run(|repo| repo.load_combo(ComboId(1))));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!query.is_finished());
    release_sender.send(()).unwrap();
    assert!(query.await.unwrap().unwrap().is_none());
    holder.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn pooled_reads_progress_while_writer_is_locked() {
    let pool = openproxy_db::DbPool::test_pool().unwrap();
    let repository = async_repository(SqlitePipelineRepository::with_db_pool(pool.clone()));
    let (locked_sender, locked_receiver) = tokio::sync::oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = pool.writer();
        locked_sender.send(()).unwrap();
        release_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
    });
    locked_receiver.await.unwrap();
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        repository.run(|repo| repo.load_combo(ComboId(1))),
    )
    .await;
    let write = tokio::spawn(repository.run(|repo| repo.clear_cooldown(ComboTargetId(1))));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let writer_is_waiting = !write.is_finished();
    release_sender.send(()).unwrap();
    holder.join().unwrap();
    assert!(read.unwrap().unwrap().is_none());
    assert!(writer_is_waiting);
    write.await.unwrap().unwrap();
}

#[tokio::test]
async fn async_repository_preserves_operation_errors() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let repository = async_repository(SqlitePipelineRepository::new(conn));
    let error = repository
        .run(|_| Err::<(), _>(openproxy_types::CoreError::ComboNotFound(7)))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        openproxy_types::CoreError::ComboNotFound(7)
    ));
}
