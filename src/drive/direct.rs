//! Asynchronous `O_DIRECT` I/O on a raw block device (#140).
//!
//! The owner's direction: "we will get rid of the file/block copies … I don't
//! want any file IO." Every drive is a block device, opened `O_DIRECT`, and
//! this is how its I/O is done:
//!
//! * **io_uring**, on a thread of its own. Requests arrive over a channel, an
//!   eventfd wakes the ring when one does, and many are in flight at once.
//!   Every buffer is owned by its operation until the kernel is done with it:
//!   an async caller can be dropped mid-request, and a buffer freed while the
//!   kernel still writes into it is memory corruption.
//! * **`pread`/`pwrite` on the blocking pool** where io_uring cannot be had —
//!   RouterOS, or a container with it disabled. Still `O_DIRECT`, still
//!   concurrent.
//!
//! What this replaced (`SasDevice` as it was) held a `std::Mutex` across
//! `submit_and_wait` on the executor thread: queue depth one per drive, and a
//! runtime worker blocked for the length of every disk operation.
//!
//! Buffers handed in here must satisfy `O_DIRECT` — page-aligned [`DmaBuf`]s
//! of whole logical blocks at block-aligned offsets. Making an arbitrary
//! request into one is the device's job (see `SasDevice`).

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::mpsc;

use super::dma::DmaBuf;
use super::{DriveError, DriveResult};

enum Kind {
    Read,
    Write,
    Sync,
}

struct Op {
    kind: Kind,
    offset: u64,
    buf: DmaBuf,
    len: usize,
    reply: tokio::sync::oneshot::Sender<(i32, DmaBuf)>,
}

/// How a device's I/O is carried out.
pub struct DirectIo {
    engine: Engine,
}

enum Engine {
    Uring { tx: Option<mpsc::Sender<Op>>, wake: RawFd, thread: Option<std::thread::JoinHandle<()>> },
    /// A duplicate of the device's fd, shared by every operation in flight:
    /// a request whose caller has gone keeps it open until it finishes, so it
    /// can never land on an fd number the process has since reused.
    Blocking(std::sync::Arc<std::os::fd::OwnedFd>),
}

impl DirectIo {
    /// io_uring when the kernel gives one, the blocking pool otherwise.
    /// `fd` stays the caller's; the ring thread works on a duplicate of it.
    pub fn new(fd: RawFd) -> DirectIo {
        match Self::uring(fd) {
            Ok(engine) => DirectIo { engine },
            Err(e) => {
                tracing::info!("io_uring unavailable ({e}); O_DIRECT through the blocking pool");
                Self::blocking(fd)
            }
        }
    }

    /// The blocking-pool engine, whatever the kernel offers — also what a
    /// test of it asks for.
    pub fn blocking(fd: RawFd) -> DirectIo {
        use std::os::fd::FromRawFd;
        let dup = unsafe { libc::dup(fd) };
        assert!(dup >= 0, "dup of a device fd failed: {}", std::io::Error::last_os_error());
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
        DirectIo { engine: Engine::Blocking(std::sync::Arc::new(owned)) }
    }

