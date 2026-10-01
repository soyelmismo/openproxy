use super::*;
use openproxy_types::config::SqliteSynchronous;

#[test]
fn reader_count_is_explicit_and_bounded() {
    let directory = crate::testing::TempDir::new("reader-count").unwrap();
    let path = directory.path().join("db.sqlite");
    let pool = DbPool::open_with_readers(&path, 4).unwrap();
    assert_eq!(pool.reader_count(), 4);
    assert!(DbPool::open_with_readers(&path, 33).is_err());
}

#[test]
fn open_creates_file_and_sets_pragmas() {
    let pool = DbPool::test_pool().expect("test pool");
    assert!((2..=8).contains(&pool.reader_count()));
    assert_eq!(pool.synchronous(), SqliteSynchronous::Full);
    let conn = pool.writer();

    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(journal.to_ascii_lowercase(), "wal");

    let fk: i64 = conn
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .expect("foreign_keys");
    assert_eq!(fk, 1);

    let busy: i64 = conn
        .pragma_query_value(None, "busy_timeout", |r| r.get(0))
        .expect("busy_timeout");
    assert_eq!(busy, 5000);

    let sync_val: i64 = conn
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .expect("synchronous");
    assert_eq!(sync_val, 2, "Default must be FULL (2)");

    let mmap: i64 = conn
        .pragma_query_value(None, "mmap_size", |r| r.get(0))
        .expect("mmap_size");
    assert_eq!(mmap, 0);

    let wal_autocheckpoint: i64 = conn
        .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
        .expect("wal_autocheckpoint");
    assert_eq!(wal_autocheckpoint, 250);

    let writer_cache: i64 = conn
        .pragma_query_value(None, "cache_size", |r| r.get(0))
        .expect("cache_size");
    assert_eq!(writer_cache, -512);

    let temp_store: i64 = conn
        .pragma_query_value(None, "temp_store", |r| r.get(0))
        .expect("temp_store");
    assert_eq!(temp_store, 1); // 1 = FILE

    drop(conn);

    let reader = pool.reader();
    let reader_cache: i64 = reader
        .pragma_query_value(None, "cache_size", |r| r.get(0))
        .expect("reader cache_size");
    assert_eq!(reader_cache, -256);

    let reader_sync: i64 = reader
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .expect("reader synchronous");
    assert_eq!(reader_sync, 2, "Reader default must be FULL (2)");

    let reader_mmap: i64 = reader
        .pragma_query_value(None, "mmap_size", |r| r.get(0))
        .expect("reader mmap_size");
    assert_eq!(reader_mmap, 0);
}

#[test]
fn pragmas_honor_selected_synchronous_modes() {
    let dir = crate::testing::TempDir::new("sync-modes").unwrap();

    // 1. Explicit Normal mode
    let path_normal = dir.path().join("normal.db");
    let pool_normal =
        DbPool::open_with_options(&path_normal, 2, SqliteSynchronous::Normal).unwrap();
    assert_eq!(pool_normal.synchronous(), SqliteSynchronous::Normal);

    let w_sync: i64 = pool_normal
        .writer()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(w_sync, 1, "Normal mode writer must have synchronous = 1");

    let r_sync: i64 = pool_normal
        .reader()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(r_sync, 1, "Normal mode reader must have synchronous = 1");

    let extra_conn = pool_normal.open_connection().unwrap();
    let extra_sync: i64 = extra_conn
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        extra_sync, 1,
        "Normal mode extra connection must inherit synchronous = 1"
    );

    // 2. Explicit Full mode
    let path_full = dir.path().join("full.db");
    let pool_full = DbPool::open_with_options(&path_full, 2, SqliteSynchronous::Full).unwrap();
    assert_eq!(pool_full.synchronous(), SqliteSynchronous::Full);

    let w_full_sync: i64 = pool_full
        .writer()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(w_full_sync, 2, "Full mode writer must have synchronous = 2");

    let r_full_sync: i64 = pool_full
        .reader()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(r_full_sync, 2, "Full mode reader must have synchronous = 2");

    let extra_full = pool_full.open_connection().unwrap();
    let extra_full_sync: i64 = extra_full
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        extra_full_sync, 2,
        "Full mode extra connection must inherit synchronous = 2"
    );

    // 3. open_with_readers defaults to safe Full mode
    let path_readers = dir.path().join("readers.db");
    let pool_readers = DbPool::open_with_readers(&path_readers, 2).unwrap();
    assert_eq!(pool_readers.synchronous(), SqliteSynchronous::Full);
    let r_def_sync: i64 = pool_readers
        .reader()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(r_def_sync, 2);

    // 4. open defaults to safe Full mode
    let path_open = dir.path().join("open.db");
    let pool_open = DbPool::open(&path_open).unwrap();
    assert_eq!(pool_open.synchronous(), SqliteSynchronous::Full);
    let o_sync: i64 = pool_open
        .writer()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(o_sync, 2);

    // 5. reopen preserves selected synchronous mode (Normal)
    pool_normal.reopen().unwrap();
    let reopen_normal_w: i64 = pool_normal
        .writer()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        reopen_normal_w, 1,
        "Reopened Normal pool writer must retain synchronous = 1"
    );
    let reopen_normal_r: i64 = pool_normal
        .reader()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        reopen_normal_r, 1,
        "Reopened Normal pool reader must retain synchronous = 1"
    );

    // 6. reopen preserves selected synchronous mode (Full)
    pool_full.reopen().unwrap();
    let reopen_full_w: i64 = pool_full
        .writer()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        reopen_full_w, 2,
        "Reopened Full pool writer must retain synchronous = 2"
    );
    let reopen_full_r: i64 = pool_full
        .reader()
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_eq!(
        reopen_full_r, 2,
        "Reopened Full pool reader must retain synchronous = 2"
    );
}

