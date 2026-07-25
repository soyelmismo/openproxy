use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let start_seq = Instant::now();
    for i in 0..20 {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // I/O
        tokio::time::sleep(Duration::from_millis(20)).await; // settle gap
    }
    println!("Sequential: {:?}", start_seq.elapsed());

    let start_conc = Instant::now();
    use governor::{Quota, RateLimiter};
    use std::num::NonZeroU32;
    use std::sync::Arc;

    let quota = Quota::with_period(Duration::from_millis(30))
        .unwrap()
        .allow_burst(NonZeroU32::new(1).unwrap());
    let limiter = Arc::new(RateLimiter::direct(quota));

    let mut handles = vec![];
    for _ in 0..20 {
        let lim = limiter.clone();
        handles.push(tokio::spawn(async move {
            lim.until_ready().await;
            tokio::time::sleep(Duration::from_millis(50)).await; // I/O
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    println!("Concurrent: {:?}", start_conc.elapsed());
}
