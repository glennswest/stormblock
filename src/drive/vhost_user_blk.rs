//! A vhost-user-blk backend (#82): a VMM (qemu, cloud-hypervisor,
//! firecracker) hands a guest's virtqueues to the engine over a Unix socket
//! and shared memory, and the engine serves the guest's virtio-blk requests
//! with no kernel block layer in the path. ublk gives the VMM a
//! `/dev/ublkbN` to open as a host device, which is a kernel block-layer
//! round trip plus the ublk command hop per I/O; this is neither.
//!
//! The protocol is QEMU's `docs/interop/vhost-user.rst`, backend side:
//!
//! - **Control**: one connection at a time on the socket, on a thread of its
//!   own (messages carry file descriptors, `SCM_RIGHTS`, so it is plain
//!   `recvmsg`). A VMM that disconnects and connects again (QEMU
//!   `reconnect=`) gets a fresh negotiation; a second VMM while one is
//!   connected is refused: one guest owns a disk.
//! - **Memory**: `SET_MEM_TABLE` hands over the guest's RAM as fds, mapped
//!   shared. Descriptors carry guest-physical addresses; ring addresses are
//!   the VMM's own virtual addresses. Both are translated through the table,
//!   and an address outside it fails the request, never the engine. A new
//!   table while queues run (memory hotplug) restarts them on it.
//! - **Queues**: each virtqueue (split ring) is served by an async task on
//!   the engine's runtime, woken by its kick eventfd. Every request in an
//!   available batch runs at once; completions go to the used ring and the
//!   call eventfd is signalled unless the guest asked not to be.
//! - **virtio-blk**: IN, OUT, FLUSH, GET_ID, DISCARD and WRITE_ZEROES, in
//!   512-byte sectors over the volume. A read-only attach offers
//!   `VIRTIO_BLK_F_RO` and fails every write at the device, not in the
//!   guest's goodwill.
//!
//! Not offered (said here so it is a decision, not an omission): the dirty
//! log (`VHOST_F_LOG_ALL`, `LOG_SHMFD`), so QEMU blocks a live migration of
//! a guest using this device rather than migrating it unsafely; inflight
//! tracking (`INFLIGHT_SHMFD`); indirect descriptors and event index;
//! `CONFIGURE_MEM_SLOTS` (memory hotplug arrives as a new `SET_MEM_TABLE`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::io::unix::AsyncFd;

use crate::drive::BlockDevice;

// ---------------------------------------------------------------- protocol

const GET_FEATURES: u32 = 1;
const SET_FEATURES: u32 = 2;
const SET_OWNER: u32 = 3;
const RESET_OWNER: u32 = 4;
const SET_MEM_TABLE: u32 = 5;
const SET_VRING_NUM: u32 = 8;
const SET_VRING_ADDR: u32 = 9;
const SET_VRING_BASE: u32 = 10;
const GET_VRING_BASE: u32 = 11;
const SET_VRING_KICK: u32 = 12;
const SET_VRING_CALL: u32 = 13;
const SET_VRING_ERR: u32 = 14;
const GET_PROTOCOL_FEATURES: u32 = 15;
const SET_PROTOCOL_FEATURES: u32 = 16;
const GET_QUEUE_NUM: u32 = 17;
const SET_VRING_ENABLE: u32 = 18;
const GET_CONFIG: u32 = 24;
const SET_CONFIG: u32 = 25;

const FLAG_VERSION: u32 = 0x1;
const FLAG_REPLY: u32 = 0x4;
const FLAG_NEED_REPLY: u32 = 0x8;

const PROTOCOL_F_MQ: u64 = 1 << 0;
const PROTOCOL_F_REPLY_ACK: u64 = 1 << 3;
const PROTOCOL_F_CONFIG: u64 = 1 << 9;

