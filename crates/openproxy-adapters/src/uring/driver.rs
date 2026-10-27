//! One OS thread owns the ring. Slots own buffers and fd references until
//! their final CQE. Ids are never reused; cancellation CQEs use a separate tag.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::{Context, Poll, Waker};

use io_uring::{IoUring, opcode, types};
use parking_lot::Mutex;

const RING_ENTRIES: u32 = 4096;
const EVENTFD_ID: u64 = 0;
const CANCEL_TAG: u64 = 1 << 63;

pub(crate) enum Op {
    Read { buf: Vec<u8> },
    Write { data: Vec<u8>, pos: usize },
    Shutdown,
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Shutdown => "shutdown",
        }
    }
}

enum Msg {
    Submit(Arc<Slot>),
    Cancel(u64),
}

#[derive(Default)]
struct SlotState {
    result: Option<i32>,
    waker: Option<Waker>,
}

pub(crate) struct Slot {
    id: u64,
    fd: Arc<OwnedFd>,
    op: Mutex<Option<Op>>,
    state: Mutex<SlotState>,
}

impl Slot {
    fn entry(&self) -> Option<io_uring::squeue::Entry> {
        let fd = types::Fd(self.fd.as_raw_fd());
        let mut guard = self.op.lock();
        let entry = match guard.as_mut()? {
            Op::Read { buf } => opcode::Recv::new(fd, buf.as_mut_ptr(), buf.len() as u32).build(),
            Op::Write { data, pos } => {
                let remaining = &data[*pos..];
                opcode::Send::new(fd, remaining.as_ptr(), remaining.len() as u32)
                    .flags(libc::MSG_NOSIGNAL)
                    .build()
            }
            Op::Shutdown => opcode::Shutdown::new(fd, libc::SHUT_WR).build(),
        };
        Some(entry.user_data(self.id))
    }

