use std::time::Instant;

#[tokio::test]
async fn bench_oauth_refresh_baseline() {
    let start = Instant::now();
    for i in 0..20 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    println!("Baseline took: {:?}", start.elapsed());
}
