//! The process's file descriptors against its limit (#510).
//!
//! pvetest1 (12.58) ran out after ~805 ublk exports in five hours: every
//! export, NVMe listener, state write and HTTP accept failed with EMFILE from
//! then on, and nothing had said it was coming. The count is in health, a
//! warning goes to the log past 80 %, and a serving engine starts with its
//! soft limit raised to the hard one.
//!
//! server8 (12.58, #522) ran out in the first second of an install: the
//! initramfs engine starts every boot export at once (85 of them, 4 queues
//! each, ~14 fds an export) against PID 1's inherited 1024, and the kernel's
//! hard limit for it is only 4096. A serving engine running as root now
//! raises the hard limit as well ([`TARGET`], within `fs.nr_open`), and a
//! set of exports that would still not fit is refused before the first one
//! is created, naming the numbers ([`room_for`]), instead of 26 volumes
//! quietly never appearing.

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

/// What a serving engine asks for: hard and soft (#522). 65536 is ~4,500
/// exports at four queues, past anything a node serves.
pub const TARGET: u64 = 65536;

/// Kept free beyond what a set of exports needs (#522): slabs, sockets,
/// state and log writes, the API's accepts.
pub const RESERVE: u64 = 256;

/// Raise the fd limit for a serving engine, at start: soft and hard to
/// [`TARGET`] (no higher than `fs.nr_open`) when the engine may (root, or
/// CAP_SYS_RESOURCE, as on every node), else the soft one to the hard one.
/// A node's engine holds a ring, a device and a runtime per volume it
/// serves: a supervisor's 1024 is not the ceiling it should meet, nor the
/// kernel's 4096 for an initramfs's PID 1 (#510, #522).
pub fn raise_soft_limit() {
    let nr_open = std::fs::read_to_string("/proc/sys/fs/nr_open").ok().and_then(|s| s.trim().parse::<u64>().ok());
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) != 0 {
            return;
        }
        let (was_soft, was_hard) = (r.rlim_cur as u64, r.rlim_max as u64);
        let want = TARGET.min(nr_open.unwrap_or(TARGET)).max(was_hard);
        if want > was_hard {
            let up = libc::rlimit { rlim_cur: want as _, rlim_max: want as _ };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &up) == 0 {
                tracing::info!("file descriptor limit raised from {was_soft}/{was_hard} to {want} (#510, #522)");
                return;
            }
        }
        if was_soft < was_hard {
            r.rlim_cur = r.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &r) == 0 {
                tracing::info!("file descriptor limit raised from {was_soft} to {was_hard} (#510)");
            }
        }
    }
}

/// Whether `needed` more fds fit under the limit with [`RESERVE`] to spare:
/// the limit and what is open, or why not, numbers named (#522).
pub fn room_for(needed: u64) -> Result<Fds, String> {
    let (Some(open), Some(limit)) = (open_now(), limit()) else {
        return Ok(Fds { open: 0, limit: 0 });
    };
    check_room(open, limit, needed)
}

fn check_room(open: u64, limit: u64, needed: u64) -> Result<Fds, String> {
    let f = Fds { open, limit };
    if open + needed + RESERVE <= limit {
        Ok(f)
    } else {
        Err(format!(
            "{needed} file descriptors needed, {open} open, limit {limit} (with {RESERVE} kept free): \
             raise RLIMIT_NOFILE for the engine (#522)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_of_exports_that_does_not_fit_is_named() {
        // server8's boot (#522): 85 exports at 14 fds against 1024.
        let e = check_room(40, 1024, 85 * 14).unwrap_err();
        assert!(e.contains("1190 file descriptors needed") && e.contains("limit 1024"), "{e}");
        assert!(check_room(40, TARGET, 85 * 14).is_ok());
        assert!(check_room(40, 4096, 85 * 14).is_ok(), "the kernel's hard limit alone fits 85");
        assert!(check_room(40, 4096, 300 * 14).is_err());
    }

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