    fn finish(&self, result: i32) {
        if result < 0 && !matches!(-result, libc::EAGAIN | libc::EINTR | libc::ECANCELED) {
            let guard = self.op.lock();
            tracing::warn!(
                op = guard.as_ref().map_or("unknown", Op::name),
                id = self.id, fd = self.fd.as_raw_fd(), errno = -result,
                error = %io::Error::from_raw_os_error(-result),
                "io_uring operation failed"
            );
        } else if result == -libc::ECANCELED {
            tracing::debug!(
                id = self.id,
                fd = self.fd.as_raw_fd(),
                errno = -result,
                "io_uring operation cancelled"
            );
        }
        let waker = {
            let mut state = self.state.lock();
            state.result = Some(result);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn poll_result(&self, cx: &Context<'_>) -> Poll<i32> {
        let mut state = self.state.lock();
        if let Some(result) = state.result {
            return Poll::Ready(result);
        }
        match &state.waker {
            Some(old) if old.will_wake(cx.waker()) => {}
            _ => state.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.state.lock().result.is_some()
    }

    pub(crate) fn take_op(&self) -> Option<Op> {
        debug_assert!(self.is_settled(), "op taken while the kernel owns it");
        self.op.lock().take()
    }
}

pub(crate) struct Driver {
    tx: Sender<Msg>,
    next_id: AtomicU64,
    sleeping: AtomicBool,
    failure: AtomicI32,
    #[cfg(test)]
    injected_errno: AtomicI32,
    queue_lock: Mutex<()>,
    kick: OwnedFd,
}

impl Driver {
    fn send(&self, msg: Msg) -> io::Result<()> {
        let _guard = self.queue_lock.lock();
        let failure = self.failure.load(Ordering::Acquire);
        if failure != 0 {
            return Err(io::Error::from_raw_os_error(failure));
        }
        self.tx
            .send(msg)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "io_uring driver stopped"))?;
        // Publish the message BEFORE checking sleeping. The driver publishes
        // sleeping BEFORE rechecking rx. SeqCst prevents a missed wakeup.
        if self.sleeping.swap(false, Ordering::SeqCst) {
            kick(self.kick.as_raw_fd())?;
        }
        Ok(())
    }

    pub(crate) fn submit(
        &self,
        fd: &Arc<OwnedFd>,
        op: Op,
        cx: &Context<'_>,
    ) -> io::Result<Arc<Slot>> {
        let failure = self.failure.load(Ordering::Acquire);
        if failure != 0 {
            return Err(io::Error::from_raw_os_error(failure));
        }
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                (id < CANCEL_TAG).then_some(id + 1)
            })
            .map_err(|_| io::Error::other("io_uring operation id exhausted"))?;
        let slot = Arc::new(Slot {
            id,
            fd: Arc::clone(fd),
            op: Mutex::new(Some(op)),
            state: Mutex::new(SlotState {
                result: None,
                waker: Some(cx.waker().clone()),
            }),
        });
        self.send(Msg::Submit(Arc::clone(&slot)))?;
        Ok(slot)
    }

    pub(crate) fn cancel(&self, slot: &Slot) {
        if !slot.is_settled()
            && let Err(error) = self.send(Msg::Cancel(slot.id))
        {
            tracing::error!(%error, id = slot.id, "io_uring cancel delivery failed");
        }
    }

    #[cfg(test)]
    pub(super) fn inject_submit_error(&self, errno: i32) {
        self.injected_errno.store(errno, Ordering::Release);
        kick(self.kick.as_raw_fd()).expect("inject kick");
    }

    pub(crate) fn spawn() -> io::Result<Arc<Driver>> {
        Self::spawn_with_entries(RING_ENTRIES)
    }

    pub(super) fn spawn_with_entries(entries: u32) -> io::Result<Arc<Driver>> {
        let ring = IoUring::new(entries)?;
        let (tx, rx) = mpsc::channel();
        let driver = Arc::new(Driver {
            tx,
            next_id: AtomicU64::new(1),
            sleeping: AtomicBool::new(false),
            failure: AtomicI32::new(0),
            #[cfg(test)]
            injected_errno: AtomicI32::new(0),
            queue_lock: Mutex::new(()),
            kick: eventfd()?,
        });
        let handle = Arc::clone(&driver);
        std::thread::Builder::new()
            .name("openproxy-io-uring".into())
            .spawn(move || driver_loop(ring, rx, &handle))?;
        Ok(driver)
    }
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: a fresh fd is exclusively owned below. Nonblocking prevents
    // kick writers from blocking Tokio if the eventfd counter is saturated.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn kick(fd: i32) -> io::Result<()> {
    let one = 1u64.to_ne_bytes();
    loop {
        // SAFETY: fd stays owned by Driver and one is eight readable bytes.
        if unsafe { libc::write(fd, one.as_ptr().cast(), one.len()) } == 8 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            // A full counter already guarantees readability.
            Some(libc::EAGAIN) => return Ok(()),
            _ => {
                tracing::error!(%error, "io_uring eventfd kick failed");
                return Err(error);
            }
        }
    }
}

fn drain_kick(fd: i32) -> io::Result<()> {
    let mut value = 0u64;
    loop {
        // SAFETY: eight writable bytes, nonblocking live eventfd.
        if unsafe { libc::read(fd, (&raw mut value).cast(), 8) } == 8 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(()),
            _ => return Err(error),
        }
    }
}

fn retry_submit(errno: i32) -> bool {
    matches!(
        errno,
        libc::EINTR | libc::EBUSY | libc::EAGAIN | libc::ENOMEM
    )
}

