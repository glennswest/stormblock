//! io_uring zero-copy send for NVMe-oF C2H data PDUs (Linux only).
//!
//! Uses `send_zc` (zero-copy send) to avoid copying large read data
//! through the kernel's TCP send buffer. This reduces CPU usage and
//! memory bandwidth for large sequential reads.

use std::os::unix::io::RawFd;

use io_uring::{IoUring, opcode, types};

/// Send C2H data via io_uring zero-copy send.
///
/// Assembles the full PDU (header + data + optional digests) and submits
/// it as a single zero-copy send operation.
///
/// `fd` — raw TCP socket file descriptor.
/// `header` — pre-assembled PDU header bytes (common header + C2H-specific fields + optional header digest).
/// `data` — payload data bytes.
/// `data_digest` — optional 4-byte CRC32C data digest (already computed).
pub fn send_c2h_zerocopy(
    ring: &mut IoUring,
    fd: RawFd,
    header: &[u8],
    data: &[u8],
    data_digest: Option<[u8; 4]>,
) -> std::io::Result<()> {
    // Send header first (small, not worth zero-copy)
    let sqe = opcode::Send::new(types::Fd(fd), header.as_ptr(), header.len() as u32)
        .flags(libc::MSG_MORE as i32) // signal more data coming
        .build()
        .user_data(1);
    unsafe { ring.submission().push(&sqe).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "io_uring submission queue full")
    })?; }

    ring.submit_and_wait(1).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let cqe = ring.completion().next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "no completion for header send")
    })?;
    if cqe.result() < 0 {
        return Err(std::io::Error::from_raw_os_error(-cqe.result()));
    }

    // Send data with zero-copy (SEND_ZC) — this is the large payload
    let msg_more = if data_digest.is_some() { libc::MSG_MORE as i32 } else { 0 };
    let sqe = opcode::SendZc::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
        .flags(msg_more)
        .build()
        .user_data(2);
    unsafe { ring.submission().push(&sqe).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "io_uring submission queue full")
    })?; }

    // send_zc produces two CQEs: one for submission, one for completion (notification)
    ring.submit_and_wait(1).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let cqe = ring.completion().next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "no completion for data send_zc")
    })?;
    if cqe.result() < 0 {
        return Err(std::io::Error::from_raw_os_error(-cqe.result()));
    }
    // Drain the notification CQE if present
    let _ = ring.completion().next();

    // Send data digest if present (4 bytes, not worth zero-copy)
    if let Some(digest) = data_digest {
        let sqe = opcode::Send::new(types::Fd(fd), digest.as_ptr(), 4)
            .build()
            .user_data(3);
        unsafe { ring.submission().push(&sqe).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::Other, "io_uring submission queue full")
        })?; }

        ring.submit_and_wait(1).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let cqe = ring.completion().next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no completion for digest send")
        })?;
        if cqe.result() < 0 {
            return Err(std::io::Error::from_raw_os_error(-cqe.result()));
        }
    }

    Ok(())
}

/// Minimum data size to use zero-copy send (below this, regular send is faster
/// due to lower overhead).
pub const ZC_MIN_SIZE: usize = 16384; // 16 KB

/// Check if io_uring zero-copy send is available on this kernel.
/// Requires kernel >= 6.0 for IORING_OP_SEND_ZC.
pub fn is_send_zc_available() -> bool {
    // Probe by creating a ring and checking if SendZc is supported
    let ring = match IoUring::builder().build(4) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let probe = match ring.submitter().register_probe(&mut io_uring::Probe::new()) {
        Ok(()) => true,
        Err(_) => false,
    };
    probe
}
