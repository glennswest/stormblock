//! Shared io_uring-style ring buffer IPC protocol.
//!
//! Defines the wire format and ring operations for zero-copy block I/O between
//! StormFS and StormBlock via shared memory (memfd). Linux-only — macOS stubs
//! return `Err(Unsupported)`.
//!
//! Memory layout (per client):
//! ```text
//! Offset    Size          Content
//! 0x0000    4096B         RingHeader (control block, atomic indices)
//! 0x1000    QD × 64B      Submission Queue (commands from client)
//! varies    QD × 32B      Completion Queue (results from server)
//! varies    QD × BUF_SZ   Data Buffers (page-aligned, zero-copy I/O)
//! ```

use std::sync::atomic::{AtomicU32, Ordering};

/// Magic number for ring header validation ("URNG").
pub const RING_MAGIC: u32 = 0x55524E47;

/// Protocol version.
pub const RING_VERSION: u16 = 1;

/// Default submission/completion queue depth.
pub const DEFAULT_QUEUE_DEPTH: u16 = 64;

/// Default data buffer size per slot (1 MB).
pub const DEFAULT_BUF_SIZE: u32 = 1024 * 1024;

// Operation codes (matches ublk kernel ABI ordering).
pub const OP_READ: u8 = 0;
pub const OP_WRITE: u8 = 1;
pub const OP_FLUSH: u8 = 2;
pub const OP_DISCARD: u8 = 3;

// ---------------------------------------------------------------------------
// Wire structures
// ---------------------------------------------------------------------------

/// Control block at the start of shared memory.
///
/// Producer/consumer indices are cache-line separated to avoid false sharing.
/// All fields after `magic` are written once by the server during setup; the
/// atomic indices are the only fields modified at runtime.
#[repr(C, align(4096))]
pub struct RingHeader {
    /// Must be [`RING_MAGIC`].
    pub magic: u32,
    /// Protocol version (currently 1).
    pub version: u16,
    /// Number of SQ/CQ entries.
    pub queue_depth: u16,
    /// Size of each data buffer in bytes.
    pub buf_size: u32,
    /// Padding to push atomic indices to separate cache lines.
    pub _pad0: [u8; 52],

    // --- cache line boundary (offset 64) ---
    /// SQ tail — written by client (producer).
    pub sq_tail: AtomicU32,
    pub _pad1: [u8; 60],

    // --- cache line boundary (offset 128) ---
    /// SQ head — written by server (consumer).
    pub sq_head: AtomicU32,
    pub _pad2: [u8; 60],

    // --- cache line boundary (offset 192) ---
    /// CQ tail — written by server (producer).
    pub cq_tail: AtomicU32,
    pub _pad3: [u8; 60],

    // --- cache line boundary (offset 256) ---
    /// CQ head — written by client (consumer).
    pub cq_head: AtomicU32,
    pub _pad4: [u8; 60],

    // --- cache line boundary (offset 320) ---
    /// Byte offset of the submission queue from shm base.
    pub sq_offset: u32,
    /// Byte offset of the completion queue from shm base.
    pub cq_offset: u32,
    /// Byte offset of the data buffer pool from shm base.
    pub data_offset: u32,
    /// Total shared memory size in bytes.
    pub total_size: u64,
    /// Block device capacity in bytes.
    pub capacity: u64,
    /// Block device sector size in bytes.
    pub block_size: u32,
    // remaining bytes to fill 4096-byte header are implicit padding from align(4096)
}

/// Submission queue entry — one command from client to server.
#[repr(C, align(64))]
pub struct RingCommand {
    /// Tag correlating this command with its completion.
    pub tag: u16,
    /// Operation code: OP_READ / OP_WRITE / OP_FLUSH / OP_DISCARD.
    pub op: u8,
    /// Reserved flags.
    pub flags: u8,
    /// Index into the data buffer pool.
    pub buf_idx: u16,
    pub _pad: u16,
    /// Byte offset on the block device.
    pub offset: u64,
    /// Number of bytes to transfer.
    pub length: u32,
    pub _pad2: [u8; 36],
}

