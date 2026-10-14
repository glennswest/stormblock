//! The node's scrub (#160): every redundant volume read in full once a
//! period (a week), in the background, throttled to a share of the drives'
//! time (10 %), paused while an install's flow-over runs, one volume at a
//! time. What it found is kept per volume (`<data_dir>/scrub.json`) and
//! answered by `GET /api/v1/volumes/{id}/scrub` and `GET /api/v1/scrub`.
//!
//! What repairs a mismatch is the volume's ([`crate::volume::thin::scrub`]):
//! a majority at `mirror:3` and up, a report at `mirror:2`, parity reported
//! and rewritten only when asked.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::volume::thin::scrub::{ScrubOptions, ScrubReport};
use crate::volume::VolumeId;

/// `[scrub]` in `stormblock.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrubConfig {
    /// Scrub on the schedule (a scrub asked for over the API runs either way).
    pub enabled: bool,
    /// Days between two full passes of a volume (default 7).
    pub period_days: u64,
    /// The share of the drives' time a scrub takes, in percent (default 10).
    pub duty_percent: u8,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        ScrubConfig { enabled: true, period_days: 7, duty_percent: 10 }
    }
}

/// One volume's last scrub.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeScrub {
    /// Seconds since the epoch.
    pub started: u64,
    pub finished: u64,
    pub repair_parity: bool,
    pub report: ScrubReport,
}

/// The one scrub running.
#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub volume: VolumeId,
    pub name: String,
    pub started: u64,
    pub done: u64,
    pub total: u64,
    pub paused: bool,
    pub repair_parity: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    volumes: HashMap<VolumeId, VolumeScrub>,
}

pub struct Scrubber {
    path: Option<PathBuf>,
    saved: Mutex<Saved>,
    running: Mutex<Option<Running>>,
    asked: Mutex<VecDeque<(VolumeId, bool)>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    wake: tokio::sync::Notify,
    /// Totals since start, for /metrics.
    pub extents: AtomicU64,
    pub mismatches: AtomicU64,
    pub repaired: AtomicU64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Scrubber {
    pub fn new(data_dir: Option<&std::path::Path>) -> Arc<Self> {
        let path = data_dir.map(|d| d.join("scrub.json"));
        let saved = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Scrubber {
            path,
            saved: Mutex::new(saved),
            running: Mutex::new(None),
            asked: Mutex::new(VecDeque::new()),
            cancel: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
            extents: AtomicU64::new(0),
            mismatches: AtomicU64::new(0),
            repaired: AtomicU64::new(0),
        })
    }

