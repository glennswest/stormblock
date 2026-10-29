//! How the engine's io_uring rings are set up (#505).
//!
//! Every ring the engine submits to is submitted from one thread only: a
//! ublk queue's worker, and an O_DIRECT drive's ring thread. So each asks the
//! kernel for `IORING_SETUP_SINGLE_ISSUER` (no locking on submission) and
//! `IORING_SETUP_DEFER_TASKRUN` (completion work run when that thread waits,
//! not by interrupting it), Linux 6.1+. A kernel that refuses them gets the
//! ring as before. Each ring must be built on the thread that submits to it:
//! the kernel takes the creating task as the only issuer.
//!
//! `STORMBLOCK_URING_PLAIN=1` builds every ring as before, for measuring.

use std::sync::atomic::{AtomicU64, Ordering};

use io_uring::{squeue, cqueue, IoUring};

/// Rings built with SINGLE_ISSUER + DEFER_TASKRUN, and without (refused, or
/// asked not to), since the process started.
static TUNED: AtomicU64 = AtomicU64::new(0);
static PLAIN: AtomicU64 = AtomicU64::new(0);

/// Whether the tuned setup is asked for.
fn wanted() -> bool {
    !std::env::var("STORMBLOCK_URING_PLAIN").is_ok_and(|v| v == "1")
}

/// A ring of `entries`, tuned when the kernel takes it. `true` with it when
/// it is tuned. Call it on the thread that will submit to the ring.
pub fn build<S, C>(entries: u32) -> std::io::Result<(IoUring<S, C>, bool)>
where
    S: squeue::EntryMarker,
    C: cqueue::EntryMarker,
{
    if wanted() {
        match IoUring::<S, C>::builder().setup_single_issuer().setup_defer_taskrun().build(entries) {
            Ok(r) => {
                TUNED.fetch_add(1, Ordering::Relaxed);
                return Ok((r, true));
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                tracing::info!("io_uring: SINGLE_ISSUER + DEFER_TASKRUN refused by this kernel; a plain ring (#505)");
            }
            Err(e) => return Err(e),
        }
    }
    let r = IoUring::<S, C>::builder().build(entries)?;
    PLAIN.fetch_add(1, Ordering::Relaxed);
    Ok((r, false))
}

/// `(tuned, plain)` rings built so far, for health.
pub fn counts() -> (u64, u64) {
    (TUNED.load(Ordering::Relaxed), PLAIN.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_is_built_tuned_or_plain_and_counted() {
        let before = counts();
        let (mut ring, tuned) = build::<squeue::Entry, cqueue::Entry>(8).expect("a ring");
        let after = counts();
        assert_eq!(after.0 + after.1, before.0 + before.1 + 1);
        assert_eq!(after.0 > before.0, tuned);
        // It works: a no-op round trip on the thread that built it.
        let nop = io_uring::opcode::Nop::new().build().user_data(7);
        unsafe { ring.submission().push(&nop).unwrap() };
        ring.submit_and_wait(1).unwrap();
        assert_eq!(ring.completion().next().map(|c| c.user_data()), Some(7));
    }
}
