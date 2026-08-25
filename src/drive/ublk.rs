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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

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
/// Encode a control command number the way the kernel does:
/// `_IOWR('u', nr, struct ublksrv_ctrl_cmd)`, with `sizeof(...) == 32`.
///
/// Derived rather than written out, so a command added later cannot be off by a
/// digit; the test below pins the results against the hex an strace shows.
const fn ublk_ctrl_cmd(nr: u32) -> u32 {
    (3 << 30) | ((std::mem::size_of::<UblkCtrlCmd>() as u32) << 16) | (0x75 << 8) | nr
}

/// The same, for the one control command the kernel declares as `_IOR` rather
/// than `_IOWR`.
///
/// The direction bits are part of the value the kernel dispatches on, so a
/// read-only command encoded as read-write does not reach its handler at all —
/// it comes back as an error, which for a *feature query* looks exactly like a
/// kernel that has no such feature. That is how `UBLK_F_UPDATE_SIZE` was
/// quietly never negotiated on a kernel that offers it.
const fn ublk_ctrl_cmd_read(nr: u32) -> u32 {
    (2 << 30) | ((std::mem::size_of::<UblkCtrlCmd>() as u32) << 16) | (0x75 << 8) | nr
}

/// Read the device's own record of itself into a `UblkCtrlDevInfo` at `addr`.
/// `_IOR`, like `GET_FEATURES`.
///
/// What a server adopting an existing device has to ask before it can serve
/// it: the queue count and depth are fixed at creation, and a recovering
/// server that guesses them differently fails `END_USER_RECOVERY` — or worse,
/// starts with fewer queues than the kernel is holding requests on.
const UBLK_U_CMD_GET_DEV_INFO: u32 = ublk_ctrl_cmd_read(0x02);
const UBLK_U_CMD_ADD_DEV: u32 = ublk_ctrl_cmd(0x04);
const UBLK_U_CMD_DEL_DEV: u32 = ublk_ctrl_cmd(0x05);
const UBLK_U_CMD_START_DEV: u32 = ublk_ctrl_cmd(0x06);
const UBLK_U_CMD_STOP_DEV: u32 = ublk_ctrl_cmd(0x07);
const UBLK_U_CMD_SET_PARAMS: u32 = ublk_ctrl_cmd(0x08);
/// Reports the kernel's `UBLK_F_*` feature mask into a `__u64` at `addr`.
/// `_IOR`, unlike every other control command here.
const UBLK_U_CMD_GET_FEATURES: u32 = ublk_ctrl_cmd_read(0x13);
/// Tell the kernel a new server is taking over a device whose old server has
/// gone. The device stays; its queues are reset to await fresh `FETCH_REQ`s.
const UBLK_U_CMD_START_USER_RECOVERY: u32 = ublk_ctrl_cmd(0x10);
/// Finish the takeover. The new server's pid goes in `cmd->data[0]`, exactly
/// as `START_DEV` carries it for a device being created.
const UBLK_U_CMD_END_USER_RECOVERY: u32 = ublk_ctrl_cmd(0x11);
/// Resize a live device. New size is in **sectors**, in `cmd->data[0]`.
const UBLK_U_CMD_UPDATE_SIZE: u32 = ublk_ctrl_cmd(0x15);

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
/// The device may be resized in place with `UBLK_U_CMD_UPDATE_SIZE`.
///
/// Negotiated at ADD_DEV, and only when the running kernel reports it — a flag
/// the kernel does not know makes ADD_DEV fail outright, so a node on an older
/// kernel must ask for the device without it and lose only the resize (#19).
const UBLK_F_UPDATE_SIZE: u64 = 1 << 10;
/// The device outlives its server: if this process goes, the block device
/// stays and another process may adopt it.
///
/// **Asked for at ADD_DEV and nowhere else.** A device created without it can
/// never be recovered, so the decision belongs to whoever creates the device
/// — which on a booting node is the initramfs, minutes before the process
/// that will want to adopt it even exists.
const UBLK_F_USER_RECOVERY: u64 = 1 << 3;
/// With recovery: I/O outstanding when the old server went is **reissued** to
/// the new one rather than failed.
///
/// The difference between a root filesystem that pauses across a handover and
/// one that sees EIO in the middle of it.
const UBLK_F_USER_RECOVERY_REISSUE: u64 = 1 << 4;