/// Completion queue entry — result from server to client.
#[repr(C, align(32))]
pub struct RingCompletion {
    /// Matches the tag from the original command.
    pub tag: u16,
    /// 0 = success, negative = errno.
    pub status: i16,
    /// Bytes transferred (for reads) or 0.
    pub result: u32,
    pub _pad: [u8; 24],
}

// ---------------------------------------------------------------------------
// Ring operations
// ---------------------------------------------------------------------------

/// Initialize a `RingHeader` in already-zeroed shared memory.
///
/// # Safety
/// `header` must point to a valid, mmap'd, zeroed region of at least 4096 bytes.
pub unsafe fn ring_header_init(
    header: *mut RingHeader,
    queue_depth: u16,
    buf_size: u32,
    capacity: u64,
    block_size: u32,
) {
    let sq_offset: u32 = 4096; // right after the header page
    let cq_offset: u32 = sq_offset + (queue_depth as u32) * 64;
    // Align data buffers to page boundary
    let data_unaligned = cq_offset + (queue_depth as u32) * 32;
    let data_offset = (data_unaligned + 4095) & !4095;
    let total_size = data_offset as u64 + (queue_depth as u64) * (buf_size as u64);

    (*header).magic = RING_MAGIC;
    (*header).version = RING_VERSION;
    (*header).queue_depth = queue_depth;
    (*header).buf_size = buf_size;
    (*header).sq_tail.store(0, Ordering::Relaxed);
    (*header).sq_head.store(0, Ordering::Relaxed);
    (*header).cq_tail.store(0, Ordering::Relaxed);
    (*header).cq_head.store(0, Ordering::Relaxed);
    (*header).sq_offset = sq_offset;
    (*header).cq_offset = cq_offset;
    (*header).data_offset = data_offset;
    (*header).total_size = total_size;
    (*header).capacity = capacity;
    (*header).block_size = block_size;
}

/// Compute total shared memory size for the given parameters.
pub fn shm_total_size(queue_depth: u16, buf_size: u32) -> usize {
    let sq_offset: u32 = 4096;
    let cq_offset = sq_offset + (queue_depth as u32) * 64;
    let data_unaligned = cq_offset + (queue_depth as u32) * 32;
    let data_offset = (data_unaligned + 4095) & !4095;
    (data_offset as u64 + (queue_depth as u64) * (buf_size as u64)) as usize
}

/// Push a command onto the submission queue.
///
/// Returns `false` if the queue is full.
///
/// # Safety
/// `header` and `sq_base` must be valid pointers into the shared memory region.
pub unsafe fn sq_push(header: *const RingHeader, sq_base: *mut RingCommand, cmd: &RingCommand) -> bool {
    let depth = (*header).queue_depth as u32;
    let tail = (*header).sq_tail.load(Ordering::Relaxed);
    let head = (*header).sq_head.load(Ordering::Acquire);

    // Full when tail is one lap ahead of head
    if tail.wrapping_sub(head) >= depth {
        return false;
    }

    let idx = tail % depth;
    let slot = sq_base.add(idx as usize);
    std::ptr::copy_nonoverlapping(cmd as *const RingCommand, slot, 1);

    // Release ensures the command data is visible before the tail advances
    (*header).sq_tail.store(tail.wrapping_add(1), Ordering::Release);
    true
}

/// Pop a command from the submission queue (server side).
///
/// Returns `None` if the queue is empty.
///
/// # Safety
/// `header` and `sq_base` must be valid pointers into the shared memory region.
pub unsafe fn sq_pop(header: *const RingHeader, sq_base: *const RingCommand) -> Option<RingCommand> {
    let depth = (*header).queue_depth as u32;
    let head = (*header).sq_head.load(Ordering::Relaxed);
    let tail = (*header).sq_tail.load(Ordering::Acquire);

    if head == tail {
        return None;
    }

    let idx = head % depth;
    let slot = sq_base.add(idx as usize);
    let mut cmd: RingCommand = std::mem::zeroed();
    std::ptr::copy_nonoverlapping(slot, &mut cmd, 1);

    (*header).sq_head.store(head.wrapping_add(1), Ordering::Release);
    Some(cmd)
}

