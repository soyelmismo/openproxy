//! Real-socket tests for the io_uring stream: echo round-trips, read
//! without prior write, shutdown, cancel-on-drop, many concurrent streams,
//! full-duplex traffic on one stream and small read buffers.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::stream::UringStream;

const TIMEOUT: Duration = Duration::from_secs(10);

async fn pair() -> (UringStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let client = TcpStream::connect(addr).await.expect("connect");
    let (server, _) = listener.accept().await.expect("accept");
    // Same handoff as `Sock::from_tcp`: leave tokio's reactor, then let the
    // ring own the blocking-mode descriptor.
    let client = client.into_std().expect("into_std");
    client.set_nonblocking(false).expect("blocking");
    (UringStream::from_std(client), server)
}

async fn echo_exact(mut server: TcpStream, len: usize) {
    let mut buf = vec![0u8; len];
    server.read_exact(&mut buf).await.expect("echo read");
    server.write_all(&buf).await.expect("echo write");
}

#[tokio::test]
async fn echo_roundtrip() {
    let (mut client, server) = pair().await;
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let echo = tokio::spawn(echo_exact(server, payload.len()));
    client.write_all(&payload).await.expect("client write");
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(TIMEOUT, client.read_exact(&mut got))
        .await
        .expect("read timeout")
        .expect("client read");
    assert_eq!(got, payload);
    tokio::time::timeout(TIMEOUT, echo)
        .await
        .expect("echo join timeout")
        .expect("echo join");
}

#[tokio::test]
async fn read_without_prior_write() {
    let (mut client, mut server) = pair().await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.write_all(b"server-first").await.expect("write");
    });
    let mut got = [0u8; 12];
    tokio::time::timeout(TIMEOUT, client.read_exact(&mut got))
        .await
        .expect("read timeout")
        .expect("client read");
    assert_eq!(&got, b"server-first");
}

#[tokio::test]
async fn small_read_buf_loses_nothing() {
    let (mut client, mut server) = pair().await;
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 253) as u8).collect();
    server.write_all(&payload).await.expect("write");
    let mut got = Vec::new();
    let mut chunk = [0u8; 7];
    while got.len() < payload.len() {
        let n = tokio::time::timeout(TIMEOUT, client.read(&mut chunk))
            .await
            .expect("read timeout")
            .expect("read");
        assert!(n > 0, "early eof");
        got.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(got, payload);
}

#[tokio::test]
async fn shutdown_reports_eof() {
    let (mut client, mut server) = pair().await;
    client.write_all(b"bye").await.expect("write");
    tokio::time::timeout(TIMEOUT, client.shutdown())
        .await
        .expect("shutdown timeout")
        .expect("shutdown");
    let mut buf = Vec::new();
    tokio::time::timeout(TIMEOUT, server.read_to_end(&mut buf))
        .await
        .expect("read timeout")
        .expect("server read");
    assert_eq!(buf, b"bye");
}

