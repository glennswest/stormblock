//! Extent garbage collection — reclaim slab slots nothing maps any more.
//!
//! An accounting fault between the Global Extent Map and a slab's slot table
//! strands capacity: the slot stays `Allocated`, naming a volume that may no
//! longer exist, while nothing references it. Deleting the slab is not a way
//! out either, since that refuses any slab with allocated slots — which is
//! exactly the state a leak produces. This module finds those slots and gives
//! them back.
//!
//! # What counts as live
//!
//! The **forward** maps are the authority: a slot is live if any volume's
//! extent map points at it. The GEM's reverse index deliberately records only
//! the *primary* owner of a copy-on-write slot (see `gem::restore_mapping` and
//! `clone_volume_map`), so `remove_volume` drops the reverse entry for slots a
//! surviving clone still shares. Collecting on the reverse index would
//! therefore free live data — the union of the forward maps is the only safe
//! basis, and it counts shared slots correctly by construction.
//!
//! # Why this cannot race a write
//!
//! Allocation and mapping are two steps with the registry lock released in
//! between, so a freshly allocated slot is briefly indistinguishable from a
//! leaked one. `SlabRegistry` reservations mark that window, and the collector
//! skips reserved slots. Callers that hold the registry and GEM locks across
//! both steps (extent migration) need no reservation, since the collector
//! takes those same locks.
//!
//! Anything the collector frees is therefore unreferenced *and* not in flight.
//! The optional two-pass confirmation adds a second, independent check for
//! paths that might be added later without a reservation.

use std::collections::HashSet;

use crate::drive::slab::{SlabId, SlotState};
use crate::drive::slab_registry::SlabRegistry;
use crate::volume::extent::VolumeId;
use crate::volume::gem::GlobalExtentMap;

/// A slot that no extent map references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub slab_id: SlabId,
    pub slot_idx: u32,
    /// Owner recorded in the slot table — a volume that usually no longer
    /// exists. Kept for the log line, not used to decide liveness.
    pub volume_id: VolumeId,
    pub virtual_extent_idx: u64,
    pub ref_count: u32,
}

impl Orphan {
    fn key(&self) -> (SlabId, u32) {
        (self.slab_id, self.slot_idx)
    }
}

/// How a collection pass should behave.
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Find and report, but free nothing.
    pub dry_run: bool,
    /// Only reclaim slots that were *also* orphaned in a previous pass.
    ///
    /// Defence in depth: an orphan has to be unreferenced twice, with the
    /// locks dropped in between, before its data is thrown away.
    pub confirm_against: Option<HashSet<(SlabId, u32)>>,
    /// Stop after reclaiming this many slots, so one pass cannot monopolise
    /// the registry lock on a badly leaked slab.
    pub max_reclaim: Option<usize>,
}

/// What a collection pass found and did.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub slabs_scanned: usize,
    pub slots_scanned: u64,
    /// Slots referenced by at least one volume.
    pub live: u64,
    /// Allocated-but-not-yet-mapped slots skipped this pass.
    pub in_flight: usize,
    /// Everything found unreferenced this pass.
    pub orphans: Vec<Orphan>,
    /// Orphans held back awaiting a second confirming pass.
    pub unconfirmed: usize,
    pub reclaimed: usize,
    pub bytes_reclaimed: u64,
    /// Orphans left because `max_reclaim` was hit.
    pub deferred: usize,
    pub dry_run: bool,
}

impl GcReport {
    /// Candidate set to feed the next pass's `confirm_against`.
    pub fn candidates(&self) -> HashSet<(SlabId, u32)> {
        self.orphans.iter().map(|o| o.key()).collect()
    }
}

