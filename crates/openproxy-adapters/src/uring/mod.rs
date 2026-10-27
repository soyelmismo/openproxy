//! Socket I/O over `io_uring` for the upstream transport.
//!
//! Production runs tokio multihread, so thread-per-core runtimes (monoio,
//! glommio, tokio-uring) do not fit. This module instead keeps one dedicated
//! OS thread that owns the ring and serves every socket in the process:
//! tokio tasks submit reads/writes through a channel, completions wake the
//! owning task through its waker, and a buffer stays pinned inside the
//! submission slot until the kernel is done with it.
//!
//! Opt-in via `OPENPROXY_IO_URING=1`. When the ring is unavailable the
//! driver fails to spawn and callers fall back to tokio sockets.

pub(crate) mod driver;
pub(crate) mod stream;
#[cfg(test)]
mod tests;

use std::sync::{Arc, LazyLock};

pub(crate) use driver::Driver;
pub use stream::UringStream;

static DRIVER: LazyLock<Option<Arc<Driver>>> = LazyLock::new(|| match Driver::spawn() {
    Ok(driver) => Some(driver),
    Err(err) => {
        tracing::warn!(error = %err, "io_uring unavailable; upstream sockets stay on tokio");
        None
    }
});

static REQUESTED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("OPENPROXY_IO_URING").is_some_and(|v| v != "0" && !v.is_empty())
});

/// The shared driver, or `None` when this kernel refuses io_uring.
pub(crate) fn driver() -> Option<&'static Arc<Driver>> {
    DRIVER.as_ref()
}

/// True when the operator asked for io_uring and the ring came up. The env
/// var is read once; flipping it at runtime does not migrate live sockets.
pub(crate) fn enabled() -> bool {
    *REQUESTED && driver().is_some()
}