/// Device states, as the kernel reports them in `ublksrv_ctrl_dev_info.state`.
///
/// Only QUIESCED is used here, and only because it is the gate on adoption:
/// the kernel moves a device LIVE -> QUIESCED once it has noticed the server
/// is gone and stopped the queues, and refuses START_USER_RECOVERY until then.
#[allow(dead_code)]
const UBLK_S_DEV_DEAD: u16 = 0;
#[allow(dead_code)]
const UBLK_S_DEV_LIVE: u16 = 1;
const UBLK_S_DEV_QUIESCED: u16 = 2;

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
    /// Whether the kernel took `UBLK_F_UPDATE_SIZE` at ADD_DEV. Set during
    /// `run()`; until then no resize is possible.
    resizable: Arc<AtomicBool>,
    /// The capacity the kernel currently believes in, in 512-byte sectors.
    /// This is what a resize moves, and what makes a repeat resize a no-op.
    dev_sectors: Arc<AtomicU64>,
    /// Ask the kernel to let another process adopt this device later.
    ///
    /// Only meaningful when creating one: the flag is fixed at ADD_DEV, so a
    /// device made without it can never be handed over.
    recoverable: bool,
    /// Adopt the device that already exists at this id rather than creating
    /// one. See [`UblkServer::adopting`].
    adopt: Option<u32>,
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
            resizable: Arc::new(AtomicBool::new(false)),
            dev_sectors: Arc::new(AtomicU64::new(0)),
            recoverable: false,
            adopt: None,
        }
    }

    /// Request a specific device ID (e.g., 0 for `/dev/ublkb0`).
    /// If an orphaned device exists at this ID, it will be deleted first.
    pub fn with_dev_id(mut self, id: u32) -> Self {
        self.requested_dev_id = Some(id);
        self
    }

    /// Create the device so that another process can adopt it later.
    ///
    /// The decision has to be made *here*, by whoever creates the device,
    /// because `UBLK_F_USER_RECOVERY` is fixed at ADD_DEV. On a booting node
    /// that is the initramfs — a process whose own binary is deleted by
    /// `switch_root` and which therefore cannot be restarted, ever. Without
    /// this flag the engine serving root is unrepeatable: if it dies the node
    /// is gone until it reboots, and no code path anywhere could recover it.
    ///
    /// Ignored, with a note, on a kernel that does not offer the flag: an
    /// unknown flag fails ADD_DEV outright, and losing the handover is better
    /// than losing the device (#19 taught this the expensive way).
    pub fn recoverable(mut self, yes: bool) -> Self {
        self.recoverable = yes;
        self
    }

    /// Serve a device that already exists, whose previous server has gone.
    ///
    /// The block device never disappears across this — that is the point. A
    /// filesystem mounted on it stays mounted, and with
    /// `UBLK_F_USER_RECOVERY_REISSUE` the I/O that was in flight when the old
    /// server went is handed to this one rather than failed.
    ///
    /// The device must have been created [`recoverable`](Self::recoverable).
    pub fn adopting(mut self, dev_id: u32) -> Self {
        self.adopt = Some(dev_id);
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

    /// Whether this device can be resized in place — i.e. the kernel took
    /// `UBLK_F_UPDATE_SIZE` when the device was added.
    pub fn resizable(&self) -> bool {
        self.resizable.load(Ordering::Relaxed)
    }

    /// Tell the kernel the device is bigger now (#19).
    ///
    /// Without this a volume resize stops at the engine: `virtual_size` moves,
    /// the ublk device keeps the capacity it was given at `SET_PARAMS`, and
    /// `xfs_growfs` finds nothing to grow into — the resize is invisible to
    /// everything above stormblock.
    ///
    /// **No quiesce.** `UPDATE_SIZE` is an independent control command and
    /// there is no consistency point to capture, so I/O keeps flowing
    /// throughout. Stalling a live `/var` to make it bigger would turn a
    /// routine day-2 operation into an outage.
    ///
    /// Growth only. The kernel would accept a smaller size, but a filesystem
    /// above it generally cannot, and the volume layer refuses the shrink
    /// before this is ever reached.
    pub fn update_size(&self, new_capacity_bytes: u64) -> DriveResult<()> {
        let dev_id = self.dev_id.load(Ordering::Relaxed);
        if dev_id < 0 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "ublk device is not running — nothing to resize"
            )));
        }
        if !self.resizable() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "ublk device {dev_id} was created without UBLK_F_UPDATE_SIZE (kernel too old) \
                 — the volume grew but /dev/ublkb{dev_id} cannot follow"
            )));
        }
        if new_capacity_bytes % 512 != 0 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "ublk size must be a whole number of 512-byte sectors, got {new_capacity_bytes}"
            )));
        }
        let sectors = new_capacity_bytes / 512;
        let current = self.dev_sectors.load(Ordering::Relaxed);
        if sectors == current {
            return Ok(());
        }
        if sectors < current {
            return Err(DriveError::Other(anyhow::anyhow!(
                "refusing to shrink ublk device {dev_id} from {current} to {sectors} sectors: \
                 a mounted filesystem cannot follow a device that got smaller"
            )));
        }

        let ctrl_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/ublk-control")
            .map_err(|e| {
                DriveError::Other(anyhow::anyhow!("failed to open /dev/ublk-control: {e}"))
            })?;
        let mut ring: IoUring<squeue::Entry128> = IoUring::builder()
            .build(8)
            .map_err(|e| DriveError::Other(anyhow::anyhow!("io_uring create failed: {e}")))?;

        submit_ctrl_cmd(
            &mut ring,
            ctrl_file.as_raw_fd(),
            UBLK_U_CMD_UPDATE_SIZE,
            dev_id as u32,
            0,
            0,
            // The new size rides in cmd->data[0], in sectors.
            sectors,
        )?;

        self.dev_sectors.store(sectors, Ordering::Relaxed);
        tracing::info!(
            "ublk device {dev_id} resized: {current} -> {sectors} sectors ({new_capacity_bytes} bytes)"
        );
        Ok(())
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

        // --- Either adopt an existing device, or create one ---
        //
        // Adoption is the same server doing the same job, minus the two steps
        // that belong to creation: the device is already there and its
        // parameters are already set. What it must not do is guess the
        // geometry — the queue count and depth are fixed at creation, and a
        // server that comes back with different ones leaves the kernel holding
        // requests on queues nobody is fetching from.
        let (assigned_id, nr_queues, queue_depth, capacity) = if let Some(id) = self.adopt {
            let mut info = UblkCtrlDevInfo::default();
            submit_ctrl_cmd(
                &mut ctrl_ring,
                ctrl_fd,
                UBLK_U_CMD_GET_DEV_INFO,
                id,
                &mut info as *mut UblkCtrlDevInfo as u64,
                std::mem::size_of::<UblkCtrlDevInfo>() as u32,
                0,
            )
            .map_err(|e| {
                DriveError::Other(anyhow::anyhow!(
                    "ublk: cannot read /dev/ublkb{id} to adopt it: {e}"
                ))
            })?;

            if info.flags & UBLK_F_USER_RECOVERY == 0 {
                return Err(DriveError::Other(anyhow::anyhow!(
                    "ublk: /dev/ublkb{id} was created without UBLK_F_USER_RECOVERY and \
                     cannot be adopted — the flag is fixed at creation, so whoever made \
                     this device had to ask for it"
                )));
            }

            // Wait for the kernel to quiesce the device before asking to
            // recover it.
            //
            // The old server standing down is not the same event as the device
            // being ready to hand over. The kernel notices the server's
            // io_uring context is gone, stops the queues and only then moves
            // the device LIVE -> QUIESCED; START_USER_RECOVERY before that
            // returns EBUSY. The two are milliseconds apart and the race is
            // reliably lost, because the adopting server asks the instant the
            // old one exits — which read as "is the previous server really
            // gone?" when it was gone and the kernel had not caught up.
            //
            // So poll the state rather than the process. Bounded, because a
            // device that never quiesces is a real failure and waiting forever
            // for it would hold the boot.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let mut cur = UblkCtrlDevInfo::default();
                submit_ctrl_cmd(
                    &mut ctrl_ring,
                    ctrl_fd,
                    UBLK_U_CMD_GET_DEV_INFO,
                    id,
                    &mut cur as *mut UblkCtrlDevInfo as u64,
                    std::mem::size_of::<UblkCtrlDevInfo>() as u32,
                    0,
                )
                .map_err(|e| {
                    DriveError::Other(anyhow::anyhow!(
                        "ublk: cannot read /dev/ublkb{id} while waiting to adopt it: {e}"
                    ))
                })?;

                if cur.state == UBLK_S_DEV_QUIESCED {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "ublk: /dev/ublkb{id} never quiesced (state {}, wanted {}): the \
                         previous server has stood down but the kernel still holds the \
                         device — it may still have I/O in flight",
                        cur.state,
                        UBLK_S_DEV_QUIESCED
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            submit_ctrl_cmd(
                &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_START_USER_RECOVERY, id, 0, 0, 0,
            )
            .map_err(|e| {
                DriveError::Other(anyhow::anyhow!(
                    "ublk: /dev/ublkb{id} would not begin recovery: {e} (is the previous \
                     server really gone?)"
                ))
            })?;

            self.dev_id.store(id as i32, Ordering::Relaxed);
            self.resizable
                .store(info.flags & UBLK_F_UPDATE_SIZE != 0, Ordering::Relaxed);
            // Geometry comes from the kernel; capacity comes from the volume
            // this server was handed. The device's parameters were set when it
            // was created and are not touched here — an adopting server serves
            // the same device, it does not redefine it.
            tracing::info!(
                "ublk: adopting /dev/ublkb{id} — {} queue(s), depth {}, serving {} bytes",
                info.nr_hw_queues, info.queue_depth, capacity
            );
            (id, info.nr_hw_queues, info.queue_depth, capacity)
        } else {
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

            // Ask the kernel what it supports before asking it for anything. A
            // flag an older kernel does not know fails ADD_DEV outright, so an
            // unsupported UBLK_F_UPDATE_SIZE must cost the resize and not the
            // device (#19).
            let kernel_features = query_features(&mut ctrl_ring, ctrl_fd).unwrap_or(0);
            let resizable = kernel_features & UBLK_F_UPDATE_SIZE != 0;
            let mut flags = UBLK_F_URING_CMD_COMP_IN_TASK | UBLK_F_CMD_IOCTL_ENCODE;
            if resizable {
                flags |= UBLK_F_UPDATE_SIZE;
            } else {
                tracing::info!(
                    "ublk: this kernel does not offer UBLK_F_UPDATE_SIZE — the device \
                     will not follow a volume resize"
                );
            }
            if self.recoverable {
                if kernel_features & UBLK_F_USER_RECOVERY != 0 {
                    flags |= UBLK_F_USER_RECOVERY;
                    // Reissue rather than fail: what was in flight when the old
                    // server went is handed to the new one. On a root
                    // filesystem the difference is a pause versus an EIO.
                    if kernel_features & UBLK_F_USER_RECOVERY_REISSUE != 0 {
                        flags |= UBLK_F_USER_RECOVERY_REISSUE;
                    }
                } else {
                    tracing::warn!(
                        "ublk: this kernel does not offer UBLK_F_USER_RECOVERY — this \
                         device cannot be handed to another process, so whatever serves \
                         it cannot be restarted"
                    );
                }
            }

            let mut dev_info = UblkCtrlDevInfo {
                nr_hw_queues: nr_queues,
                queue_depth,
                max_io_buf_bytes: DEFAULT_MAX_IO_BYTES,
                dev_id: req_id,
                ublksrv_pid: std::process::id() as i32,
                flags,
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
            // What the kernel echoed back, not what was asked for.
            self.resizable
                .store(dev_info.flags & UBLK_F_UPDATE_SIZE != 0, Ordering::Relaxed);
            tracing::info!(
                "ublk device created: dev_id={}{}",
                assigned_id,
                if dev_info.flags & UBLK_F_USER_RECOVERY != 0 {
                    " (recoverable)"
                } else {
                    ""
                }
            );
            (assigned_id, nr_queues, queue_depth, capacity)
        };

        // --- SET_PARAMS ---
        //
        // Creation only. An adopted device already has its parameters, and
        // setting them again on a live device would be redefining something a
        // mounted filesystem is currently using.
        let sectors = capacity / 512;
        self.dev_sectors.store(sectors, Ordering::Relaxed);
        if self.adopt.is_none() {
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
        }

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

        // --- The device goes live (all queues now registered with kernel) ---
        //
        // Creating: START_DEV. Adopting: END_USER_RECOVERY. Both carry the
        // server's pid in data[0], and both mean the same thing to the kernel
        // — there is a server here now, hand it the queues.
        let (go_live, what) = if self.adopt.is_some() {
            (UBLK_U_CMD_END_USER_RECOVERY, "recovered")
        } else {
            (UBLK_U_CMD_START_DEV, "started")
        };
        submit_ctrl_cmd(
            &mut ctrl_ring, ctrl_fd, go_live,
            assigned_id, 0, 0,
            std::process::id() as u64,
        )?;
        tracing::debug!("ublk: /dev/ublkb{assigned_id} {what}");
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
        self.running.store(false, Ordering::SeqCst);

        // A recoverable device is **let go of**, not stopped.
        //
        // STOP_DEV tears the device down: the kernel shuts the block device,
        // and every filesystem mounted on it takes write errors on the way
        // out —
        //
        //     EXT4-fs (ublkb4): shut down requested (2)
        //     I/O error, dev ublkb4, sector 120 op 0x1:(WRITE)
        //     JBD2: I/O error when updating journal superblock
        //
        // — after which there is nothing left to recover and no successor can
        // adopt anything. That is right for a server that is finished with a
        // device and wrong for one being handed over, and asking for
        // UBLK_F_USER_RECOVERY at creation is precisely the statement that
        // this device outlives its server. So honour it: close the queues,
        // release the char device, and leave. The kernel sees the server go,
        // quiesces the device, and holds it for whoever comes next.
        //
        // The filesystems stay mounted throughout. They pause while the device
        // is quiesced and resume when recovery completes, which is the whole
        // point of a handover — a node whose root is on one of these cannot
        // unmount it to be polite.
        // Adopted devices too: this server inherited a device that was created
        // recoverable, and handing it on is the same act as handing it over
        // was. A node upgrades its engine by doing this repeatedly.
        let recoverable = self.recoverable || self.adopt.is_some();
        if recoverable {
            tracing::info!(
                "ublk: releasing /dev/ublkb{assigned_id} for recovery (not stopping it)"
            );
        } else {
            tracing::info!("ublk server shutting down");
            // --- STOP_DEV ---
            let _ = submit_ctrl_cmd(
                &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_STOP_DEV, assigned_id, 0, 0, 0,
            );
        }

        // Wait for all workers to exit
        for w in workers {
            let _ = w.join();
        }

        // Release every reference to /dev/ublkcN BEFORE DEL_DEV: the kernel's
        // synchronous DEL_DEV blocks until the char device is fully released
        // (mmaps and fds), so issuing it while we still hold them deadlocks
        // the shutdown.
        //
        // For a recoverable device this release *is* the handover: dropping
        // the last reference is what tells the kernel the server has gone.
        for desc_ptr in &desc_ptrs {
            unsafe {
                libc::munmap(*desc_ptr as *mut libc::c_void, desc_buf_size);
            }
        }
        drop(char_file);

        if recoverable {
            tracing::info!("ublk device /dev/ublkb{assigned_id} released");
            return Ok(());
        }

        // --- DEL_DEV ---
        let _ = submit_ctrl_cmd(
            &mut ctrl_ring, ctrl_fd, UBLK_U_CMD_DEL_DEV, assigned_id, 0, 0, 0,
        );

        tracing::info!("ublk device /dev/ublkb{} removed", assigned_id);
        // ctrl_file dropped here, closing its fd
        Ok(())
    }
}

