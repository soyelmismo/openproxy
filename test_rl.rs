use governor::{Quota, RateLimiter, state::InMemoryState, state::direct::NotKeyed, clock::DefaultClock};
use std::num::NonZeroU32;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let quota = Quota::per_second(NonZeroU32::new(10).unwrap());
    let limiter = RateLimiter::direct(quota);
    println!("Limiter created");
}