#[tokio::test]
async fn eof_after_peer_close() {
    let (mut client, server) = pair().await;
    drop(server);
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(TIMEOUT, client.read(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_duplex_on_one_stream() {
    // h2 polls read and write of one connection concurrently; both
    // directions must progress without stealing each other's completion.
    let (client, mut server) = pair().await;
    let (mut rd, mut wr) = tokio::io::split(client);
    let up: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
    let down: Vec<u8> = (0..300_000u32).map(|i| (i % 239) as u8).collect();
    let (up2, down2) = (up.clone(), down.clone());
    let peer = tokio::spawn(async move {
        let (mut srd, mut swr) = server.split();
        let reader = async {
            let mut got = vec![0u8; up2.len()];
            srd.read_exact(&mut got).await.expect("peer read");
            got
        };
        let writer = async { swr.write_all(&down2).await.expect("peer write") };
        let (got, ()) = tokio::join!(reader, writer);
        assert_eq!(got, up2);
    });
    let writer = tokio::spawn(async move { wr.write_all(&up).await.expect("write") });
    let mut got = vec![0u8; down.len()];
    tokio::time::timeout(TIMEOUT, rd.read_exact(&mut got))
        .await
        .expect("read timeout")
        .expect("read");
    assert_eq!(got, down);
    tokio::time::timeout(TIMEOUT, writer)
        .await
        .expect("writer timeout")
        .expect("writer");
    tokio::time::timeout(TIMEOUT, peer)
        .await
        .expect("peer timeout")
        .expect("peer");
}

#[tokio::test]
async fn cancel_on_drop_keeps_driver_alive() {
    let (mut client, server) = pair().await;
    // Start a read that can never complete, then drop the stream mid-flight.
    let pending = tokio::spawn(async move {
        let mut buf = [0u8; 16];
        let _ = client.read(&mut buf).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    pending.abort();
    let _ = pending.await;

    // The cancelled op releases the fd: the peer sees EOF.
    let mut server = server;
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, server.read(&mut buf))
        .await
        .expect("peer eof timeout")
        .expect("peer read");
    assert_eq!(n, 0);

    // The driver still serves fresh streams after the cancel.
    let (mut client2, mut server2) = pair().await;
    server2.write_all(b"still-alive").await.expect("write");
    let mut got = [0u8; 11];
    tokio::time::timeout(TIMEOUT, client2.read_exact(&mut got))
        .await
        .expect("read timeout")
        .expect("client read");
    assert_eq!(&got, b"still-alive");
}

#[tokio::test]
async fn cancelled_write_does_not_ack_old_bytes_as_new_buffer() {
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use tokio::io::AsyncWrite;

    let (mut client, mut server) = pair().await;
    let mut cx = Context::from_waker(Waker::noop());
    let first = Pin::new(&mut client).poll_write(&mut cx, b"first-buffer");
    tokio::time::sleep(Duration::from_millis(20)).await;
    // A caller can drop a write future after Pending, then use a different
    // slice. An adapter must never report the old length for that new slice.
    let second = Pin::new(&mut client).poll_write(&mut cx, b"x");
    if let Poll::Ready(Ok(n)) = second {
        assert!(
            n <= 1,
            "poll_write acknowledged {n} bytes for a 1-byte slice"
        );
    }
    assert!(
        matches!(first, Poll::Ready(Ok(12))),
        "accepted data must be acknowledged on the SAME poll: {first:?}"
    );
    tokio::time::timeout(TIMEOUT, client.flush())
        .await
        .unwrap()
        .unwrap();
    let mut wire = [0; 13];
    tokio::time::timeout(TIMEOUT, server.read_exact(&mut wire))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&wire, b"first-bufferx");
}

#[tokio::test]
async fn empty_read_returns_immediately_without_socket_traffic() {
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use tokio::io::{AsyncRead, ReadBuf};

    let (mut client, _server) = pair().await;
    let mut cx = Context::from_waker(Waker::noop());
    let mut data = [];
    let mut buf = ReadBuf::new(&mut data);
    assert!(matches!(
        Pin::new(&mut client).poll_read(&mut cx, &mut buf),
        Poll::Ready(Ok(()))
    ));
}

#[tokio::test]
async fn flush_and_shutdown_surface_write_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let client = TcpStream::connect(listener.local_addr().expect("addr"))
        .await
        .expect("connect");
    let (_server, _) = listener.accept().await.expect("accept");
    let client = client.into_std().expect("into_std");
    client.set_nonblocking(false).expect("blocking");
    // Deterministic EPIPE, independent of peer scheduling or TCP reset timing.
    client.shutdown(std::net::Shutdown::Write).expect("SHUT_WR");
    let mut client = UringStream::from_std(client);
    client
        .write_all(b"cannot-send")
        .await
        .expect("buffer accepted");
    let err = tokio::time::timeout(TIMEOUT, client.flush())
        .await
        .expect("flush timeout")
        .expect_err("flush must expose EPIPE");
    assert_eq!(err.raw_os_error(), Some(libc::EPIPE));
    let err = tokio::time::timeout(TIMEOUT, client.shutdown())
        .await
        .expect("shutdown timeout")
        .expect_err("shutdown must preserve write error");
    assert_eq!(err.raw_os_error(), Some(libc::EPIPE));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_ring_keeps_kick_reader_and_all_operations_live() {
    use super::driver::{Driver, Op};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    // Far smaller than the 4096-entry production SQ. Hundreds of ops force
    // full-queue, kick/rearm and concurrent producer interleavings.
    let driver = Driver::spawn_with_entries(2).expect("tiny ring");
    let mut peers = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..256 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let socket = TcpStream::connect(listener.local_addr().expect("addr"))
            .await
            .expect("connect");
        let (peer, _) = listener.accept().await.expect("accept");
        let socket = socket.into_std().expect("into_std");
        socket.set_nonblocking(false).expect("blocking");
        let fd = Arc::new(OwnedFd::from(socket));
        let driver = Arc::clone(&driver);
        readers.push(tokio::spawn(async move {
            let slot = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(driver.submit(&fd, Op::Read { buf: vec![0; 1] }, cx))
            })
            .await
            .expect("submit");
            let result = std::future::poll_fn(|cx| slot.poll_result(cx)).await;
            assert_eq!(result, 1);
        }));
        peers.push(peer);
    }
    for peer in &mut peers {
        peer.write_all(b"k").await.expect("peer write");
    }
    for reader in readers {
        tokio::time::timeout(TIMEOUT, reader)
            .await
            .expect("tiny ring stuck")
            .expect("join");
    }
}