// ===========================================================================
// Control command submission
// ===========================================================================

/// Submit a control command on `/dev/ublk-control` and wait for the CQE.
/// Ask the kernel for its `UBLK_F_*` feature mask.
///
/// Older kernels do not implement `GET_FEATURES` at all; that is not an error
/// here, it just means no optional feature can be negotiated.
/// Who is serving `/dev/ublkbN`, according to the kernel.
///
/// `None` if there is no such device; `Some(0)` if it exists with no server,
/// which is what an orphan looks like after its server died.
///
/// The kernel's own record, written at `ADD_DEV`. It is the only trustworthy
/// answer to "who has to stand down before I can take this over" — a pid file
/// would be a guess about a process that may already have been replaced.
/// Wait until every device is serving again, or say which are not.
///
/// A thread that has not exited is a thread that reached its I/O loop; it is
/// not a device the kernel will accept reads on. Recovery finishes when
/// END_USER_RECOVERY brings the device back to LIVE, and until then a read of
/// a filesystem on it fails — which is how an engine that had just adopted six
/// devices could not create a directory on one of them, and died with EIO on
/// the filesystem it was serving itself.
///
/// Returns the ids that never came back, empty meaning all of them did.
pub fn wait_live(dev_ids: &[u32], timeout: std::time::Duration) -> DriveResult<Vec<u32>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut pending = Vec::new();
        for &id in dev_ids {
            if dev_state(id)? != Some(UBLK_S_DEV_LIVE) {
                pending.push(id);
            }
        }
        if pending.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(pending);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The device's state, or `None` if there is no such device.
