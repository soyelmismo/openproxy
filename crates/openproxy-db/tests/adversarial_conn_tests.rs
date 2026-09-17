use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use openproxy_db::DbPool;

#[test]
fn test_try_reader_for_zero_timeout_uncontended() {
    let pool = DbPool::test_pool().expect("test pool");
    let start = Instant::now();
    let guard = pool.try_reader_for(Duration::ZERO);
    let elapsed = start.elapsed();

    assert!(guard.is_some(), "Uncontended try_reader_for(ZERO) must return Some");
    assert!(elapsed < Duration::from_millis(10), "Must return immediately");
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

    assert!(result.is_none(), "Saturated try_reader_for(ZERO) must return None");
    assert!(
        elapsed < Duration::from_millis(10),
        "Saturated try_reader_for(ZERO) took too long: {elapsed:?}"
    );

    // Release one and retry
    drop(held.pop());
    let retry = pool.try_reader_for(Duration::ZERO);
    assert!(retry.is_some(), "After releasing one reader, try_reader_for(ZERO) must succeed");
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

        assert!(guard.is_some(), "Uncontended try_reader_for({micros}µs) must return Some");
        assert!(elapsed < Duration::from_millis(10), "Took too long: {elapsed:?}");
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
    assert!(count >= 4);

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
                    let val: i64 = guard.query_row("SELECT 1", [], |r| r.get(0)).expect("query");
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

    assert!(total_acquired > 0, "At least some threads must have acquired the contested reader");
    assert!(total_timed_out > 0, "Under high contention, timeouts must occur");
    drop(permanent_held);
}
