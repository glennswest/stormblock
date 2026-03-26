//! ublk (userspace block device) server — exports a BlockDevice via io_uring URING_CMD.
//!
//! Linux 6.0+ only. Uses the kernel's ublk driver (`ublk_drv` module) to create
//! `/dev/ublkbN` block devices. All communication happens through io_uring
//! `IORING_OP_URING_CMD` — no TCP, no protocol parsing, just direct kernel↔userspace I/O.
//!
//! Lower overhead than NBD: no TCP stack, no protocol framing, just io_uring
//! command descriptors directly to/from the kernel block layer.
//!
//! Requires: `modprobe ublk_drv` on the host.

use std::fs::OpenOptions;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Wrapper around a raw pointer to make it `Send`.
/// Safety: the mmap'd descriptor memory is valid for the lifetime of the worker
/// and is not accessed from other threads for the same queue.
struct SendPtr(*const UblkIoDesc);
unsafe impl Send for SendPtr {}

use io_uring::{IoUring, opcode, types, squeue};

use super::{BlockDevice, DriveError, DriveResult};

// ---------------------------------------------------------------------------
// ublk control commands — ioctl-encoded (_IOWR('u', nr, ublksrv_ctrl_cmd))
//
// Modern kernels (6.1+) use ioctl-encoded cmd_op values for uring_cmd.
// Encoding: (3 << 30) | (sizeof_ctrl_cmd << 16) | ('u' << 8) | nr
// sizeof(ublksrv_ctrl_cmd) = 32, so base = 0xC0207500
// ---------------------------------------------------------------------------
const UBLK_U_CMD_ADD_DEV: u32 = 0xC020_7504;
const UBLK_U_CMD_DEL_DEV: u32 = 0xC020_7505;
const UBLK_U_CMD_START_DEV: u32 = 0xC020_7506;
const UBLK_U_CMD_STOP_DEV: u32 = 0xC020_7507;
const UBLK_U_CMD_SET_PARAMS: u32 = 0xC020_7508;

// ---------------------------------------------------------------------------
// ublk I/O commands — ioctl-encoded (_IOWR('u', nr, ublksrv_io_cmd))
// sizeof(ublksrv_io_cmd) = 16, so base = 0xC0107500
// ---------------------------------------------------------------------------
const UBLK_U_IO_FETCH_REQ: u32 = 0xC010_7520;
const UBLK_U_IO_COMMIT_AND_FETCH_REQ: u32 = 0xC010_7521;

// ---------------------------------------------------------------------------
// ublk I/O operations (in UblkIoDesc.op_flags bits 0-7)
// ---------------------------------------------------------------------------
const UBLK_IO_OP_READ: u8 = 0;
const UBLK_IO_OP_WRITE: u8 = 1;
const UBLK_IO_OP_FLUSH: u8 = 2;
const UBLK_IO_OP_DISCARD: u8 = 3;
const UBLK_IO_OP_WRITE_ZEROES: u8 = 5;

// ---------------------------------------------------------------------------
// ublk parameter types
// ---------------------------------------------------------------------------
const UBLK_PARAM_TYPE_BASIC: u32 = 1 << 0;
const UBLK_PARAM_TYPE_DISCARD: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// ublk feature flags
// ---------------------------------------------------------------------------
const UBLK_F_URING_CMD_COMP_IN_TASK: u64 = 1 << 1;
const UBLK_F_CMD_IOCTL_ENCODE: u64 = 1 << 6;

/// Default max I/O buffer size (512 KB).
const DEFAULT_MAX_IO_BYTES: u32 = 512 * 1024;

/// Default I/O queue depth.
const DEFAULT_QUEUE_DEPTH: u16 = 128;

// ===========================================================================
// Kernel ABI structures — must match include/uapi/linux/ublk_cmd.h exactly.
// ===========================================================================

/// Device info exchanged during ADD_DEV / GET_DEV_INFO.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkCtrlDevInfo {
    nr_hw_queues: u16,
    queue_depth: u16,
    state: u16,
    _pad0: u16,
    max_io_buf_bytes: u32,
    dev_id: u32,
    ublksrv_pid: i32,
    _pad1: u32,
    flags: u64,
    ublksrv_flags: u64,
    owner_uid: u32,
    owner_gid: u32,
    _reserved1: u64,
    _reserved2: u64,
}