/// Push a completion onto the completion queue.
///
/// Returns `false` if the queue is full.
///
/// # Safety
/// `header` and `cq_base` must be valid pointers into the shared memory region.
pub unsafe fn cq_push(
    header: *const RingHeader,
    cq_base: *mut RingCompletion,
    comp: &RingCompletion,
) -> bool {
    let depth = (*header).queue_depth as u32;
    let tail = (*header).cq_tail.load(Ordering::Relaxed);
    let head = (*header).cq_head.load(Ordering::Acquire);

    if tail.wrapping_sub(head) >= depth {
        return false;
    }

    let idx = tail % depth;
    let slot = cq_base.add(idx as usize);
    std::ptr::copy_nonoverlapping(comp as *const RingCompletion, slot, 1);

    (*header).cq_tail.store(tail.wrapping_add(1), Ordering::Release);
    true
}

/// Pop a completion from the completion queue (client side).
///
/// Returns `None` if the queue is empty.
///
/// # Safety
/// `header` and `cq_base` must be valid pointers into the shared memory region.
pub unsafe fn cq_pop(
    header: *const RingHeader,
    cq_base: *const RingCompletion,
) -> Option<RingCompletion> {
    let depth = (*header).queue_depth as u32;
    let head = (*header).cq_head.load(Ordering::Relaxed);
    let tail = (*header).cq_tail.load(Ordering::Acquire);

    if head == tail {
        return None;
    }

    let idx = head % depth;
    let slot = cq_base.add(idx as usize);
    let mut comp: RingCompletion = std::mem::zeroed();
    std::ptr::copy_nonoverlapping(slot, &mut comp, 1);

    (*header).cq_head.store(head.wrapping_add(1), Ordering::Release);
    Some(comp)
}

/// Check if the submission queue is full.
///
/// # Safety
/// `header` must be a valid pointer.
pub unsafe fn sq_full(header: *const RingHeader) -> bool {
    let depth = (*header).queue_depth as u32;
    let tail = (*header).sq_tail.load(Ordering::Relaxed);
    let head = (*header).sq_head.load(Ordering::Acquire);
    tail.wrapping_sub(head) >= depth
}

/// Check if the completion queue is empty.
///
/// # Safety
/// `header` must be a valid pointer.
pub unsafe fn cq_empty(header: *const RingHeader) -> bool {
    let head = (*header).cq_head.load(Ordering::Relaxed);
    let tail = (*header).cq_tail.load(Ordering::Acquire);
    head == tail
}

/// Return a pointer to data buffer at index `idx`.
///
/// # Safety
/// `shm_base` must be a valid pointer to the start of the shared memory region.
/// `idx` must be less than `queue_depth`.
pub unsafe fn data_buf_ptr(shm_base: *mut u8, header: *const RingHeader, idx: u16) -> *mut u8 {
    let offset = (*header).data_offset as usize + (idx as usize) * ((*header).buf_size as usize);
    shm_base.add(offset)
}

// ---------------------------------------------------------------------------
// Shared memory setup (Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub mod shm {
    use super::*;
    use std::os::unix::io::RawFd;

    /// Create a new shared memory region via `memfd_create`.
    ///
    /// Returns `(fd, mmap_ptr, total_size)`.
    pub fn create_shm(
        queue_depth: u16,
        buf_size: u32,
    ) -> std::io::Result<(RawFd, *mut u8, usize)> {
        let total = shm_total_size(queue_depth, buf_size);

        // memfd_create(name, MFD_CLOEXEC)
        let name = b"stormblock-ring\0";
        let fd = unsafe {
            libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC)
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Set size
        let ret = unsafe { libc::ftruncate(fd, total as libc::off_t) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        // mmap
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok((fd, ptr as *mut u8, total))
    }

    /// Attach to an existing shared memory fd (client side).
    ///
    /// Returns `(mmap_ptr, total_size)`.
    pub fn attach_shm(fd: RawFd) -> std::io::Result<(*mut u8, usize)> {
        // Get the size from the fd
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::fstat(fd, &mut stat) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let total = stat.st_size as usize;

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok((ptr as *mut u8, total))
    }

    /// Create an eventfd for signaling.
    pub fn create_eventfd() -> std::io::Result<RawFd> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }

    /// Write to an eventfd (signal).
    pub fn eventfd_write(fd: RawFd, val: u64) -> std::io::Result<()> {
        let ret = unsafe {
            libc::write(fd, &val as *const u64 as *const libc::c_void, 8)
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Read from an eventfd (consume signal). Returns the counter value.
    pub fn eventfd_read(fd: RawFd) -> std::io::Result<u64> {
        let mut val: u64 = 0;
        let ret = unsafe {
            libc::read(fd, &mut val as *mut u64 as *mut libc::c_void, 8)
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(val)
        }
    }

    /// Blocking eventfd read using poll(2).
    pub fn eventfd_wait(fd: RawFd, timeout_ms: i32) -> std::io::Result<u64> {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if ret == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "eventfd poll timeout",
            ));
        }
        eventfd_read(fd)
    }

    /// Unmap shared memory region.
    ///
    /// # Safety
    /// `ptr` and `size` must correspond to a valid mmap'd region.
    pub unsafe fn unmap_shm(ptr: *mut u8, size: usize) {
        libc::munmap(ptr as *mut libc::c_void, size);
    }
}

