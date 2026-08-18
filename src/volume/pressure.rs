//! Pool accounting and growth on disk pressure (#18).
//!
//! Thin volumes overcommit, so a pool can run out of **physical** space while
//! every volume still reports free virtual space. Without something watching,
//! the first sign is writes failing while `df` inside the consumer still shows
//! room — invisible until it bites, and confusing when it does.
//!
//! This module supplies the two things that were missing: pool-level accounting
//! (per-slab numbers exist; nothing summed them) and a watcher that adds a slab
//! when utilisation crosses a high-water mark.
//!
//! # Grow on pressure, never preallocate
//!
//! Preallocating to the virtual size gives back all the space thin provisioning
//! saved, so the pool grows by one slab at a time, when it is actually needed.
//! stormcos is the motivating consumer: it boots from a ~6 GB image carrying a
//! deliberately small 4 GiB physical reserve — small so the image stays
//! downloadable — and is expected to grow into its real disk after boot.
//!
//! # Why a new slab rather than a bigger one
//!
//! A slab's data region starts at a fixed offset chosen at format time, past a
//! slot table sized for exactly the slots it will ever have. Growing one in
//! place would move every byte of data. Adding a slab costs a format of unused
//! space and nothing else, and the registry already spreads allocation across
//! whatever slabs exist.
//!
//! # Where the space comes from
//!
//! From [`GrowthSource`]s the operator configured, and nowhere else. The
//! watcher never goes looking for a disk to claim: formatting the wrong device
//! is unrecoverable, and "it had no filesystem on it" is not consent. A
//! directory source is the safe general case — it creates a new backing file
//! and can be pointed at the unused tail of the node's own disk by mounting it
//! there.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::drive::filedev::FileDevice;
use crate::drive::slab::Slab;
use crate::drive::slab_registry::SlabRegistry;
use crate::drive::BlockDevice;
use crate::placement::topology::StorageTier;

/// What a pool currently holds, in slots and in bytes.
///
/// Sampled from the registry rather than tracked incrementally: the slabs are
/// the authority, and a counter that drifts from them is worse than no counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolUsage {
    pub slabs: usize,
    pub total_slots: u64,
    pub free_slots: u64,
    pub allocated_slots: u64,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub allocated_bytes: u64,
    /// Per-tier breakdown, so pressure on the hot tier is visible even when the
    /// pool as a whole looks comfortable.
    pub by_tier: Vec<TierUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierUsage {
    pub tier: String,
    pub slabs: usize,
    pub total_slots: u64,
    pub free_slots: u64,
}

impl PoolUsage {
    /// Fraction of the pool in use, 0.0–100.0. An empty pool reads as 100%
    /// used rather than 0%: no capacity is a pressure condition, not a
    /// comfortable one, and reporting it as empty-and-fine is how a node with
    /// no slabs looks healthy right up until the first write.
    pub fn used_pct(&self) -> f64 {
        if self.total_slots == 0 {
            return 100.0;
        }
        (self.allocated_slots as f64 / self.total_slots as f64) * 100.0
    }

    /// Sample the registry.
    pub async fn sample(registry: &Arc<RwLock<SlabRegistry>>) -> PoolUsage {
        let reg = registry.read().await;
        let mut by_tier: std::collections::BTreeMap<String, TierUsage> = Default::default();
        let (mut total_bytes, mut free_bytes) = (0u64, 0u64);

        for (_, slab) in reg.iter() {
            let entry = by_tier.entry(format!("{:?}", slab.tier())).or_insert(TierUsage {
                tier: format!("{:?}", slab.tier()),
                slabs: 0,
                total_slots: 0,
                free_slots: 0,
            });
            entry.slabs += 1;
            entry.total_slots += slab.total_slots();
            entry.free_slots += slab.free_slots();
            total_bytes += slab.total_slots() * slab.slot_size();
            free_bytes += slab.free_slots() * slab.slot_size();
        }

        let total_slots = reg.total_slots();
        let free_slots = reg.total_free_slots();
        PoolUsage {
            slabs: reg.iter().count(),
            total_slots,
            free_slots,
            allocated_slots: total_slots.saturating_sub(free_slots),
            total_bytes,
            free_bytes,
            allocated_bytes: total_bytes.saturating_sub(free_bytes),
            by_tier: by_tier.into_values().collect(),
        }
    }
}