impl Default for UblkCtrlDevInfo {
    fn default() -> Self {
        // Safety: all-zeros is valid for this struct.
        unsafe { std::mem::zeroed() }
    }
}

/// Control command payload — must match kernel `ublksrv_ctrl_cmd` exactly.
/// Placed in the 80-byte SQE cmd area (remaining bytes zeroed).
///
/// Kernel layout (include/uapi/linux/ublk_cmd.h):
///   __u32 dev_id;        // offset 0
///   __u16 queue_id;      // offset 4
///   __u16 len;           // offset 6 — buffer size
///   __u64 addr;          // offset 8 — user-space pointer
///   __u64 data[1];       // offset 16 — inline data
///   __u16 dev_path_len;  // offset 24
///   __u16 pad;           // offset 26
///   __u32 reserved;      // offset 28
/// Total: 32 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkCtrlCmd {
    dev_id: u32,        // offset 0
    queue_id: u16,      // offset 4
    len: u16,           // offset 6
    addr: u64,          // offset 8
    data: u64,          // offset 16
    dev_path_len: u16,  // offset 24
    _pad: u16,          // offset 26
    _reserved: u32,     // offset 28
}

impl UblkCtrlCmd {
    fn new(dev_id: u32) -> Self {
        Self {
            dev_id,
            queue_id: 0xFFFF, // -1: required by kernel for non-queue-specific cmds
            len: 0,
            addr: 0,
            data: 0,
            dev_path_len: 0,
            _pad: 0,
            _reserved: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Basic device parameters.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkParamBasic {
    attrs: u32,
    logical_bs_shift: u8,
    physical_bs_shift: u8,
    io_opt_shift: u8,
    io_min_shift: u8,
    max_sectors: u32,
    chunk_sectors: u32,
    dev_sectors: u64,
    virt_boundary_mask: u64,
}

/// Discard parameters.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkParamDiscard {
    discard_alignment: u32,
    discard_granularity: u32,
    max_discard_sectors: u32,
    max_write_zeroes_sectors: u32,
    max_discard_segments: u16,
    _reserved0: u16,
    _reserved1: u32,
}

/// Combined parameters envelope (basic + discard).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkParams {
    len: u32,
    types: u32,
    basic: UblkParamBasic,
    discard: UblkParamDiscard,
}

/// I/O command payload (in SQE cmd area for `/dev/ublkcN`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkIoCmd {
    q_id: u16,
    tag: u16,
    result: i32,
    addr: u64,
}

impl UblkIoCmd {
    fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// I/O descriptor (read-only, from mmap'd shared buffer).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UblkIoDesc {
    op_flags: u32,
    nr_sectors: u32,
    start_sector: u64,
    addr: u64,
}

// ===========================================================================
// Public API
// ===========================================================================

/// ublk server — exports any `Arc<dyn BlockDevice>` as `/dev/ublkbN`.
///
/// All communication with the kernel uses io_uring URING_CMD. Each I/O queue
/// runs on its own OS thread with a dedicated io_uring ring.
pub struct UblkServer {
    device: Arc<dyn BlockDevice>,
    dev_id: AtomicI32,
    requested_dev_id: Option<u32>,
    nr_queues: u16,
    queue_depth: u16,
    running: Arc<AtomicBool>,
}

