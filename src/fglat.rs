//! How long the node's own volume I/O takes (#410): what a flow-over paces
//! itself by.
//!
//! Every read, write and flush a volume serves (ublk, NVMe/TCP, iSCSI, the
//! API) is timed into a histogram kept per second for the last minute,
//! split two ways: by class (reads and writes, and flushes, which are what a
//! journal's commit and an fsync wait for) and by whether a flow-over was
//! moving extents at that moment. The flow-over compares the two: the node's
//! latency while it moves against the node's latency while it does not (its
//! pauses, the boot grace before it). A disk that is slow anyway is slow in
//! both, and the flow-over runs flat out; a disk the flow-over is making
//! slower shows it, and the flow-over backs off.
//!
//! Lock-free: a record is two atomic adds (and, once a second, a bucket
//! cleared). Coarse on purpose: half-octave bins.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// What was timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A read or a write.
    Io = 0,
    /// A flush (a FUA write counts as a write).
    Flush = 1,
}

/// Seconds kept.
const SECONDS: usize = 64;
/// Half-octave bins of microseconds: bin 2k is [2^k, 1.5·2^k), 2k+1 is
/// [1.5·2^k, 2^(k+1)); 64 bins reach ~2^32 µs (over an hour).
const BINS: usize = 64;

struct Bucket {
    /// The second this bucket counts (since [`base`], plus one: 0 = never).
    second: AtomicU64,
    bins: [AtomicU64; BINS],
}

#[allow(clippy::declare_interior_mutable_const)]
const BUCKET: Bucket = Bucket { second: AtomicU64::new(0), bins: [const { AtomicU64::new(0) }; BINS] };
#[allow(clippy::declare_interior_mutable_const)]
const SET: [Bucket; SECONDS] = [BUCKET; SECONDS];

/// [class][moving][second % SECONDS].
static BUCKETS: [[[Bucket; SECONDS]; 2]; 2] = [[SET, SET], [SET, SET]];

/// Whether a flow-over's own I/O is under way.
static MOVING: AtomicBool = AtomicBool::new(false);

fn base() -> Instant {
    static B: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *B.get_or_init(Instant::now)
}

fn now_second() -> u64 {
    base().elapsed().as_secs() + 1
}

fn bin_of(us: u64) -> usize {
    let us = us.max(1);
    let lg = 63 - us.leading_zeros() as usize;
    let half = if lg >= 1 { ((us >> (lg - 1)) & 1) as usize } else { 0 };
    (2 * lg + half).min(BINS - 1)
}

/// The middle of a bin, in microseconds.
fn bin_value(bin: usize) -> f64 {
    let lg = (bin / 2) as i32;
    let lo = 2f64.powi(lg) * if bin % 2 == 1 { 1.5 } else { 1.0 };
    let hi = 2f64.powi(lg) * if bin % 2 == 1 { 2.0 } else { 1.5 };
    (lo + hi) / 2.0
}

/// A flow-over says when its own I/O is under way, so what the node sees
/// then is told from what it sees without it.
pub fn set_moving(on: bool) {
    MOVING.store(on, Relaxed);
}

pub fn moving() -> bool {
    MOVING.load(Relaxed)
}

/// Count one request of `class` that took `took`.
pub fn record(class: Class, took: Duration) {
    record_at(class, MOVING.load(Relaxed), now_second(), took);
}

fn record_at(class: Class, moving: bool, second: u64, took: Duration) {
    let b = &BUCKETS[class as usize][moving as usize][(second % SECONDS as u64) as usize];
    let was = b.second.load(Relaxed);
    if was != second && b.second.compare_exchange(was, second, Relaxed, Relaxed).is_ok() {
        // A new second for this slot: what it counted a minute ago goes. A
        // record racing the clear may be lost; this is a gauge, not a ledger.
        for x in &b.bins {
            x.store(0, Relaxed);
        }
    }
    b.bins[bin_of(took.as_micros() as u64)].fetch_add(1, Relaxed);
}