/// `VHOST_USER_F_PROTOCOL_FEATURES`.
const F_PROTOCOL_FEATURES: u64 = 1 << 30;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const BLK_F_SEG_MAX: u64 = 1 << 2;
const BLK_F_RO: u64 = 1 << 5;
const BLK_F_BLK_SIZE: u64 = 1 << 6;
const BLK_F_FLUSH: u64 = 1 << 9;
const BLK_F_MQ: u64 = 1 << 12;
const BLK_F_DISCARD: u64 = 1 << 13;
const BLK_F_WRITE_ZEROES: u64 = 1 << 14;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Largest message payload this backend accepts (a full memory table is 8
/// regions of 32 bytes plus 8; a config read is at most 256).
const MAX_PAYLOAD: usize = 4096;
/// Regions in one `SET_MEM_TABLE` (`VHOST_MEMORY_BASELINE_NREGIONS`).
const MAX_REGIONS: usize = 8;
/// Queues offered. A VMM asks for at most this many (`GET_QUEUE_NUM`).
pub const MAX_QUEUES: usize = 8;
/// Largest ring size accepted (virtio's limit).
const MAX_RING: u32 = 32768;
/// Segments per request the guest may use (`seg_max`).
const SEG_MAX: u32 = 128;
/// Largest discard / write-zeroes, in sectors (1 GiB).
const MAX_ZERO_SECTORS: u32 = 1 << 21;

/// The device's identity and policy.
#[derive(Debug, Clone)]
pub struct BlkSpec {
    /// Fail every write, discard and write-zeroes at the device.
    pub read_only: bool,
    /// What GET_ID answers (at most 20 bytes are used).
    pub serial: String,
}

/// What the device has done, for the API and tests.
#[derive(Debug, Default)]
pub struct Counters {
    pub connections: std::sync::atomic::AtomicU64,
    pub requests: std::sync::atomic::AtomicU64,
    pub errors: std::sync::atomic::AtomicU64,
    /// A VMM is connected right now.
    pub connected: AtomicBool,
}

/// A served vhost-user-blk socket. Dropping it does not stop it: call
/// [`VhostUserBlk::stop`].
pub struct VhostUserBlk {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    pub counters: Arc<Counters>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl VhostUserBlk {
    /// Listen on `path` (replacing a stale socket there) and serve `dev` to
    /// the VMM that connects, on `rt`'s runtime.
    pub fn serve(
        path: &Path,
        dev: Arc<dyn BlockDevice>,
        spec: BlkSpec,
        rt: tokio::runtime::Handle,
    ) -> std::io::Result<VhostUserBlk> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (s, c) = (stop.clone(), counters.clone());
        let thread = std::thread::Builder::new()
            .name("vhost-user-blk".into())
            .spawn(move || accept_loop(listener, dev, spec, rt, s, c))?;
        Ok(VhostUserBlk { path: path.to_path_buf(), stop, counters, thread: Mutex::new(Some(thread)) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop serving: the VMM is disconnected, every queue stops, the socket
    /// is removed. Waits at most `budget` for the control thread.
    pub fn stop(&self, budget: std::time::Duration) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
        let t = self.thread.lock().unwrap().take();
        if let Some(t) = t {
            let deadline = std::time::Instant::now() + budget;
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    dev: Arc<dyn BlockDevice>,
    spec: BlkSpec,
    rt: tokio::runtime::Handle,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                counters.connections.fetch_add(1, Ordering::Relaxed);
                counters.connected.store(true, Ordering::SeqCst);
                let mut conn = Conn::new(stream, dev.clone(), spec.clone(), rt.clone(), counters.clone());
                if let Err(e) = conn.run(&listener, &stop) {
                    tracing::info!("vhost-user-blk: VMM connection ended: {e}");
                }
                conn.reset();
                counters.connected.store(false, Ordering::SeqCst);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!("vhost-user-blk: accept: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

// ------------------------------------------------------------------ memory

struct Region {
    gpa: u64,
    size: u64,
    uva: u64,
    /// Start of the mapping (at `mmap_offset` 0 of the fd).
    map: *mut u8,
    map_len: usize,
    /// Where guest memory starts inside the mapping.
    offset: u64,
}

// SAFETY: the mapping is shared memory owned by this struct until drop;
// access is through raw copies bounded by `size`.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: unmapping what `map` mapped, once.
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
    }
}

/// The guest's memory as the VMM handed it over.
#[derive(Default)]
struct GuestMemory {
    regions: Vec<Region>,
}

impl GuestMemory {
    /// A pointer to `len` bytes at guest-physical `gpa`, when they lie in one
    /// region.
    fn gpa(&self, gpa: u64, len: u64) -> Option<*mut u8> {
        self.regions.iter().find_map(|r| {
            let off = gpa.checked_sub(r.gpa)?;
            (off.checked_add(len)? <= r.size)
                // SAFETY: inside the mapping, as just checked.
                .then(|| unsafe { r.map.add((r.offset + off) as usize) })
        })
    }

    /// The same for the VMM's virtual address `uva` (ring addresses).
    fn uva(&self, uva: u64, len: u64) -> Option<*mut u8> {
        self.regions.iter().find_map(|r| {
            let off = uva.checked_sub(r.uva)?;
            (off.checked_add(len)? <= r.size)
                // SAFETY: inside the mapping, as just checked.
                .then(|| unsafe { r.map.add((r.offset + off) as usize) })
        })
    }
}

// ------------------------------------------------------------------- rings

/// A queue's addresses, as the VMM set them.
#[derive(Default, Clone, Copy)]
struct RingAddr {
    desc: u64,
    used: u64,
    avail: u64,
}

#[derive(Default)]
struct QueueSetup {
    num: u32,
    addr: Option<RingAddr>,
    base: u16,
    kick: Option<Arc<OwnedFd>>,
    /// Read by the worker at each completion: QEMU replaces the call fd
    /// while a queue runs (masking an MSI-X vector).
    call: Arc<Mutex<Option<Arc<OwnedFd>>>>,
    running: Option<Running>,
}

struct Running {
    stop: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<u16>,
}

/// A started split ring.
struct Ring {
    num: u16,
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    last_avail: u16,
    used_idx: u16,
}

// SAFETY: the pointers are into the shared guest memory, which the worker
// keeps alive with an `Arc<GuestMemory>`.
unsafe impl Send for Ring {}

impl Ring {
    fn avail_idx(&self) -> u16 {
        // SAFETY: inside the ring the VMM described, checked at start.
        let v = unsafe { std::ptr::read_volatile(self.avail.add(2) as *const u16) };
        fence(Ordering::Acquire);
        u16::from_le(v)
    }
    fn avail_flags(&self) -> u16 {
        // SAFETY: as above.
        u16::from_le(unsafe { std::ptr::read_volatile(self.avail as *const u16) })
    }
    fn avail_head(&self, i: u16) -> u16 {
        let slot = (i % self.num) as usize;
        // SAFETY: slot < num, checked size at start.
        u16::from_le(unsafe { std::ptr::read_volatile(self.avail.add(4 + 2 * slot) as *const u16) })
    }
    fn desc(&self, i: u16) -> Desc {
        // SAFETY: i < num, checked by the caller.
        unsafe {
            let p = self.desc.add(16 * i as usize);
            Desc {
                addr: u64::from_le(std::ptr::read_volatile(p as *const u64)),
                len: u32::from_le(std::ptr::read_volatile(p.add(8) as *const u32)),
                flags: u16::from_le(std::ptr::read_volatile(p.add(12) as *const u16)),
                next: u16::from_le(std::ptr::read_volatile(p.add(14) as *const u16)),
            }
        }
    }
    fn push_used(&mut self, head: u16, len: u32) {
        let slot = (self.used_idx % self.num) as usize;
        // SAFETY: slot < num, the used ring's size was checked at start.
        unsafe {
            let p = self.used.add(4 + 8 * slot);
            std::ptr::write_volatile(p as *mut u32, (head as u32).to_le());
            std::ptr::write_volatile(p.add(4) as *mut u32, len.to_le());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
    }
    fn publish_used(&self) {
        fence(Ordering::Release);
        // SAFETY: the used ring's idx field.
        unsafe { std::ptr::write_volatile(self.used.add(2) as *mut u16, self.used_idx.to_le()) };
        fence(Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// -------------------------------------------------------------- connection

struct Conn {
    stream: UnixStream,
    dev: Arc<dyn BlockDevice>,
    spec: BlkSpec,
    rt: tokio::runtime::Handle,
    counters: Arc<Counters>,
    features: u64,
    protocol: u64,
    mem: Arc<RwLock<Arc<GuestMemory>>>,
    queues: HashMap<u32, QueueSetup>,
}

impl Conn {
    fn new(
        stream: UnixStream,
        dev: Arc<dyn BlockDevice>,
        spec: BlkSpec,
        rt: tokio::runtime::Handle,
        counters: Arc<Counters>,
    ) -> Conn {
        Conn {
            stream,
            dev,
            spec,
            rt,
            counters,
            features: 0,
            protocol: 0,
            mem: Arc::new(RwLock::new(Arc::new(GuestMemory::default()))),
            queues: HashMap::new(),
        }
    }

    fn offered_features(&self) -> u64 {
        let mut f = VIRTIO_F_VERSION_1
            | F_PROTOCOL_FEATURES
            | BLK_F_SEG_MAX
            | BLK_F_BLK_SIZE
            | BLK_F_FLUSH
            | BLK_F_MQ
            | BLK_F_DISCARD
            | BLK_F_WRITE_ZEROES;
        if self.spec.read_only {
            f |= BLK_F_RO;
        }
        f
    }

    /// `struct virtio_blk_config`, 60 bytes.
    fn config(&self) -> [u8; 60] {
        let mut c = [0u8; 60];
        let sectors = self.dev.capacity_bytes() / 512;
        let bs = self.dev.block_size().max(512);
        c[0..8].copy_from_slice(&sectors.to_le_bytes());
        c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        c[20..24].copy_from_slice(&bs.to_le_bytes());
        c[34..36].copy_from_slice(&(MAX_QUEUES as u16).to_le_bytes());
        c[36..40].copy_from_slice(&MAX_ZERO_SECTORS.to_le_bytes());
        c[40..44].copy_from_slice(&1u32.to_le_bytes());
        c[44..48].copy_from_slice(&(bs / 512).to_le_bytes());
        c[48..52].copy_from_slice(&MAX_ZERO_SECTORS.to_le_bytes());
        c[52..56].copy_from_slice(&1u32.to_le_bytes());
        c[56] = 1; // write_zeroes_may_unmap
        c
    }

    fn run(&mut self, listener: &UnixListener, stop: &AtomicBool) -> std::io::Result<()> {
        self.stream.set_read_timeout(Some(std::time::Duration::from_millis(200)))?;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            // One guest owns a disk: a second VMM is refused, not queued
            // behind the first.
            while let Ok((extra, _)) = listener.accept() {
                tracing::warn!("vhost-user-blk: a second VMM connected while one is; refused");
                drop(extra);
            }
            let (req, flags, payload, fds) = match recv_msg(&self.stream) {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(()),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(e) => return Err(e),
            };
            let reply = self.handle(req, &payload, fds)?;
            match reply {
                Some(body) => send_reply(&mut self.stream, req, &body)?,
                None if flags & FLAG_NEED_REPLY != 0 && self.protocol & PROTOCOL_F_REPLY_ACK != 0 => {
                    send_reply(&mut self.stream, req, &0u64.to_le_bytes())?
                }
                None => {}
            }
        }
    }

    fn handle(&mut self, req: u32, p: &[u8], mut fds: Vec<OwnedFd>) -> std::io::Result<Option<Vec<u8>>> {
        let u32_at = |o: usize| p.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let u64_at = |o: usize| p.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
        match req {
            GET_FEATURES => Ok(Some(self.offered_features().to_le_bytes().to_vec())),
            SET_FEATURES => {
                self.features = u64_at(0).ok_or_else(|| bad("SET_FEATURES"))? & self.offered_features();
                Ok(None)
            }
            GET_PROTOCOL_FEATURES => {
                Ok(Some((PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK | PROTOCOL_F_CONFIG).to_le_bytes().to_vec()))
            }
            SET_PROTOCOL_FEATURES => {
                self.protocol =
                    u64_at(0).ok_or_else(|| bad("SET_PROTOCOL_FEATURES"))? & (PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK | PROTOCOL_F_CONFIG);
                Ok(None)
            }
            GET_QUEUE_NUM => Ok(Some((MAX_QUEUES as u64).to_le_bytes().to_vec())),
            SET_OWNER => Ok(None),
            RESET_OWNER => {
                self.reset();
                Ok(None)
            }
            SET_MEM_TABLE => {
                let n = u32_at(0).ok_or_else(|| bad("SET_MEM_TABLE"))? as usize;
                if n == 0 || n > MAX_REGIONS || fds.len() != n {
                    return Err(bad("SET_MEM_TABLE: region count and fds disagree"));
                }
                let mut regions = Vec::with_capacity(n);
                for (i, fd) in fds.drain(..).enumerate() {
                    let o = 8 + 32 * i;
                    let (gpa, size, uva, off) = (
                        u64_at(o).ok_or_else(|| bad("region"))?,
                        u64_at(o + 8).ok_or_else(|| bad("region"))?,
                        u64_at(o + 16).ok_or_else(|| bad("region"))?,
                        u64_at(o + 24).ok_or_else(|| bad("region"))?,
                    );
                    let map_len = (off + size) as usize;
                    // SAFETY: mapping a shared memory fd the VMM handed over;
                    // the result is checked and owned by `Region`.
                    let map = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            map_len,
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_SHARED,
                            fd.as_raw_fd(),
                            0,
                        )
                    };
                    if map == libc::MAP_FAILED {
                        return Err(std::io::Error::last_os_error());
                    }
                    regions.push(Region { gpa, size, uva, map: map as *mut u8, map_len, offset: off });
                }
                // Running queues stop on the old table and start again on
                // the new; the old is unmapped when the last of them lets go.
                let running: Vec<u32> =
                    self.queues.iter().filter(|(_, q)| q.running.is_some()).map(|(i, _)| *i).collect();
                for &i in &running {
                    self.stop_queue(i);
                }
                *self.mem.write().unwrap() = Arc::new(GuestMemory { regions });
                for i in running {
                    self.start_queue(i)?;
                }
                Ok(None)
            }
            SET_VRING_NUM => {
                let (i, num) = (u32_at(0).ok_or_else(|| bad("SET_VRING_NUM"))?, u32_at(4).ok_or_else(|| bad("SET_VRING_NUM"))?);
                if i as usize >= MAX_QUEUES || num == 0 || num > MAX_RING || !num.is_power_of_two() {
                    return Err(bad("SET_VRING_NUM: queue or size out of range"));
                }
                self.queues.entry(i).or_default().num = num;
                Ok(None)
            }
            SET_VRING_ADDR => {
                let i = u32_at(0).ok_or_else(|| bad("SET_VRING_ADDR"))?;
                let a = RingAddr {
                    desc: u64_at(8).ok_or_else(|| bad("SET_VRING_ADDR"))?,
                    used: u64_at(16).ok_or_else(|| bad("SET_VRING_ADDR"))?,
                    avail: u64_at(24).ok_or_else(|| bad("SET_VRING_ADDR"))?,
                };
                self.queue(i)?.addr = Some(a);
                Ok(None)
            }
            SET_VRING_BASE => {
                let (i, base) = (u32_at(0).ok_or_else(|| bad("SET_VRING_BASE"))?, u32_at(4).ok_or_else(|| bad("SET_VRING_BASE"))?);
                self.queue(i)?.base = base as u16;
                Ok(None)
            }
            GET_VRING_BASE => {
                let i = u32_at(0).ok_or_else(|| bad("GET_VRING_BASE"))?;
                let base = self.stop_queue(i);
                let mut r = i.to_le_bytes().to_vec();
                r.extend_from_slice(&(base as u32).to_le_bytes());
                Ok(Some(r))
            }
            SET_VRING_KICK | SET_VRING_CALL | SET_VRING_ERR => {
                let v = u64_at(0).ok_or_else(|| bad("SET_VRING_*FD"))?;
                let i = (v & 0xff) as u32;
                let fd = if v & 0x100 == 0 { fds.pop().map(Arc::new) } else { None };
                let q = self.queue(i)?;
                match req {
                    SET_VRING_KICK => {
                        q.kick = fd;
                        self.start_queue(i)?;
                    }
                    SET_VRING_CALL => *q.call.lock().unwrap() = fd,
                    _ => {}
                }
                Ok(None)
            }
            // Rings start on their kick (lenient, as libvhost-user's users
            // are); an explicit disable pauses one.
            SET_VRING_ENABLE => {
                let (i, on) = (u32_at(0).ok_or_else(|| bad("SET_VRING_ENABLE"))?, u32_at(4).unwrap_or(1));
                if on == 0 {
                    self.stop_queue(i);
                } else {
                    self.start_queue(i)?;
                }
                Ok(None)
            }
            GET_CONFIG => {
                let (off, size) = (u32_at(0).unwrap_or(0) as usize, u32_at(4).unwrap_or(0) as usize);
                let cfg = self.config();
                let mut r = p.get(0..12).map(|b| b.to_vec()).unwrap_or_else(|| vec![0; 12]);
                let mut body = vec![0u8; size.min(256)];
                for (k, b) in body.iter_mut().enumerate() {
                    *b = cfg.get(off + k).copied().unwrap_or(0);
                }
                r.extend_from_slice(&body);
                Ok(Some(r))
            }
            // The guest may write `writeback`; the engine is write-back with
            // flushes either way.
            SET_CONFIG => Ok(None),
            other => {
                tracing::debug!("vhost-user-blk: request {other} not supported, ignored");
                Ok(None)
            }
        }
    }

    fn queue(&mut self, i: u32) -> std::io::Result<&mut QueueSetup> {
        if i as usize >= MAX_QUEUES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("queue {i} out of range")));
        }
        Ok(self.queues.entry(i).or_default())
    }

    /// Start queue `i` when it has everything a ring needs and is not
    /// running already.
    fn start_queue(&mut self, i: u32) -> std::io::Result<()> {
        let mem = self.mem.clone();
        let (dev, spec, counters) = (self.dev.clone(), self.spec.clone(), self.counters.clone());
        let Some(q) = self.queues.get_mut(&i) else { return Ok(()) };
        if q.running.is_some() {
            return Ok(());
        }
        let (Some(addr), Some(kick)) = (q.addr, q.kick.clone()) else { return Ok(()) };
        let num = q.num as u16;
        let current = mem.read().unwrap().clone();
        let n = q.num as u64;
        let (Some(desc), Some(avail), Some(used)) = (
            current.uva(addr.desc, 16 * n),
            current.uva(addr.avail, 6 + 2 * n),
            current.uva(addr.used, 6 + 8 * n),
        ) else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("queue {i}: ring outside guest memory")));
        };
        // SAFETY: the used ring's idx, inside the mapping just checked.
        let used_idx = u16::from_le(unsafe { std::ptr::read_volatile(used.add(2) as *const u16) });
        let ring = Ring { num, desc, avail, used, last_avail: q.base, used_idx };
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let call = q.call.clone();
        let task = {
            let (stop, wake) = (stop.clone(), wake.clone());
            let _guard = self.rt.enter();
            tokio::spawn(serve_queue(ring, current, kick, call, dev, spec, counters, stop, wake))
        };
        q.running = Some(Running { stop, wake, task });
        Ok(())
    }

    /// Stop queue `i`, wait for its in-flight requests, and answer where the
    /// guest should resume (the next available index).
    fn stop_queue(&mut self, i: u32) -> u16 {
        let Some(q) = self.queues.get_mut(&i) else { return 0 };
        match q.running.take() {
            Some(r) => {
                r.stop.store(true, Ordering::SeqCst);
                r.wake.notify_one();
                let base = self.rt.block_on(r.task).unwrap_or(q.base);
                q.base = base;
                base
            }
            None => q.base,
        }
    }

    /// The VMM went away: every queue stops, the memory is let go.
    fn reset(&mut self) {
        let ids: Vec<u32> = self.queues.keys().copied().collect();
        for i in ids {
            self.stop_queue(i);
        }
        self.queues.clear();
        *self.mem.write().unwrap() = Arc::new(GuestMemory::default());
        self.features = 0;
        self.protocol = 0;
    }
}

