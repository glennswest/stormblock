//! How a flow-over is going (#401): what it moved, what it found all zeros
//! and did not copy, the bytes it wrote and since when. One per process (a
//! node runs one flow-over, its system half then its data half), read by
//! `/api/v1/health` beside `flow_over_remaining`, which says what is left.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

/// The process's flow-over.
pub static FLOW: FlowProgress = FlowProgress {
    began: Mutex::new(None),
    moved: AtomicU64::new(0),
    zeroed: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
};

pub struct FlowProgress {
    began: Mutex<Option<Instant>>,
    moved: AtomicU64,
    zeroed: AtomicU64,
    bytes: AtomicU64,
}

/// What [`FlowProgress::snapshot`] reports.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Snapshot {
    /// Extents copied to the local disk.
    pub moved: u64,
    /// Extents that read as all zeros: unmapped, not copied.
    pub zeroed: u64,
    /// Bytes copied.
    pub bytes: u64,
    /// Since the flow-over began.
    pub seconds: f64,
    /// Bytes copied per second, in MB (10^6).
    pub mb_per_s: f64,
    /// Extents (moved or zeroed) per second.
    pub extents_per_s: f64,
}

impl FlowProgress {
    /// The flow-over starts (its first half); later calls change nothing.
    pub fn begin(&self) {
        let mut b = self.began.lock().unwrap_or_else(|e| e.into_inner());
        b.get_or_insert_with(Instant::now);
    }

    pub fn moved(&self, extents: u64, bytes: u64) {
        self.moved.fetch_add(extents, Relaxed);
        self.bytes.fetch_add(bytes * extents, Relaxed);
    }

    pub fn zeroed(&self, extents: u64) {
        self.zeroed.fetch_add(extents, Relaxed);
    }

    /// Nothing when no flow-over has begun in this process.
    pub fn snapshot(&self) -> Option<Snapshot> {
        let began = (*self.began.lock().unwrap_or_else(|e| e.into_inner()))?;
        let seconds = began.elapsed().as_secs_f64();
        let (moved, zeroed, bytes) = (self.moved.load(Relaxed), self.zeroed.load(Relaxed), self.bytes.load(Relaxed));
        let per = |n: f64| if seconds > 0.0 { n / seconds } else { 0.0 };
        Some(Snapshot {
            moved,
            zeroed,
            bytes,
            seconds,
            mb_per_s: per(bytes as f64 / 1e6),
            extents_per_s: per((moved + zeroed) as f64),
        })
    }

    /// Seconds left at the rate so far, for `remaining` extents.
    pub fn eta_seconds(&self, remaining: u64) -> Option<f64> {
        let s = self.snapshot()?;
        (s.extents_per_s > 0.0).then(|| remaining as f64 / s.extents_per_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_eta_from_what_was_counted() {
        let p = FlowProgress {
            began: Mutex::new(None),
            moved: AtomicU64::new(0),
            zeroed: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        };
        assert!(p.snapshot().is_none());
        p.begin();
        p.moved(3, 1 << 20);
        p.zeroed(1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let s = p.snapshot().unwrap();
        assert_eq!((s.moved, s.zeroed, s.bytes), (3, 1, 3 << 20));
        assert!(s.mb_per_s > 0.0 && s.extents_per_s > 0.0);
        let eta = p.eta_seconds(100).unwrap();
        assert!((eta - 100.0 / s.extents_per_s).abs() / eta < 0.5, "{eta}");
    }
}
