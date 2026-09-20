#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_db::DbPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn test_try_reader_for_zero_timeout_uncontended() {
    let pool = DbPool::test_pool().expect("test pool");
    let start = Instant::now();
    let guard = pool.try_reader_for(Duration::ZERO);
    let elapsed = start.elapsed();

    assert!(
        guard.is_some(),
        "Uncontended try_reader_for(ZERO) must return Some"
    );
    assert!(
        elapsed < Duration::from_millis(10),
        "Must return immediately"
    );
    let val: i64 = guard
        .expect("guard present")
        .query_row("SELECT 100", [], |r| r.get(0))
        .expect("query");
    assert_eq!(val, 100);
}

#[test]
fn test_try_reader_for_zero_timeout_fully_saturated() {
    let pool = DbPool::test_pool().expect("test pool");
    let count = pool.reader_count();
    assert!(count >= 2, "Expected at least 2 readers");

    // Hold all readers
    let mut held = Vec::new();
    for _ in 0..count {
        held.push(pool.reader_guard());
    }

    let start = Instant::now();
    let result = pool.try_reader_for(Duration::ZERO);
    let elapsed = start.elapsed();

    assert!(
        result.is_none(),
        "Saturated try_reader_for(ZERO) must return None"
    );
    assert!(
        elapsed < Duration::from_millis(10),
        "Saturated try_reader_for(ZERO) took too long: {elapsed:?}"
    );

    // Release one and retry
    drop(held.pop());
    let retry = pool.try_reader_for(Duration::ZERO);
    assert!(
        retry.is_some(),
        "After releasing one reader, try_reader_for(ZERO) must succeed"
    );
}

#[test]
fn test_try_reader_for_microsecond_timeout_saturated() {
    let pool = DbPool::test_pool().expect("test pool");
    let count = pool.reader_count();

    let mut held = Vec::new();
    for _ in 0..count {
        held.push(pool.reader_guard());
    }

    for micros in [1, 5, 25, 100, 500] {
        let dur = Duration::from_micros(micros);
        let start = Instant::now();
        let result = pool.try_reader_for(dur);
        let elapsed = start.elapsed();

        assert!(
            result.is_none(),
            "Saturated try_reader_for({micros}µs) must return None"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "try_reader_for({micros}µs) hung: {elapsed:?}"
        );
    }
}

#[test]
fn test_try_reader_for_microsecond_timeout_uncontended() {
    let pool = DbPool::test_pool().expect("test pool");
    for micros in [1, 10, 100] {
        let start = Instant::now();
        let guard = pool.try_reader_for(Duration::from_micros(micros));
        let elapsed = start.elapsed();

        assert!(
            guard.is_some(),
            "Uncontended try_reader_for({micros}µs) must return Some"
        );
        assert!(
            elapsed < Duration::from_millis(10),
            "Took too long: {elapsed:?}"
        );
    }
}

#[test]
fn test_try_reader_for_acquires_freed_reader_under_saturation() {
    let pool = Arc::new(DbPool::test_pool().expect("test pool"));
    let count = pool.reader_count();

    let mut held = Vec::new();
    for _ in 0..count {
        held.push(pool.reader_guard());
    }

    let to_release = held.pop().expect("held reader");
    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        drop(to_release);
    });

    let start = Instant::now();
    let acquired = pool.try_reader_for(Duration::from_millis(50));
    let elapsed = start.elapsed();

    release_thread.join().expect("join");
    assert!(acquired.is_some(), "Must acquire reader released after 5ms");
    assert!(
        elapsed < Duration::from_millis(45),
        "Should acquire as soon as released, took {elapsed:?}"
    );
}

#[test]
fn test_try_reader_for_high_concurrency_stress() {
    let pool = Arc::new(DbPool::test_pool().expect("test pool"));
    let count = pool.reader_count();
    assert!(count >= 2, "Expected at least 2 readers");

    // Keep count - 1 readers permanently held
    let mut permanent_held = Vec::new();
    for _ in 0..(count - 1) {
        permanent_held.push(pool.reader_guard());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();

    for i in 0..12 {
        let pool = Arc::clone(&pool);
        let stop = Arc::clone(&stop);
        workers.push(std::thread::spawn(move || {
            let mut acquired_count = 0usize;
            let mut timeout_count = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let timeout = match i % 3 {
                    0 => Duration::ZERO,
                    1 => Duration::from_micros(50),
                    _ => Duration::from_millis(2),
                };
                if let Some(guard) = pool.try_reader_for(timeout) {
                    let val: i64 = guard
                        .query_row("SELECT 1", [], |r| r.get(0))
                        .expect("query");
                    assert_eq!(val, 1);
                    acquired_count += 1;
                    std::thread::yield_now();
                } else {
                    timeout_count += 1;
                }
            }
            (acquired_count, timeout_count)
        }));
    }

    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);

    let mut total_acquired = 0;
    let mut total_timed_out = 0;
    for w in workers {
        let (acq, to) = w.join().expect("join");
        total_acquired += acq;
        total_timed_out += to;
    }

    assert!(
        total_acquired > 0,
        "At least some threads must have acquired the contested reader"
    );
    assert!(
        total_timed_out > 0,
        "Under high contention, timeouts must occur"
    );
    drop(permanent_held);
}

