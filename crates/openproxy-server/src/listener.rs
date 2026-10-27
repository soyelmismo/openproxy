//! TCP listeners with a backlog sized for bursts of concurrent streams.
//!
//! `TcpListener::bind` inherits the standard library backlog of 128. When
//! hundreds of clients connect at once the accept queue overflows, the kernel
//! drops SYNs and each dropped client waits for a 1 s SYN retransmit before it
//! can even send its request. The kernel caps the value at
//! `net.core.somaxconn`, so asking for more is harmless on small hosts.

use std::io;
use tokio::net::{TcpListener, TcpSocket};

/// Accept queue requested for public and admin listeners.
pub const LISTEN_BACKLOG: u32 = 4096;

/// Binds the first address `addr` resolves to, with [`LISTEN_BACKLOG`].
pub async fn bind(addr: &str) -> io::Result<TcpListener> {
    let mut last_error = None;
    for resolved in tokio::net::lookup_host(addr).await? {
        let socket = if resolved.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        // Same as `TcpListener::bind` on Unix: allow fast restarts while old
        // connections sit in TIME_WAIT.
        #[cfg(unix)]
        socket.set_reuseaddr(true)?;
        match socket.bind(resolved) {
            Ok(()) => return socket.listen(LISTEN_BACKLOG),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "address resolved to nothing")
    }))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn binds_ephemeral_loopback_port() {
        let listener = super::bind("127.0.0.1:0").await.unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn rejects_port_already_in_use() {
        let first = super::bind("127.0.0.1:0").await.unwrap();
        let taken = first.local_addr().unwrap().to_string();
        assert!(super::bind(&taken).await.is_err());
    }
}
