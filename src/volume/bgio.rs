//! Background reads give way to the node's API (#475).
//!
//! The golden check and the digester (#384) read every byte of the node's
//! goldens. On server1's drive-managed SMR disk, the check after a reboot ran
//! beside the kubelet's container-root clones, and each clone (a few table
//! pages, one copy-on-write, a flush) waited 10–20 s on the disk instead of
//! 2–4. A clone is worth more than the check: nothing waits on the check
//! but a verdict.
//!
//! So a background reader asks [`wait_clear`] before each read: it goes on
//! only when no API call that changes something is being served, and none
//! has ended for a short while ([`clear_window`]). The API's middleware marks
//! those calls with [`begin`]. And on a rotating disk it uses at most
//! [`duty_percent`] of the disk's time ([`duty_pause`] after each read). A
//! reader is never starved for good: after [`MAX_WAIT`] of waiting it reads
//! once anyway.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// API calls that change something, being served now.
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// When the last one ended, in ms since [`base`] (0 = none yet).
static LAST_END_MS: AtomicU64 = AtomicU64::new(0);

/// The longest a background read waits for the API at a time.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    // +1: a call that ends in the first millisecond is still "one ended".
    base().elapsed().as_millis() as u64 + 1
}

/// A call the API is serving that changes something. Dropped when it is
/// answered (or abandoned).
pub struct ForegroundOp(());

impl Drop for ForegroundOp {
    fn drop(&mut self) {
        LAST_END_MS.store(now_ms(), Relaxed);
        IN_FLIGHT.fetch_sub(1, Relaxed);
    }
}

/// Mark a call that changes something as being served.
pub fn begin() -> ForegroundOp {
    let _ = base();
    IN_FLIGHT.fetch_add(1, Relaxed);
    ForegroundOp(())
}

/// Calls being served now.
pub fn in_flight() -> u64 {
    IN_FLIGHT.load(Relaxed)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

/// How long the API must have been still before a background read:
/// `STORMBLOCK_BG_CLEAR_MS`, 1000 on a rotating disk, 100 otherwise.
pub fn clear_window(rotational: bool) -> Duration {
    Duration::from_millis(env_u64("STORMBLOCK_BG_CLEAR_MS").unwrap_or(if rotational { 1000 } else { 100 }))
}

/// The share of a disk's time a background reader takes:
/// `STORMBLOCK_BG_DUTY_PERCENT` (1–100), 25 on a rotating disk, 100 otherwise.
pub fn duty_percent(rotational: bool) -> u64 {
    env_u64("STORMBLOCK_BG_DUTY_PERCENT").map(|p| p.clamp(1, 100)).unwrap_or(if rotational { 25 } else { 100 })
}

/// How long to leave the disk alone after a read that took `took`, so the
/// reader keeps to its [`duty_percent`].
pub fn duty_pause(took: Duration, rotational: bool) -> Duration {
    let p = duty_percent(rotational);
    if p >= 100 {
        return Duration::ZERO;
    }
    took.mul_f64((100 - p) as f64 / p as f64).min(Duration::from_secs(5))
}

/// Whether a background read may go now.
fn clear(window: Duration) -> bool {
    if IN_FLIGHT.load(Relaxed) > 0 {
        return false;
    }
    let last = LAST_END_MS.load(Relaxed);
    last == 0 || now_ms().saturating_sub(last) >= window.as_millis() as u64
}

/// Wait until no API call that changes something is being served and none
/// ended within [`clear_window`], or [`MAX_WAIT`] has passed. Answers how
/// long it waited.
pub async fn wait_clear(rotational: bool) -> Duration {
    let window = clear_window(rotational);
    let start = Instant::now();
    while !clear(window) && start.elapsed() < MAX_WAIT {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    start.elapsed()
}

/// Wait for the node's own volume I/O to be still for `quiet`, at most
/// `max` (#278's boot wait, for a background reader started at boot).
/// Answers how long it waited and whether it went quiet.
pub async fn wait_for_quiet(max: Duration, quiet: Duration) -> (Duration, bool) {
    let start = Instant::now();
    if max.is_zero() {
        return (Duration::ZERO, false);
    }
    let step = quiet.min(Duration::from_millis(500)).max(Duration::from_millis(1));
    let mut seen = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
    let mut still_since = start;
    loop {
        if still_since.elapsed() >= quiet {
            return (start.elapsed(), true);
        }
        if start.elapsed() >= max {
            return (start.elapsed(), false);
        }
        tokio::time::sleep(step).await;
        let cur = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        if cur != seen {
            seen = cur;
            still_since = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #475: on a rotating disk a background reader takes a quarter of the
    /// disk's time; on flash it does not pause; never more than 5 s.
    #[test]
    fn the_duty_pause_keeps_a_reader_to_its_share_of_the_disk() {
        assert_eq!(duty_pause(Duration::from_millis(100), true), Duration::from_millis(300));
        assert_eq!(duty_pause(Duration::from_millis(100), false), Duration::ZERO);
        assert_eq!(duty_pause(Duration::from_secs(10), true), Duration::from_secs(5));
    }

    /// #475: a background read waits while an API call that changes
    /// something is served, and for the clear window after it ends.
    #[tokio::test]
    async fn a_background_read_waits_for_the_api_and_a_little_after() {
        let op = begin();
        assert!(!clear(Duration::ZERO), "a call in flight holds background reads");
        let waiter = tokio::spawn(async { wait_clear(false).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiter.is_finished(), "still waiting while the call is served");
        drop(op);
        let waited = tokio::time::timeout(Duration::from_secs(5), waiter).await.unwrap().unwrap();
        assert!(waited >= Duration::from_millis(200 + 100), "waited {waited:?}: the call, then the clear window");
        assert_eq!(in_flight(), 0);
    }
}