/// Scan every slab and reclaim slots no volume maps.
///
/// Both locks must be held by the caller for the whole call — that is what
/// makes the live set consistent with the slot tables it is compared against.
/// Take them in the same order as the rest of the engine: GEM, then registry.
pub async fn collect(
    gem: &GlobalExtentMap,
    registry: &mut SlabRegistry,
    opts: GcOptions,
) -> GcReport {
    let mut report = GcReport {
        dry_run: opts.dry_run,
        in_flight: registry.in_flight_count(),
        ..Default::default()
    };

    // Live set from the forward maps — see the module note on why the reverse
    // index is not usable here.
    let mut live: HashSet<(SlabId, u32)> = HashSet::new();
    for vid in gem.volume_ids() {
        if let Some(map) = gem.get_volume_map(&vid) {
            for loc in map.extents.values() {
                live.insert((loc.slab_id, loc.slot_idx));
            }
        }
    }

    // Pass 1: find, without mutating.
    for (&slab_id, slab) in registry.iter() {
        report.slabs_scanned += 1;
        let total = slab.total_slots();
        report.slots_scanned += total;

        for slot_idx in 0..total as u32 {
            let Some(slot) = slab.get_slot(slot_idx) else {
                continue;
            };
            if slot.state == SlotState::Free {
                continue;
            }
            if live.contains(&(slab_id, slot_idx)) {
                report.live += 1;
                continue;
            }
            if registry.is_reserved(slab_id, slot_idx) {
                continue;
            }
            report.orphans.push(Orphan {
                slab_id,
                slot_idx,
                volume_id: slot.volume_id,
                virtual_extent_idx: slot.virtual_extent_idx,
                ref_count: slot.ref_count,
            });
        }
    }

    if report.orphans.is_empty() {
        return report;
    }

    // Pass 2: decide what is actually eligible.
    let eligible: Vec<Orphan> = match &opts.confirm_against {
        Some(prev) => {
            let (confirmed, held): (Vec<_>, Vec<_>) = report
                .orphans
                .iter()
                .cloned()
                .partition(|o| prev.contains(&o.key()));
            report.unconfirmed = held.len();
            confirmed
        }
        None => report.orphans.clone(),
    };

    if opts.dry_run {
        tracing::info!(
            orphans = report.orphans.len(),
            eligible = eligible.len(),
            unconfirmed = report.unconfirmed,
            "extent gc (dry run) — nothing freed"
        );
        return report;
    }

    let limit = opts.max_reclaim.unwrap_or(usize::MAX);
    for orphan in eligible {
        if report.reclaimed >= limit {
            report.deferred += 1;
            continue;
        }
        let Some(slab) = registry.get_mut(&orphan.slab_id) else {
            continue;
        };
        let slot_size = slab.slot_size();
        match slab.free(orphan.slot_idx).await {
            Ok(()) => {
                report.reclaimed += 1;
                report.bytes_reclaimed += slot_size;
                tracing::info!(
                    slab = %orphan.slab_id,
                    slot = orphan.slot_idx,
                    stale_owner = %orphan.volume_id,
                    extent = orphan.virtual_extent_idx,
                    ref_count = orphan.ref_count,
                    "reclaimed orphaned extent"
                );
            }
            Err(e) => tracing::warn!(
                slab = %orphan.slab_id,
                slot = orphan.slot_idx,
                "could not reclaim orphaned extent: {e}"
            ),
        }
    }

    if report.reclaimed > 0 || report.deferred > 0 {
        tracing::info!(
            reclaimed = report.reclaimed,
            bytes = report.bytes_reclaimed,
            deferred = report.deferred,
            unconfirmed = report.unconfirmed,
            "extent gc complete"
        );
    }

    report
}

/// Counts from a pass, without the orphan list — cheap to keep around for
/// `GET /api/v1/slabs/gc` and the logs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GcSummary {
    pub slabs_scanned: usize,
    pub slots_scanned: u64,
    pub live: u64,
    pub in_flight: usize,
    pub orphans: usize,
    pub unconfirmed: usize,
    pub reclaimed: usize,
    pub bytes_reclaimed: u64,
    pub deferred: usize,
    pub dry_run: bool,
}

impl From<&GcReport> for GcSummary {
    fn from(r: &GcReport) -> Self {
        GcSummary {
            slabs_scanned: r.slabs_scanned,
            slots_scanned: r.slots_scanned,
            live: r.live,
            in_flight: r.in_flight,
            orphans: r.orphans.len(),
            unconfirmed: r.unconfirmed,
            reclaimed: r.reclaimed,
            bytes_reclaimed: r.bytes_reclaimed,
            deferred: r.deferred,
            dry_run: r.dry_run,
        }
    }
}

