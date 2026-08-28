//! Placement engine — extent-level data placement across storage domains.
//!
//! The placement system manages where data lives. Instead of static RAID arrays
//! where volumes are bound to a fixed set of drives, the placement engine
//! treats each extent independently: data flows toward compute (attraction),
//! replicates for safety, tiers by temperature, and adjusts as devices come
//! and go.
//!
//! # Foundation: Snapshot-Fenced Cold Copies
//!
//! The correctness guarantee starts with cold copies. A cold copy converges to
//! a specific **snapshot** — not to live data. Every extent in the cold copy
//! matches the same point in time. This is the invariant that makes everything
//! else safe: you can't have an inconsistent replica.
//!
//! # Data Flow Model
//!
//! ```text
//! Remote iSCSI ──attraction──▶ Local NVMe (hot, close to CPU)
//!                                    │
//!                              safety replication
//!                                    │
//!                                    ▼
//!                              Local SAS (warm)
//!                                    │
//!                               cold backup
//!                                    │
//!                                    ▼
//!                              Remote archive (cold)
//! ```
//!
//! Each flow is a snapshot-fenced replication stream. The live volume is
//! unaware — all I/O goes through the BlockDevice trait (via ublk), and
//! the placement engine handles replication in the background.

pub mod domain;
pub mod topology;
pub mod cold;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::volume::extent::VolumeId;
use crate::placement::domain::{FailureDomain, DEFAULT_RUNG};
use crate::volume::gem::{GlobalExtentMap, Leg, ParityGroup, parity_vext};

/// What a parity slot records as its virtual extent.
fn parity_vext_for(g: &ParityGroup, leg: Leg) -> u64 {
    let i = g.legs.iter().position(|l| *l == leg).unwrap_or(0);
    // The stripe is not carried on the group; the caller passes it through
    // `vext_idx` when it has it. Here we only need the leg index.
    parity_vext(i as u8, 0)
}
use crate::volume::snapshot::snapshot_diff;
use crate::volume::thin::{ThinVolumeHandle, PlacementPolicy};

pub use topology::{StorageTier, Locality, StorageDevice};
pub use cold::{ColdCopy, ReplicationResult, ReplicationError};

/// Errors during extent placement operations.
#[derive(Debug)]
pub enum PlacementError {
    SlabFull,
    ExtentNotFound { volume_id: VolumeId, vext_idx: u64 },
    SlabNotFound(SlabId),
    NoDestination,
    ReadFailed { slab_id: SlabId, slot_idx: u32, error: String },
    WriteFailed { slab_id: SlabId, slot_idx: u32, error: String },
    Other(String),
    /// The destination would put two legs of one extent on one domain.
    DomainCollision(SlabId),
}

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlacementError::SlabFull => write!(f, "destination slab is full"),
            PlacementError::ExtentNotFound { volume_id, vext_idx } => {
                write!(f, "extent not found: volume {volume_id} vext {vext_idx}")
            }
            PlacementError::SlabNotFound(id) => write!(f, "slab {id} not found"),
            PlacementError::NoDestination => write!(f, "no suitable destination slab"),
            PlacementError::DomainCollision(id) => write!(
                f, "slab {id} shares a failure domain with another leg of the extent"
            ),
            PlacementError::ReadFailed { slab_id, slot_idx, error } => {
                write!(f, "read failed: slab {slab_id} slot {slot_idx}: {error}")
            }
            PlacementError::WriteFailed { slab_id, slot_idx, error } => {
                write!(f, "write failed: slab {slab_id} slot {slot_idx}: {error}")
            }
            PlacementError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PlacementError {}

/// Result of migrating a single extent.
pub struct MigrateExtentResult {
    pub volume_id: VolumeId,
    pub vext_idx: u64,
    pub source_slab: SlabId,
    pub dest_slab: SlabId,
    pub dest_slot: u32,
}

/// Result of evacuating all extents from a slab.
pub struct EvacuateResult {
    pub slab_id: SlabId,
    pub migrated: u64,
    pub failed: u64,
}

/// Strategy for rebalancing extents across slabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceStrategy {
    /// Move extents from overloaded slabs to underloaded ones.
    EvenDistribution,
    /// Move extents to their preferred tier.
    TierAffinity,
    /// Spread by failure domain at a rung (#71): first separate legs of one
    /// extent or stripe that share a domain (placed before the labels
    /// existed), then even out allocation across the domains — what a node
    /// does after a shelf is added.
    ByFailureDomain { rung: String },
}

/// Result of a rebalance operation.
pub struct RebalanceResult {
    pub moved: u64,
    pub skipped: u64,
    pub failed: u64,
}

/// The placement engine — manages cold copies and replication streams.
///
/// Currently focused on snapshot-fenced cold copies. Future iterations
/// will add: device attraction (hot migration toward compute), organic
/// RAID level transitions, and tiered data flow.
pub struct PlacementEngine {
    /// Available storage devices, indexed by ID.
    devices: HashMap<Uuid, StorageDevice>,
    /// Cold copies, indexed by cold copy ID.
    cold_copies: HashMap<Uuid, ColdCopy>,
    /// Volume → list of cold copy IDs.
    volume_copies: HashMap<VolumeId, Vec<Uuid>>,
}

impl PlacementEngine {
    /// Create a new placement engine.
    pub fn new() -> Self {
        PlacementEngine {
            devices: HashMap::new(),
            cold_copies: HashMap::new(),
            volume_copies: HashMap::new(),
        }
    }

    /// Register a storage device with the placement engine.
    pub fn add_device(&mut self, device: StorageDevice) -> Uuid {
        let id = device.id;
        tracing::info!("placement: registered device {}", device);
        self.devices.insert(id, device);
        id
    }

    /// Remove a storage device.
    pub fn remove_device(&mut self, id: Uuid) -> Option<StorageDevice> {
        let dev = self.devices.remove(&id);
        if let Some(ref d) = dev {
            tracing::info!("placement: removed device {}", d.name);
        }
        dev
    }