/// Somewhere the pool is allowed to take capacity from.
///
/// Configured, never discovered. Each source is claimed at most once, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrowthSource {
    /// A block device or file the pool may claim whole.
    ///
    /// Adopted rather than reformatted when it already carries a readable slab,
    /// so a source that was claimed before a reboot comes back with its data.
    /// A device that is *not* a slab is formatted, which destroys whatever is
    /// on it — naming it here is what authorises that.
    Device { path: String },
    /// A directory to create new backing files in.
    ///
    /// The safe general case, and the one that fits "grow into the free tail of
    /// the node's own disk": mount the spare space and point this at it. Each
    /// claim creates one file of `slab_bytes` and formats it, so nothing that
    /// already exists is ever overwritten.
    Directory {
        path: String,
        /// Size of each backing file. `None` uses `min_slab_bytes`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slab_bytes: Option<u64>,
    },
}

impl GrowthSource {
    fn describe(&self) -> String {
        match self {
            GrowthSource::Device { path } => format!("device {path}"),
            GrowthSource::Directory { path, .. } => format!("directory {path}"),
        }
    }
}

/// How the watcher behaves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PressureConfig {
    /// Watch for pressure at all. Off by default — a pool with no configured
    /// sources has nothing to do, and a node that has not opted in should not
    /// find slabs appearing.
    pub enabled: bool,
    /// Add capacity at or above this percentage used (default 80).
    pub high_water_pct: f64,
    /// Seconds between checks (default 60).
    pub check_interval_secs: u64,
    /// Smallest slab worth adding, in bytes (default 1 GiB). A source that
    /// cannot supply this much is skipped rather than half-used.
    pub min_slab_bytes: u64,
    /// Never let the pool exceed this many slabs. A backstop against a
    /// misconfigured source list, not a capacity policy (default 64).
    pub max_slabs: usize,
    /// Sources, claimed in order.
    pub sources: Vec<GrowthSource>,
}

impl Default for PressureConfig {
    fn default() -> Self {
        PressureConfig {
            enabled: false,
            high_water_pct: 80.0,
            check_interval_secs: 60,
            min_slab_bytes: 1024 * 1024 * 1024,
            max_slabs: 64,
            sources: Vec::new(),
        }
    }
}

/// Why a check did not grow the pool, or that it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GrowthDecision {
    /// Below the mark; nothing to do.
    BelowMark,
    /// Over the mark, and a slab was added.
    Grew { source: String, added_bytes: u64 },
    /// Over the mark with every source already claimed. This is the state an
    /// operator has to see: the pool is under pressure and the engine is out of
    /// ways to answer it.
    SourcesExhausted,
    /// Over the mark but already at `max_slabs`.
    SlabLimitReached { slabs: usize },
    /// Over the mark and the claim failed. The source is retired so a broken
    /// path is not retried every interval.
    Failed { source: String, error: String },
}

/// What the watcher has seen, for the API and for must-gather.
#[derive(Debug, Clone, Serialize)]
pub struct PressureStatus {
    pub enabled: bool,
    pub high_water_pct: f64,
    pub usage: PoolUsage,
    pub used_pct: f64,
    pub under_pressure: bool,
    /// Sources not yet claimed or retired.
    pub sources_remaining: usize,
    pub slabs_added: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_decision: Option<GrowthDecision>,
}

/// Decide whether a sample calls for growth. Pure, so the policy is testable
/// without a disk under it.
pub fn under_pressure(usage: &PoolUsage, high_water_pct: f64) -> bool {
    usage.used_pct() >= high_water_pct
}

