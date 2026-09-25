//! Automatic, per-volume rebuild (#146).
//!
//! Redundancy is per volume: a volume's members sit on different drives,
//! and there is no drive-level array to rebuild. So when a drive fails,
//! what has to be rebuilt is **the volumes with a member on it**, each onto
//! drives of its own choosing. Their extents are spread across the pool,
//! so those rebuilds read from and write to many drives at once. That is
//! the difference between hours and the days one spare disk would take.
//!
//! This is the queue that runs them:
//!
//! * **One queue for the node**, ordered by `margin`: how many more losses
//!   the volume's least protected extent can take. A volume one failure
//!   from losing data goes first, whichever drive failure put it there.
//! * **Several volumes at once** (`parallel`), and several extents at once
//!   within each (`extents_in_flight`). A big volume is not rebuilt one
//!   slot at a time.
//! * **One byte budget** (`max_bytes_per_sec`, a shared
//!   [`Throttle`](crate::volume::throttle::Throttle)) for every rebuild on
//!   the node, so ten rebuilds take no more of the drives than one would.
//! * **Durable as it goes.** Every few thousand rebuilt legs, the map is
//!   persisted and only then are the replaced slots freed. A crash loses
//!   at most that much progress, and never frees a slot the map on disk
//!   still names.
//! * A volume hit by a **second failure while it is rebuilding** is
//!   rebuilt again when the current pass ends: that pass may already be
//!   past the extents the new failure took.
//!
//! A job is what asked: a drive's health report, or an operator's `POST`.
//! It is done when every volume it named has been rebuilt as far as the
//! pool allows. A job ends `partial` when a volume is still not healthy,
//! for example when there are not enough domains left to hold its policy.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::volume::throttle::Throttle;
use crate::volume::{HealthState, ResyncCheckpoint, ResyncOptions, VolumeId, VolumeManager};

/// `[rebuild]` in `stormblock.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RebuildConfig {
    /// Rebuild automatically when a drive is reported failing or failed.
    pub automatic: bool,
    /// Volumes rebuilt at once (default 4).
    pub parallel: usize,
    /// Extents (or stripes) of one volume rebuilt at once (default 4).
    pub extents_in_flight: usize,
    /// Bytes per second across every rebuild on the node; 0 = unlimited.
    pub max_bytes_per_sec: u64,
}