/// The `q` quantile of `class` over the last `window`, taken while a
/// flow-over was moving (`moving`) or not, and how many requests it is of.
/// `None` with nothing counted.
pub fn quantile(class: Class, moving: bool, window: Duration, q: f64) -> Option<(Duration, u64)> {
    quantile_at(class, moving, now_second(), window, q)
}

fn quantile_at(class: Class, moving: bool, now: u64, window: Duration, q: f64) -> Option<(Duration, u64)> {
    let secs = window.as_secs().clamp(1, SECONDS as u64 - 1);
    let mut sum = [0u64; BINS];
    for b in &BUCKETS[class as usize][moving as usize] {
        let s = b.second.load(Relaxed);
        if s != 0 && s <= now && now - s < secs {
            for (i, x) in b.bins.iter().enumerate() {
                sum[i] += x.load(Relaxed);
            }
        }
    }
    let total: u64 = sum.iter().sum();
    if total == 0 {
        return None;
    }
    let want = ((total as f64) * q.clamp(0.0, 1.0)).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in sum.iter().enumerate() {
        seen += n;
        if seen >= want {
            return Some((Duration::from_micros(bin_value(i) as u64), total));
        }
    }
    None
}

/// Times a request into [`record`] when dropped (an error or a cancelled
/// request counts too: the caller waited that long).
pub struct Timer {
    class: Class,
    started: Instant,
}

impl Timer {
    pub fn new(class: Class) -> Self {
        Timer { class, started: Instant::now() }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        record(self.class, self.started.elapsed());
    }
}

/// For tests: requests that took `took`, as if served now while a flow-over
/// was (or was not) moving.
#[doc(hidden)]
pub fn record_for_test(class: Class, moving: bool, took: Duration, n: u64) {
    for _ in 0..n {
        record_at(class, moving, now_second(), took);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_hold_their_values() {
        for us in [1u64, 2, 3, 5, 100, 999, 1000, 25_000, 2_000_000] {
            let v = bin_value(bin_of(us));
            assert!(v >= us as f64 * 0.66 && v <= us as f64 * 1.5, "{us} µs reads as {v}");
        }
    }

    #[test]
    fn a_quantile_is_of_the_window_and_the_phase_asked_for() {
        // Seconds far from anything a real record uses, so other tests in
        // this process do not land in them.
        let now = 1_000_000;
        for _ in 0..90 {
            record_at(Class::Flush, true, now, Duration::from_millis(1));
        }
        for _ in 0..10 {
            record_at(Class::Flush, true, now - 2, Duration::from_millis(400));
        }
        record_at(Class::Flush, false, now, Duration::from_millis(80));
        let (p50, n) = quantile_at(Class::Flush, true, now, Duration::from_secs(5), 0.5).unwrap();
        assert_eq!(n, 100);
        assert!(p50 < Duration::from_millis(2), "{p50:?}");
        let (p99, _) = quantile_at(Class::Flush, true, now, Duration::from_secs(5), 0.99).unwrap();
        assert!(p99 > Duration::from_millis(250), "{p99:?}");
        // The older second is out of a 2 s window.
        let (p99, n) = quantile_at(Class::Flush, true, now, Duration::from_secs(2), 0.99).unwrap();
        assert_eq!(n, 90);
        assert!(p99 < Duration::from_millis(2), "{p99:?}");
        // The other phase, the other class.
        let (still, n) = quantile_at(Class::Flush, false, now, Duration::from_secs(5), 0.9).unwrap();
        assert_eq!(n, 1);
        assert!(still > Duration::from_millis(50) && still < Duration::from_millis(120), "{still:?}");
        assert!(quantile_at(Class::Io, true, now, Duration::from_secs(5), 0.9).is_none());
        // A minute on, the slot is reused and starts over.
        record_at(Class::Flush, true, now + SECONDS as u64, Duration::from_millis(7));
        let (_, n) = quantile_at(Class::Flush, true, now + SECONDS as u64, Duration::from_secs(1), 0.5).unwrap();
        assert_eq!(n, 1);
    }
}