impl UblkServer {
    /// Create a new ublk server for the given block device.
    pub fn new(device: Arc<dyn BlockDevice>) -> Self {
        UblkServer {
            device,
            dev_id: AtomicI32::new(-1),
            requested_dev_id: None,
            nr_queues: 1,
            queue_depth: DEFAULT_QUEUE_DEPTH,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Request a specific device ID (e.g., 0 for `/dev/ublkb0`).
    /// If an orphaned device exists at this ID, it will be deleted first.
    pub fn with_dev_id(mut self, id: u32) -> Self {
        self.requested_dev_id = Some(id);
        self
    }

    /// Set the number of I/O queues (default: 1).
    pub fn with_queues(mut self, nr_queues: u16) -> Self {
        self.nr_queues = nr_queues.max(1);
        self
    }

    /// Set the queue depth (default: 128).
    pub fn with_queue_depth(mut self, depth: u16) -> Self {
        self.queue_depth = depth.max(1);
        self
    }

    /// Block device path (e.g., `/dev/ublkb0`). Valid after `run()` starts.
    pub fn dev_path(&self) -> String {
        let id = self.dev_id.load(Ordering::Relaxed);
        format!("/dev/ublkb{}", id)
    }

    /// Run the ublk server until the shutdown signal fires.
    ///
    /// Creates the kernel block device, starts I/O worker threads, and blocks
    /// until `shutdown` receives a value. On return, the block device is removed.
    pub async fn run(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> DriveResult<()> {
        let capacity = self.device.capacity_bytes();
        let block_size = self.device.block_size();
        let nr_queues = self.nr_queues;
        let queue_depth = self.queue_depth;

        // --- Open /dev/ublk-control ---
        let ctrl_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/ublk-control")
            .map_err(|e| DriveError::Other(anyhow::anyhow!(
                "failed to open /dev/ublk-control: {e} (is ublk_drv loaded?)"
            )))?;
        let ctrl_fd = ctrl_file.as_raw_fd();

        // Create control io_uring ring (Entry128 needed for UringCmd80)
        let mut ctrl_ring: IoUring<squeue::Entry128> = IoUring::builder()
            .build(32)
            .map_err(|e| DriveError::Other(anyhow::anyhow!(
                "io_uring create failed: {e}"
            )))?;

        // --- ADD_DEV ---
        let req_id = self.requested_dev_id.unwrap_or(u32::MAX);

        // Clean up orphaned device at the requested ID (ignore errors)
        if req_id != u32::MAX {
            let _ = submit_ctrl_cmd(
                &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_STOP_DEV, req_id, 0, 0, 0,
            );
            let _ = submit_ctrl_cmd(
                &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_DEL_DEV, req_id, 0, 0, 0,
            );
        }

        let mut dev_info = UblkCtrlDevInfo {
            nr_hw_queues: nr_queues,
            queue_depth,
            max_io_buf_bytes: DEFAULT_MAX_IO_BYTES,
            dev_id: req_id,
            ublksrv_pid: std::process::id() as i32,
            flags: UBLK_F_URING_CMD_COMP_IN_TASK | UBLK_F_CMD_IOCTL_ENCODE,
            ..Default::default()
        };

        submit_ctrl_cmd(
            &mut ctrl_ring,
            ctrl_fd,
            UBLK_U_CMD_ADD_DEV,
            req_id,
            &mut dev_info as *mut UblkCtrlDevInfo as u64,
            std::mem::size_of::<UblkCtrlDevInfo>() as u32,
            0,
        )?;

        let assigned_id = dev_info.dev_id;
        self.dev_id.store(assigned_id as i32, Ordering::Relaxed);
        tracing::info!("ublk device created: dev_id={}", assigned_id);

        // --- SET_PARAMS ---
        let sectors = capacity / 512;
        let bs_shift = block_size.trailing_zeros() as u8;
        let max_sectors = DEFAULT_MAX_IO_BYTES / 512;

        let mut params = UblkParams {
            len: std::mem::size_of::<UblkParams>() as u32,
            types: UBLK_PARAM_TYPE_BASIC | UBLK_PARAM_TYPE_DISCARD,
            basic: UblkParamBasic {
                attrs: 0,
                logical_bs_shift: bs_shift,
                physical_bs_shift: bs_shift,
                io_opt_shift: 12, // 4096
                io_min_shift: bs_shift,
                max_sectors,
                chunk_sectors: 0,
                dev_sectors: sectors,
                virt_boundary_mask: 0,
            },
            discard: UblkParamDiscard {
                discard_alignment: 0,
                discard_granularity: block_size,
                max_discard_sectors: max_sectors,
                max_write_zeroes_sectors: max_sectors,
                max_discard_segments: 1,
                _reserved0: 0,
                _reserved1: 0,
            },
        };

        submit_ctrl_cmd(
            &mut ctrl_ring,
            ctrl_fd,
            UBLK_U_CMD_SET_PARAMS,
            assigned_id,
            &mut params as *mut UblkParams as u64,
            std::mem::size_of::<UblkParams>() as u32,
            0,
        )?;

        tracing::info!(
            "ublk params: capacity={}B, block_size={}B, sectors={}",
            capacity, block_size, sectors,
        );

        // --- Open /dev/ublkcN ---
        // In containers, devtmpfs may not auto-create the char device node.
        // If it doesn't exist, read major:minor from sysfs and mknod it.
        let char_path = format!("/dev/ublkc{}", assigned_id);
        let char_file = {
            // First try: direct open (works on hosts with devtmpfs)
            let mut retries = 10u32;
            let opened = loop {
                match OpenOptions::new().read(true).write(true).open(&char_path) {
                    Ok(f) => break Some(f),
                    Err(_) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(_) => break None,
                }
            };

            match opened {
                Some(f) => f,
                None => {
                    // Fallback: create device node from sysfs major:minor
                    let sysfs = format!("/sys/class/ublk-char/ublkc{}/dev", assigned_id);
                    let dev_str = std::fs::read_to_string(&sysfs).map_err(|e| {
                        DriveError::Other(anyhow::anyhow!(
                            "{char_path} missing and sysfs {sysfs} unreadable: {e}"
                        ))
                    })?;
                    let parts: Vec<&str> = dev_str.trim().split(':').collect();
                    if parts.len() != 2 {
                        return Err(DriveError::Other(anyhow::anyhow!(
                            "bad sysfs dev format: {dev_str:?}"
                        )));
                    }
                    let major: u32 = parts[0].parse().map_err(|_| {
                        DriveError::Other(anyhow::anyhow!("bad major: {}", parts[0]))
                    })?;
                    let minor: u32 = parts[1].parse().map_err(|_| {
                        DriveError::Other(anyhow::anyhow!("bad minor: {}", parts[1]))
                    })?;

                    let c_path = std::ffi::CString::new(char_path.clone())
                        .map_err(|e| DriveError::Other(e.into()))?;
                    let dev = libc::makedev(major, minor);
                    let rc = unsafe {
                        libc::mknod(c_path.as_ptr(), libc::S_IFCHR | 0o666, dev)
                    };
                    if rc != 0 {
                        return Err(DriveError::Other(anyhow::anyhow!(
                            "mknod {} ({}:{}) failed: {}",
                            char_path, major, minor,
                            std::io::Error::last_os_error(),
                        )));
                    }
                    tracing::info!("mknod {} ({}:{})", char_path, major, minor);

                    OpenOptions::new().read(true).write(true).open(&char_path)
                        .map_err(|e| DriveError::Other(anyhow::anyhow!(
                            "failed to open {} after mknod: {e}", char_path
                        )))?
                }
            }
        };
        let char_fd = char_file.as_raw_fd();

        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as libc::off_t;
        let desc_buf_size = queue_depth as usize * std::mem::size_of::<UblkIoDesc>();

        let mut desc_ptrs: Vec<*const UblkIoDesc> = Vec::with_capacity(nr_queues as usize);
        for q in 0..nr_queues {
            let mmap_offset = q as libc::off_t * page_size;
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    desc_buf_size,
                    libc::PROT_READ,
                    libc::MAP_SHARED | libc::MAP_POPULATE,
                    char_fd,
                    mmap_offset,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(DriveError::Other(anyhow::anyhow!(
                    "mmap ublk queue {} descriptors failed: {}",
                    q, std::io::Error::last_os_error(),
                )));
            }
            desc_ptrs.push(ptr as *const UblkIoDesc);
        }

        // --- Spawn per-queue I/O worker threads ---
        // Workers must submit FETCH_REQ before START_DEV (kernel checks
        // nr_queues_ready == nr_hw_queues). Use a barrier for sync.
        let startup_barrier = Arc::new(std::sync::Barrier::new(nr_queues as usize + 1));
        self.running.store(true, Ordering::SeqCst);

        let mut workers = Vec::with_capacity(nr_queues as usize);
        for q in 0..nr_queues {
            let device = self.device.clone();
            let running = self.running.clone();
            let raw_char_fd = char_fd;
            let desc_base = SendPtr(desc_ptrs[q as usize]);
            let depth = queue_depth;
            let max_io = DEFAULT_MAX_IO_BYTES as usize;
            let rt_handle = tokio::runtime::Handle::current();
            let barrier = startup_barrier.clone();

            let handle = std::thread::Builder::new()
                .name(format!("ublk-q{}", q))
                .spawn(move || {
                    let desc_base = desc_base;
                    queue_worker(
                        q, raw_char_fd, desc_base.0, depth, max_io,
                        device, running, rt_handle, barrier,
                    );
                })
                .map_err(|e| DriveError::Other(anyhow::anyhow!(
                    "failed to spawn ublk queue {} worker: {e}", q
                )))?;

            workers.push(handle);
        }

        // Wait for all workers to submit their initial FETCH_REQs
        startup_barrier.wait();

        // --- START_DEV (all queues now registered with kernel) ---
        // START_DEV: data[0] must contain the server PID (kernel checks > 0)
        submit_ctrl_cmd(
            &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_START_DEV,
            assigned_id, 0, 0,
            std::process::id() as u64,
        )?;
        // mknod /dev/ublkbN block device if not present (container workaround)
        let blk_path = format!("/dev/ublkb{}", assigned_id);
        if !std::path::Path::new(&blk_path).exists() {
            let sysfs = format!("/sys/block/ublkb{}/dev", assigned_id);
            if let Ok(dev_str) = std::fs::read_to_string(&sysfs) {
                let parts: Vec<&str> = dev_str.trim().split(':').collect();
                if let (Some(Ok(maj)), Some(Ok(min))) = (
                    parts.first().map(|s| s.parse::<u32>()),
                    parts.get(1).map(|s| s.parse::<u32>()),
                ) {
                    let c_path = std::ffi::CString::new(blk_path.clone()).unwrap();
                    let dev = libc::makedev(maj, min);
                    if unsafe { libc::mknod(c_path.as_ptr(), libc::S_IFBLK | 0o666, dev) } == 0 {
                        tracing::info!("mknod {} ({}:{})", blk_path, maj, min);
                    }
                }
            }
        }
        tracing::info!("ublk device started: {}", blk_path);

        // --- Wait for shutdown signal ---
        let _ = shutdown.changed().await;
        tracing::info!("ublk server shutting down");
        self.running.store(false, Ordering::SeqCst);

        // --- STOP_DEV ---
        let _ = submit_ctrl_cmd(
            &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_STOP_DEV, assigned_id, 0, 0, 0,
        );

        // Wait for all workers to exit
        for w in workers {
            let _ = w.join();
        }

        // --- DEL_DEV ---
        let _ = submit_ctrl_cmd(
            &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_DEL_DEV, assigned_id, 0, 0, 0,
        );

        // Unmap descriptor buffers
        for desc_ptr in &desc_ptrs {
            unsafe {
                libc::munmap(*desc_ptr as *mut libc::c_void, desc_buf_size);
            }
        }

        tracing::info!("ublk device /dev/ublkb{} removed", assigned_id);
        // char_file and ctrl_file dropped here, closing fds
        Ok(())
    }
}

// ===========================================================================
// Control command submission
// ===========================================================================

/// Submit a control command on `/dev/ublk-control` and wait for the CQE.
fn submit_ctrl_cmd(
    ring: &mut IoUring<squeue::Entry128>,
    ctrl_fd: RawFd,
    cmd_op: u32,
    dev_id: u32,
    addr: u64,
    len: u32,
    data: u64,
) -> DriveResult<i32> {
    let mut ctrl_cmd = UblkCtrlCmd::new(dev_id);
    ctrl_cmd.addr = addr;
    ctrl_cmd.len = len as u16;
    ctrl_cmd.data = data;

    // Copy struct bytes into the 80-byte cmd payload (zero-padded)
    let mut cmd_bytes = [0u8; 80];
    let src = ctrl_cmd.as_bytes();
    cmd_bytes[..src.len()].copy_from_slice(src);

    let sqe = opcode::UringCmd80::new(types::Fd(ctrl_fd), cmd_op)
        .cmd(cmd_bytes)
        .build();

    unsafe {
        ring.submission()
            .push(&sqe)
            .map_err(|_| DriveError::Other(anyhow::anyhow!("ublk ctrl SQ full")))?;
    }

    ring.submit_and_wait(1)
        .map_err(|e| DriveError::Other(anyhow::anyhow!("ublk ctrl submit: {e}")))?;

    let cqe = ring.completion().next()
        .ok_or_else(|| DriveError::Other(anyhow::anyhow!("ublk ctrl: no CQE")))?;

    let result = cqe.result();
    if result < 0 {
        return Err(DriveError::Other(anyhow::anyhow!(
            "ublk ctrl cmd {:#x} failed: {}",
            cmd_op,
            std::io::Error::from_raw_os_error(-result),
        )));
    }

    Ok(result)
}

// ===========================================================================
// Per-queue I/O worker (runs on a dedicated OS thread)
// ===========================================================================

/// I/O worker loop for a single ublk queue.
///
/// Runs on its own OS thread with a dedicated io_uring ring. Uses
/// `tokio::runtime::Handle::block_on()` to bridge async BlockDevice calls.
#[allow(clippy::too_many_arguments)]
fn queue_worker(
    queue_id: u16,
    char_fd: RawFd,
    desc_base: *const UblkIoDesc,
    queue_depth: u16,
    max_io_bytes: usize,
    device: Arc<dyn BlockDevice>,
    running: Arc<AtomicBool>,
    rt_handle: tokio::runtime::Handle,
    startup_barrier: Arc<std::sync::Barrier>,
) {
    // Per-queue io_uring ring
    let mut ring: IoUring<squeue::Entry128> = match IoUring::builder()
        .build(queue_depth as u32)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("ublk queue {}: io_uring create failed: {e}", queue_id);
            startup_barrier.wait(); // unblock main even on failure
            return;
        }
    };