/// The watcher's own state: which sources are left, and what happened last.
pub struct PressureWatcher {
    cfg: PressureConfig,
    registry: Arc<RwLock<SlabRegistry>>,
    slot_size: u64,
    /// Sources not yet claimed. Popped from the front; a failed claim is
    /// dropped rather than put back, so one bad path costs one attempt.
    remaining: std::collections::VecDeque<GrowthSource>,
    slabs_added: u64,
    last: Option<GrowthDecision>,
}

impl PressureWatcher {
    pub fn new(
        cfg: PressureConfig,
        registry: Arc<RwLock<SlabRegistry>>,
        slot_size: u64,
    ) -> Self {
        let remaining = cfg.sources.iter().cloned().collect();
        PressureWatcher { cfg, registry, slot_size, remaining, slabs_added: 0, last: None }
    }

    pub async fn status(&self) -> PressureStatus {
        let usage = PoolUsage::sample(&self.registry).await;
        PressureStatus {
            enabled: self.cfg.enabled,
            high_water_pct: self.cfg.high_water_pct,
            used_pct: usage.used_pct(),
            under_pressure: under_pressure(&usage, self.cfg.high_water_pct),
            usage,
            sources_remaining: self.remaining.len(),
            slabs_added: self.slabs_added,
            last_decision: self.last.clone(),
        }
    }

    /// One evaluation. Returns what it decided, which is also recorded.
    pub async fn check(&mut self) -> GrowthDecision {
        let usage = PoolUsage::sample(&self.registry).await;
        let used = usage.used_pct();
        metrics::gauge!("stormblock_pool_used_pct").set(used);
        metrics::gauge!("stormblock_pool_total_bytes").set(usage.total_bytes as f64);
        metrics::gauge!("stormblock_pool_free_bytes").set(usage.free_bytes as f64);

        let decision = if !under_pressure(&usage, self.cfg.high_water_pct) {
            GrowthDecision::BelowMark
        } else if usage.slabs >= self.cfg.max_slabs {
            // Loud, and every interval: a pool wedged at its slab limit while
            // under pressure is not a steady state anyone should discover late.
            tracing::error!(
                used_pct = used,
                slabs = usage.slabs,
                "pool is under pressure and already at its slab limit"
            );
            GrowthDecision::SlabLimitReached { slabs: usage.slabs }
        } else {
            match self.remaining.pop_front() {
                None => {
                    tracing::error!(
                        used_pct = used,
                        "pool is under pressure with every growth source claimed — \
                         it will run out of physical space"
                    );
                    GrowthDecision::SourcesExhausted
                }
                Some(source) => {
                    let what = source.describe();
                    tracing::info!(used_pct = used, source = %what, "pool under pressure — claiming");
                    match self.claim(&source).await {
                        Ok(added) => {
                            self.slabs_added += 1;
                            metrics::counter!("stormblock_pool_slabs_added_total").increment(1);
                            tracing::info!(
                                source = %what,
                                added_bytes = added,
                                "pool grew by one slab"
                            );
                            GrowthDecision::Grew { source: what, added_bytes: added }
                        }
                        Err(e) => {
                            metrics::counter!("stormblock_pool_growth_failures_total").increment(1);
                            tracing::error!(source = %what, "claiming growth source failed: {e}");
                            GrowthDecision::Failed { source: what, error: e }
                        }
                    }
                }
            }
        };

        self.last = Some(decision.clone());
        decision
    }