    pub fn last(&self, id: &VolumeId) -> Option<VolumeScrub> {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).volumes.get(id).cloned()
    }

    pub fn all(&self) -> HashMap<VolumeId, VolumeScrub> {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).volumes.clone()
    }

    pub fn running(&self) -> Option<Running> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn queued(&self) -> Vec<VolumeId> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(v, _)| *v).collect()
    }

    /// Scrub `id` next (asked for over the API).
    pub fn ask(&self, id: VolumeId, repair_parity: bool) {
        let mut q = self.asked.lock().unwrap_or_else(|e| e.into_inner());
        if !q.iter().any(|(v, _)| *v == id) {
            q.push_back((id, repair_parity));
        }
        drop(q);
        self.wake.notify_one();
    }

    /// Stop the scrub running (its findings so far are kept).
    pub fn cancel(&self) -> bool {
        match self.cancel.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(c) => {
                c.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Forget a deleted volume.
    pub fn forget(&self, id: &VolumeId) {
        let gone = self.saved.lock().unwrap_or_else(|e| e.into_inner()).volumes.remove(id).is_some();
        if gone {
            self.save();
        }
    }

    fn record(&self, id: VolumeId, s: VolumeScrub) {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).volumes.insert(id, s);
        self.save();
    }

    fn save(&self) {
        let Some(p) = &self.path else { return };
        let bytes = {
            let g = self.saved.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::to_vec_pretty(&*g)
        };
        match bytes {
            Ok(b) => {
                if let Err(e) = crate::serve::wiring::write_atomic(p, &b) {
                    tracing::warn!("{}: {e}", p.display());
                }
            }
            Err(e) => tracing::warn!("scrub.json: {e}"),
        }
    }

    /// The next volume the schedule owes a pass: redundant, not held by the
    /// rebuild queue, its last pass the oldest and older than the period.
    async fn due(&self, state: &crate::mgmt::AppState, period: Duration) -> Option<VolumeId> {
        let candidates: Vec<VolumeId> = {
            let vm = state.volume_manager.lock().await;
            vm.volume_ids()
                .into_iter()
                .filter(|id| vm.get_volume_handle(id).is_some_and(|h| !h.redundancy().is_none()))
                .collect()
        };
        let saved = self.all();
        let cutoff = now().saturating_sub(period.as_secs());
        candidates
            .into_iter()
            .filter(|id| !state.rebuilds.holds(id))
            .map(|id| (saved.get(&id).map(|s| s.finished).unwrap_or(0), id))
            .filter(|(last, _)| *last <= cutoff)
            .min_by_key(|(last, id)| (*last, id.0))
            .map(|(_, id)| id)
    }

    /// Scrub one volume now, recording what it found.
    pub async fn run_one(self: &Arc<Self>, state: &Arc<crate::mgmt::AppState>, id: VolumeId, repair_parity: bool) -> Option<ScrubReport> {
        let handle = state.volume_manager.lock().await.get_volume_handle(&id)?;
        let name = handle.name().await;
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        let started = now();
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Running { volume: id, name: name.clone(), started, done: 0, total: 0, paused: false, repair_parity });
        let opts = ScrubOptions { repair_parity, duty_percent: state.config.scrub.duty_percent, cancel };
        let flow = state.flow_over_remaining.clone();
        let me = self.clone();
        let paused = {
            let me = me.clone();
            move || {
                let p = flow.load(Ordering::Relaxed) > 0;
                if let Some(r) = me.running.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    r.paused = p;
                }
                p
            }
        };
        let progress = {
            let me = me.clone();
            move |done: u64, total: u64| {
                if let Some(r) = me.running.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    r.done = done;
                    r.total = total;
                }
            }
        };
        tracing::info!(volume = %id.0, name, "scrub: started");
        let report = handle.scrub(&opts, paused, progress).await;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.extents.fetch_add(report.extents + report.stripes, Ordering::Relaxed);
        self.mismatches.fetch_add(report.mirror_mismatches + report.parity_mismatches, Ordering::Relaxed);
        self.repaired.fetch_add(report.mirror_repaired + report.parity_rewritten, Ordering::Relaxed);
        metrics::counter!("stormblock_scrub_units_total").increment(report.extents + report.stripes);
        metrics::counter!("stormblock_scrub_bytes_total").increment(report.bytes);
        metrics::counter!("stormblock_scrub_mismatches_total", "kind" => "mirror").increment(report.mirror_mismatches);
        metrics::counter!("stormblock_scrub_mismatches_total", "kind" => "parity").increment(report.parity_mismatches);
        metrics::counter!("stormblock_scrub_repaired_total", "kind" => "mirror").increment(report.mirror_repaired);
        metrics::counter!("stormblock_scrub_repaired_total", "kind" => "parity").increment(report.parity_rewritten);
        metrics::counter!("stormblock_scrub_unreadable_total").increment(report.unreadable);
        if report.clean() {
            tracing::info!(volume = %id.0, name, extents = report.extents, stripes = report.stripes, seconds = report.seconds, "scrub: clean");
        } else {
            tracing::warn!(
                volume = %id.0, name,
                mirror_mismatches = report.mirror_mismatches, mirror_repaired = report.mirror_repaired,
                mirror_reported = report.mirror_reported, parity_mismatches = report.parity_mismatches,
                parity_rewritten = report.parity_rewritten, unreadable = report.unreadable,
                "scrub: found mismatches (GET /api/v1/volumes/{}/scrub)", id.0
            );
        }
        // A cancelled pass is kept as found, but the schedule owes a full one.
        let finished = if report.cancelled { 0 } else { now() };
        self.record(id, VolumeScrub { started, finished, repair_parity, report: report.clone() });
        Some(report)
    }

    /// The scrubber: what was asked for first, then the schedule; checked
    /// every ten minutes and whenever something is asked.
    pub fn spawn(self: &Arc<Self>, state: &Arc<crate::mgmt::AppState>) {
        let me = self.clone();
        let state = Arc::downgrade(state);
        tokio::spawn(async move {
            loop {
                let Some(st) = state.upgrade() else { return };
                let asked = me.asked.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let next = match asked {
                    Some(a) => Some(a),
                    None if st.config.scrub.enabled && st.flow_over_remaining.load(Ordering::Relaxed) <= 0 => {
                        let period = Duration::from_secs(st.config.scrub.period_days.max(1) * 86_400);
                        me.due(&st, period).await.map(|v| (v, false))
                    }
                    None => None,
                };
                match next {
                    Some((id, repair)) => {
                        me.run_one(&st, id, repair).await;
                        drop(st);
                    }
                    None => {
                        drop(st);
                        tokio::select! {
                            _ = me.wake.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(600)) => {}
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::drive::BlockDevice;
    use crate::placement::topology::StorageTier;
    use crate::volume::redundancy::RedundancyPolicy;
    use crate::volume::{CreateOptions, VolumeManager};

    const MIB: u64 = 1 << 20;

    async fn node(dir: &tempfile::TempDir) -> (Arc<crate::mgmt::AppState>, VolumeId, VolumeId) {
        let mut vm = VolumeManager::new(MIB);
        for i in 0..2 {
            let path = dir.path().join(format!("d{i}.bin")).to_string_lossy().to_string();
            let dev = FileDevice::open_with_capacity(&path, 64 * MIB).await.unwrap();
            vm.add_slab(Slab::format(Arc::new(dev), MIB, StorageTier::Hot).await.unwrap()).await;
        }
        let m = vm
            .create_volume_with("m", 8 * MIB, CreateOptions::redundant(RedundancyPolicy::parse("mirror:2").unwrap()))
            .await
            .unwrap();
        let one = vm.create_volume_any("one", 8 * MIB).await.unwrap();
        vm.get_volume(&m).unwrap().write(0, &[5u8; 4096]).await.unwrap();
        let mut config = crate::mgmt::config::StormBlockConfig::default();
        config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        (Arc::new(crate::mgmt::AppState::new(config, vm, reg, gem)), m, one)
    }

    /// The schedule owes a pass to a redundant volume never scrubbed or
    /// last scrubbed a period ago, never to a single copy; what it found
    /// survives a restart (scrub.json).
    #[tokio::test]
    async fn the_schedule_owes_a_redundant_volume_a_pass_a_period() {
        let dir = tempfile::tempdir().unwrap();
        let (state, m, _one) = node(&dir).await;
        let week = Duration::from_secs(7 * 86_400);
        let s = &state.scrubber;
        assert_eq!(s.due(&state, week).await, Some(m), "never scrubbed: due; the single copy never is");
        s.run_one(&state, m, false).await.unwrap();
        assert_eq!(s.due(&state, week).await, None, "scrubbed just now");
        let mut last = s.last(&m).unwrap();
        last.finished = now() - 8 * 86_400;
        s.record(m, last);
        assert_eq!(s.due(&state, week).await, Some(m), "a week and a day ago: due again");
        let again = Scrubber::new(Some(dir.path()));
        assert!(again.last(&m).is_some(), "kept in scrub.json");
    }

    /// A scrub waits while an install's flow-over runs, says so, and goes on
    /// once it is done.
    #[tokio::test]
    async fn a_scrub_waits_while_a_flow_over_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (state, m, _) = node(&dir).await;
        state.flow_over_remaining.store(12, Ordering::Relaxed);
        let run = {
            let (st, s) = (state.clone(), state.scrubber.clone());
            tokio::spawn(async move { s.run_one(&st, m, false).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r = state.scrubber.running().expect("running");
        assert!(r.paused, "paused while the flow-over runs");
        assert_eq!(r.done, 0);
        state.flow_over_remaining.store(0, Ordering::Relaxed);
        let report = tokio::time::timeout(Duration::from_secs(30), run).await.unwrap().unwrap().unwrap();
        assert_eq!(report.extents, 1);
        assert!(report.clean());
        assert!(state.scrubber.running().is_none());
    }
}