#[test]
fn try_writer_for_returns_none_when_lock_is_held() {
    let pool = DbPool::test_pool().expect("test pool");

    let _guard = pool.writer();

    let start = std::time::Instant::now();
    let result = pool.try_writer_for(std::time::Duration::from_millis(50));
    let elapsed = start.elapsed();

    assert!(result.is_none(), "lock should not be acquirable while held");
    assert!(
        elapsed < std::time::Duration::from_millis(150),
        "try_writer_for waited {elapsed:?}; should have failed fast"
    );
}

#[test]
fn try_writer_for_succeeds_when_lock_is_free() {
    let pool = DbPool::test_pool().expect("test pool");

    let start = std::time::Instant::now();
    let guard = pool
        .try_writer_for(std::time::Duration::from_millis(100))
        .expect("lock should be available");
    let elapsed = start.elapsed();

    assert!(elapsed < std::time::Duration::from_millis(50));
    drop(guard);
}

#[tokio::test]
async fn test_spawn_read_and_spawn_write() {
    let pool = DbPool::test_pool().expect("test pool");

    // Write via spawn_write
    pool.spawn_write(|conn| {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS test_spawn (id INTEGER PRIMARY KEY, val TEXT)",
            [],
        )
        .map_err(|e| CoreError::Database {
            message: e.to_string(),
            source: None,
        })?;
        conn.execute("INSERT INTO test_spawn (id, val) VALUES (1, 'hello')", [])
            .map_err(|e| CoreError::Database {
                message: e.to_string(),
                source: None,
            })?;
        Ok(())
    })
    .await
    .expect("spawn_write");

    // Read via spawn_read
    let val: String = pool
        .spawn_read(|conn| {
            conn.query_row("SELECT val FROM test_spawn WHERE id = 1", [], |row| {
                row.get(0)
            })
            .map_err(|e| CoreError::Database {
                message: e.to_string(),
                source: None,
            })
        })
        .await
        .expect("spawn_read");

    assert_eq!(val, "hello");
}

#[test]
fn reader_opportunistically_bypasses_held_reader_without_blocking() {
    let pool = DbPool::test_pool().expect("test pool");
    assert!(pool.readers.len() >= 2, "pool must have multiple readers");

    // Force next_reader to index 0
    pool.next_reader.store(0, Ordering::Relaxed);

    // Lock reader 0 directly to simulate a long-running query
    let _held_guard = pool.readers[0].lock();

    let start = std::time::Instant::now();
    let acquired = pool.reader();
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "reader() took {elapsed:?}; should have bypassed reader 0 immediately"
    );

    let val: i64 = acquired
        .query_row("SELECT 42", [], |row| row.get(0))
        .expect("query_row on reader");
    assert_eq!(val, 42);
    drop(acquired);

    // Also test reader_guard bypasses reader 0
    pool.next_reader.store(0, Ordering::Relaxed);
    let guard = pool.reader_guard();
    let val_guard: i64 = guard
        .query_row("SELECT 84", [], |row| row.get(0))
        .expect("query_row on reader_guard");
    assert_eq!(val_guard, 84);
}

#[test]
fn reader_contention_with_one_held_reader() {
    let pool = std::sync::Arc::new(DbPool::test_pool().expect("test pool"));
    assert!(pool.readers.len() >= 2);
    let _held = pool.readers[0].lock();

    let (tx, rx) = std::sync::mpsc::channel();

    let pool_clone = std::sync::Arc::clone(&pool);
    std::thread::spawn(move || {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = std::sync::Arc::clone(&pool_clone);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let guard = pool.reader();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    let val: i64 = guard.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
                    assert_eq!(val, 1);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let _ = tx.send(());
    });

    let res = rx.recv_timeout(std::time::Duration::from_secs(3));
    assert!(
        res.is_ok(),
        "DEADLOCK DETECTED: reader acquisition blocked on locked reader 0!"
    );
}

#[test]
fn try_reader_for_times_out_when_all_readers_locked() {
    let pool = DbPool::test_pool().expect("test pool");

    let _guards: Vec<_> = pool.readers.iter().map(|r| r.lock()).collect();

    let start = std::time::Instant::now();
    let result = pool.try_reader_for(std::time::Duration::from_millis(50));
    let elapsed = start.elapsed();

    assert!(
        result.is_none(),
        "try_reader_for must return None when all readers are locked"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(40),
        "try_reader_for must wait for the specified timeout: elapsed {elapsed:?}"
    );
}

#[test]
fn try_reader_for_acquires_freed_reader_when_other_readers_held() {
    let pool = std::sync::Arc::new(DbPool::test_pool().expect("test pool"));
    pool.next_reader.store(0, Ordering::Relaxed);

    let held: Vec<_> = pool
        .readers
        .iter()
        .skip(1)
        .map(|reader| reader.lock())
        .collect();
    let release_pool = Arc::clone(&pool);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let release_thread = std::thread::spawn(move || {
        let guard = release_pool.readers[0].lock();
        ready_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        drop(guard);
    });
    ready_rx.recv().unwrap();

    let acquired = pool.try_reader_for(std::time::Duration::from_millis(60));
    release_thread.join().unwrap();

    assert!(
        acquired.is_some(),
        "try_reader_for should acquire reader 1 once freed before timeout!"
    );
    drop(held);
}