    /// Turn one source into a registered slab. Returns the bytes it added.
    async fn claim(&mut self, source: &GrowthSource) -> Result<u64, String> {
        let device: Arc<dyn BlockDevice> = match source {
            GrowthSource::Device { path } => {
                let dev: Arc<dyn BlockDevice> = crate::drive::open_one_drive(path)
                    .await
                    .map_err(|e| format!("opening {path}: {e}"))?
                    .into();
                if dev.capacity_bytes() < self.cfg.min_slab_bytes {
                    return Err(format!(
                        "{path} holds {} bytes, below the {} minimum",
                        dev.capacity_bytes(),
                        self.cfg.min_slab_bytes
                    ));
                }
                dev
            }
            GrowthSource::Directory { path, slab_bytes } => {
                let size = slab_bytes.unwrap_or(self.cfg.min_slab_bytes);
                if size < self.cfg.min_slab_bytes {
                    return Err(format!(
                        "{path}: slab_bytes {size} is below the {} minimum",
                        self.cfg.min_slab_bytes
                    ));
                }
                let dir = PathBuf::from(path);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("creating {}: {e}", dir.display()))?;
                // A fresh name every time: this must never land on a file that
                // already exists, whoever put it there.
                let file = dir.join(format!("slab-{}.slab", uuid::Uuid::new_v4().simple()));
                if file.exists() {
                    return Err(format!("{} already exists", file.display()));
                }
                let dev = FileDevice::open_with_capacity(
                    file.to_str().ok_or_else(|| "non-UTF-8 path".to_string())?,
                    size,
                )
                .await
                .map_err(|e| format!("creating {}: {e}", file.display()))?;
                Arc::new(dev)
            }
        };

        // Adopt before formatting. A source claimed before a reboot still
        // carries its slab and its data; reformatting it because it was listed
        // as a growth source would throw that away silently.
        let slab = match Slab::open(device.clone()).await {
            Ok(existing) if existing.slot_size() == self.slot_size => {
                tracing::info!(
                    source = %source.describe(),
                    slab = %existing.slab_id().0,
                    "adopted an existing slab rather than reformatting it"
                );
                existing
            }
            Ok(existing) => {
                return Err(format!(
                    "carries a slab with {}-byte slots, but this pool uses {}",
                    existing.slot_size(),
                    self.slot_size
                ))
            }
            Err(_) => Slab::format(device, self.slot_size, StorageTier::Hot)
                .await
                .map_err(|e| format!("formatting: {e}"))?,
        };

        let added = slab.total_slots() * slab.slot_size();
        self.registry.write().await.add(slab);
        Ok(added)
    }
}