// ------------------------------------------------------------ queue worker

#[allow(clippy::too_many_arguments)]
async fn serve_queue(
    mut ring: Ring,
    mem: Arc<GuestMemory>,
    kick: Arc<OwnedFd>,
    call: Arc<Mutex<Option<Arc<OwnedFd>>>>,
    dev: Arc<dyn BlockDevice>,
    spec: BlkSpec,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
) -> u16 {
    let kick_fd = match AsyncFd::new(kick.as_raw_fd()) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("vhost-user-blk: kick fd: {e}");
            return ring.last_avail;
        }
    };
    loop {
        // Whatever is available, first: requests can be queued before the
        // first kick, and a kick can cover several.
        loop {
            let avail = ring.avail_idx();
            if avail == ring.last_avail {
                break;
            }
            let mut batch = Vec::new();
            while ring.last_avail != avail {
                batch.push(ring.avail_head(ring.last_avail));
                ring.last_avail = ring.last_avail.wrapping_add(1);
            }
            let done = futures_util::future::join_all(
                batch.iter().map(|&head| request(&ring, &mem, head, dev.as_ref(), &spec, &counters)),
            )
            .await;
            for (head, len) in batch.into_iter().zip(done) {
                ring.push_used(head, len);
            }
            ring.publish_used();
            if ring.avail_flags() & AVAIL_F_NO_INTERRUPT == 0 {
                let c = call.lock().unwrap().clone();
                if let Some(c) = c {
                    signal(c.as_raw_fd());
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            return ring.last_avail;
        }
        tokio::select! {
            r = kick_fd.readable() => {
                if let Ok(mut g) = r {
                    let mut v = [0u8; 8];
                    // SAFETY: reading an eventfd's counter into 8 bytes; the
                    // counter is reset, so the readiness goes with it.
                    unsafe { libc::read(kick.as_raw_fd(), v.as_mut_ptr() as *mut libc::c_void, 8) };
                    g.clear_ready();
                }
            }
            _ = wake.notified() => {}
        }
        if stop.load(Ordering::SeqCst) {
            return ring.last_avail;
        }
    }
}

fn signal(fd: RawFd) {
    let one: u64 = 1;
    // SAFETY: writing 8 bytes to an eventfd.
    unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
}

/// One segment of a request, in guest memory.
struct Seg {
    ptr: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: pointers into the shared guest memory kept alive by the caller's
// `Arc<GuestMemory>`.
unsafe impl Send for Seg {}
unsafe impl Sync for Seg {}

/// Serve one descriptor chain; the bytes written into the guest's buffers.
async fn request(
    ring: &Ring,
    mem: &GuestMemory,
    head: u16,
    dev: &dyn BlockDevice,
    spec: &BlkSpec,
    counters: &Counters,
) -> u32 {
    counters.requests.fetch_add(1, Ordering::Relaxed);
    // Walk the chain; a malformed one is failed without touching anything.
    let mut segs = Vec::new();
    let mut i = head;
    for _ in 0..ring.num {
        if i >= ring.num {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let d = ring.desc(i);
        if d.flags & DESC_F_INDIRECT != 0 {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let Some(ptr) = mem.gpa(d.addr, d.len as u64) else {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        segs.push(Seg { ptr, len: d.len as usize, writable: d.flags & DESC_F_WRITE != 0 });
        if d.flags & DESC_F_NEXT == 0 {
            break;
        }
        i = d.next;
    }
    // The status byte is the last byte of the last writable segment.
    let Some(last) = segs.last().filter(|s| s.writable && s.len > 0) else {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return 0;
    };
    // SAFETY: inside the segment.
    let status_ptr = unsafe { last.ptr.add(last.len - 1) };
    let readable: Vec<&Seg> = segs.iter().filter(|s| !s.writable).collect();
    let mut header = [0u8; 16];
    if gather(&readable, 0, &mut header) < 16 {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the status byte, inside the guest's buffer.
        unsafe { std::ptr::write_volatile(status_ptr, S_IOERR) };
        return 1;
    }
    let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let in_len: usize = readable.iter().map(|s| s.len).sum::<usize>() - 16;
    let writable: Vec<&Seg> = segs.iter().filter(|s| s.writable).collect();
    let out_room: usize = writable.iter().map(|s| s.len).sum::<usize>() - 1;

    let offset = sector.saturating_mul(512);
    let cap = dev.capacity_bytes();
    let (status, written) = match kind {
        T_IN => {
            let mut buf = vec![0u8; out_room];
            if offset.saturating_add(out_room as u64) > cap {
                (S_IOERR, 0)
            } else {
                match dev.read(offset, &mut buf).await {
                    Ok(_) => (S_OK, scatter(&writable, &buf)),
                    Err(_) => (S_IOERR, 0),
                }
            }
        }
        T_OUT if spec.read_only => (S_IOERR, 0),
        T_OUT => {
            let mut buf = vec![0u8; in_len];
            gather(&readable, 16, &mut buf);
            if offset.saturating_add(in_len as u64) > cap {
                (S_IOERR, 0)
            } else {
                match dev.write(offset, &buf).await {
                    Ok(_) => (S_OK, 0),
                    Err(_) => (S_IOERR, 0),
                }
            }
        }
        T_FLUSH => match dev.flush().await {
            Ok(()) => (S_OK, 0),
            Err(_) => (S_IOERR, 0),
        },
        T_GET_ID => {
            let mut id = [0u8; 20];
            let s = spec.serial.as_bytes();
            id[..s.len().min(20)].copy_from_slice(&s[..s.len().min(20)]);
            (S_OK, scatter(&writable, &id[..out_room.min(20)]))
        }
        T_DISCARD | T_WRITE_ZEROES if spec.read_only => (S_IOERR, 0),
        T_DISCARD | T_WRITE_ZEROES => {
            let mut segs16 = vec![0u8; in_len];
            gather(&readable, 16, &mut segs16);
            let mut status = S_OK;
            for s in segs16.chunks_exact(16) {
                let start = u64::from_le_bytes(s[0..8].try_into().unwrap()).saturating_mul(512);
                let len = u32::from_le_bytes(s[8..12].try_into().unwrap()) as u64 * 512;
                if len > MAX_ZERO_SECTORS as u64 * 512 || start.saturating_add(len) > cap {
                    status = S_IOERR;
                    break;
                }
                let r = if kind == T_DISCARD { dev.discard(start, len).await } else { dev.write_zeroes(start, len).await };
                if r.is_err() {
                    status = S_IOERR;
                    break;
                }
            }
            (status, 0)
        }
        _ => (S_UNSUPP, 0),
    };
    if status != S_OK {
        counters.errors.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: the status byte, inside the guest's buffer.
    unsafe { std::ptr::write_volatile(status_ptr, status) };
    written as u32 + 1
}

/// Copy readable bytes from `skip` on into `out`; how many were copied.
fn gather(segs: &[&Seg], mut skip: usize, out: &mut [u8]) -> usize {
    let mut n = 0;
    for s in segs {
        if n == out.len() {
            break;
        }
        if skip >= s.len {
            skip -= s.len;
            continue;
        }
        let take = (s.len - skip).min(out.len() - n);
        // SAFETY: inside the segment and inside `out`.
        unsafe { std::ptr::copy_nonoverlapping(s.ptr.add(skip), out.as_mut_ptr().add(n), take) };
        n += take;
        skip = 0;
    }
    n
}

/// Copy `data` into the writable segments; how many bytes went in.
fn scatter(segs: &[&Seg], data: &[u8]) -> usize {
    let mut n = 0;
    for s in segs {
        if n == data.len() {
            break;
        }
        let take = s.len.min(data.len() - n);
        // SAFETY: inside the segment and inside `data`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(n), s.ptr, take) };
        n += take;
    }
    n
}

// ------------------------------------------------------------- messages

/// One message: request, flags, payload and the fds that came with it.
/// `None` when the VMM closed the connection.
#[allow(clippy::type_complexity)]
fn recv_msg(stream: &UnixStream) -> std::io::Result<Option<(u32, u32, Vec<u8>, Vec<OwnedFd>)>> {
    let mut hdr = [0u8; 12];
    let mut cmsg = [0u8; 256];
    let mut iov = libc::iovec { iov_base: hdr.as_mut_ptr() as *mut libc::c_void, iov_len: 12 };
    // SAFETY: a zeroed msghdr, then pointers to buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg.len() as _;
    // SAFETY: receiving into the buffers described by `msg`.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(None);
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote into `cmsg`.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for k in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*data.add(k)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    let mut s = stream;
    if (n as usize) < 12 {
        s.read_exact(&mut hdr[n as usize..])?;
    }
    let req = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let flags = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    let size = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
    if size > MAX_PAYLOAD {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("message {req}: {size}-byte payload")));
    }
    let mut payload = vec![0u8; size];
    // The payload follows at once; a timeout in the middle of a message is
    // not a pause between messages.
    stream.set_read_timeout(None)?;
    let r = s.read_exact(&mut payload);
    stream.set_read_timeout(Some(std::time::Duration::from_millis(200)))?;
    r?;
    Ok(Some((req, flags, payload, fds)))
}

fn send_reply(stream: &mut UnixStream, req: u32, body: &[u8]) -> std::io::Result<()> {
    let mut m = Vec::with_capacity(12 + body.len());
    m.extend_from_slice(&req.to_le_bytes());
    m.extend_from_slice(&(FLAG_VERSION | FLAG_REPLY).to_le_bytes());
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(body);
    stream.write_all(&m)
}

#[cfg(test)]
mod tests;