pub fn dev_state(dev_id: u32) -> DriveResult<Option<u16>> {
    let ctrl = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/ublk-control")
        .map_err(|e| {
            DriveError::Other(anyhow::anyhow!(
                "cannot open /dev/ublk-control: {e} (is ublk_drv loaded?)"
            ))
        })?;
    let mut ring: IoUring<squeue::Entry128> = IoUring::builder()
        .build(8)
        .map_err(|e| DriveError::Other(anyhow::anyhow!("io_uring create failed: {e}")))?;
    let mut info = UblkCtrlDevInfo::default();
    match submit_ctrl_cmd(
        &mut ring,
        ctrl.as_raw_fd(),
        UBLK_U_CMD_GET_DEV_INFO,
        dev_id,
        &mut info as *mut UblkCtrlDevInfo as u64,
        std::mem::size_of::<UblkCtrlDevInfo>() as u32,
        0,
    ) {
        Ok(_) => Ok(Some(info.state)),
        Err(_) => Ok(None),
    }
}

pub fn server_pid(dev_id: u32) -> DriveResult<Option<i32>> {
    let ctrl = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/ublk-control")
        .map_err(|e| {
            DriveError::Other(anyhow::anyhow!(
                "cannot open /dev/ublk-control: {e} (is ublk_drv loaded?)"
            ))
        })?;
    let mut ring: IoUring<squeue::Entry128> = IoUring::builder()
        .build(8)
        .map_err(|e| DriveError::Other(anyhow::anyhow!("io_uring create failed: {e}")))?;
    let mut info = UblkCtrlDevInfo::default();
    match submit_ctrl_cmd(
        &mut ring,
        ctrl.as_raw_fd(),
        UBLK_U_CMD_GET_DEV_INFO,
        dev_id,
        &mut info as *mut UblkCtrlDevInfo as u64,
        std::mem::size_of::<UblkCtrlDevInfo>() as u32,
        0,
    ) {
        Ok(_) => Ok(Some(info.ublksrv_pid)),
        // No such device is an answer, not a failure.
        Err(_) => Ok(None),
    }
}

