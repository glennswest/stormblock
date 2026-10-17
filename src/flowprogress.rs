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
    source: Mutex::new(None),
    source_device: Mutex::new(None),
    persists: AtomicU64::new(0),
    pace: Mutex::new(None),
    validation: Mutex::new(None),
    hot: AtomicU64::new(0),
};

pub struct FlowProgress {
    began: Mutex<Option<Instant>>,
    moved: AtomicU64,
    zeroed: AtomicU64,
    bytes: AtomicU64,
    /// The source's fabric path, when it is reached over NVMe/TCP (#407).
    source: Mutex<Option<crate::drive::nvmeof_dev::FabricPath>>,
    /// The source's device, for its paths (#450); not kept alive by this.
    source_device: Mutex<Option<std::sync::Weak<dyn crate::drive::BlockDevice>>>,
    /// Persists made (#410: one for a batch of moves).
    persists: AtomicU64,
    /// How it is paced, and why (#410).
    pace: Mutex<Option<Pace>>,
    /// Its validation (#412).
    validation: Mutex<Option<ValidationReport>>,
    /// Extents moved ahead of the rest because the node read them (#411).
    hot: AtomicU64,
}

/// A flow-over's validation by sha256 (#412): each moved extent read back
/// from the local disk and compared with the appliance's.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ValidationReport {
    /// `running`, `passed` or `corrupt` (FlowOverCorrupt: a drive that does
    /// not read back what it was given; those extents are read from the
    /// appliance, and the node does not go local).
    pub state: &'static str,
    /// Extents to check, and checked equal (repaired ones included).
    pub total: u64,
    pub validated: u64,
    /// Written since the move by a writable volume: not comparable.
    pub skipped: u64,
    pub mismatched: u64,
    pub repaired: u64,
    pub unrepaired: u64,
    pub bytes: u64,
    pub seconds: f64,
    pub mb_per_s: f64,
}

/// How a flow-over is paced, and why (#410): flat out unless the node's own
/// I/O is measurably slower for it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Pace {
    /// Extents a window moves at once now, and at most.
    pub window: usize,
    pub max_window: usize,
    /// `full` or `backing_off`.
    pub state: &'static str,
    /// What the pacing last went by.
    pub reason: String,
    /// The node's reads/writes and flushes, p90, while the flow-over moves
    /// (last 5 s) and without it (last measured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_p90_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush_p90_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_io_p90_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_flush_p90_ms: Option<f64>,
    /// Times it backed off, and the seconds it spent paused.
    pub pauses: u64,
    pub paused_seconds: f64,
}

/// What [`FlowProgress::snapshot`] reports.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
    /// The source's path (#407): its MTU, whether that is jumbo, the bytes
    /// one command carries. Absent for a local source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<crate::drive::nvmeof_dev::FabricPath>,
    /// The source's paths over NVMe/TCP (#450): one per target address and
    /// local NIC, with the bytes each has carried. Empty for a local source.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<crate::drive::nvmeof_dev::PathStats>,
    /// Persists made so far (#410).
    pub persists: u64,
    /// How it is paced, and why (#410).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace: Option<Pace>,
    /// Its validation (#412).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationReport>,
    /// Extents moved first because the node read them from the appliance (#411).
    pub hot_moved: u64,
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

    /// The source being read from now.
    pub fn source(&self, path: Option<crate::drive::nvmeof_dev::FabricPath>) {
        *self.source.lock().unwrap_or_else(|e| e.into_inner()) = path;
    }

    /// The device being read from now, for its paths (#450).
    pub fn source_device(&self, dev: &std::sync::Arc<dyn crate::drive::BlockDevice>) {
        *self.source_device.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::downgrade(dev));
    }

    pub fn hot(&self, n: u64) {
        self.hot.fetch_add(n, Relaxed);
    }

    pub fn persisted(&self) {
        self.persists.fetch_add(1, Relaxed);
    }

    pub fn validation(&self, v: ValidationReport) {
        *self.validation.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
    }

    pub fn pace(&self, p: Pace) {
        *self.pace.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
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
            source_path: *self.source.lock().unwrap_or_else(|e| e.into_inner()),
            paths: self
                .source_device
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|d| d.fabric_paths())
                .unwrap_or_default(),
            persists: self.persists.load(Relaxed),
            pace: self.pace.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            validation: self.validation.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            hot_moved: self.hot.load(Relaxed),
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
            source: Mutex::new(None),
            source_device: Mutex::new(None),
            persists: AtomicU64::new(0),
            pace: Mutex::new(None),
            validation: Mutex::new(None),
            hot: AtomicU64::new(0),
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