/// Run one pass, taking the locks in the engine's canonical order.
pub async fn run_once(
    gem: &std::sync::Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: &std::sync::Arc<tokio::sync::RwLock<SlabRegistry>>,
    opts: GcOptions,
) -> GcReport {
    let gem_guard = gem.read().await;
    let mut reg_guard = registry.write().await;
    collect(&gem_guard, &mut reg_guard, opts).await
}

/// Start the background collector.
///
/// Returns a handle holding the most recent summary, so the API can report
/// when the collector last ran and what it found.
pub fn spawn(
    gem: std::sync::Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: std::sync::Arc<tokio::sync::RwLock<SlabRegistry>>,
    cfg: crate::mgmt::config::GcConfig,
) -> std::sync::Arc<tokio::sync::RwLock<Option<GcSummary>>> {
    let last = std::sync::Arc::new(tokio::sync::RwLock::new(None));
    let out = last.clone();

    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(cfg.interval_secs.max(1));
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so the collector does not
        // scan before volume metadata has finished loading — every slot would
        // look unreferenced.
        ticker.tick().await;

        let mut prev: Option<HashSet<(SlabId, u32)>> = None;
        loop {
            ticker.tick().await;

            let opts = GcOptions {
                dry_run: cfg.dry_run,
                confirm_against: if cfg.confirm_passes {
                    Some(prev.clone().unwrap_or_default())
                } else {
                    None
                },
                max_reclaim: Some(cfg.max_reclaim_per_pass),
            };

            let report = run_once(&gem, &registry, opts).await;
            prev = Some(report.candidates());
            *last.write().await = Some(GcSummary::from(&report));
        }
    });

    tracing::info!(
        interval_secs = cfg.interval_secs,
        confirm_passes = cfg.confirm_passes,
        dry_run = cfg.dry_run,
        "extent gc worker started"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::drive::BlockDevice;
    use crate::placement::topology::StorageTier;
    use crate::volume::gem::ExtentLocation;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn make_registry(slot_size: u64) -> (SlabRegistry, SlabId, String) {
        let dir = std::env::temp_dir().join("stormblock-gc-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("gc-{}.bin", Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path_str);
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(&path_str, 32 * 1024 * 1024)
                .await
                .unwrap(),
        );
        let slab = Slab::format(dev, slot_size, StorageTier::Hot).await.unwrap();
        let id = slab.slab_id();
        let mut reg = SlabRegistry::new();
        reg.add(slab);
        (reg, id, path_str)
    }

    fn cleanup(p: &str) {
        let _ = std::fs::remove_file(p);
    }

    /// The leak from #37: slots allocated, owner gone, nothing mapping them.
    #[tokio::test]
    async fn reclaims_slots_no_volume_maps() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let ghost = VolumeId::new();

        let mut orphaned = Vec::new();
        for vext in 0..5u64 {
            let slab = reg.get_mut(&slab_id).unwrap();
            orphaned.push(slab.allocate(ghost, vext).await.unwrap());
        }
        // Nothing was ever recorded in the GEM, and nothing is in flight —
        // precisely the post-leak state.
        let gem = GlobalExtentMap::new();
        let free_before = reg.get(&slab_id).unwrap().free_slots();

        let report = collect(&gem, &mut reg, GcOptions::default()).await;

        assert_eq!(report.orphans.len(), 5);
        assert_eq!(report.reclaimed, 5);
        assert_eq!(report.bytes_reclaimed, 5 * 64 * 1024);
        assert_eq!(report.live, 0);
        assert_eq!(
            reg.get(&slab_id).unwrap().free_slots(),
            free_before + 5,
            "space came back"
        );
        cleanup(&path);
    }

    /// A slot shared by a clone has no reverse-index entry once the source is
    /// gone. Collecting on the reverse index would free it; collecting on the
    /// forward maps must not.
    #[tokio::test]
    async fn keeps_slots_a_clone_still_shares() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let source = VolumeId::new();
        let clone = VolumeId::new();

        let slot = reg
            .get_mut(&slab_id)
            .unwrap()
            .allocate(source, 0)
            .await
            .unwrap();

        let mut gem = GlobalExtentMap::new();
        gem.insert(
            source,
            0,
            ExtentLocation {
                slab_id,
                slot_idx: slot,
                ref_count: 1,
                generation: 1,
            },
        );
        gem.clone_volume_map(source, clone);
        // Delete the source: its reverse-index claim goes with it, but the
        // clone's forward mapping survives.
        gem.remove_volume(source);
        assert!(gem.reverse_lookup(slab_id, slot).is_none());

        let report = collect(&gem, &mut reg, GcOptions::default()).await;

        assert_eq!(report.reclaimed, 0, "clone's data must survive");
        assert_eq!(report.live, 1);
        assert!(report.orphans.is_empty());
        assert_eq!(
            reg.get(&slab_id).unwrap().get_slot(slot).unwrap().state,
            SlotState::Allocated
        );
        cleanup(&path);
    }

    /// A slot allocated but not yet mapped is a write in progress, not a leak.
    #[tokio::test]
    async fn skips_in_flight_allocations() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let vol = VolumeId::new();

        let slot = reg
            .get_mut(&slab_id)
            .unwrap()
            .allocate(vol, 0)
            .await
            .unwrap();
        reg.reserve(slab_id, slot);

        let gem = GlobalExtentMap::new();
        let report = collect(&gem, &mut reg, GcOptions::default()).await;

        assert_eq!(report.in_flight, 1);
        assert_eq!(report.reclaimed, 0, "an in-flight write must not be freed");
        assert!(report.orphans.is_empty());

        // Once mapped and committed, it is simply live.
        reg.commit(slab_id, slot);
        let mut gem2 = GlobalExtentMap::new();
        gem2.insert(
            vol,
            0,
            ExtentLocation {
                slab_id,
                slot_idx: slot,
                ref_count: 1,
                generation: 1,
            },
        );
        let report = collect(&gem2, &mut reg, GcOptions::default()).await;
        assert_eq!(report.reclaimed, 0);
        assert_eq!(report.live, 1);
        cleanup(&path);
    }

    #[tokio::test]
    async fn dry_run_reports_without_freeing() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let ghost = VolumeId::new();
        for vext in 0..3u64 {
            reg.get_mut(&slab_id)
                .unwrap()
                .allocate(ghost, vext)
                .await
                .unwrap();
        }
        let free_before = reg.get(&slab_id).unwrap().free_slots();
        let gem = GlobalExtentMap::new();

        let report = collect(
            &gem,
            &mut reg,
            GcOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await;

        assert_eq!(report.orphans.len(), 3);
        assert_eq!(report.reclaimed, 0);
        assert_eq!(reg.get(&slab_id).unwrap().free_slots(), free_before);
        cleanup(&path);
    }

    /// Two-pass mode frees nothing it has not seen orphaned before.
    #[tokio::test]
    async fn two_pass_confirmation_defers_first_sighting() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let ghost = VolumeId::new();
        for vext in 0..4u64 {
            reg.get_mut(&slab_id)
                .unwrap()
                .allocate(ghost, vext)
                .await
                .unwrap();
        }
        let gem = GlobalExtentMap::new();

        let first = collect(
            &gem,
            &mut reg,
            GcOptions {
                confirm_against: Some(HashSet::new()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(first.reclaimed, 0, "nothing confirmed yet");
        assert_eq!(first.unconfirmed, 4);

        let second = collect(
            &gem,
            &mut reg,
            GcOptions {
                confirm_against: Some(first.candidates()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(second.reclaimed, 4, "confirmed by the second pass");
        cleanup(&path);
    }

    #[tokio::test]
    async fn max_reclaim_bounds_one_pass() {
        let (mut reg, slab_id, path) = make_registry(64 * 1024).await;
        let ghost = VolumeId::new();
        for vext in 0..10u64 {
            reg.get_mut(&slab_id)
                .unwrap()
                .allocate(ghost, vext)
                .await
                .unwrap();
        }
        let gem = GlobalExtentMap::new();

        let report = collect(
            &gem,
            &mut reg,
            GcOptions {
                max_reclaim: Some(4),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(report.reclaimed, 4);
        assert_eq!(report.deferred, 6);
        cleanup(&path);
    }
}