impl Default for RebuildConfig {
    fn default() -> Self {
        RebuildConfig { automatic: true, parallel: 4, extents_in_flight: 4, max_bytes_per_sec: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildState {
    Queued,
    Running,
    /// Rebuilt, and healthy.
    Done,
    /// Rebuilt as far as the pool allows, and still not healthy: see the
    /// errors.
    Partial,
    Cancelled,
}

impl RebuildState {
    fn finished(self) -> bool {
        matches!(self, RebuildState::Done | RebuildState::Partial | RebuildState::Cancelled)
    }
}

/// One volume within a job.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeRebuild {
    pub volume: VolumeId,
    pub name: String,
    pub state: RebuildState,
    /// Losses the volume could still take when it was queued.
    pub margin: usize,
    pub legs_rebuilt: usize,
    pub legs_added: usize,
    pub bytes_copied: u64,
    pub unrecoverable: usize,
    /// Health after the rebuild.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthState>,
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RebuildJob {
    pub id: u64,
    /// Why: `drive <path> <state>`, or `requested`.
    pub reason: String,
    /// The drive whose report started it, if one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    pub state: RebuildState,
    pub volumes: Vec<VolumeRebuild>,
    pub bytes_copied: u64,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

/// Settings that can change while rebuilds run.
#[derive(Debug, Clone, Serialize)]
pub struct RebuildSettings {
    pub automatic: bool,
    pub parallel: usize,
    pub extents_in_flight: usize,
    pub max_bytes_per_sec: u64,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    seq: u64,
    jobs: BTreeMap<u64, RebuildJob>,
    done: HashMap<u64, tokio::sync::watch::Sender<bool>>,
    /// (margin, arrival, volume): the first is the most endangered.
    queue: BTreeSet<(usize, u64, VolumeId)>,
    queued: HashMap<VolumeId, (usize, u64)>,
    running: HashMap<VolumeId, Arc<AtomicBool>>,
    /// Hit again while running: run once more when this pass ends.
    rerun: HashSet<VolumeId>,
}

pub struct Rebuilds {
    volumes: Arc<tokio::sync::Mutex<VolumeManager>>,
    throttle: Arc<Throttle>,
    automatic: AtomicBool,
    parallel: AtomicUsize,
    in_flight: AtomicUsize,
    inner: std::sync::Mutex<Inner>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Rebuilds {
    pub fn new(volumes: Arc<tokio::sync::Mutex<VolumeManager>>, cfg: &RebuildConfig) -> Arc<Self> {
        Arc::new(Rebuilds {
            volumes,
            throttle: Arc::new(Throttle::new(cfg.max_bytes_per_sec)),
            automatic: AtomicBool::new(cfg.automatic),
            parallel: AtomicUsize::new(cfg.parallel.max(1)),
            in_flight: AtomicUsize::new(cfg.extents_in_flight.max(1)),
            inner: std::sync::Mutex::new(Inner::default()),
        })
    }

    pub fn automatic(&self) -> bool {
        self.automatic.load(Ordering::Relaxed)
    }

    pub fn settings(&self) -> RebuildSettings {
        RebuildSettings {
            automatic: self.automatic(),
            parallel: self.parallel.load(Ordering::Relaxed),
            extents_in_flight: self.in_flight.load(Ordering::Relaxed),
            max_bytes_per_sec: self.throttle.rate(),
        }
    }

    /// Change settings. A lower `parallel` lets running rebuilds finish; a
    /// higher one starts queued volumes now.
    pub fn set(self: &Arc<Self>, automatic: Option<bool>, parallel: Option<usize>, in_flight: Option<usize>, rate: Option<u64>) {
        if let Some(a) = automatic {
            self.automatic.store(a, Ordering::Relaxed);
        }
        if let Some(p) = parallel {
            self.parallel.store(p.max(1), Ordering::Relaxed);
        }
        if let Some(n) = in_flight {
            self.in_flight.store(n.max(1), Ordering::Relaxed);
        }
        if let Some(r) = rate {
            self.throttle.set_rate(r);
        }
        self.pump();
    }

    /// Bytes every rebuild on the node has copied since it started.
    pub fn bytes_copied(&self) -> u64 {
        self.throttle.taken()
    }

    pub fn jobs(&self) -> Vec<RebuildJob> {
        self.inner.lock().unwrap().jobs.values().rev().cloned().collect()
    }

    pub fn job(&self, id: u64) -> Option<RebuildJob> {
        self.inner.lock().unwrap().jobs.get(&id).cloned()
    }

    /// Volumes waiting and rebuilding right now.
    pub fn counts(&self) -> (usize, usize) {
        let g = self.inner.lock().unwrap();
        (g.queue.len(), g.running.len())
    }

    /// Whether a rebuild holds this volume (queued or running).
    pub fn holds(&self, volume: &VolumeId) -> bool {
        let g = self.inner.lock().unwrap();
        g.queued.contains_key(volume) || g.running.contains_key(volume)
    }

    /// The unfinished job a drive's report started, if any.
    pub fn active_for_drive(&self, drive: &str) -> Option<u64> {
        let g = self.inner.lock().unwrap();
        g.jobs
            .values()
            .find(|j| !j.state.finished() && j.drive.as_deref() == Some(drive))
            .map(|j| j.id)
    }

    /// Resolves when the job is finished (at once if there is no such job).
    pub async fn wait(&self, id: u64) {
        let rx = { self.inner.lock().unwrap().done.get(&id).map(|tx| tx.subscribe()) };
        if let Some(mut rx) = rx {
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    /// Queue a rebuild of `volumes` (unreplicated ones are skipped: there is
    /// nothing to rebuild them from). Returns the job id.
    pub async fn start(self: &Arc<Self>, reason: String, drive: Option<String>, volumes: Vec<VolumeId>) -> u64 {
        // Margin and name, read before the queue lock: health walks a
        // volume's map.
        let mut entries = Vec::new();
        {
            let vm = self.volumes.lock().await;
            let names: HashMap<VolumeId, String> =
                vm.list_volumes().await.into_iter().map(|(id, name, ..)| (id, name)).collect();
            let mut seen = HashSet::new();
            for id in volumes {
                if !seen.insert(id) {
                    continue;
                }
                let Some(h) = vm.get_volume_handle(&id) else { continue };
                if h.redundancy().is_none() {
                    continue;
                }
                let margin = h.health().await.margin;
                entries.push((id, names.get(&id).cloned().unwrap_or_default(), margin));
            }
        }
        let id = {
            let mut g = self.inner.lock().unwrap();
            g.next_id += 1;
            let id = g.next_id;
            let mut job = RebuildJob {
                id,
                reason,
                drive,
                state: RebuildState::Queued,
                volumes: Vec::new(),
                bytes_copied: 0,
                started_at: now(),
                finished_at: None,
            };
            for (vol, name, margin) in entries {
                let state = if g.running.contains_key(&vol) {
                    // Already being rebuilt: this failure may be behind it.
                    g.rerun.insert(vol);
                    RebuildState::Running
                } else {
                    // Queued once, at the most urgent margin anyone gave it.
                    let seq = g.seq;
                    g.seq += 1;
                    match g.queued.get(&vol).copied() {
                        Some((m, _)) if m <= margin => {}
                        Some((m, s)) => {
                            g.queue.remove(&(m, s, vol));
                            g.queue.insert((margin, seq, vol));
                            g.queued.insert(vol, (margin, seq));
                        }
                        None => {
                            g.queue.insert((margin, seq, vol));
                            g.queued.insert(vol, (margin, seq));
                        }
                    }
                    RebuildState::Queued
                };
                job.volumes.push(VolumeRebuild {
                    volume: vol,
                    name,
                    state,
                    margin,
                    legs_rebuilt: 0,
                    legs_added: 0,
                    bytes_copied: 0,
                    unrecoverable: 0,
                    health: None,
                    errors: Vec::new(),
                    started_at: None,
                    finished_at: None,
                });
            }
            let (tx, _) = tokio::sync::watch::channel(false);
            g.done.insert(id, tx);
            g.jobs.insert(id, job);
            Self::settle_job(&mut g, id);
            // Keep the history bounded: the last 64 finished jobs.
            let finished: Vec<u64> = g.jobs.values().filter(|j| j.state.finished()).map(|j| j.id).collect();
            if finished.len() > 64 {
                for old in &finished[..finished.len() - 64] {
                    g.jobs.remove(old);
                    g.done.remove(old);
                }
            }
            id
        };
        self.pump();
        id
    }

    /// Stop a job: its queued volumes leave the queue unless another job
    /// wants them, and its running ones stop between extents. What has been
    /// rebuilt stays rebuilt.
    pub fn cancel(self: &Arc<Self>, id: u64) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(job) = g.jobs.get(&id) else { return false };
        if job.state.finished() {
            return true;
        }
        let mine: Vec<VolumeId> = job.volumes.iter().filter(|v| !v.state.finished()).map(|v| v.volume).collect();
        for vol in mine {
            let wanted_elsewhere = g.jobs.values().any(|j| {
                j.id != id && !j.state.finished() && j.volumes.iter().any(|v| v.volume == vol && !v.state.finished())
            });
            if !wanted_elsewhere {
                if let Some((m, s)) = g.queued.remove(&vol) {
                    g.queue.remove(&(m, s, vol));
                }
                if let Some(flag) = g.running.get(&vol) {
                    flag.store(true, Ordering::Relaxed);
                }
                g.rerun.remove(&vol);
            }
            if let Some(v) = g.jobs.get_mut(&id).unwrap().volumes.iter_mut().find(|v| v.volume == vol) {
                v.state = RebuildState::Cancelled;
                v.finished_at = Some(now());
            }
        }
        Self::settle_job(&mut g, id);
        true
    }

    /// Recompute a job's state from its volumes; signal waiters when done.
    fn settle_job(g: &mut Inner, id: u64) {
        let Some(job) = g.jobs.get_mut(&id) else { return };
        if job.state.finished() {
            return;
        }
        job.bytes_copied = job.volumes.iter().map(|v| v.bytes_copied).sum();
        let all_finished = job.volumes.iter().all(|v| v.state.finished());
        if !all_finished {
            if job.volumes.iter().any(|v| v.state == RebuildState::Running) {
                job.state = RebuildState::Running;
            }
            return;
        }
        job.state = if job.volumes.iter().any(|v| v.state == RebuildState::Cancelled) {
            RebuildState::Cancelled
        } else if job.volumes.iter().any(|v| v.state == RebuildState::Partial) {
            RebuildState::Partial
        } else {
            RebuildState::Done
        };
        job.finished_at = Some(now());
        if let Some(tx) = g.done.get(&id) {
            let _ = tx.send(true);
        }
    }

    /// Start queued volumes while there is room.
    fn pump(self: &Arc<Self>) {
        let mut g = self.inner.lock().unwrap();
        let parallel = self.parallel.load(Ordering::Relaxed);
        while g.running.len() < parallel {
            let Some(first) = g.queue.iter().next().copied() else { break };
            g.queue.remove(&first);
            let vol = first.2;
            g.queued.remove(&vol);
            let cancel = Arc::new(AtomicBool::new(false));
            g.running.insert(vol, cancel.clone());
            let ids: Vec<u64> = g.jobs.keys().copied().collect();
            for id in ids {
                let job = g.jobs.get_mut(&id).unwrap();
                if job.state.finished() {
                    continue;
                }
                for v in job.volumes.iter_mut().filter(|v| v.volume == vol && v.state == RebuildState::Queued) {
                    v.state = RebuildState::Running;
                    v.started_at = Some(now());
                }
                Self::settle_job(&mut g, id);
            }
            let me = self.clone();
            tokio::spawn(async move { me.run_one(vol, cancel).await });
        }
    }

    async fn run_one(self: Arc<Self>, vol: VolumeId, cancel: Arc<AtomicBool>) {
        let handle = { self.volumes.lock().await.get_volume_handle(&vol) };
        let (report, health) = match handle {
            None => (None, None),
            Some(handle) => {
                // Make the map durable, then free what it stopped naming.
                let vm = self.volumes.clone();
                let h2 = handle.clone();
                let checkpoint: ResyncCheckpoint = Arc::new(move |owed| {
                    let vm = vm.clone();
                    let h = h2.clone();
                    Box::pin(async move {
                        vm.lock().await.persist().await;
                        h.release_slots(&owed).await;
                    })
                });
                let opts = ResyncOptions {
                    verify: false,
                    in_flight: self.in_flight.load(Ordering::Relaxed),
                    throttle: Some(self.throttle.clone()),
                    cancel: Some(cancel.clone()),
                    checkpoint: Some(checkpoint),
                    checkpoint_every: 4096,
                };
                let mut report = handle.resync_with(&opts).await;
                self.volumes.lock().await.persist().await;
                let owed = std::mem::take(&mut report.owed);
                handle.release_slots(&owed).await;
                let health = handle.health().await;
                tracing::info!(
                    volume = %vol, rebuilt = report.legs_rebuilt, added = report.legs_added,
                    bytes = report.bytes_copied, health = %health.state,
                    "volume rebuild finished"
                );
                (Some(report), Some(health))
            }
        };

        let requeue = {
            let mut g = self.inner.lock().unwrap();
            g.running.remove(&vol);
            let cancelled = cancel.load(Ordering::Relaxed);
            let again = g.rerun.remove(&vol) && !cancelled;
            let state = match (&health, cancelled) {
                (_, true) => RebuildState::Cancelled,
                (None, _) => RebuildState::Done, // the volume is gone: nothing left to rebuild
                (Some(h), _) if h.state == HealthState::Healthy => RebuildState::Done,
                _ => RebuildState::Partial,
            };
            let ids: Vec<u64> = g.jobs.keys().copied().collect();
            for id in ids {
                let job = g.jobs.get_mut(&id).unwrap();
                if job.state.finished() {
                    continue;
                }
                for v in job.volumes.iter_mut().filter(|v| v.volume == vol && v.state == RebuildState::Running) {
                    if let Some(r) = &report {
                        v.legs_rebuilt += r.legs_rebuilt;
                        v.legs_added += r.legs_added;
                        v.bytes_copied += r.bytes_copied;
                        v.unrecoverable += r.unrecoverable;
                        for e in &r.errors {
                            if v.errors.len() < 16 {
                                v.errors.push(e.clone());
                            }
                        }
                    }
                    v.health = health.as_ref().map(|h| h.state);
                    if again {
                        // Stays running: the rerun is part of this job.
                        continue;
                    }
                    v.state = state;
                    v.finished_at = Some(now());
                }
                Self::settle_job(&mut g, id);
            }
            again
        };
        if requeue {
            let margin = match self.volumes.lock().await.get_volume_handle(&vol) {
                Some(h) => h.health().await.margin,
                None => 0,
            };
            let mut g = self.inner.lock().unwrap();
            let seq = g.seq;
            g.seq += 1;
            g.queue.insert((margin, seq, vol));
            g.queued.insert(vol, (margin, seq));
            // Its job entries read `running` through the requeue; `pump`
            // only promotes `queued` ones, so mark them queued again.
            let ids: Vec<u64> = g.jobs.keys().copied().collect();
            for id in ids {
                let job = g.jobs.get_mut(&id).unwrap();
                for v in job.volumes.iter_mut().filter(|v| v.volume == vol && v.state == RebuildState::Running) {
                    v.state = RebuildState::Queued;
                }
            }
        }
        self.pump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::placement::topology::StorageTier;
    use crate::volume::{CreateOptions, RedundancyPolicy};

    async fn slab(dir: &std::path::Path, name: &str, slot: u64) -> Slab {
        let path = dir.join(name);
        let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), 8 * 1024 * 1024).await.unwrap();
        Slab::format(Arc::new(dev), slot, StorageTier::Hot).await.unwrap()
    }

    /// A drive fails under many mirrored volumes: one job rebuilds every
    /// volume with a member there, several at once, and each comes back
    /// healthy with nothing left on the drive.
    #[tokio::test]
    async fn a_failed_drive_rebuilds_every_volume_with_a_member_there() {
        let dir = std::env::temp_dir().join(format!("stormblock-rebuild-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let slot = 4096u64;
        let mut vm = VolumeManager::new(slot);
        let mut sids = Vec::new();
        for n in ["a", "b", "c", "d"] {
            let s = slab(&dir, n, slot).await;
            sids.push(s.slab_id());
            vm.add_slab(s).await;
        }
        let mut vols = Vec::new();
        for i in 0..8 {
            let id = vm
                .create_volume_with(&format!("v{i}"), 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
                .await
                .unwrap();
            let v = vm.get_volume(&id).unwrap();
            for e in 0..6u64 {
                v.write(e * slot, &vec![(i * 16 + e as usize) as u8; slot as usize]).await.unwrap();
            }
            vols.push(id);
        }
        let plain = vm.create_volume_any("plain", 1 << 20).await.unwrap();
        vm.get_volume(&plain).unwrap().write(0, &[9u8; 4096]).await.unwrap();

        let lost = sids[0];
        let touched = vm.distrust_slab(lost).await;
        assert!(touched.len() >= 4, "most volumes have a member on a: {touched:?}");
        let gem = vm.gem().clone();
        let volumes = Arc::new(tokio::sync::Mutex::new(vm));
        let rb = Rebuilds::new(volumes.clone(), &RebuildConfig { parallel: 3, ..Default::default() });
        let job = rb.start("drive a failed".into(), Some("a".into()), touched.clone()).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), rb.wait(job)).await.expect("the rebuild finishes");

        let j = rb.job(job).unwrap();
        assert_eq!(j.state, RebuildState::Done, "{j:?}");
        assert_eq!(j.volumes.len(), touched.len());
        assert!(j.volumes.iter().all(|v| v.state == RebuildState::Done && v.legs_rebuilt > 0));
        assert!(j.bytes_copied > 0);
        assert_eq!(rb.counts(), (0, 0));

        let vm = volumes.lock().await;
        let g = gem.read().await;
        for (i, id) in vols.iter().enumerate() {
            assert_eq!(vm.health(id).await.unwrap().state, HealthState::Healthy);
            if let Some(m) = g.get_volume_map(id) {
                assert!(m.all_legs().all(|l| l.slab_id != lost), "volume {i} left a member on the failed drive");
            }
            let v = vm.get_volume(id).unwrap();
            for e in 0..6u64 {
                let mut buf = vec![0u8; slot as usize];
                v.read(e * slot, &mut buf).await.unwrap();
                assert!(buf.iter().all(|&b| b == (i * 16 + e as usize) as u8));
            }
        }
        // The unreplicated volume was never queued: nothing to rebuild from.
        assert!(!j.volumes.iter().any(|v| v.volume == plain));
        drop(g);
        drop(vm);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The queue takes the most endangered first: a `mirror:3` that lost
    /// one member (margin 1) waits behind a `mirror:2` that lost one
    /// (margin 0).
    #[tokio::test]
    async fn the_most_endangered_volume_goes_first() {
        let dir = std::env::temp_dir().join(format!("stormblock-rebuild-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let slot = 4096u64;
        let mut vm = VolumeManager::new(slot);
        let mut sids = Vec::new();
        for n in ["a", "b", "c", "d"] {
            let s = slab(&dir, n, slot).await;
            sids.push(s.slab_id());
            vm.add_slab(s).await;
        }
        let three = vm
            .create_volume_with("three", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(3)))
            .await
            .unwrap();
        let two = vm
            .create_volume_with("two", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
            .await
            .unwrap();
        for id in [three, two] {
            vm.get_volume(&id).unwrap().write(0, &[1u8; 4096]).await.unwrap();
        }
        // A slab both have a leg on.
        let lost = {
            let g = vm.gem().read().await;
            let two_legs: HashSet<_> = g.get_volume_map(&two).unwrap().all_legs().map(|l| l.slab_id).collect();
            g.get_volume_map(&three).unwrap().all_legs().map(|l| l.slab_id).find(|s| two_legs.contains(s)).unwrap()
        };
        vm.distrust_slab(lost).await;
        let volumes = Arc::new(tokio::sync::Mutex::new(vm));
        // One at a time: `start` promotes the first of the queue before it
        // returns, so the job already shows which one that was.
        let rb = Rebuilds::new(volumes.clone(), &RebuildConfig { parallel: 1, ..Default::default() });
        let job = rb.start("test".into(), None, vec![three, two]).await;
        let j = rb.job(job).unwrap();
        let m = |id: VolumeId| j.volumes.iter().find(|v| v.volume == id).unwrap().margin;
        assert_eq!(m(three), 1);
        assert_eq!(m(two), 0);
        let running: Vec<VolumeId> =
            j.volumes.iter().filter(|v| v.state == RebuildState::Running).map(|v| v.volume).collect();
        assert_eq!(running, vec![two], "margin 0 first: {j:?}");
        tokio::time::timeout(std::time::Duration::from_secs(30), rb.wait(job)).await.unwrap();
        assert_eq!(rb.job(job).unwrap().state, RebuildState::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With too few domains left to hold the policy the job ends `partial`
    /// and says why, rather than reporting success.
    #[tokio::test]
    async fn a_rebuild_with_nowhere_to_go_is_partial() {
        let dir = std::env::temp_dir().join(format!("stormblock-rebuild-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let slot = 4096u64;
        let mut vm = VolumeManager::new(slot);
        let a = slab(&dir, "a", slot).await;
        let ida = a.slab_id();
        vm.add_slab(a).await;
        vm.add_slab(slab(&dir, "b", slot).await).await;
        let id = vm
            .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
            .await
            .unwrap();
        vm.get_volume(&id).unwrap().write(0, &[1u8; 4096]).await.unwrap();
        vm.distrust_slab(ida).await;
        let volumes = Arc::new(tokio::sync::Mutex::new(vm));
        let rb = Rebuilds::new(volumes.clone(), &RebuildConfig::default());
        let job = rb.start("test".into(), None, vec![id]).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), rb.wait(job)).await.unwrap();
        let j = rb.job(job).unwrap();
        assert_eq!(j.state, RebuildState::Partial, "{j:?}");
        assert_eq!(j.volumes[0].health, Some(HealthState::Degraded));
        assert!(!j.volumes[0].errors.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