    // Pre-allocate I/O buffers (one per tag)
    let mut bufs: Vec<Vec<u8>> = (0..queue_depth)
        .map(|_| vec![0u8; max_io_bytes])
        .collect();

    // Submit initial FETCH_REQ for all tags — registers this queue with kernel
    for tag in 0..queue_depth {
        if submit_io_fetch(&mut ring, char_fd, queue_id, tag, &bufs[tag as usize]).is_err() {
            tracing::error!("ublk queue {}: initial FETCH_REQ failed for tag {}", queue_id, tag);
            startup_barrier.wait();
            return;
        }
    }

    if let Err(e) = ring.submit() {
        tracing::error!("ublk queue {}: initial submit failed: {e}", queue_id);
        startup_barrier.wait();
        return;
    }

    // Signal main thread: this queue's FETCH_REQs are submitted
    startup_barrier.wait();

    // I/O loop (START_DEV has been called by main thread after barrier)
    while running.load(Ordering::Relaxed) {
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => {
                if running.load(Ordering::Relaxed) {
                    tracing::error!("ublk queue {}: submit_and_wait: {e}", queue_id);
                }
                break;
            }
        }

        // Collect completions first (avoids double mutable borrow of ring)
        let cqes: Vec<(u16, i32)> = ring.completion()
            .map(|cqe| (cqe.user_data() as u16, cqe.result()))
            .collect();