/// Non-Linux stubs.
#[cfg(not(target_os = "linux"))]
pub mod shm {
    use std::os::unix::io::RawFd;

    fn unsupported() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "shared memory ring IPC is Linux-only",
        )
    }

    pub fn create_shm(_qd: u16, _bs: u32) -> std::io::Result<(RawFd, *mut u8, usize)> {
        Err(unsupported())
    }
    pub fn attach_shm(_fd: RawFd) -> std::io::Result<(*mut u8, usize)> {
        Err(unsupported())
    }
    pub fn create_eventfd() -> std::io::Result<RawFd> {
        Err(unsupported())
    }
    pub fn eventfd_write(_fd: RawFd, _val: u64) -> std::io::Result<()> {
        Err(unsupported())
    }
    pub fn eventfd_read(_fd: RawFd) -> std::io::Result<u64> {
        Err(unsupported())
    }
    pub fn eventfd_wait(_fd: RawFd, _timeout_ms: i32) -> std::io::Result<u64> {
        Err(unsupported())
    }
    /// # Safety
    /// No-op on non-Linux platforms.
    pub unsafe fn unmap_shm(_ptr: *mut u8, _size: usize) {}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate page-aligned memory for test ring buffers.
    /// RingHeader requires 4096-byte alignment; Vec<u8> only guarantees 1.
    struct AlignedMem {
        ptr: *mut u8,
        layout: std::alloc::Layout,
    }

    impl AlignedMem {
        fn new(size: usize) -> Self {
            let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!ptr.is_null());
            Self { ptr, layout }
        }
        fn as_mut_ptr(&mut self) -> *mut u8 {
            self.ptr
        }
    }

    impl Drop for AlignedMem {
        fn drop(&mut self) {
            unsafe { std::alloc::dealloc(self.ptr, self.layout) };
        }
    }

    fn alloc_header() -> AlignedMem {
        AlignedMem::new(shm_total_size(4, 4096))
    }

    #[test]
    fn ring_header_init_and_validate() {
        let mut mem = alloc_header();
        let header = mem.as_mut_ptr() as *mut RingHeader;
        unsafe {
            ring_header_init(header, 4, 4096, 1024 * 1024, 512);
            assert_eq!((*header).magic, RING_MAGIC);
            assert_eq!((*header).version, RING_VERSION);
            assert_eq!((*header).queue_depth, 4);
            assert_eq!((*header).buf_size, 4096);
            assert_eq!((*header).capacity, 1024 * 1024);
            assert_eq!((*header).block_size, 512);
            assert_eq!((*header).sq_offset, 4096);
        }
    }

    #[test]
    fn sq_push_pop_roundtrip() {
        let mut mem = alloc_header();
        let header = mem.as_mut_ptr() as *mut RingHeader;
        unsafe {
            ring_header_init(header, 4, 4096, 1024 * 1024, 512);
            let sq_base = mem.as_mut_ptr().add((*header).sq_offset as usize) as *mut RingCommand;

            let cmd = RingCommand {
                tag: 42,
                op: OP_READ,
                flags: 0,
                buf_idx: 1,
                _pad: 0,
                offset: 8192,
                length: 4096,
                _pad2: [0; 36],
            };

            assert!(sq_push(header, sq_base, &cmd));

            let popped = sq_pop(header, sq_base).unwrap();
            assert_eq!(popped.tag, 42);
            assert_eq!(popped.op, OP_READ);
            assert_eq!(popped.buf_idx, 1);
            assert_eq!(popped.offset, 8192);
            assert_eq!(popped.length, 4096);

            // Queue should be empty now
            assert!(sq_pop(header, sq_base).is_none());
        }
    }

    #[test]
    fn cq_push_pop_roundtrip() {
        let mut mem = alloc_header();
        let header = mem.as_mut_ptr() as *mut RingHeader;
        unsafe {
            ring_header_init(header, 4, 4096, 1024 * 1024, 512);
            let cq_base =
                mem.as_mut_ptr().add((*header).cq_offset as usize) as *mut RingCompletion;

            let comp = RingCompletion {
                tag: 42,
                status: 0,
                result: 4096,
                _pad: [0; 24],
            };

            assert!(cq_push(header, cq_base, &comp));

            let popped = cq_pop(header, cq_base).unwrap();
            assert_eq!(popped.tag, 42);
            assert_eq!(popped.status, 0);
            assert_eq!(popped.result, 4096);

            assert!(cq_pop(header, cq_base).is_none());
        }
    }

    #[test]
    fn ring_full_empty_detection() {
        let mut mem = alloc_header();
        let header = mem.as_mut_ptr() as *mut RingHeader;
        unsafe {
            ring_header_init(header, 4, 4096, 1024 * 1024, 512);
            let sq_base = mem.as_mut_ptr().add((*header).sq_offset as usize) as *mut RingCommand;
            let cq_base =
                mem.as_mut_ptr().add((*header).cq_offset as usize) as *mut RingCompletion;

            // SQ starts empty
            assert!(!sq_full(header));
            assert!(cq_empty(header));

            // Fill the SQ (depth=4)
            for i in 0..4u16 {
                let cmd = RingCommand {
                    tag: i,
                    op: OP_WRITE,
                    flags: 0,
                    buf_idx: i,
                    _pad: 0,
                    offset: 0,
                    length: 0,
                    _pad2: [0; 36],
                };
                assert!(sq_push(header, sq_base, &cmd));
            }

            assert!(sq_full(header));

            // Can't push when full
            let extra = RingCommand {
                tag: 99,
                op: OP_READ,
                flags: 0,
                buf_idx: 0,
                _pad: 0,
                offset: 0,
                length: 0,
                _pad2: [0; 36],
            };
            assert!(!sq_push(header, sq_base, &extra));

            // Pop one — should no longer be full
            let _ = sq_pop(header, sq_base).unwrap();
            assert!(!sq_full(header));

            // CQ: push one, should no longer be empty
            let comp = RingCompletion {
                tag: 0,
                status: 0,
                result: 0,
                _pad: [0; 24],
            };
            assert!(cq_push(header, cq_base, &comp));
            assert!(!cq_empty(header));

            // Pop it
            let _ = cq_pop(header, cq_base).unwrap();
            assert!(cq_empty(header));
        }
    }

    #[test]
    fn data_buf_ptr_offsets() {
        let mut mem = alloc_header();
        let header = mem.as_mut_ptr() as *mut RingHeader;
        let base = mem.as_mut_ptr();
        unsafe {
            ring_header_init(header, 4, 4096, 1024 * 1024, 512);
            let d_off = (*header).data_offset as usize;

            let p0 = data_buf_ptr(base, header, 0);
            assert_eq!(p0 as usize - base as usize, d_off);

            let p1 = data_buf_ptr(base, header, 1);
            assert_eq!(p1 as usize - p0 as usize, 4096);

            let p3 = data_buf_ptr(base, header, 3);
            assert_eq!(p3 as usize - p0 as usize, 3 * 4096);
        }
    }

    #[test]
    fn struct_sizes() {
        assert_eq!(std::mem::size_of::<RingCommand>(), 64);
        assert_eq!(std::mem::size_of::<RingCompletion>(), 32);
        assert_eq!(std::mem::size_of::<RingHeader>(), 4096);
    }
}
