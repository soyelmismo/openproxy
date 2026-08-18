//! Time measurement helpers.

use std::sync::LazyLock;
use std::time::Instant;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Returns monotonic milliseconds elapsed since proxy startup (always > 0).
#[inline]
pub fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_ms_advances() {
        let t1 = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t2 = now_ms();
        assert!(t2 >= t1);
    }
}