    /// List all registered devices.
    pub fn devices(&self) -> impl Iterator<Item = &StorageDevice> {
        self.devices.values()
    }

    /// Create a cold copy of a volume, targeting a snapshot.
    ///
    /// The cold copy will converge to the given snapshot once `replicate()`
    /// is called. The target device must have enough capacity for the volume.
    #[allow(clippy::too_many_arguments)]
    pub fn create_cold_copy(
        &mut self,
        volume_id: VolumeId,
        target: Arc<dyn BlockDevice>,
        target_snapshot: VolumeId,
        total_extents: u64,
        extent_size: u64,
        tier: StorageTier,
        locality: Locality,
    ) -> Uuid {
        let cold = ColdCopy::new(
            volume_id,
            target,
            target_snapshot,
            total_extents,
            extent_size,
            tier,
            locality,
        );
        let id = cold.id();
        tracing::info!(
            "placement: created cold copy {} for volume {} → snapshot {}",
            id, volume_id, target_snapshot,
        );
        self.volume_copies
            .entry(volume_id)
            .or_default()
            .push(id);
        self.cold_copies.insert(id, cold);
        id
    }

    /// Get a cold copy by ID.
    pub fn get_cold_copy(&self, id: &Uuid) -> Option<&ColdCopy> {
        self.cold_copies.get(id)
    }

    /// Get a mutable cold copy by ID.
    pub fn get_cold_copy_mut(&mut self, id: &Uuid) -> Option<&mut ColdCopy> {
        self.cold_copies.get_mut(id)
    }

