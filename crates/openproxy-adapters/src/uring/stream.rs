//! `AsyncRead`/`AsyncWrite` socket backed by the shared io_uring driver.
//!
//! Reads and writes have independent slots. The read tail survives small
//! caller buffers. Writes are bounded and buffered: accepting bytes returns
//! `Ready(Ok(n))` on that SAME poll, never `Pending` with a write side effect.
//! `poll_flush` drains accepted bytes (including partial sends) and surfaces
//! errors; `poll_shutdown` flushes before SHUT_WR. A cancelled write future
//! can therefore present a different buffer without misattributing old bytes.

use std::io;
use std::net::TcpStream as StdTcpStream;
use std::os::unix::io::OwnedFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::driver::{Driver, Op, Slot};

const IO_CAP: usize = 32 * 1024;

pub struct UringStream {
    fd: Arc<OwnedFd>,
    read_buf: Vec<u8>,
    read_slot: Option<Arc<Slot>>,
    leftover: Option<(Vec<u8>, usize, usize)>,
    write_slot: Option<Arc<Slot>>,
    write_error: Option<i32>,
    shutdown_in_flight: bool,
    write_closed: bool,
}

fn is_retry(result: i32) -> bool {
    result == -libc::EAGAIN || result == -libc::EINTR
}

fn driver() -> io::Result<&'static Driver> {
    super::driver()
        .map(AsRef::as_ref)
        .ok_or_else(|| io::Error::other("io_uring driver unavailable"))
}

impl UringStream {
    /// The caller must deregister from tokio using `into_std()` first.
    pub(crate) fn from_std(stream: StdTcpStream) -> Self {
        Self {
            fd: Arc::new(OwnedFd::from(stream)),
            read_buf: vec![0; IO_CAP],
            read_slot: None,
            leftover: None,
            write_slot: None,
            write_error: None,
            shutdown_in_flight: false,
            write_closed: false,
        }
    }

    fn serve_leftover(&mut self, buf: &mut ReadBuf<'_>) -> bool {
        let Some((data, pos, len)) = self.leftover.as_mut() else {
            return false;
        };
        let n = (*len - *pos).min(buf.remaining());
        buf.put_slice(&data[*pos..*pos + n]);
        *pos += n;
        if *pos == *len
            && let Some((data, _, _)) = self.leftover.take()
        {
            self.read_buf = data;
        }
        true
    }

    fn flush_write(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(errno) = self.write_error {
            return Poll::Ready(Err(io::Error::from_raw_os_error(errno)));
        }
        while let Some(slot) = self.write_slot.clone() {
            let result = ready!(slot.poll_result(cx));
            self.write_slot = None;
            let Some(Op::Write { data, mut pos }) = slot.take_op() else {
                return Poll::Ready(Err(io::Error::other("io_uring write slot lost its buffer")));
            };
            if is_retry(result) {
                self.write_slot = Some(driver()?.submit(&self.fd, Op::Write { data, pos }, cx)?);
                return Poll::Pending;
            }
            if result <= 0 {
                let errno = if result == 0 { libc::EIO } else { -result };
                self.write_error = Some(errno);
                return Poll::Ready(Err(io::Error::from_raw_os_error(errno)));
            }
            pos += result as usize;
            if pos < data.len() {
                self.write_slot = Some(driver()?.submit(&self.fd, Op::Write { data, pos }, cx)?);
                return Poll::Pending;
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for UringStream {
    fn drop(&mut self) {
        // The driver's slots retain the fd/buffers until the final CQE.
        let Some(driver) = super::driver() else {
            return;
        };
        for slot in [self.read_slot.take(), self.write_slot.take()]
            .into_iter()
            .flatten()
        {
            driver.cancel(&slot);
        }
    }
}

impl AsyncRead for UringStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.serve_leftover(buf) {
                return Poll::Ready(Ok(()));
            }
            let Some(slot) = this.read_slot.clone() else {
                let read_buf = std::mem::take(&mut this.read_buf);
                this.read_slot =
                    Some(driver()?.submit(&this.fd, Op::Read { buf: read_buf }, cx)?);
                return Poll::Pending;
            };
            let result = ready!(slot.poll_result(cx));
            this.read_slot = None;
            let Some(Op::Read { buf: data }) = slot.take_op() else {
                return Poll::Ready(Err(io::Error::other("io_uring read slot lost its buffer")));
            };
            if is_retry(result) {
                this.read_slot = Some(driver()?.submit(&this.fd, Op::Read { buf: data }, cx)?);
                return Poll::Pending;
            }
            if result <= 0 {
                this.read_buf = data;
                return Poll::Ready(if result == 0 {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                });
            }
            this.leftover = Some((data, 0, result as usize));
        }
    }
}

impl AsyncWrite for UringStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.shutdown_in_flight || this.write_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write after shutdown",
            )));
        }
        ready!(this.flush_write(cx))?;
        let n = buf.len().min(IO_CAP);
        if n > 0 {
            this.write_slot = Some(driver()?.submit(
                &this.fd,
                Op::Write {
                    data: buf[..n].to_vec(),
                    pos: 0,
                },
                cx,
            )?);
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.shutdown_in_flight {
            return Poll::Ready(Ok(()));
        }
        this.flush_write(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.shutdown_in_flight {
            ready!(this.flush_write(cx))?;
            if this.write_closed {
                return Poll::Ready(Ok(()));
            }
            this.write_slot = Some(driver()?.submit(&this.fd, Op::Shutdown, cx)?);
            this.shutdown_in_flight = true;
        }
        let Some(slot) = this.write_slot.as_ref() else {
            return Poll::Ready(Err(io::Error::other("io_uring shutdown slot missing")));
        };
        let result = ready!(slot.poll_result(cx));
        this.write_slot = None;
        this.shutdown_in_flight = false;
        this.write_closed = true;
        if result < 0 && result != -libc::ENOTCONN {
            this.write_error = Some(-result);
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }
        Poll::Ready(Ok(()))
    }
}