        for (tag, res) in cqes {
            // Negative = device stopping or error
            if res < 0 {
                if res != -(libc::ENODEV) {
                    tracing::warn!(
                        "ublk queue {} tag {}: CQE error {}",
                        queue_id, tag, res,
                    );
                }
                continue;
            }

            // Read the I/O descriptor for this tag
            let desc = unsafe { &*desc_base.add(tag as usize) };
            let op = (desc.op_flags & 0xFF) as u8;
            let offset = desc.start_sector * 512;
            let length = desc.nr_sectors as usize * 512;

            // Dispatch the I/O operation
            let io_result: i32 = match op {
                UBLK_IO_OP_READ => {
                    let buf = &mut bufs[tag as usize][..length];
                    match rt_handle.block_on(device.read(offset, buf)) {
                        Ok(_) => length as i32,
                        Err(e) => {
                            tracing::error!("ublk read @{}+{}: {e}", offset, length);
                            -(libc::EIO)
                        }
                    }
                }
                UBLK_IO_OP_WRITE => {
                    let buf = &bufs[tag as usize][..length];
                    match rt_handle.block_on(device.write(offset, buf)) {
                        Ok(_) => length as i32,
                        Err(e) => {
                            tracing::error!("ublk write @{}+{}: {e}", offset, length);
                            -(libc::EIO)
                        }
                    }
                }
                UBLK_IO_OP_FLUSH => {
                    match rt_handle.block_on(device.flush()) {
                        Ok(()) => 0,
                        Err(e) => {
                            tracing::error!("ublk flush: {e}");
                            -(libc::EIO)
                        }
                    }
                }
                UBLK_IO_OP_DISCARD => {
                    match rt_handle.block_on(device.discard(offset, length as u64)) {
                        Ok(()) => 0,
                        Err(e) => {
                            tracing::error!("ublk discard @{}+{}: {e}", offset, length);
                            -(libc::EIO)
                        }
                    }
                }
                UBLK_IO_OP_WRITE_ZEROES => {
                    // Write zeroes = zero-fill the region (treat as discard for thin volumes)
                    match rt_handle.block_on(device.discard(offset, length as u64)) {
                        Ok(()) => 0,
                        Err(_) => 0, // best-effort: report success even if unsupported
                    }
                }
                _ => {
                    tracing::warn!(
                        "ublk queue {} tag {}: unknown op {}",
                        queue_id, tag, op,
                    );
                    -(libc::ENOTSUP)
                }
            };

            // Submit COMMIT_AND_FETCH_REQ (completes current + fetches next)
            let io_cmd = UblkIoCmd {
                q_id: queue_id,
                tag,
                result: io_result,
                addr: bufs[tag as usize].as_ptr() as u64,
            };

            let mut cmd_bytes = [0u8; 80];
            let src = io_cmd.as_bytes();
            cmd_bytes[..src.len()].copy_from_slice(src);

            let sqe = opcode::UringCmd80::new(
                types::Fd(char_fd),
                UBLK_U_IO_COMMIT_AND_FETCH_REQ,
            )
            .cmd(cmd_bytes)
            .build()
            .user_data(tag as u64);

            unsafe {
                if ring.submission().push(&sqe).is_err() {
                    tracing::error!("ublk queue {}: SQ full on commit", queue_id);
                }
            }
        }
    }

    tracing::info!("ublk queue {} worker exiting", queue_id);
}

