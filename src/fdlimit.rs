//! The process's file descriptors against its limit (#510).
//!
//! pvetest1 (12.58) ran out after ~805 ublk exports in five hours: every
//! export, NVMe listener, state write and HTTP accept failed with EMFILE from
//! then on, and nothing had said it was coming. The count is in health, a
//! warning goes to the log past 80 %, and a serving engine starts with its
//! soft limit raised to the hard one.

use std::sync::atomic::{AtomicU64, Ordering};

/// Open fds and the soft limit: `{open, limit}`.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
pub struct Fds {
    pub open: u64,
    pub limit: u64,
}

/// The fds this process has open now (`/proc/self/fd`), less the one the
/// listing itself holds.
pub fn open_now() -> Option<u64> {
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
    Some(n.saturating_sub(1))
}

/// The soft `RLIMIT_NOFILE`.
pub fn limit() -> Option<u64> {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } == 0).then_some(r.rlim_cur as u64)
}

/// Both, for health; a warning in the log past 80 % (once a minute).
pub fn report() -> Option<Fds> {
    let f = Fds { open: open_now()?, limit: limit()? };
    if f.limit > 0 && f.open * 5 >= f.limit * 4 {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if now.saturating_sub(LAST.swap(now, Ordering::Relaxed)) >= 60 {
            tracing::warn!("{} of {} file descriptors open: past 80 % (#510)", f.open, f.limit);
        }
    }
    Some(f)
}

/// Raise the soft fd limit to the hard one (a serving engine, at start). A
/// node's engine holds a ring, a device and a runtime per volume it serves;
/// a supervisor's 1024 is not the ceiling it should meet.
pub fn raise_soft_limit() {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 && r.rlim_cur < r.rlim_max {
            let was = r.rlim_cur;
            r.rlim_cur = r.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &r) == 0 {
                tracing::info!("file descriptor limit raised from {was} to {} (#510)", r.rlim_max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_and_the_limit_are_read() {
        let f = report().expect("a Linux /proc and rlimit");
        assert!(f.open >= 3 && f.limit >= f.open, "{f:?}");
    }

    /// #510: a runtime registered for `/debug/tasks` and then dropped gives
    /// its fds back. The list kept every export's handle, and with it the
    /// runtime's epoll fd and eventfd: two or more fds per export, forever.
    #[test]
    fn a_registered_runtime_dropped_gives_its_fds_back() {
        let make = || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let listed = crate::mgmt::debug::register_runtime("fd-test (#510)", rt.handle().clone());
            rt.block_on(async { tokio::task::yield_now().await });
            // FD_LEAK_510=1: kept, as before the fix (the negative control).
            if std::env::var_os("FD_LEAK_510").is_some() {
                listed.forever();
            } else {
                drop(listed);
            }
            drop(rt);
        };
        make();
        let before = open_now().unwrap();
        for _ in 0..50 {
            make();
        }
        let after = open_now().unwrap();
        // Other tests in this process open and close fds meanwhile; a leak is
        // at least 100 here.
        assert!(after < before + 20, "50 runtimes registered and dropped left {} fd(s) open", after.saturating_sub(before));
    }
}
