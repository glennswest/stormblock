//! ublk's batch I/O (`UBLK_F_BATCH_IO`, Linux 7.0; #505).
//!
//! In place of a FETCH_REQ per tag and a COMMIT_AND_FETCH_REQ per request, a
//! queue sends three commands: PREP_IO_CMDS once (every tag, with its buffer),
//! one multishot FETCH_IO_CMDS that writes the tags of new requests into a
//! provided buffer (u16 each), and COMMIT_IO_CMDS with the results of every
//! request finished since the last one. Read from the 7.2 uapi
//! (`linux/ublk_cmd.h`), the driver (`ublk_drv.c`) and its selftest
//! (`tools/testing/selftests/ublk/batch.c`):
//!
//! - PREP and COMMIT: `struct ublk_batch_io` in the command area (q_id,
//!   flags, nr_elem, elem_bytes), the elements at `sqe->addr`, their bytes in
//!   `sqe->len`. An element is `{tag u16, buf_index u16, result s32}` and,
//!   with `UBLK_BATCH_F_HAS_BUF_ADDR` (a server that maps the data into its
//!   own buffers, as this one does), the buffer's address: 16 bytes.
//! - FETCH: elem_bytes 2, flags 0, `IORING_URING_CMD_MULTISHOT` in the
//!   command flags, `IOSQE_BUFFER_SELECT` and the buffer group: a provided
//!   buffer ring of one entry registered with `IOU_PBUF_RING_INC` (the
//!   kernel consumes the buffer a completion at a time). Each completion's
//!   `res` is the bytes of tags written after the last ones; while
//!   `IORING_CQE_F_MORE` is set the fetch goes on, else (or `-ENOBUFS`) the
//!   buffer is given back and a fetch sent again.
//!
//! The crate's `UringCmd80` sets no length, command flags or buffer group, so
//! those are written into the SQE at their offsets (the io_uring ABI).

use std::os::unix::io::RawFd;

use io_uring::{opcode, squeue, types};

pub const UBLK_U_IO_PREP_IO_CMDS: u32 = 0xC010_7525;
pub const UBLK_U_IO_COMMIT_IO_CMDS: u32 = 0xC010_7526;
pub const UBLK_U_IO_FETCH_IO_CMDS: u32 = 0xC010_7527;
pub const UBLK_BATCH_F_HAS_BUF_ADDR: u16 = 1 << 1;
/// `IORING_URING_CMD_MULTISHOT`.
pub const URING_CMD_MULTISHOT: u32 = 1 << 1;
/// `IOU_PBUF_RING_INC`.
pub const PBUF_RING_INC: u16 = 2;
/// One element of PREP and COMMIT, with the buffer's address.
pub const ELEM_BYTES: usize = 16;

/// `struct ublk_batch_io`: 16 bytes at the start of the command area.
pub fn batch_cmd(q_id: u16, flags: u16, nr_elem: u16, elem_bytes: u8) -> [u8; 80] {
    let mut c = [0u8; 80];
    c[0..2].copy_from_slice(&q_id.to_ne_bytes());
    c[2..4].copy_from_slice(&flags.to_ne_bytes());
    c[4..6].copy_from_slice(&nr_elem.to_ne_bytes());
    c[6] = elem_bytes;
    c
}

/// One element: tag, buffer index (0: no auto registration), result, and the
/// buffer's address.
pub fn put_elem(out: &mut Vec<u8>, tag: u16, result: i32, buf_addr: u64) {
    out.extend_from_slice(&tag.to_ne_bytes());
    out.extend_from_slice(&0u16.to_ne_bytes());
    out.extend_from_slice(&result.to_ne_bytes());
    out.extend_from_slice(&buf_addr.to_ne_bytes());
}

/// Set an SQE's `len` (offset 24), command flags (28) and buffer group (40):
/// fields `UringCmd80` leaves at zero.
fn patch(sqe: squeue::Entry128, len: u32, cmd_flags: u32, buf_group: Option<u16>) -> squeue::Entry128 {
    // Safety: `Entry128` is `repr(C)`: the 64-byte SQE then 64 command bytes.
    let mut raw: [u8; 128] = unsafe { std::mem::transmute(sqe) };
    raw[24..28].copy_from_slice(&len.to_ne_bytes());
    let f = u32::from_ne_bytes(raw[28..32].try_into().unwrap()) | cmd_flags;
    raw[28..32].copy_from_slice(&f.to_ne_bytes());
    if let Some(g) = buf_group {
        raw[40..42].copy_from_slice(&g.to_ne_bytes());
    }
    unsafe { std::mem::transmute(raw) }
}

/// PREP_IO_CMDS or COMMIT_IO_CMDS over `elems` (kept alive by the caller
/// until the command's completion).
pub fn elems_sqe(char_fd: RawFd, op: u32, q_id: u16, elems: &[u8], user_data: u64) -> squeue::Entry128 {
    let n = (elems.len() / ELEM_BYTES) as u16;
    let sqe = opcode::UringCmd80::new(types::Fd(char_fd), op)
        .cmd(batch_cmd(q_id, UBLK_BATCH_F_HAS_BUF_ADDR, n, ELEM_BYTES as u8))
        .addr(elems.as_ptr() as u64)
        .build()
        .user_data(user_data);
    patch(sqe, elems.len() as u32, 0, None)
}