    pub fn engine_name(&self) -> &'static str {
        match self.engine {
            Engine::Uring { .. } => "io_uring",
            Engine::Blocking(_) => "blocking",
        }
    }

    fn uring(fd: RawFd) -> std::io::Result<Engine> {
        let ring = io_uring::IoUring::new(256)?;
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if wake < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            unsafe { libc::close(wake) };
            return Err(std::io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::channel::<Op>();
        let thread = std::thread::Builder::new()
            .name("stormblock-uring".into())
            .spawn(move || ring_thread(ring, dup, wake, rx))?;
        Ok(Engine::Uring { tx: Some(tx), wake, thread: Some(thread) })
    }

    async fn submit(&self, kind: Kind, offset: u64, buf: DmaBuf, len: usize) -> DriveResult<(usize, DmaBuf)> {
        match &self.engine {
            Engine::Uring { tx, wake, .. } => {
                let (reply, rx) = tokio::sync::oneshot::channel();
                tx.as_ref()
                    .ok_or(DriveError::DeviceNotReady)?
                    .send(Op { kind, offset, buf, len, reply })
                    .map_err(|_| DriveError::DeviceNotReady)?;
                let one: u64 = 1;
                unsafe { libc::write(*wake, &one as *const u64 as *const libc::c_void, 8) };
                let (res, buf) = rx.await.map_err(|_| DriveError::DeviceNotReady)?;
                if res < 0 {
                    return Err(DriveError::Io(std::io::Error::from_raw_os_error(-res)));
                }
                Ok((res as usize, buf))
            }
            Engine::Blocking(fd) => {
                let fd = fd.clone();
                tokio::task::spawn_blocking(move || {
                    use std::os::fd::AsRawFd;
                    blocking_op(fd.as_raw_fd(), kind, offset, buf, len)
                })
                    .await
                    .map_err(|e| DriveError::Other(e.into()))?
            }
        }
    }

    /// Read `len` bytes at `offset` into a fresh aligned buffer. Short reads
    /// are continued; a read past the end of the device returns what there is.
    pub async fn read(&self, offset: u64, len: usize) -> DriveResult<DmaBuf> {
        let mut out = DmaBuf::alloc(len);
        let mut done = 0usize;
        while done < len {
            let (n, chunk) = self.submit(Kind::Read, offset + done as u64, DmaBuf::alloc(len - done), len - done).await?;
            out[done..done + n].copy_from_slice(&chunk[..n]);
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(out)
    }

    /// Write all of `buf[..len]` at `offset`.
    pub async fn write(&self, offset: u64, buf: DmaBuf, len: usize) -> DriveResult<usize> {
        let (mut n, mut buf) = self.submit(Kind::Write, offset, buf, len).await?;
        while n < len {
            // A short O_DIRECT write is rare and stops on a block boundary;
            // finish it from where it stopped.
            let mut rest = DmaBuf::alloc(len - n);
            rest[..len - n].copy_from_slice(&buf[n..len]);
            let (m, b) = self.submit(Kind::Write, offset + n as u64, rest, len - n).await?;
            if m == 0 {
                return Err(DriveError::Io(std::io::Error::new(std::io::ErrorKind::WriteZero, "short write")));
            }
            n += m;
            buf = b;
        }
        Ok(len)
    }

    pub async fn sync(&self) -> DriveResult<()> {
        // A one-byte buffer, never touched: `DmaBuf::alloc(0)` would be a
        // zero-size allocation.
        self.submit(Kind::Sync, 0, DmaBuf::alloc(1), 0).await.map(|_| ())
    }
}

impl Drop for DirectIo {
    fn drop(&mut self) {
        if let Engine::Uring { tx, wake, thread } = &mut self.engine {
            // Hang up, wake the ring, and let it finish what is in flight.
            drop(tx.take());
            let one: u64 = 1;
            unsafe { libc::write(*wake, &one as *const u64 as *const libc::c_void, 8) };
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
            unsafe { libc::close(*wake) };
        }
    }
}

fn blocking_op(fd: RawFd, kind: Kind, offset: u64, mut buf: DmaBuf, len: usize) -> DriveResult<(usize, DmaBuf)> {
    let r = unsafe {
        match kind {
            Kind::Read => libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, len, offset as libc::off_t),
            Kind::Write => libc::pwrite(fd, buf.as_ptr() as *const libc::c_void, len, offset as libc::off_t),
            Kind::Sync => libc::fsync(fd) as isize,
        }
    };
    if r < 0 {
        return Err(DriveError::Io(std::io::Error::last_os_error()));
    }
    Ok((r as usize, buf))
}

/// The ring's own thread. It owns `fd` (a duplicate) and closes it on exit.
fn ring_thread(mut ring: io_uring::IoUring, fd: RawFd, wake: RawFd, rx: mpsc::Receiver<Op>) {
    use io_uring::{opcode, types};
    const WAKE: u64 = u64::MAX;
    let mut wake_buf = [0u8; 8];
    let mut next: u64 = 0;
    let mut inflight: HashMap<u64, Op> = HashMap::new();
    let mut pending: VecDeque<Op> = VecDeque::new();
    let mut hung_up = false;

    let arm_wake = |ring: &mut io_uring::IoUring, buf: &mut [u8; 8]| {
        let sqe = opcode::Read::new(types::Fd(wake), buf.as_mut_ptr(), 8).build().user_data(WAKE);
        // Safety: the buffer lives in this thread's frame for the ring's life.
        unsafe {
            let _ = ring.submission().push(&sqe);
        }
    };
    arm_wake(&mut ring, &mut wake_buf);

    loop {
        loop {
            match rx.try_recv() {
                Ok(op) => pending.push_back(op),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    hung_up = true;
                    break;
                }
            }
        }
        while let Some(mut op) = pending.pop_front() {
            let tag = next;
            next = next.wrapping_add(1) % (u64::MAX - 1);
            let sqe = match op.kind {
                Kind::Read => opcode::Read::new(types::Fd(fd), op.buf.as_mut_ptr(), op.len as u32)
                    .offset(op.offset)
                    .build(),
                Kind::Write => opcode::Write::new(types::Fd(fd), op.buf.as_ptr(), op.len as u32)
                    .offset(op.offset)
                    .build(),
                Kind::Sync => opcode::Fsync::new(types::Fd(fd)).build(),
            }
            .user_data(tag);
            // Safety: the buffer moves into `inflight` and stays there — at a
            // stable heap address — until its completion is reaped.
            let pushed = unsafe { ring.submission().push(&sqe).is_ok() };
            if !pushed {
                pending.push_front(op);
                break;
            }
            inflight.insert(tag, op);
        }
        if hung_up && inflight.is_empty() && pending.is_empty() {
            break;
        }
        if let Err(e) = ring.submit_and_wait(1) {
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            tracing::error!("io_uring submit failed: {e}; failing {} request(s)", inflight.len());
            for (_, op) in inflight.drain() {
                let _ = op.reply.send((-libc::EIO, op.buf));
            }
            for op in pending.drain(..) {
                let _ = op.reply.send((-libc::EIO, op.buf));
            }
            if hung_up {
                break;
            }
            continue;
        }
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        for (tag, res) in done {
            if tag == WAKE {
                if !hung_up {
                    arm_wake(&mut ring, &mut wake_buf);
                }
                continue;
            }
            if let Some(op) = inflight.remove(&tag) {
                let _ = op.reply.send((res, op.buf));
            }
        }
    }
    unsafe { libc::close(fd) };
}