/// Submit a FETCH_REQ for one tag.
fn submit_io_fetch(
    ring: &mut IoUring<squeue::Entry128>,
    char_fd: RawFd,
    queue_id: u16,
    tag: u16,
    buf: &[u8],
) -> DriveResult<()> {
    let io_cmd = UblkIoCmd {
        q_id: queue_id,
        tag,
        result: 0,
        addr: buf.as_ptr() as u64,
    };

    let mut cmd_bytes = [0u8; 80];
    let src = io_cmd.as_bytes();
    cmd_bytes[..src.len()].copy_from_slice(src);

    let sqe = opcode::UringCmd80::new(types::Fd(char_fd), UBLK_U_IO_FETCH_REQ)
        .cmd(cmd_bytes)
        .build()
        .user_data(tag as u64);

    unsafe {
        ring.submission()
            .push(&sqe)
            .map_err(|_| DriveError::Other(anyhow::anyhow!("ublk SQ full")))?;
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ublk_abi_struct_sizes() {
        assert_eq!(std::mem::size_of::<UblkCtrlDevInfo>(), 64);
        assert_eq!(std::mem::size_of::<UblkCtrlCmd>(), 32);
        assert_eq!(std::mem::size_of::<UblkIoCmd>(), 16);
        assert_eq!(std::mem::size_of::<UblkIoDesc>(), 24);
        assert_eq!(std::mem::size_of::<UblkParamBasic>(), 32);
        assert_eq!(std::mem::size_of::<UblkParamDiscard>(), 24);
        assert_eq!(std::mem::size_of::<UblkParams>(), 64);
    }

    #[test]
    fn ublk_ctrl_cmd_layout() {
        let cmd = UblkCtrlCmd::new(42);
        assert_eq!(cmd.dev_id, 42);
        assert_eq!(cmd.queue_id, 0xFFFF); // -1 required by kernel
        assert_eq!(cmd.addr, 0);
        assert_eq!(cmd.len, 0);
        let bytes = cmd.as_bytes();
        assert_eq!(bytes.len(), 32);
        // dev_id at offset 0, little-endian
        assert_eq!(bytes[0], 42);
        assert_eq!(bytes[1], 0);
        // queue_id at offset 4, should be 0xFFFF
        assert_eq!(bytes[4], 0xFF);
        assert_eq!(bytes[5], 0xFF);
        // len at offset 6
        assert_eq!(bytes[6], 0);
        // addr at offset 8
        assert_eq!(bytes[8], 0);
    }

    #[test]
    fn ublk_io_cmd_layout() {
        let cmd = UblkIoCmd {
            q_id: 0,
            tag: 7,
            result: 0,
            addr: 0xDEAD_BEEF,
        };
        let bytes = cmd.as_bytes();
        assert_eq!(bytes.len(), 16);
        // tag at offset 2 (after q_id u16), little-endian
        assert_eq!(bytes[2], 7);
        assert_eq!(bytes[3], 0);
    }
}