/// Ask whoever is serving these devices to stand down, and wait until it has.
///
/// **This is the handover, not a hunt.** The kernel will not run two servers
/// for one device, so a takeover begins by ending the one in place — and the
/// kernel is asked who that is rather than guessed at. `SIGTERM` first,
/// because the outgoing server has a slab to flush; `SIGKILL` only if it will
/// not leave, because a device held by a half-dead server is worse than one
/// whose server was shot.
///
/// Several devices usually share a server — the initramfs engine serves four
/// from one process — so the pids are collected and asked once.
/// Every ublk device the kernel currently has, by id.
///
/// Read from sysfs, because the kernel is the only party that knows: a device
/// outlives the process that created it, which is the whole basis of the
/// handover.
pub fn devices() -> DriveResult<Vec<u32>> {
    let mut ids = Vec::new();
    let dir = match std::fs::read_dir("/sys/class/ublk-char") {
        Ok(d) => d,
        // No ublk driver loaded, or no devices: not an error, just none.
        Err(_) => return Ok(ids),
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name.strip_prefix("ublkc") {
            if let Ok(id) = n.parse::<u32>() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Devices served by the same processes as `dev_ids`, but not in that list.
///
/// Standing a server down stops **every** device it serves, not the ones the
/// caller happened to name. Anything here is a device that would be left with
/// no server at all — its filesystem stays mounted and every I/O to it returns
/// EIO, at a moment when the node has no way to recover it.
pub fn also_served_by(dev_ids: &[u32]) -> DriveResult<Vec<u32>> {
    let mut pids: Vec<i32> = Vec::new();
    for &id in dev_ids {
        if let Some(pid) = server_pid(id)? {
            if pid > 0 && pid != std::process::id() as i32 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let mut orphans = Vec::new();
    for id in devices()? {
        if dev_ids.contains(&id) {
            continue;
        }
        if let Some(pid) = server_pid(id)? {
            if pids.contains(&pid) {
                orphans.push(id);
            }
        }
    }
    Ok(orphans)
}

pub fn stand_down(dev_ids: &[u32], grace: std::time::Duration) -> DriveResult<()> {
    let mut pids: Vec<i32> = Vec::new();
    for &id in dev_ids {
        if let Some(pid) = server_pid(id)? {
            // Zero is an already-orphaned device; ourselves would be the
            // handover ending itself.
            //
            // And never pid 1. A ublk server is never init, so a device
            // reporting init as its server is reporting something stale or
            // wrong — and SIGTERM to init is not a signal, it is a shutdown
            // request. On this node that means PID 1 begins powering the
            // machine off in the middle of a storage handover, which presents
            // as "Kernel panic - Attempted to kill init" at the exact moment
            // the devices change hands, with nothing in any log to say why.
            if pid == 1 {
                tracing::error!(
                    "ublk: /dev/ublkb{id} names pid 1 as its server, which cannot be true \
                     — not signalling it. The handover will wait for this device instead \
                     of shutting the node down."
                );
                continue;
            }
            if pid > 0 && pid != std::process::id() as i32 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    if pids.is_empty() {
        return Ok(());
    }

    for &pid in &pids {
        tracing::info!("ublk: asking server {pid} to stand down");
        // SAFETY: kill on a pid the kernel just reported.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    // Wait for the *devices*, not for the process.
    //
    // What adoption needs is every device quiesced, and the kernel says so
    // directly. Whether the old server has finished exiting afterwards is its
    // own business: a process that has released every char device is holding
    // nothing, and waiting for it to tidy up is waiting for nothing.
    //
    // That distinction was fifteen seconds of a thirty-nine second boot. The
    // incumbent released all seven devices in under seven seconds and then sat
    // there; stand_down burned its entire grace on the pid, killed it, and
    // recovery itself took 0.6s. The node was waiting on a formality.
    let deadline = std::time::Instant::now() + grace;
    let mut killed = false;
    loop {
        let pending: Vec<u32> = dev_ids
            .iter()
            .copied()
            .filter(|&id| !matches!(dev_state(id), Ok(Some(UBLK_S_DEV_QUIESCED)) | Ok(None)))
            .collect();
        if pending.is_empty() {
            tracing::info!("ublk: {} device(s) quiesced and ready to adopt", dev_ids.len());
            return Ok(());
        }

        if std::time::Instant::now() >= deadline {
            // SAFETY: signal 0 asks whether it is there without touching it.
            let alive: Vec<i32> = pids
                .iter()
                .copied()
                .filter(|&p| unsafe { libc::kill(p, 0) == 0 })
                .collect();
            if !killed && !alive.is_empty() {
                // Only now, and only because a device is still not quiesced —
                // which is the one thing that actually blocks the handover.
                for pid in alive {
                    tracing::warn!(
                        "ublk: {} device(s) still not quiesced and server {pid} is still \
                         running; killing it",
                        pending.len()
                    );
                    // SAFETY: as above.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                killed = true;
                // A short second chance: SIGKILL closes the char devices, and
                // the kernel quiesces on that.
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            tracing::warn!(
                "ublk: {} device(s) never quiesced: {}",
                pending.len(),
                pending.iter().map(|d| format!("/dev/ublkb{d}")).collect::<Vec<_>>().join(", ")
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn query_features(
    ring: &mut IoUring<squeue::Entry128>,
    ctrl_fd: RawFd,
) -> DriveResult<u64> {
    let mut features: u64 = 0;
    submit_ctrl_cmd(
        ring,
        ctrl_fd,
        UBLK_U_CMD_GET_FEATURES,
        u32::MAX,
        &mut features as *mut u64 as u64,
        std::mem::size_of::<u64>() as u32,
        0,
    )?;
    Ok(features)
}

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
    //
    // Bounded wait, not an indefinite one.
    //
    // `submit_and_wait(1)` sleeps until the kernel has something to say. On a
    // busy device that is the right thing and costs nothing; on an *idle* one
    // it means the worker never looks at `running` again, and a shutdown is
    // noticed only when some I/O happens to arrive. During a handover that is
    // the difference between releasing a device and never releasing it: six
    // devices with traffic stood down in seconds, the seventh — a
    // freshly-mounted, idle volume — never did, so the process could not exit,
    // and the successor spent its entire fifteen-second grace waiting before
    // killing it.
    //
    // A tenth of a second bounds how long a shutdown can go unnoticed. Ten
    // wakeups a second on a queue that is doing nothing is not a cost worth
    // measuring against a storage handover that hangs.
    let idle_wait = types::Timespec::new().sec(0).nsec(100_000_000);
    let wait_args = types::SubmitArgs::new().timespec(&idle_wait);
    while running.load(Ordering::Relaxed) {
        match ring.submitter().submit_with_args(1, &wait_args) {
            Ok(_) => {}
            // The wait expired with nothing to do: go round, which is where
            // `running` is looked at.
            Err(ref e) if e.raw_os_error() == Some(libc::ETIME) => {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
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

    /// The derived command numbers against the hex an strace shows, so the
    /// encoder and the kernel's ABI cannot drift apart unnoticed (#19).
    #[test]
    fn ctrl_command_numbers_match_the_ioctl_encoding() {
        assert_eq!(UBLK_U_CMD_ADD_DEV, 0xC020_7504);
        assert_eq!(UBLK_U_CMD_DEL_DEV, 0xC020_7505);
        assert_eq!(UBLK_U_CMD_START_DEV, 0xC020_7506);
        assert_eq!(UBLK_U_CMD_STOP_DEV, 0xC020_7507);
        assert_eq!(UBLK_U_CMD_SET_PARAMS, 0xC020_7508);
        // _IOR, not _IOWR: 0x8… rather than 0xC…, and the difference is the
        // whole command as far as the kernel's dispatch is concerned.
        assert_eq!(UBLK_U_CMD_GET_FEATURES, 0x8020_7513);
        assert_ne!(UBLK_U_CMD_GET_FEATURES, ublk_ctrl_cmd(0x13));
        assert_eq!(UBLK_U_CMD_UPDATE_SIZE, 0xC020_7515);
        // And the flag the resize is negotiated with.
        assert_eq!(UBLK_F_UPDATE_SIZE, 1 << 10);

        // Handover. START/END are _IOWR like the rest; GET_DEV_INFO is _IOR,
        // and encoding it as _IOWR would not reach the kernel's handler at all
        // — the same mistake that kept UBLK_F_UPDATE_SIZE from ever being
        // negotiated, which is why it is asserted rather than assumed.
        assert_eq!(UBLK_U_CMD_START_USER_RECOVERY, 0xC020_7510);
        assert_eq!(UBLK_U_CMD_END_USER_RECOVERY, 0xC020_7511);
        assert_eq!(UBLK_U_CMD_GET_DEV_INFO, 0x8020_7502);
        assert_ne!(UBLK_U_CMD_GET_DEV_INFO, ublk_ctrl_cmd(0x02));
        assert_eq!(UBLK_F_USER_RECOVERY, 1 << 3);
        assert_eq!(UBLK_F_USER_RECOVERY_REISSUE, 1 << 4);
    }

    /// The two options are independent and neither is on by default.
    ///
    /// `recoverable` belongs to whoever *creates* a device and is fixed at
    /// that moment; `adopting` belongs to whoever takes one over. A server
    /// asked to do both is creating nothing, so adoption wins.
    #[test]
    fn recovery_is_opt_in_at_both_ends() {
        use crate::drive::filedev::FileDevice;

        let dir = std::env::temp_dir().join("stormblock-ublk-opts");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("d-{}.bin", uuid::Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { FileDevice::open_with_capacity(&p, 1 << 20).await.unwrap() });
        let dev: Arc<dyn BlockDevice> = Arc::new(dev);

        let plain = UblkServer::new(dev.clone());
        assert!(!plain.recoverable, "a device is not recoverable unless asked");
        assert!(plain.adopt.is_none());

        let made = UblkServer::new(dev.clone()).recoverable(true).with_dev_id(0);
        assert!(made.recoverable);
        assert!(made.adopt.is_none(), "creating, not adopting");

        let taken = UblkServer::new(dev).adopting(7);
        assert_eq!(taken.adopt, Some(7));
        assert!(!taken.recoverable, "adoption does not create anything");

        let _ = std::fs::remove_file(&p);
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