/// The multishot FETCH_IO_CMDS into buffer group `bgid` of `buf_len` bytes.
pub fn fetch_sqe(char_fd: RawFd, q_id: u16, buf_len: usize, bgid: u16, user_data: u64) -> squeue::Entry128 {
    let sqe = opcode::UringCmd80::new(types::Fd(char_fd), UBLK_U_IO_FETCH_IO_CMDS)
        .cmd(batch_cmd(q_id, 0, (buf_len / 2) as u16, 2))
        .build()
        .user_data(user_data)
        .flags(squeue::Flags::BUFFER_SELECT);
    patch(sqe, 0, URING_CMD_MULTISHOT, Some(bgid))
}

/// A provided buffer ring of one entry, holding the fetch buffer (#505).
pub struct FetchRing {
    /// The ring: one `io_uring_buf` of 16 bytes, page-aligned; its `tail` is
    /// the u16 at byte 14.
    ring: *mut u8,
    /// Where the kernel writes the tags.
    pub buf: Vec<u8>,
    tail: u16,
    /// Read up to here in `buf` since the buffer was last given back.
    pub off: usize,
}

const PAGE: usize = 4096;

impl FetchRing {
    /// Register a ring of one entry as group `bgid` on `ring`, with a fetch
    /// buffer for `depth` tags.
    pub fn register<S: squeue::EntryMarker, C: io_uring::cqueue::EntryMarker>(
        uring: &io_uring::IoUring<S, C>,
        bgid: u16,
        depth: u16,
    ) -> std::io::Result<FetchRing> {
        let layout = std::alloc::Layout::from_size_align(PAGE, PAGE).unwrap();
        let ring = unsafe { std::alloc::alloc_zeroed(layout) };
        if ring.is_null() {
            return Err(std::io::Error::other("no memory for the fetch buffer ring"));
        }
        // Safety: a page that lives as long as the FetchRing, and the ring
        // with it (the worker drops both together).
        if let Err(e) = unsafe { uring.submitter().register_buf_ring_with_flags(ring as u64, 1, bgid, PBUF_RING_INC) } {
            unsafe { std::alloc::dealloc(ring, layout) };
            return Err(e);
        }
        let len = (depth as usize * 2).div_ceil(PAGE) * PAGE;
        Ok(FetchRing { ring, buf: vec![0u8; len], tail: 0, off: 0 })
    }

    /// Give the fetch buffer (back) to the kernel.
    pub fn provide(&mut self) {
        // `io_uring_buf { addr u64, len u32, bid u16, resv u16 }`; the ring's
        // tail shares the first entry's `resv`.
        unsafe {
            let e = self.ring;
            std::ptr::write_unaligned(e as *mut u64, self.buf.as_ptr() as u64);
            std::ptr::write_unaligned(e.add(8) as *mut u32, self.buf.len() as u32);
            std::ptr::write_unaligned(e.add(12) as *mut u16, 0);
            self.tail = self.tail.wrapping_add(1);
            let tail = &*(e.add(14) as *const std::sync::atomic::AtomicU16);
            tail.store(self.tail, std::sync::atomic::Ordering::Release);
        }
        self.off = 0;
    }

    /// The tags in the next `n` bytes the kernel wrote.
    pub fn take(&mut self, n: usize) -> Vec<u16> {
        let end = (self.off + n).min(self.buf.len());
        let tags = self.buf[self.off..end].chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect();
        self.off = end;
        tags
    }
}

impl Drop for FetchRing {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(PAGE, PAGE).unwrap();
        unsafe { std::alloc::dealloc(self.ring, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_commands_match_the_uapi() {
        // _IOWR('u', 0x25..0x27, struct ublk_batch_io): 16 bytes.
        let iowr = |nr: u32| (3u32 << 30) | (16 << 16) | ((b'u' as u32) << 8) | nr;
        assert_eq!(UBLK_U_IO_PREP_IO_CMDS, iowr(0x25));
        assert_eq!(UBLK_U_IO_COMMIT_IO_CMDS, iowr(0x26));
        assert_eq!(UBLK_U_IO_FETCH_IO_CMDS, iowr(0x27));
        let c = batch_cmd(3, UBLK_BATCH_F_HAS_BUF_ADDR, 128, 16);
        assert_eq!(&c[..8], &[3, 0, 2, 0, 128, 0, 16, 0]);
        let mut e = Vec::new();
        put_elem(&mut e, 7, -5, 0x1122_3344_5566_7788);
        assert_eq!(e.len(), ELEM_BYTES);
        assert_eq!(&e[..8], &[7, 0, 0, 0, 0xfb, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn a_fetch_sqe_carries_multishot_buffer_select_and_its_group() {
        let sqe = fetch_sqe(9, 1, 256, 5, 42);
        let raw: [u8; 128] = unsafe { std::mem::transmute(sqe) };
        assert_eq!(raw[0], 46, "IORING_OP_URING_CMD");
        assert_ne!(raw[1] & (1 << 5), 0, "IOSQE_BUFFER_SELECT");
        assert_eq!(u32::from_ne_bytes(raw[28..32].try_into().unwrap()), URING_CMD_MULTISHOT);
        assert_eq!(u16::from_ne_bytes(raw[40..42].try_into().unwrap()), 5);
        assert_eq!(u64::from_ne_bytes(raw[32..40].try_into().unwrap()), 42);
        // The command area at byte 48: q_id 1, flags 0, 128 elements of 2 bytes.
        assert_eq!(&raw[48..56], &[1, 0, 0, 0, 128, 0, 2, 0]);
        let e = elems_sqe(9, UBLK_U_IO_COMMIT_IO_CMDS, 1, &[0u8; 32], 7);
        let raw: [u8; 128] = unsafe { std::mem::transmute(e) };
        assert_eq!(u32::from_ne_bytes(raw[24..28].try_into().unwrap()), 32, "len");
        assert_eq!(&raw[48..56], &[1, 0, 2, 0, 2, 0, 16, 0]);
    }
}