    /// List cold copies for a volume.
    pub fn volume_cold_copies(&self, volume_id: &VolumeId) -> Vec<&ColdCopy> {
        self.volume_copies
            .get(volume_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.cold_copies.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replicate a cold copy from its target snapshot.
    ///
    /// Reads unsynced extents from the snapshot and writes them to the
    /// cold copy's target device. Returns when all extents are synced
    /// or the shutdown signal fires.
    pub async fn replicate(
        &mut self,
        cold_copy_id: &Uuid,
        snapshot: &dyn BlockDevice,
        rate_limit: Option<u64>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<ReplicationResult, ReplicationError> {
        let cold = self.cold_copies.get_mut(cold_copy_id)
            .ok_or_else(|| ReplicationError::ReadFailed {
                extent_idx: 0,
                error: "cold copy not found".into(),
            })?;

        cold::replicate(snapshot, cold, rate_limit, shutdown).await
    }

    /// Advance a cold copy to a new snapshot using incremental diff.
    ///
    /// Computes which extents changed between the old and new snapshot,
    /// marks only those as needing re-sync, then replicates just the delta.
    pub async fn advance_cold_copy(
        &mut self,
        cold_copy_id: &Uuid,
        old_snapshot: &ThinVolumeHandle,
        new_snapshot: &ThinVolumeHandle,
        new_snapshot_device: &dyn BlockDevice,
        rate_limit: Option<u64>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<ReplicationResult, ReplicationError> {
        let old_id = old_snapshot.volume_id();
        let new_snap_id = new_snapshot.volume_id();

        // Compute diff via GEM
        let changed = {
            let gem = new_snapshot.gem().read().await;
            snapshot_diff(&gem, old_id, new_snap_id)
        };

        tracing::info!(
            "placement: advancing cold copy {}: {} extents changed",
            cold_copy_id, changed.len(),
        );

        // Advance state
        let cold = self.cold_copies.get_mut(cold_copy_id)
            .ok_or_else(|| ReplicationError::ReadFailed {
                extent_idx: 0,
                error: "cold copy not found".into(),
            })?;
        cold.advance_to(new_snap_id, &changed);

        // Replicate only the delta
        cold::replicate(new_snapshot_device, cold, rate_limit, shutdown).await
    }

    /// Remove a cold copy.
    pub fn remove_cold_copy(&mut self, id: &Uuid) -> Option<ColdCopy> {
        if let Some(cold) = self.cold_copies.remove(id) {
            if let Some(ids) = self.volume_copies.get_mut(&cold.volume_id()) {
                ids.retain(|cid| cid != id);
            }
            Some(cold)
        } else {
            None
        }
    }

    // ── Extent-level placement operations ──────────────────────────────

    /// Migrate a single extent from one slab to another.
    ///
    /// Moves the extent's **primary** leg; `migrate_leg` moves a named one.
    /// If `dest_slab_id` is None, picks the best slab for the given tier
    /// (excluding the source and the domains of the extent's other legs).
    pub async fn migrate_extent(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        vext_idx: u64,
        dest_slab_id: Option<SlabId>,
    ) -> Result<MigrateExtentResult, PlacementError> {
        let from = gem.lookup(volume_id, vext_idx)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx })?
            .slab_id;
        self.migrate_leg(gem, registry, volume_id, vext_idx, from, dest_slab_id).await
    }

    /// Move the leg of `(volume, extent)` that lives on `from_slab` to another
    /// slab, keeping every other leg where it is.
    ///
    /// Reads the slot, allocates a slot in the destination, writes the data,
    /// then rewrites **every** map that named the old slot — a golden and its
    /// clones share the physical slot, and all of them must follow — and
    /// frees the old slot outright, since nothing references it any more.
    /// The destination is kept off the domains of the extent's other legs, so
    /// a move never collapses a mirror onto one drive.
    pub async fn migrate_leg(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        vext_idx: u64,
        from_slab: SlabId,
        dest_slab_id: Option<SlabId>,
    ) -> Result<MigrateExtentResult, PlacementError> {
        let loc = gem.lookup(volume_id, vext_idx)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx })?
            .clone();
        let old = loc.leg_on(from_slab)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx })?;
        let others: Vec<FailureDomain> = loc
            .legs()
            .filter(|l| *l != old)
            .map(|l| registry.domain_of(&l.slab_id))
            .collect();
        let new = self
            .move_slot(gem, registry, volume_id, vext_idx, old, loc.ref_count, loc.generation, &others, dest_slab_id)
            .await?;
        Ok(MigrateExtentResult {
            volume_id,
            vext_idx,
            source_slab: old.slab_id,
            dest_slab: new.slab_id,
            dest_slot: new.slot_idx,
        })
    }

    /// Move one parity leg of a stripe off `from_slab`.
    pub async fn migrate_parity_leg(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        stripe: u64,
        from_slab: SlabId,
        dest_slab_id: Option<SlabId>,
    ) -> Result<MigrateExtentResult, PlacementError> {
        let g = gem.lookup_parity(volume_id, stripe)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx: stripe })?
            .clone();
        let old = g.legs.iter().copied().find(|l| l.slab_id == from_slab)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx: stripe })?;
        let mut others: Vec<FailureDomain> = g
            .legs
            .iter()
            .filter(|l| **l != old)
            .map(|l| registry.domain_of(&l.slab_id))
            .collect();
        // Keep parity off the stripe's data members too.
        for vext in g.members(stripe) {
            if let Some(l) = gem.lookup(volume_id, vext) {
                for leg in l.legs() {
                    others.push(registry.domain_of(&leg.slab_id));
                }
            }
        }
        let new = self
            .move_slot(gem, registry, volume_id, parity_vext_for(&g, old), old, g.ref_count, g.generation, &others, dest_slab_id)
            .await?;
        Ok(MigrateExtentResult {
            volume_id,
            vext_idx: stripe,
            source_slab: old.slab_id,
            dest_slab: new.slab_id,
            dest_slot: new.slot_idx,
        })
    }

    /// The physical move behind `migrate_leg` and `migrate_parity_leg`.
    /// `vext_idx` is what the destination slot records as its extent — a
    /// parity leg records its tagged index.
    #[allow(clippy::too_many_arguments)]
    async fn move_slot(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        vext_idx: u64,
        old: Leg,
        ref_count: u32,
        generation: u64,
        keep_apart_from: &[FailureDomain],
        dest_slab_id: Option<SlabId>,
    ) -> Result<Leg, PlacementError> {
        let source_slab_id = old.slab_id;
        let source_slot_idx = old.slot_idx;

        // Read data from source slot
        let slot_size = registry.get(&source_slab_id)
            .ok_or(PlacementError::SlabNotFound(source_slab_id))?
            .slot_size();
        let mut data = vec![0u8; slot_size as usize];

        registry.get(&source_slab_id)
            .ok_or(PlacementError::SlabNotFound(source_slab_id))?
            .read_slot(source_slot_idx, 0, &mut data)
            .await
            .map_err(|e| PlacementError::ReadFailed {
                slab_id: source_slab_id,
                slot_idx: source_slot_idx,
                error: e.to_string(),
            })?;

        // Pick destination slab
        let dest_id = match dest_slab_id {
            Some(id) => {
                // Verify it exists and has space
                let slab = registry.get(&id)
                    .ok_or(PlacementError::SlabNotFound(id))?;
                if slab.free_slots() == 0 {
                    return Err(PlacementError::SlabFull);
                }
                if registry.collides(&id, keep_apart_from, DEFAULT_RUNG) {
                    return Err(PlacementError::DomainCollision(id));
                }
                if registry.is_quarantined(&id) {
                    return Err(PlacementError::NoDestination);
                }
                id
            }
            None => {
                // Pick best slab on the same tier, excluding source
                let source_tier = registry.get(&source_slab_id)
                    .ok_or(PlacementError::SlabNotFound(source_slab_id))?
                    .tier();
                self.best_slab_excluding(registry, source_tier, source_slab_id, keep_apart_from)?
            }
        };

        // Allocate slot in destination slab, recorded as the same extent so
        // the slot-table fallback agrees with the map.
        let dest_slot = registry.get_mut(&dest_id)
            .ok_or(PlacementError::SlabNotFound(dest_id))?
            .allocate_gen(volume_id, vext_idx, generation)
            .await
            .map_err(|_| PlacementError::SlabFull)?;

        // Write data to destination slot
        registry.get(&dest_id)
            .ok_or(PlacementError::SlabNotFound(dest_id))?
            .write_slot(dest_slot, 0, &data)
            .await
            .map_err(|e| PlacementError::WriteFailed {
                slab_id: dest_id,
                slot_idx: dest_slot,
                error: e.to_string(),
            })?;

        // A shared slot stays shared: the new slot carries the same count.
        if ref_count > 1 {
            if let Some(slab) = registry.get_mut(&dest_id) {
                for _ in 1..ref_count {
                    if let Err(e) = slab.inc_ref(dest_slot).await {
                        tracing::warn!(
                            volume = %volume_id, slab = %dest_id, slot = dest_slot,
                            "could not carry the share count to the moved slot: {e}"
                        );
                        break;
                    }
                }
            }
        }

        // Every map that named the old slot now names the new one.
        let new = Leg::new(dest_id, dest_slot);
        let mut moves = HashMap::new();
        moves.insert(old, new);
        gem.rewrite_legs(&moves);

        // Nothing references the source slot any more: free it outright.
        if let Some(slab) = registry.get_mut(&source_slab_id) {
            if let Err(e) = slab.free(source_slot_idx).await {
                // The extent now lives on the destination, so this only
                // strands the source copy — but that is still lost capacity.
                tracing::warn!(
                    volume = %volume_id, slab = %source_slab_id, slot = source_slot_idx,
                    "migration could not release the source extent: {e}"
                );
            }
        }

        Ok(new)
    }

    /// Evacuate all extents from a slab, moving them to other available slabs.
    ///
    /// Iterates the GEM's reverse index for all extents on the target slab.
    /// For each extent, calls `migrate_extent()` to move it elsewhere.
    /// Respects shutdown signal. Returns count of migrated/failed.
    pub async fn evacuate_slab(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        slab_id: SlabId,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<EvacuateResult, PlacementError> {
        // Verify slab exists
        if registry.get(&slab_id).is_none() {
            return Err(PlacementError::SlabNotFound(slab_id));
        }

        let mut migrated = 0u64;
        let mut failed = 0u64;

        loop {
            // Check shutdown
            if *shutdown.borrow() {
                tracing::info!("placement: evacuate_slab interrupted by shutdown");
                break;
            }

            // Re-collect remaining extents each iteration (GEM changes as we migrate)
            let extents = gem.slab_extents(slab_id);
            if let Some((vol_id, vext_idx, _loc)) = extents.first() {
                match self.migrate_leg(gem, registry, *vol_id, *vext_idx, slab_id, None).await {
                    Ok(_) => migrated += 1,
                    Err(e) => {
                        tracing::warn!(
                            "placement: failed to evacuate vol={} vext={}: {}",
                            vol_id, vext_idx, e
                        );
                        failed += 1;
                        // Skip this extent to avoid infinite loop
                        break;
                    }
                }
                continue;
            }

            let parity = gem.slab_parity(slab_id);
            let Some((vol_id, stripe, _)) = parity.first() else { break };
            match self.migrate_parity_leg(gem, registry, *vol_id, *stripe, slab_id, None).await {
                Ok(_) => migrated += 1,
                Err(e) => {
                    tracing::warn!(
                        "placement: failed to evacuate parity vol={} stripe={}: {}",
                        vol_id, stripe, e
                    );
                    failed += 1;
                    break;
                }
            }
        }

        Ok(EvacuateResult {
            slab_id,
            migrated,
            failed,
        })
    }

    /// Rebalance extents across slabs.
    ///
    /// Two strategies:
    /// - `EvenDistribution`: Move extents from overloaded slabs to underloaded ones.
    /// - `TierAffinity`: Move extents to their preferred tier based on volume placement policy.
    pub async fn rebalance(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        strategy: RebalanceStrategy,
        volume_policies: &HashMap<VolumeId, PlacementPolicy>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<RebalanceResult, PlacementError> {
        match strategy {
            RebalanceStrategy::EvenDistribution => {
                self.rebalance_even(gem, registry, shutdown).await
            }
            RebalanceStrategy::TierAffinity => {
                self.rebalance_tier_affinity(gem, registry, volume_policies, shutdown).await
            }
            RebalanceStrategy::ByFailureDomain { rung } => {
                self.rebalance_by_domain(gem, registry, &rung, shutdown).await
            }
        }
    }

    /// Pass 1: legs of one extent (or members and parity of one stripe) that
    /// collide at `rung` are separated. Pass 2: allocation is evened out
    /// across the domains at `rung`.
    async fn rebalance_by_domain(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        rung: &str,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<RebalanceResult, PlacementError> {
        let mut moved = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;

        // Pass 1 — collisions.
        let mut work: Vec<(VolumeId, u64, Leg, bool)> = Vec::new();
        for vol in gem.volume_ids() {
            let Some(map) = gem.get_volume_map(&vol) else { continue };
            for (vext, loc) in &map.extents {
                if loc.mirrors.is_empty() {
                    continue;
                }
                let mut seen: Vec<FailureDomain> = Vec::new();
                for leg in loc.legs() {
                    let d = registry.domain_of(&leg.slab_id);
                    if !d.is_empty() && seen.iter().any(|t| t.same_at(&d, rung)) {
                        work.push((vol, *vext, leg, false));
                    } else {
                        seen.push(d);
                    }
                }
            }
            for (stripe, g) in &map.parity {
                let mut seen: Vec<FailureDomain> = Vec::new();
                for vext in g.members(*stripe) {
                    if let Some(l) = map.extents.get(&vext) {
                        for leg in l.legs() {
                            let d = registry.domain_of(&leg.slab_id);
                            if !d.is_empty() && seen.iter().any(|t| t.same_at(&d, rung)) {
                                work.push((vol, vext, leg, false));
                            } else {
                                seen.push(d);
                            }
                        }
                    }
                }
                for leg in &g.legs {
                    let d = registry.domain_of(&leg.slab_id);
                    if !d.is_empty() && seen.iter().any(|t| t.same_at(&d, rung)) {
                        work.push((vol, *stripe, *leg, true));
                    } else {
                        seen.push(d);
                    }
                }
            }
        }
        for (vol, idx, leg, is_parity) in work {
            if *shutdown.borrow() {
                break;
            }
            // The domain the leg must leave and the ones it must stay off:
            // everything else the extent or stripe already sits on.
            let res = if is_parity {
                self.migrate_parity_leg_at(gem, registry, vol, idx, leg.slab_id, rung).await
            } else {
                self.migrate_leg_at(gem, registry, vol, idx, leg.slab_id, rung).await
            };
            match res {
                Ok(_) => moved += 1,
                Err(PlacementError::NoDestination) | Err(PlacementError::SlabFull) => skipped += 1,
                Err(_) => failed += 1,
            }
        }

        // Pass 2 — even out across domains.
        loop {
            if *shutdown.borrow() {
                break;
            }
            // Per domain key at the rung: (allocated, total, slabs).
            let mut domains: Vec<(FailureDomain, u64, u64, Vec<SlabId>)> = Vec::new();
            for (&id, slab) in registry.iter() {
                let d = registry.domain_of(&id);
                if d.is_empty() {
                    continue;
                }
                match domains.iter_mut().find(|(k, _, _, _)| k.same_at(&d, rung)) {
                    Some(e) => {
                        e.1 += slab.allocated_slots();
                        e.2 += slab.total_slots();
                        e.3.push(id);
                    }
                    None => domains.push((d, slab.allocated_slots(), slab.total_slots(), vec![id])),
                }
            }
            if domains.len() < 2 {
                break;
            }
            let total_alloc: u64 = domains.iter().map(|d| d.1).sum();
            let total_cap: u64 = domains.iter().map(|d| d.2).sum();
            if total_cap == 0 {
                break;
            }
            let avg = total_alloc * 1000 / total_cap;
            let Some(over) = domains
                .iter()
                .filter(|d| d.2 > 0 && d.1 * 1000 / d.2 > avg + 50)
                .max_by_key(|d| d.1 * 1000 / d.2)
            else {
                break;
            };
            let Some(under) = domains
                .iter()
                .filter(|d| d.2 > 0 && d.1 * 1000 / d.2 < avg)
                .filter(|d| d.3.iter().any(|s| registry.get(s).map(|x| x.free_slots() > 0).unwrap_or(false)))
                .min_by_key(|d| d.1 * 1000 / d.2)
            else {
                break;
            };
            // Any leg on any slab of the overloaded domain, that can go to
            // the underloaded one without colliding with its extent's others.
            let mut candidate: Option<(VolumeId, u64, SlabId, bool)> = None;
            'find: for s in &over.3 {
                for (vol, vext, _) in gem.slab_extents(*s) {
                    candidate = Some((vol, vext, *s, false));
                    break 'find;
                }
                for (vol, stripe, _) in gem.slab_parity(*s) {
                    candidate = Some((vol, stripe, *s, true));
                    break 'find;
                }
            }
            let Some((vol, idx, from, is_parity)) = candidate else { break };
            let dest = under
                .3
                .iter()
                .copied()
                .filter(|s| registry.get(s).map(|x| x.free_slots() > 0).unwrap_or(false))
                .max_by_key(|s| registry.get(s).map(|x| x.free_slots()).unwrap_or(0));
            let Some(dest) = dest else { break };
            let res = if is_parity {
                self.migrate_parity_leg(gem, registry, vol, idx, from, Some(dest)).await
            } else {
                self.migrate_leg(gem, registry, vol, idx, from, Some(dest)).await
            };
            match res {
                Ok(_) => moved += 1,
                Err(PlacementError::DomainCollision(_)) | Err(PlacementError::SlabFull) => {
                    skipped += 1;
                    break;
                }
                Err(_) => {
                    failed += 1;
                    break;
                }
            }
        }

        Ok(RebalanceResult { moved, skipped, failed })
    }

    /// `migrate_leg` with the domain comparison at `rung` rather than the
    /// default, so a collision at `shelf` is resolved by a slab on another
    /// shelf.
    async fn migrate_leg_at(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        vext_idx: u64,
        from_slab: SlabId,
        rung: &str,
    ) -> Result<MigrateExtentResult, PlacementError> {
        let loc = gem.lookup(volume_id, vext_idx)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx })?
            .clone();
        let old = loc.leg_on(from_slab)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx })?;
        let others: Vec<FailureDomain> = loc
            .legs()
            .filter(|l| *l != old)
            .map(|l| registry.domain_of(&l.slab_id))
            .collect();
        let tier = registry.get(&from_slab).ok_or(PlacementError::SlabNotFound(from_slab))?.tier();
        let dest = self.best_slab_apart_at(registry, tier, from_slab, &others, rung)?;
        let new = self
            .move_slot(gem, registry, volume_id, vext_idx, old, loc.ref_count, loc.generation, &others, Some(dest))
            .await?;
        Ok(MigrateExtentResult { volume_id, vext_idx, source_slab: old.slab_id, dest_slab: new.slab_id, dest_slot: new.slot_idx })
    }

    async fn migrate_parity_leg_at(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_id: VolumeId,
        stripe: u64,
        from_slab: SlabId,
        rung: &str,
    ) -> Result<MigrateExtentResult, PlacementError> {
        let g = gem.lookup_parity(volume_id, stripe)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx: stripe })?
            .clone();
        let old = g.legs.iter().copied().find(|l| l.slab_id == from_slab)
            .ok_or(PlacementError::ExtentNotFound { volume_id, vext_idx: stripe })?;
        let mut others: Vec<FailureDomain> = g.legs.iter().filter(|l| **l != old).map(|l| registry.domain_of(&l.slab_id)).collect();
        for vext in g.members(stripe) {
            if let Some(l) = gem.lookup(volume_id, vext) {
                others.extend(l.legs().map(|leg| registry.domain_of(&leg.slab_id)));
            }
        }
        let tier = registry.get(&from_slab).ok_or(PlacementError::SlabNotFound(from_slab))?.tier();
        let dest = self.best_slab_apart_at(registry, tier, from_slab, &others, rung)?;
        let new = self
            .move_slot(gem, registry, volume_id, parity_vext_for(&g, old), old, g.ref_count, g.generation, &others, Some(dest))
            .await?;
        Ok(MigrateExtentResult { volume_id, vext_idx: stripe, source_slab: old.slab_id, dest_slab: new.slab_id, dest_slot: new.slot_idx })
    }

    fn best_slab_apart_at(
        &self,
        registry: &SlabRegistry,
        tier: StorageTier,
        exclude: SlabId,
        keep_apart_from: &[FailureDomain],
        rung: &str,
    ) -> Result<SlabId, PlacementError> {
        let pick = |ids: Vec<SlabId>| -> Option<SlabId> {
            ids.into_iter()
                .filter(|id| *id != exclude && !registry.is_quarantined(id))
                .filter(|id| !registry.collides(id, keep_apart_from, rung))
                .filter_map(|id| registry.get(&id).map(|s| (id, s.free_slots())))
                .filter(|(_, f)| *f > 0)
                .max_by_key(|(_, f)| *f)
                .map(|(id, _)| id)
        };
        if let Some(id) = pick(registry.by_tier(tier).to_vec()) {
            return Ok(id);
        }
        pick(registry.iter().map(|(id, _)| *id).collect()).ok_or(PlacementError::NoDestination)
    }

    /// Even distribution: move extents from slabs with usage above average to slabs below average.
    async fn rebalance_even(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<RebalanceResult, PlacementError> {
        let mut moved = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;

        loop {
            if *shutdown.borrow() {
                break;
            }

            // Calculate average usage fraction across all slabs
            let slab_stats: Vec<(SlabId, u64, u64)> = registry.iter()
                .map(|(&id, slab)| (id, slab.allocated_slots(), slab.total_slots()))
                .collect();

            if slab_stats.is_empty() || slab_stats.len() < 2 {
                break;
            }

            let total_allocated: u64 = slab_stats.iter().map(|(_, a, _)| a).sum();
            let total_capacity: u64 = slab_stats.iter().map(|(_, _, t)| t).sum();

            if total_capacity == 0 {
                break;
            }

            // Average usage ratio (as parts per 1000 for integer math)
            let avg_ratio_ppm = (total_allocated * 1000) / total_capacity;

            // Find the most overloaded slab (highest usage ratio)
            let overloaded = slab_stats.iter()
                .filter(|(_, alloc, total)| {
                    if *total == 0 { return false; }
                    let ratio = (*alloc * 1000) / *total;
                    // Only consider if significantly above average (>5% margin)
                    ratio > avg_ratio_ppm + 50
                })
                .max_by_key(|(_, alloc, total)| (*alloc * 1000) / *total);

            let overloaded_id = match overloaded {
                Some((id, _, _)) => *id,
                None => break, // No slab significantly above average
            };

            // Find an underloaded slab to receive the extent
            let underloaded = slab_stats.iter()
                .filter(|(id, alloc, total)| {
                    if *total == 0 || *id == overloaded_id { return false; }
                    let ratio = (*alloc * 1000) / *total;
                    ratio < avg_ratio_ppm && registry.get(id).map(|s| s.free_slots() > 0).unwrap_or(false)
                })
                .min_by_key(|(_, alloc, total)| (*alloc * 1000) / *total);

            let underloaded_id = match underloaded {
                Some((id, _, _)) => *id,
                None => break,
            };

            // Pick one extent from overloaded slab
            let extents = gem.slab_extents(overloaded_id);
            if extents.is_empty() {
                break;
            }

            let (vol_id, vext_idx, _) = &extents[0];

            match self.migrate_extent(gem, registry, *vol_id, *vext_idx, Some(underloaded_id)).await {
                Ok(_) => moved += 1,
                Err(PlacementError::SlabFull) => {
                    skipped += 1;
                    break;
                }
                Err(_) => {
                    failed += 1;
                    break;
                }
            }
        }

        Ok(RebalanceResult { moved, skipped, failed })
    }

    /// Tier affinity: move extents to their volume's preferred tier when possible.
    async fn rebalance_tier_affinity(
        &self,
        gem: &mut GlobalExtentMap,
        registry: &mut SlabRegistry,
        volume_policies: &HashMap<VolumeId, PlacementPolicy>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<RebalanceResult, PlacementError> {
        let mut moved = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;

        // Collect all extents that are on the wrong tier
        let misplaced: Vec<(VolumeId, u64, SlabId, StorageTier)> = {
            let mut result = Vec::new();
            for (&vol_id, policy) in volume_policies {
                if let Some(iter) = gem.volume_extents(&vol_id) {
                    for (&vext_idx, loc) in iter {
                        let current_tier = registry.get(&loc.slab_id)
                            .map(|s| s.tier());
                        if let Some(tier) = current_tier {
                            if tier != policy.preferred_tier {
                                result.push((vol_id, vext_idx, loc.slab_id, policy.preferred_tier));
                            }
                        }
                    }
                }
            }
            result
        };

        for (vol_id, vext_idx, _source_slab, preferred_tier) in misplaced {
            if *shutdown.borrow() {
                break;
            }

            // Find a slab on the preferred tier with space
            let dest = registry.best_slab_for_tier(preferred_tier);
            match dest {
                Some(dest_id) => {
                    match self.migrate_extent(gem, registry, vol_id, vext_idx, Some(dest_id)).await {
                        Ok(_) => moved += 1,
                        Err(PlacementError::SlabFull) => skipped += 1,
                        Err(_) => failed += 1,
                    }
                }
                None => skipped += 1,
            }
        }

        Ok(RebalanceResult { moved, skipped, failed })
    }

    /// Find the best slab on a given tier, excluding a specific slab.
    fn best_slab_excluding(
        &self,
        registry: &SlabRegistry,
        tier: StorageTier,
        exclude: SlabId,
        keep_apart_from: &[FailureDomain],
    ) -> Result<SlabId, PlacementError> {
        // First try same tier
        let candidates: Vec<(SlabId, u64)> = registry.by_tier(tier)
            .iter()
            .filter(|&&id| id != exclude && !registry.is_quarantined(&id))
            .filter(|&id| !registry.collides(id, keep_apart_from, DEFAULT_RUNG))
            .filter_map(|id| {
                registry.get(id).and_then(|s| {
                    if s.free_slots() > 0 { Some((*id, s.free_slots())) } else { None }
                })
            })
            .collect();

        if let Some((id, _)) = candidates.iter().max_by_key(|(_, free)| *free) {
            return Ok(*id);
        }

        // Fallback: any tier with space, excluding source
        for (id, slab) in registry.iter() {
            if *id != exclude
                && !registry.is_quarantined(id)
                && slab.free_slots() > 0
                && !registry.collides(id, keep_apart_from, DEFAULT_RUNG)
            {
                return Ok(*id);
            }
        }

        Err(PlacementError::NoDestination)
    }
}

impl Default for PlacementEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::gem::ExtentLocation;
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::drive::slab::Slab;
    use crate::drive::slab_registry::SlabRegistry;
    use crate::raid::{RaidArray, RaidLevel};
    use crate::volume::gem::GlobalExtentMap;
    use crate::volume::snapshot::create_snapshot;
    use crate::volume::thin::{ThinVolume, PlacementPolicy};

    async fn setup_test_env(
        slot_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Arc<tokio::sync::RwLock<GlobalExtentMap>>, Arc<tokio::sync::RwLock<SlabRegistry>>, PlacementEngine, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-placement-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
        let mut paths = Vec::new();
        for i in 0..2 {
            let path = dir.join(format!("{test_id}-member-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }

        let array = RaidArray::create(RaidLevel::Raid1, devices, None)
            .await
            .unwrap();
        let backing: Arc<dyn BlockDevice> = Arc::new(array);

        let slab = Slab::format(backing, slot_size, StorageTier::Hot)
            .await
            .unwrap();

        let mut registry = SlabRegistry::new();
        registry.add(slab);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let gem = Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new()));

        let vol = ThinVolume::new("source".to_string(), 32 * 1024 * 1024, slot_size);
        let vol_handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
        ));
        let engine = PlacementEngine::new();
        (vol_handle, gem, registry, engine, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn engine_full_cold_copy_lifecycle() {
        let slot_size = 4096u64;
        let (vol_handle, gem, registry, mut engine, mut paths) = setup_test_env(slot_size).await;

        // Write data
        vol_handle.write(0, &vec![0xAA; 4096]).await.unwrap();
        vol_handle.write(4096, &vec![0xBB; 4096]).await.unwrap();

        // Snapshot
        let source_id = vol_handle.volume_id();
        let snap1 = {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            let snap_vol = create_snapshot(source_id, "snap1", 32 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap();
            Arc::new(ThinVolumeHandle::new(
                snap_vol,
                gem.clone(),
                registry.clone(),
                PlacementPolicy::default(),
            ))
        };
        let snap1_id = snap1.volume_id();

        // Create cold copy target
        let cold_path = {
            let test_id = uuid::Uuid::new_v4().simple().to_string();
            let dir = std::env::temp_dir().join("stormblock-placement-test");
            let path = dir.join(format!("{test_id}-cold.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            paths.push(path_str.clone());
            path_str
        };
        let cold_dev = Arc::new(
            FileDevice::open_with_capacity(&cold_path, 32 * 1024 * 1024)
                .await
                .unwrap(),
        ) as Arc<dyn BlockDevice>;

        let total_extents = 32 * 1024 * 1024 / slot_size;
        let vol_id = vol_handle.volume_id();

        let cold_id = engine.create_cold_copy(
            vol_id,
            cold_dev.clone(),
            snap1_id,
            total_extents,
            slot_size,
            StorageTier::Cold,
            Locality::Local,
        );

        // Replicate
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let result = engine
            .replicate(&cold_id, snap1.as_ref(), None, &rx)
            .await
            .unwrap();
        assert!(result.consistent);

        // Verify data
        let mut buf = vec![0u8; 4096];
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));
        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        // Modify live volume
        vol_handle.write(0, &vec![0xDD; 4096]).await.unwrap();

        // Snapshot 2
        let snap2 = {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            let snap_vol = create_snapshot(source_id, "snap2", 32 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap();
            Arc::new(ThinVolumeHandle::new(
                snap_vol,
                gem.clone(),
                registry.clone(),
                PlacementPolicy::default(),
            ))
        };

        // Advance cold copy
        let result2 = engine
            .advance_cold_copy(
                &cold_id,
                snap1.as_ref(),
                snap2.as_ref(),
                snap2.as_ref(),
                None,
                &rx,
            )
            .await
            .unwrap();
        assert!(result2.consistent);

        // Verify updated data
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xDD));
        // Extent 1 unchanged
        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        // Check volume_cold_copies
        let copies = engine.volume_cold_copies(&vol_id);
        assert_eq!(copies.len(), 1);
        assert!(copies[0].is_consistent());

        // Remove cold copy
        let removed = engine.remove_cold_copy(&cold_id);
        assert!(removed.is_some());
        assert!(engine.volume_cold_copies(&vol_id).is_empty());

        cleanup(&paths);
    }

    #[test]
    fn engine_device_management() {
        let mut engine = PlacementEngine::new();
        assert_eq!(engine.devices().count(), 0);

        // Can't test with real devices in a unit test, but we can test
        // the HashMap operations
        assert!(engine.remove_device(Uuid::new_v4()).is_none());
    }

    // ── Helpers for slab-level placement tests ─────────────────────────

    use crate::drive::slab::DEFAULT_SLOT_SIZE;

    async fn make_slab(size: u64, tier: StorageTier) -> (Slab, String) {
        let dir = std::env::temp_dir().join("stormblock-placement-slab-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("pl-{}.bin", uuid::Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(&path_str, size).await.unwrap()
        );
        let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, tier).await.unwrap();
        (slab, path_str)
    }

    #[tokio::test]
    async fn test_migrate_extent() {
        let (mut slab_a, p1) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let (slab_b, p2) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let slab_a_id = slab_a.slab_id();
        let slab_b_id = slab_b.slab_id();

        let vol = VolumeId::new();

        // Allocate and write data in slab A
        let slot = slab_a.allocate(vol, 0).await.unwrap();
        slab_a.write_slot(slot, 0, &vec![0xDE; 4096]).await.unwrap();

        // Set up GEM
        let mut gem = GlobalExtentMap::new();
        gem.insert(vol, 0, ExtentLocation::new(slab_a_id, slot));

        let mut registry = SlabRegistry::new();
        registry.add(slab_a);
        registry.add(slab_b);

        let engine = PlacementEngine::new();

        // Migrate extent to slab B
        let result = engine.migrate_extent(
            &mut gem, &mut registry, vol, 0, Some(slab_b_id)
        ).await.unwrap();

        assert_eq!(result.source_slab, slab_a_id);
        assert_eq!(result.dest_slab, slab_b_id);
        assert_eq!(result.volume_id, vol);

        // Verify GEM points to slab B
        let loc = gem.lookup(vol, 0).unwrap();
        assert_eq!(loc.slab_id, slab_b_id);
        assert_eq!(loc.generation, 2);

        // Verify data integrity
        let dest_slab = registry.get(&slab_b_id).unwrap();
        let mut buf = vec![0u8; 4096];
        dest_slab.read_slot(result.dest_slot, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xDE));

        // Source slab slot should be freed
        let src_slab = registry.get(&slab_a_id).unwrap();
        assert_eq!(src_slab.find_slot(vol, 0), None);

        // Reverse index should point to new location
        assert!(gem.reverse_lookup(slab_a_id, slot).is_none());
        assert_eq!(gem.reverse_lookup(slab_b_id, result.dest_slot), Some((vol, 0)));

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn test_evacuate_slab() {
        let (mut slab_a, p1) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let (slab_b, p2) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let slab_a_id = slab_a.slab_id();
        let slab_b_id = slab_b.slab_id();

        let vol = VolumeId::new();

        // Allocate 5 extents in slab A
        let mut gem = GlobalExtentMap::new();
        for i in 0..5 {
            let slot = slab_a.allocate(vol, i).await.unwrap();
            slab_a.write_slot(slot, 0, &vec![(i as u8 + 0x10); 4096]).await.unwrap();
            gem.insert(vol, i, ExtentLocation::new(slab_a_id, slot));
        }

        let mut registry = SlabRegistry::new();
        registry.add(slab_a);
        registry.add(slab_b);

        let engine = PlacementEngine::new();
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let result = engine.evacuate_slab(&mut gem, &mut registry, slab_a_id, &rx)
            .await.unwrap();

        assert_eq!(result.migrated, 5);
        assert_eq!(result.failed, 0);

        // No extents should remain on slab A
        assert!(gem.slab_extents(slab_a_id).is_empty());

        // All extents should now be on slab B
        for i in 0..5 {
            let loc = gem.lookup(vol, i).unwrap();
            assert_eq!(loc.slab_id, slab_b_id);

            // Verify data integrity
            let slab = registry.get(&slab_b_id).unwrap();
            let mut buf = vec![0u8; 4096];
            slab.read_slot(loc.slot_idx, 0, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&b| b == (i as u8 + 0x10)));
        }

        // Source slab should have all slots free
        let src = registry.get(&slab_a_id).unwrap();
        assert_eq!(src.allocated_slots(), 0);

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn test_rebalance_even() {
        // Slab A: 5 MB device → ~4 slots (small)
        // Slab B: 10 MB device → ~9 slots (larger)
        // Put all extents on slab A → A is overloaded
        let (mut slab_a, p1) = make_slab(5 * 1024 * 1024, StorageTier::Hot).await;
        let (slab_b, p2) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let slab_a_id = slab_a.slab_id();
        let slab_b_id = slab_b.slab_id();
        let total_a = slab_a.total_slots();

        let vol = VolumeId::new();
        let mut gem = GlobalExtentMap::new();

        // Fill slab A completely
        for i in 0..total_a {
            let slot = slab_a.allocate(vol, i).await.unwrap();
            slab_a.write_slot(slot, 0, &vec![0xFF; 512]).await.unwrap();
            gem.insert(vol, i, ExtentLocation::new(slab_a_id, slot));
        }

        let mut registry = SlabRegistry::new();
        registry.add(slab_a);
        registry.add(slab_b);

        let engine = PlacementEngine::new();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let policies = HashMap::new();

        let result = engine.rebalance(
            &mut gem, &mut registry,
            RebalanceStrategy::EvenDistribution,
            &policies, &rx,
        ).await.unwrap();

        // Should have moved some extents to balance things out
        assert!(result.moved > 0, "rebalance should have moved at least one extent");

        // Slab B should now have some allocated extents
        let b_alloc = registry.get(&slab_b_id).unwrap().allocated_slots();
        assert!(b_alloc > 0, "slab B should have received extents");

        // Slab A should have fewer than it started with
        let a_alloc = registry.get(&slab_a_id).unwrap().allocated_slots();
        assert!(a_alloc < total_a, "slab A should have fewer extents after rebalance");

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn test_rebalance_tier_affinity() {
        // Slab A: Cold tier — wrong tier for this volume
        // Slab B: Hot tier — preferred tier
        let (mut slab_cold, p1) = make_slab(10 * 1024 * 1024, StorageTier::Cold).await;
        let (slab_hot, p2) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let cold_id = slab_cold.slab_id();
        let hot_id = slab_hot.slab_id();

        let vol = VolumeId::new();
        let mut gem = GlobalExtentMap::new();

        // Put 3 extents on the cold slab
        for i in 0..3 {
            let slot = slab_cold.allocate(vol, i).await.unwrap();
            slab_cold.write_slot(slot, 0, &vec![(i as u8 + 0x50); 4096]).await.unwrap();
            gem.insert(vol, i, ExtentLocation::new(cold_id, slot));
        }

        let mut registry = SlabRegistry::new();
        registry.add(slab_cold);
        registry.add(slab_hot);

        let engine = PlacementEngine::new();
        let (_tx, rx) = tokio::sync::watch::channel(false);

        // Volume prefers hot tier
        let mut policies = HashMap::new();
        policies.insert(vol, PlacementPolicy {
            preferred_tier: StorageTier::Hot,
            tier_fallback: vec![StorageTier::Warm, StorageTier::Cold],
        });

        let result = engine.rebalance(
            &mut gem, &mut registry,
            RebalanceStrategy::TierAffinity,
            &policies, &rx,
        ).await.unwrap();

        assert_eq!(result.moved, 3);
        assert_eq!(result.failed, 0);

        // All extents should now be on the hot slab
        for i in 0..3 {
            let loc = gem.lookup(vol, i).unwrap();
            assert_eq!(loc.slab_id, hot_id);

            // Verify data integrity
            let slab = registry.get(&hot_id).unwrap();
            let mut buf = vec![0u8; 4096];
            slab.read_slot(loc.slot_idx, 0, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&b| b == (i as u8 + 0x50)));
        }

        // Cold slab should be empty
        assert!(gem.slab_extents(cold_id).is_empty());

        cleanup(&[p1, p2]);
    }

    #[test]
    fn placement_error_display() {
        let vol = VolumeId::new();
        let slab = SlabId::new();

        assert!(PlacementError::SlabFull.to_string().contains("full"));
        assert!(PlacementError::ExtentNotFound { volume_id: vol, vext_idx: 5 }
            .to_string().contains("not found"));
        assert!(PlacementError::SlabNotFound(slab).to_string().contains("not found"));
        assert!(PlacementError::NoDestination.to_string().contains("destination"));
        assert!(PlacementError::ReadFailed { slab_id: slab, slot_idx: 0, error: "io".into() }
            .to_string().contains("read failed"));
        assert!(PlacementError::WriteFailed { slab_id: slab, slot_idx: 0, error: "io".into() }
            .to_string().contains("write failed"));
        assert!(PlacementError::Other("test".into()).to_string().contains("test"));
    }
}