#[test]
fn test_empirically_verify_sqlite_pragmas_on_writer_and_all_readers() {
    let pool = DbPool::test_pool().expect("test pool");
    let count = pool.reader_count();
    assert_eq!(count, 2, "Expected exactly 2 readers in pool");

    // 1. Writer PRAGMAs
    {
        let writer = pool.writer();
        let mmap: i64 = writer
            .pragma_query_value(None, "mmap_size", |r| r.get(0))
            .expect("writer mmap_size");
        let temp: i64 = writer
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .expect("writer temp_store");
        let wal: i64 = writer
            .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
            .expect("writer wal_autocheckpoint");
        let cache: i64 = writer
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .expect("writer cache_size");

        assert_eq!(mmap, 0, "Writer mmap_size must be 0");
        assert_eq!(temp, 1, "Writer temp_store must be 1 (FILE)");
        assert_eq!(wal, 250, "Writer wal_autocheckpoint must be 250");
        assert_eq!(cache, -512, "Writer cache_size must be -512 (512 KiB)");
    }

    // 2. Both readers simultaneously held to guarantee distinct connection handles
    {
        let r0 = pool.reader_guard();
        let r1 = pool.reader_guard();

        for (idx, r) in [(0, &r0), (1, &r1)] {
            let mmap: i64 = r
                .pragma_query_value(None, "mmap_size", |r| r.get(0))
                .expect("reader mmap_size");
            let temp: i64 = r
                .pragma_query_value(None, "temp_store", |r| r.get(0))
                .expect("reader temp_store");
            let wal: i64 = r
                .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
                .expect("reader wal_autocheckpoint");
            let cache: i64 = r
                .pragma_query_value(None, "cache_size", |r| r.get(0))
                .expect("reader cache_size");

            assert_eq!(mmap, 0, "Reader {idx} mmap_size must be 0");
            assert_eq!(temp, 1, "Reader {idx} temp_store must be 1 (FILE)");
            assert_eq!(wal, 250, "Reader {idx} wal_autocheckpoint must be 250");
            assert_eq!(
                cache, -256,
                "Reader {idx} cache_size must be -256 (256 KiB)"
            );
        }
    }

    // 3. Extra connection opened via open_connection()
    {
        let extra = pool.open_connection().expect("open extra connection");
        let mmap: i64 = extra
            .pragma_query_value(None, "mmap_size", |r| r.get(0))
            .expect("extra mmap_size");
        let temp: i64 = extra
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .expect("extra temp_store");
        let wal: i64 = extra
            .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
            .expect("extra wal_autocheckpoint");
        let cache: i64 = extra
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .expect("extra cache_size");

        assert_eq!(mmap, 0, "Extra connection mmap_size must be 0");
        assert_eq!(temp, 1, "Extra connection temp_store must be 1 (FILE)");
        assert_eq!(wal, 250, "Extra connection wal_autocheckpoint must be 250");
        assert_eq!(
            cache, -256,
            "Extra connection cache_size must be -256 (256 KiB)"
        );
    }

    // 4. Persistence across reopen()
    pool.reopen().expect("pool reopen");
    {
        let writer = pool.writer();
        let mmap: i64 = writer
            .pragma_query_value(None, "mmap_size", |r| r.get(0))
            .expect("reopened writer mmap_size");
        let temp: i64 = writer
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .expect("reopened writer temp_store");
        let wal: i64 = writer
            .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
            .expect("reopened writer wal_autocheckpoint");
        let cache: i64 = writer
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .expect("reopened writer cache_size");

        assert_eq!(mmap, 0);
        assert_eq!(temp, 1);
        assert_eq!(wal, 250);
        assert_eq!(cache, -512);
    }
}
