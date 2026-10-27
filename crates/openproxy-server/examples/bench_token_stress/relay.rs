//! Transparent loopback TCP relay for benchmark network emulation.
//!
//! Inserts artificial one-way delays per direction without OS-level netns,
//! tc, or firewall mutation.
//!
//! Note on latency: `--network-delay-ms N` adds an artificial delay of N ms
//! in each direction (client->mock and mock->client). Because the request and
//! response legs are delayed independently, observed RTT increases by at least
//! 2 * N ms plus scheduling and serialization overhead. RTT is not guaranteed
//! to equal 2 * N ms exactly.

use anyhow::Context as _;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Spawn a loopback TCP relay listening on an ephemeral port that forwards
/// connections to `target_addr`.
///
/// Returns the bound local `SocketAddr` and a broadcast sender to signal shutdown.
pub async fn start_relay(
    target_addr: SocketAddr,
    delay_ms: u64,
) -> anyhow::Result<(SocketAddr, broadcast::Sender<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind ephemeral relay listener")?;
    let local_addr = listener.local_addr()?;
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let shutdown_rx = shutdown_tx.clone();
    let shutdown_conn_tx = shutdown_tx.clone();

    tokio::spawn(async move {
        let mut shutdown = shutdown_rx.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    break;
                }
                accept_res = listener.accept() => {
                    let (inbound, _) = match accept_res {
                        Ok(conn) => conn,
                        Err(err) => {
                            tracing::debug!("relay accept error: {err}");
                            break;
                        }
                    };
                    let shutdown_conn = shutdown_conn_tx.subscribe();
                    tokio::spawn(async move {
                        if let Err(e) =
                            Box::pin(handle_connection(inbound, target_addr, delay_ms, shutdown_conn)).await
                        {
                            tracing::debug!("relay connection error: {e}");
                        }
                    });
                }
            }
        }
    });

    Ok((local_addr, shutdown_tx))
}

async fn handle_connection(
    mut inbound: TcpStream,
    target_addr: SocketAddr,
    delay_ms: u64,
    mut shutdown: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let mut outbound = TcpStream::connect(target_addr)
        .await
        .context("relay connect to upstream target")?;

    let (mut in_read, mut in_write) = inbound.split();
    let (mut out_read, mut out_write) = outbound.split();

    let delay = Duration::from_millis(delay_ms);

    // Forward inbound -> outbound
    let client_to_server = async {
        let mut buf = [0u8; 16384];
        loop {
            let n = in_read.read(&mut buf).await?;
            if n == 0 {
                out_write.shutdown().await?;
                break;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            out_write.write_all(&buf[..n]).await?;
            out_write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    // Forward outbound -> inbound
    let server_to_client = async {
        let mut buf = [0u8; 16384];
        loop {
            let n = out_read.read(&mut buf).await?;
            if n == 0 {
                in_write.shutdown().await?;
                break;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            in_write.write_all(&buf[..n]).await?;
            in_write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::select! {
        _ = shutdown.recv() => {
            // Graceful abort on runner shutdown
        }
        res = client_to_server => {
            if let Err(e) = res {
                tracing::debug!("relay client_to_server ended with error: {e}");
            }
        }
        res = server_to_client => {
            if let Err(e) = res {
                tracing::debug!("relay server_to_client ended with error: {e}");
            }
        }
    }

    Ok(())
}