fn driver_loop(mut ring: IoUring, rx: Receiver<Msg>, driver: &Driver) {
    let mut event_armed = false;
    let mut pending = VecDeque::new();
    let mut in_flight: HashMap<u64, Arc<Slot>> = HashMap::new();
    let fatal = loop {
        while let Ok(msg) = rx.try_recv() {
            pending.push_back(msg);
        }
        {
            let mut sq = ring.submission();
            // Reserve a kick reader BEFORE any ordinary entry. If the SQ is
            // full, do not wait: submit/drain, then try arming it again.
            if !event_armed {
                let entry =
                    opcode::PollAdd::new(types::Fd(driver.kick.as_raw_fd()), libc::POLLIN as u32)
                        .build()
                        .user_data(EVENTFD_ID);
                // SAFETY: poll references only the driver's live eventfd.
                if unsafe { sq.push(&entry) }.is_ok() {
                    event_armed = true;
                }
            }
            while let Some(msg) = pending.pop_front() {
                let (entry, slot, cancel_id) = match msg {
                    Msg::Submit(slot) => match slot.entry() {
                        Some(entry) => (entry, Some(slot), 0),
                        None => continue,
                    },
                    Msg::Cancel(id) => (
                        opcode::AsyncCancel::new(id)
                            .build()
                            .user_data(id | CANCEL_TAG),
                        None,
                        id,
                    ),
                };
                // SAFETY: in_flight retains fd and buffer until the CQE.
                if unsafe { sq.push(&entry) }.is_err() {
                    pending.push_front(match slot {
                        Some(slot) => Msg::Submit(slot),
                        None => Msg::Cancel(cancel_id),
                    });
                    break;
                }
                if let Some(slot) = slot {
                    in_flight.insert(slot.id, slot);
                }
            }
        }
        let mut want = 0;
        if pending.is_empty() && event_armed {
            driver.sleeping.store(true, Ordering::SeqCst);
            match rx.try_recv() {
                Ok(msg) => pending.push_back(msg),
                Err(mpsc::TryRecvError::Empty) => want = 1,
                Err(mpsc::TryRecvError::Disconnected) => break libc::EPIPE,
            }
        }
        #[cfg(test)]
        let injected_errno = driver.injected_errno.swap(0, Ordering::AcqRel);
        #[cfg(not(test))]
        let injected_errno = 0;
        let submitted = if injected_errno == 0 {
            ring.submit_and_wait(want)
        } else {
            Err(io::Error::from_raw_os_error(injected_errno))
        };
        driver.sleeping.store(false, Ordering::SeqCst);
        let mut event_error = None;
        for cqe in ring.completion() {
            let id = cqe.user_data();
            if id == EVENTFD_ID {
                event_armed = false;
                if cqe.result() < 0 {
                    if !matches!(-cqe.result(), libc::EAGAIN | libc::EINTR) {
                        event_error = Some(-cqe.result());
                    }
                } else if let Err(error) = drain_kick(driver.kick.as_raw_fd()) {
                    event_error = Some(error.raw_os_error().unwrap_or(libc::EIO));
                }
            } else if id & CANCEL_TAG == 0
                && let Some(slot) = in_flight.remove(&id)
            {
                slot.finish(cqe.result());
            }
        }
        if let Some(errno) = event_error {
            tracing::error!(errno, "io_uring eventfd reader failed");
            break errno;
        }
        if let Err(error) = submitted {
            let errno = error.raw_os_error().unwrap_or(libc::EIO);
            tracing::warn!(%error, errno, want, in_flight = in_flight.len(), pending = pending.len(), "io_uring submit failed");
            if !retry_submit(errno) {
                break errno;
            }
            if errno != libc::EINTR {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    };
    {
        let _guard = driver.queue_lock.lock();
        driver.failure.store(fatal, Ordering::Release);
    }
    // Cancellation must finish BEFORE waking callers with failure: callers
    // may immediately reclaim a read buffer after observing the result.
    let cancelled = ring
        .submitter()
        .register_sync_cancel(None, types::CancelBuilder::any());
    if cancelled
        .as_ref()
        .is_err_and(|e| e.raw_os_error() != Some(libc::ENOENT))
    {
        tracing::error!(error = ?cancelled.err(), "io_uring fatal cleanup could not cancel; retaining kernel-owned buffers");
        // Older kernels can lack sync cancellation. Quarantine the allocations
        // rather than freeing memory the kernel might still reference. This is
        // only the terminal failure path; no further ops use this driver.
        for slot in in_flight.values() {
            if let Some(op) = slot.op.lock().take() {
                std::mem::forget(op);
            }
            std::mem::forget(Arc::clone(&slot.fd));
        }
    }
    drop(ring);
    for slot in in_flight.into_values() {
        slot.finish(-fatal);
    }
    while let Ok(msg) = rx.try_recv() {
        pending.push_back(msg);
    }
    for msg in pending {
        if let Msg::Submit(slot) = msg {
            slot.finish(-fatal);
        }
    }
}
