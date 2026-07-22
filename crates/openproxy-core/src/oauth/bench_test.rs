use futures::stream::{StreamExt, FuturesUnordered};

#[tokio::main]
async fn main() {
    let start = std::time::Instant::now();
    let mut tasks = FuturesUnordered::new();
    let accounts: Vec<usize> = (0..20).collect();

    // Using a token bucket with futures unordered to bound concurrency
    let rate_limit = 20.0 / 60.0; // 20 per minute

    // Simulate token bucket with 3s stagger
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));

    for _ in 0..20 {
        interval.tick().await;
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }));
    }

    while let Some(_) = tasks.next().await {}

    println!("Fast took: {:?}", start.elapsed());
}