/// Run the watcher in the background, returning the status it keeps current.
pub fn spawn(
    cfg: PressureConfig,
    registry: Arc<RwLock<SlabRegistry>>,
    slot_size: u64,
) -> Arc<RwLock<Option<PressureStatus>>> {
    let status = Arc::new(RwLock::new(None));
    let out = status.clone();
    let interval_secs = cfg.check_interval_secs.max(1);
    let sources = cfg.sources.len();

    tokio::spawn(async move {
        let mut watcher = PressureWatcher::new(cfg, registry, slot_size);
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so the pool is not judged
        // before volume metadata has loaded and the slabs are registered.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            watcher.check().await;
            *status.write().await = Some(watcher.status().await);
        }
    });

    tracing::info!(interval_secs, sources, "pool pressure watcher started");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total: u64, free: u64) -> PoolUsage {
        PoolUsage {
            slabs: 1,
            total_slots: total,
            free_slots: free,
            allocated_slots: total - free,
            total_bytes: total * 4096,
            free_bytes: free * 4096,
            allocated_bytes: (total - free) * 4096,
            by_tier: Vec::new(),
        }
    }

    #[test]
    fn the_mark_is_inclusive_and_an_empty_pool_is_pressure() {
        assert!(!under_pressure(&usage(100, 21), 80.0), "79% is below the mark");
        assert!(under_pressure(&usage(100, 20), 80.0), "80% is the mark, not past it");
        assert!(under_pressure(&usage(100, 0), 80.0));

        // No capacity at all reads as full, not as empty-and-fine — otherwise a
        // node with no slabs looks healthy until its first write.
        assert_eq!(usage(0, 0).used_pct(), 100.0);
        assert!(under_pressure(&usage(0, 0), 80.0));
    }

    /// A slab-backed pool, so the accounting is read off real slabs.
    async fn pool(slabs: usize, slot: u64, bytes: u64) -> (Arc<RwLock<SlabRegistry>>, TempDirs) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut reg = SlabRegistry::new();
        for i in 0..slabs {
            let path = dir.path().join(format!("s{i}.slab"));
            let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), bytes).await.unwrap();
            reg.add(Slab::format(Arc::new(dev), slot, StorageTier::Hot).await.unwrap());
        }
        (Arc::new(RwLock::new(reg)), TempDirs(vec![dir]))
    }

    struct TempDirs(Vec<tempfile::TempDir>);

    #[tokio::test]
    async fn usage_sums_the_slabs_and_breaks_down_by_tier() {
        let (registry, _d) = pool(3, 1 << 20, 16 << 20).await;
        let u = PoolUsage::sample(&registry).await;
        assert_eq!(u.slabs, 3);
        assert!(u.total_slots >= 3 * 14, "three ~16 MiB slabs of 1 MiB slots: {u:?}");
        assert_eq!(u.free_slots, u.total_slots, "nothing allocated yet");
        assert_eq!(u.allocated_slots, 0);
        assert_eq!(u.used_pct(), 0.0);
        assert_eq!(u.by_tier.len(), 1, "all hot");
        assert_eq!(u.by_tier[0].total_slots, u.total_slots);
        assert_eq!(u.total_bytes, u.total_slots * (1 << 20));
    }

    /// The whole point: cross the mark, and a slab appears.
    #[tokio::test]
    async fn crossing_the_mark_claims_a_directory_source() {
        let (registry, _d) = pool(1, 1 << 20, 16 << 20).await;
        let grow = tempfile::TempDir::new().unwrap();

        let cfg = PressureConfig {
            enabled: true,
            high_water_pct: 80.0,
            min_slab_bytes: 1 << 20,
            sources: vec![GrowthSource::Directory {
                path: grow.path().to_str().unwrap().to_string(),
                slab_bytes: Some(16 << 20),
            }],
            ..Default::default()
        };
        let mut w = PressureWatcher::new(cfg, registry.clone(), 1 << 20);

        // Comfortable: nothing happens, and the source is untouched.
        assert_eq!(w.check().await, GrowthDecision::BelowMark);
        assert_eq!(w.status().await.sources_remaining, 1);
        assert_eq!(std::fs::read_dir(grow.path()).unwrap().count(), 0);

        // Fill it past the mark.
        let before = {
            let mut reg = registry.write().await;
            let total = reg.total_slots();
            let id = *reg.iter().next().unwrap().0;
            let slab = reg.get_mut(&id).unwrap();
            for _ in 0..(total * 9 / 10) {
                slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
            }
            total
        };
        assert!(under_pressure(&PoolUsage::sample(&registry).await, 80.0));

        match w.check().await {
            GrowthDecision::Grew { added_bytes, .. } => assert!(added_bytes > 0),
            other => panic!("expected growth, got {other:?}"),
        }

        // A slab file was created, and the pool is bigger for it.
        assert_eq!(std::fs::read_dir(grow.path()).unwrap().count(), 1);
        let after = PoolUsage::sample(&registry).await;
        assert_eq!(after.slabs, 2);
        assert!(after.total_slots > before, "{} vs {before}", after.total_slots);
        // And the pressure it was added to relieve is gone.
        assert!(!under_pressure(&after, 80.0), "{}% after growth", after.used_pct());

        let st = w.status().await;
        assert_eq!(st.sources_remaining, 0);
        assert_eq!(st.slabs_added, 1);
    }

    /// Out of sources under pressure is a state an operator must be able to
    /// see, not a silent no-op.
    #[tokio::test]
    async fn exhausted_sources_are_reported_every_check() {
        let (registry, _d) = pool(1, 1 << 20, 16 << 20).await;
        {
            let mut reg = registry.write().await;
            let total = reg.total_slots();
            let id = *reg.iter().next().unwrap().0;
            let slab = reg.get_mut(&id).unwrap();
            for _ in 0..total {
                slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
            }
        }

        let mut w = PressureWatcher::new(
            PressureConfig { enabled: true, ..Default::default() },
            registry,
            1 << 20,
        );
        assert_eq!(w.check().await, GrowthDecision::SourcesExhausted);
        // Every interval, not once: this does not resolve itself.
        assert_eq!(w.check().await, GrowthDecision::SourcesExhausted);
        assert!(w.status().await.under_pressure);
    }

    /// A broken source costs one attempt, not one per interval forever.
    #[tokio::test]
    async fn a_failing_source_is_retired_after_one_attempt() {
        let (registry, _d) = pool(1, 1 << 20, 16 << 20).await;
        {
            let mut reg = registry.write().await;
            let total = reg.total_slots();
            let id = *reg.iter().next().unwrap().0;
            let slab = reg.get_mut(&id).unwrap();
            for _ in 0..total {
                slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
            }
        }

        let cfg = PressureConfig {
            enabled: true,
            min_slab_bytes: 1 << 20,
            sources: vec![
                // Too small to be worth adding.
                GrowthSource::Directory {
                    path: tempfile::TempDir::new().unwrap().path().to_str().unwrap().to_string(),
                    slab_bytes: Some(4096),
                },
            ],
            ..Default::default()
        };
        let mut w = PressureWatcher::new(cfg, registry, 1 << 20);

        match w.check().await {
            GrowthDecision::Failed { error, .. } => assert!(error.contains("below"), "{error}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        // Retired, not retried.
        assert_eq!(w.check().await, GrowthDecision::SourcesExhausted);
    }

    /// The slab limit is a backstop, and it wins over an available source.
    #[tokio::test]
    async fn the_slab_limit_stops_growth() {
        let (registry, _d) = pool(2, 1 << 20, 16 << 20).await;
        {
            let mut reg = registry.write().await;
            let ids: Vec<_> = reg.iter().map(|(id, _)| *id).collect();
            for id in ids {
                let slab = reg.get_mut(&id).unwrap();
                let n = slab.total_slots();
                for _ in 0..n {
                    slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
                }
            }
        }
        let grow = tempfile::TempDir::new().unwrap();
        let cfg = PressureConfig {
            enabled: true,
            max_slabs: 2,
            min_slab_bytes: 1 << 20,
            sources: vec![GrowthSource::Directory {
                path: grow.path().to_str().unwrap().to_string(),
                slab_bytes: Some(16 << 20),
            }],
            ..Default::default()
        };
        let mut w = PressureWatcher::new(cfg, registry, 1 << 20);
        assert_eq!(w.check().await, GrowthDecision::SlabLimitReached { slabs: 2 });
        // The source was not consumed, and nothing was written.
        assert_eq!(w.status().await.sources_remaining, 1);
        assert_eq!(std::fs::read_dir(grow.path()).unwrap().count(), 0);
    }

    /// A source that already carries a slab is adopted, not reformatted — the
    /// reboot case, where throwing the data away would be silent.
    #[tokio::test]
    async fn an_existing_slab_on_a_source_is_adopted_with_its_data() {
        let (registry, _d) = pool(1, 1 << 20, 16 << 20).await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("claimed.slab");
        let p = path.to_str().unwrap().to_string();

        // A slab that already exists, with something in it.
        let known = {
            let dev = FileDevice::open_with_capacity(&p, 16 << 20).await.unwrap();
            let mut slab = Slab::format(Arc::new(dev), 1 << 20, StorageTier::Hot).await.unwrap();
            slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
            (slab.slab_id(), slab.allocated_slots())
        };
        assert_eq!(known.1, 1);

        {
            let mut reg = registry.write().await;
            let total = reg.total_slots();
            let id = *reg.iter().next().unwrap().0;
            let slab = reg.get_mut(&id).unwrap();
            for _ in 0..total {
                slab.allocate(crate::volume::extent::VolumeId::new(), 0).await.unwrap();
            }
        }

        let cfg = PressureConfig {
            enabled: true,
            min_slab_bytes: 1 << 20,
            sources: vec![GrowthSource::Device { path: p }],
            ..Default::default()
        };
        let mut w = PressureWatcher::new(cfg, registry.clone(), 1 << 20);
        assert!(matches!(w.check().await, GrowthDecision::Grew { .. }));

        let reg = registry.read().await;
        let adopted = reg.get(&known.0).expect("the same slab, by id — not a fresh format");
        assert_eq!(adopted.allocated_slots(), 1, "its allocation survived the claim");
    }
}
