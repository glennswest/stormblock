//! Progress and rate settings for an array's rebuild and scrub.
//!
//! The work itself is in `RaidArray` (`run_rebuild`, `start_scrub`): both walk
//! the member data a lock unit at a time under the stripe locks, so live I/O
//! keeps flowing around them.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

/// A rebuild as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RebuildStatus {
    /// The slots being rebuilt.
    pub slots: Vec<usize>,
    /// Member data bytes rebuilt, of `total_bytes`.
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub running: bool,
    /// Bytes a second since it started.
    pub rate_bytes_per_sec: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Progress of a running (or the last) rebuild.
#[derive(Debug)]
pub struct RebuildProgress {
    pub slots: Vec<usize>,
    pub total_bytes: u64,
    start: u64,
    done: AtomicU64,
    cancelled: AtomicBool,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
    started: Instant,
}

impl RebuildProgress {
    pub fn new(slots: Vec<usize>, total_bytes: u64, start: u64) -> Arc<Self> {
        Arc::new(RebuildProgress {
            slots,
            total_bytes,
            start,
            done: AtomicU64::new(start),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            started: Instant::now(),
        })
    }

    pub fn set_done(&self, bytes: u64) {
        self.done.store(bytes, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        self.done() as f64 * 100.0 / self.total_bytes as f64
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn finish(&self, error: Option<String>) {
        *self.error.lock().unwrap() = error;
        self.finished.store(true, Ordering::SeqCst);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn status(&self) -> RebuildStatus {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        RebuildStatus {
            slots: self.slots.clone(),
            done_bytes: self.done(),
            total_bytes: self.total_bytes,
            percent: (self.percent() * 10.0).round() / 10.0,
            running: !self.is_finished(),
            rate_bytes_per_sec: ((self.done() - self.start.min(self.done())) as f64 / secs) as u64,
            error: self.error(),
        }
    }
}

/// A scrub as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ScrubStatus {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub running: bool,
    pub repair: bool,
    /// Stripes (or mirror units) whose members disagreed.
    pub mismatches: u64,
    pub repaired: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Progress tracking for a running (or the last) scrub.
#[derive(Debug)]
pub struct ScrubProgress {
    pub total_bytes: u64,
    pub repair: bool,
    done: AtomicU64,
    /// Units whose members disagreed.
    pub errors_found: AtomicU64,
    /// Of those, rewritten (only when repairing).
    pub errors_repaired: AtomicU64,
    cancelled: AtomicBool,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
}

impl ScrubProgress {
    pub fn new(total_bytes: u64, repair: bool) -> Arc<Self> {
        Arc::new(ScrubProgress {
            total_bytes,
            repair,
            done: AtomicU64::new(0),
            errors_found: AtomicU64::new(0),
            errors_repaired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
        })
    }

    pub fn advance_bytes(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        self.done() as f64 * 100.0 / self.total_bytes as f64
    }

    pub fn found(&self) -> u64 {
        self.errors_found.load(Ordering::Relaxed)
    }

    pub fn repaired(&self) -> u64 {
        self.errors_repaired.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn finish(&self, error: Option<String>) {
        *self.error.lock().unwrap() = error;
        self.finished.store(true, Ordering::SeqCst);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> ScrubStatus {
        ScrubStatus {
            done_bytes: self.done(),
            total_bytes: self.total_bytes,
            percent: (self.percent() * 10.0).round() / 10.0,
            running: !self.is_finished(),
            repair: self.repair,
            mismatches: self.found(),
            repaired: self.repaired(),
            error: self.error.lock().unwrap().clone(),
        }
    }
}

/// How a scrub runs.
#[derive(Debug, Clone)]
pub struct ScrubConfig {
    /// Member data bytes a second (0 = unlimited).
    pub max_bytes_per_sec: u64,
    /// Rewrite parity (or the other mirror legs) on a mismatch; else report.
    pub repair: bool,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        ScrubConfig { max_bytes_per_sec: 0, repair: true }
    }
}

/// How fast a rebuild may go.
#[derive(Debug, Clone, Default)]
pub struct RebuildConfig {
    /// Member data bytes a second (0 = unlimited). Live I/O already
    /// interleaves with a rebuild (it takes the stripe locks a few MiB at a
    /// time); this is for a shelf whose drives cannot take both at full rate.
    pub max_bytes_per_sec: u64,
    /// Member data locked and rebuilt at once (0 = 4 MiB).
    pub batch_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_progress() {
        let p = RebuildProgress::new(vec![3], 1000, 200);
        assert_eq!(p.done(), 200);
        p.set_done(500);
        assert!((p.percent() - 50.0).abs() < 0.01);
        assert!(p.status().running);
        p.finish(None);
        assert!(!p.status().running);
        assert_eq!(RebuildProgress::new(vec![], 0, 0).percent(), 100.0);
    }

    #[test]
    fn scrub_progress() {
        let p = ScrubProgress::new(100, false);
        p.advance_bytes(25);
        p.errors_found.fetch_add(2, Ordering::Relaxed);
        let s = p.status();
        assert_eq!(s.mismatches, 2);
        assert!((s.percent - 25.0).abs() < 0.01);
        p.cancel();
        assert!(p.is_cancelled());
    }
}