#[tokio::test]
async fn submit_errors_retry_or_fail_all_slots_without_hanging() {
    use super::driver::{Driver, Op};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    for errno in [libc::EINTR, libc::EBUSY, libc::ENOMEM, libc::EIO] {
        let driver = Driver::spawn_with_entries(2).expect("ring");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let socket = TcpStream::connect(listener.local_addr().expect("addr"))
            .await
            .expect("connect");
        let (mut peer, _) = listener.accept().await.expect("accept");
        let socket = socket.into_std().expect("into_std");
        socket.set_nonblocking(false).expect("blocking");
        let fd = Arc::new(OwnedFd::from(socket));
        let slot = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(driver.submit(&fd, Op::Read { buf: vec![0; 1] }, cx))
        })
        .await
        .expect("submit");
        driver.inject_submit_error(errno);
        if errno != libc::EIO {
            peer.write_all(b"r").await.expect("write");
        }
        let result = tokio::time::timeout(TIMEOUT, std::future::poll_fn(|cx| slot.poll_result(cx)))
            .await
            .expect("slot stuck after submit error");
        assert_eq!(result, if errno == libc::EIO { -libc::EIO } else { 1 });
        if errno == libc::EIO {
            let result = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(driver.submit(&fd, Op::Shutdown, cx))
            })
            .await;
            assert!(
                result.is_err_and(|e| e.raw_os_error() == Some(libc::EIO)),
                "dead driver must reject new slots"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_streams() {
    let mut handles = Vec::new();
    for i in 0..64u32 {
        handles.push(tokio::spawn(async move {
            let (mut client, server) = pair().await;
            let payload: Vec<u8> = (0..8_192u32).map(|j| ((i + j) % 251) as u8).collect();
            tokio::spawn(echo_exact(server, payload.len()));
            client.write_all(&payload).await.expect("write");
            let mut got = vec![0u8; payload.len()];
            tokio::time::timeout(TIMEOUT, client.read_exact(&mut got))
                .await
                .expect("timeout")
                .expect("read");
            assert_eq!(got, payload);
        }));
    }
    for handle in handles {
        tokio::time::timeout(TIMEOUT, handle)
            .await
            .expect("stream timeout")
            .expect("stream join");
    }
}
