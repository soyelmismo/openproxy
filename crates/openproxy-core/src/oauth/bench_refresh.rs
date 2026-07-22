use std::time::Instant;

#[tokio::main]
pub async fn main() {
    let start = Instant::now();
    // Simulate 20 accounts
    for i in 0..20 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        // simulate refresh
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // simulate settle gap
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!("Baseline took: {:?}", start.elapsed());
}
