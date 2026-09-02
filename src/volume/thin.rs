//! Thin volume — virtual size, on-demand extent allocation via slabs.
//!
//! `ThinVolume` implements `BlockDevice`, so target protocols see volumes
//! as plain block devices. Physical storage is allocated on first write
//! (allocate-on-write) from slab slots via the Global Extent Map (GEM).
//!
//! Redundancy lives here too, per volume: a mirrored volume writes every
//! extent to `copies` legs on distinct failure domains and reads from any;
//! a parity volume keeps stripes of `data` extents with P (and Q) legs and
//! updates them read-modify-write under a per-stripe lock. A slab a write
//! fails on goes into the volume's **failed set** — skipped for reads and
//! writes, reported as degraded — until `resync` rebuilds what was on it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Serialize, Deserialize};

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};
use crate::drive::slab::{SlabId, SlabRole};
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::domain::FailureDomain;
use crate::placement::topology::StorageTier;
use super::extent::VolumeId;
use super::gem::{ExtentLocation, GlobalExtentMap, Leg, ParityGroup, parity_vext};
use super::redundancy::{Redundancy, RedundancyPolicy};
use super::stripe;

/// A physical extent with reference counting for COW snapshots.
/// Legacy type — kept for metadata V1 compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalExtent {
    pub array_id: crate::raid::RaidArrayId,
    pub offset: u64,
    pub length: u64,
    pub ref_count: u32,
}

/// Volume purpose — how the volume will be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumePurpose {
    #[default]
    Partition,
    StormFS,
    ObjectStore,
    KeyValue,
    Boot,
}


impl fmt::Display for VolumePurpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumePurpose::Partition => write!(f, "partition"),
            VolumePurpose::StormFS => write!(f, "stormfs"),
            VolumePurpose::ObjectStore => write!(f, "objstore"),
            VolumePurpose::KeyValue => write!(f, "kv"),
            VolumePurpose::Boot => write!(f, "boot"),
        }
    }
}

/// Placement policy for a volume — controls which slab tiers are preferred.
#[derive(Debug, Clone)]
pub struct PlacementPolicy {
    pub preferred_tier: StorageTier,
    pub tier_fallback: Vec<StorageTier>,
    /// Which half of the node's mutable storage this volume lives in.
    ///
    /// A hard boundary, and the only one that survives an install: a
    /// `System` volume never allocates in a data slab and a `Data` volume
    /// never allocates in a system slab, so replacing the goldens cannot
    /// take a copy-on-write extent of the node's identity with it (#88).
    /// Tier is a preference with a fallback chain; this is not.
    pub role: SlabRole,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        PlacementPolicy {
            preferred_tier: StorageTier::Hot,
            tier_fallback: vec![StorageTier::Warm, StorageTier::Cool, StorageTier::Cold],
            role: SlabRole::System,
        }
    }
}

/// Volume manager errors.
#[derive(Debug)]
pub enum VolumeError {
    NoSpace,
    VolumeNotFound(VolumeId),
    InvalidSize(String),
    Drive(DriveError),
    AllocatorError(String),
    /// A shrink was asked for without saying so explicitly (#19).
    ShrinkRefused { current: u64, requested: u64 },
    /// The node cannot place the legs a policy asks for on distinct domains.
    InsufficientDomains { policy: String, needed: usize, available: usize },
    /// A policy change that would need the data re-striped, not re-copied.
    RestripeRequired { from: String, to: String },
    /// The volume is sealed: it is what clones are taken from, and nothing
    /// writes to it (#76).
    Sealed(VolumeId),
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumeError::NoSpace => write!(f, "no free slots available"),
            VolumeError::VolumeNotFound(id) => write!(f, "volume {id} not found"),
            VolumeError::InvalidSize(msg) => write!(f, "invalid size: {msg}"),
            VolumeError::Drive(e) => write!(f, "drive error: {e}"),
            VolumeError::AllocatorError(msg) => write!(f, "allocator error: {msg}"),
            VolumeError::ShrinkRefused { current, requested } => write!(
                f,
                "refusing to shrink from {current} to {requested} bytes: a filesystem on this \
                 volume generally cannot follow, and the extents past the new end are freed \
                 immediately — use the explicit shrink path if that is really what you want"
            ),
            VolumeError::InsufficientDomains { policy, needed, available } => write!(
                f,
                "redundancy {policy} needs {needed} distinct failure domains with free space; \
                 this node has {available}"
            ),
            VolumeError::RestripeRequired { from, to } => write!(
                f,
                "changing redundancy from {from} to {to} would re-stripe the data; only \
                 none/mirror to mirror is applied in place (resync after setting it)"
            ),
            VolumeError::Sealed(id) => write!(
                f,
                "volume {id} is sealed: it is what clones are taken from, and it takes no writes"
            ),
        }
    }
}

impl std::error::Error for VolumeError {}

impl From<DriveError> for VolumeError {
    fn from(e: DriveError) -> Self {
        VolumeError::Drive(e)
    }
}

impl From<VolumeError> for DriveError {
    fn from(e: VolumeError) -> Self {
        DriveError::Other(anyhow::anyhow!("{e}"))
    }
}

/// A thin-provisioned volume backed by slabs via the Global Extent Map.
///
/// Virtual blocks are mapped to slab slots on demand. Implements `BlockDevice`
/// for use by target protocols (NVMe-oF, iSCSI). Storage is allocated from
/// any slab in the registry according to the placement policy.
pub struct ThinVolume {
    pub(crate) id: VolumeId,
    pub(crate) name: String,
    pub(crate) virtual_size: u64,
    pub(crate) slot_size: u64,
    #[allow(dead_code)]
    pub(crate) purpose: VolumePurpose,
    pub(crate) device_id: DeviceId,
}

impl ThinVolume {
    pub fn new(
        name: String,
        virtual_size: u64,
        slot_size: u64,
    ) -> Self {
        let id = VolumeId::new();
        let device_id = DeviceId {
            uuid: id.0,
            serial: format!("vol-{}", &id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{id}"),
        };

        ThinVolume {
            id,
            name,
            virtual_size,
            slot_size,
            purpose: VolumePurpose::Partition,
            device_id,
        }
    }

    /// Restore a volume from persisted config (recovery path).
    pub fn restore(
        id: VolumeId,
        name: String,
        virtual_size: u64,
        slot_size: u64,
    ) -> Self {
        let device_id = DeviceId {
            uuid: id.0,
            serial: format!("vol-{}", &id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{id}"),
        };

        ThinVolume {
            id,
            name,
            virtual_size,
            slot_size,
            purpose: VolumePurpose::Partition,
            device_id,
        }
    }

    pub fn id(&self) -> VolumeId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn slot_size(&self) -> u64 {
        self.slot_size
    }
}

/// Whether a volume's data is all there, all protected, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    /// Every leg the policy asks for is present on a trusted slab.
    Healthy,
    /// Something is missing but everything is still readable.
    Degraded,
    /// At least one extent cannot be read from what is left.
    Failed,
}

impl fmt::Display for HealthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthState::Healthy => write!(f, "healthy"),
            HealthState::Degraded => write!(f, "degraded"),
            HealthState::Failed => write!(f, "failed"),
        }
    }
}

/// What `health` found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeHealth {
    pub state: HealthState,
    pub redundancy: String,
    pub extents: usize,
    /// Legs the policy asks for across all extents (and parity legs).
    pub legs_expected: usize,
    /// Legs not present on a trusted slab.
    pub legs_missing: usize,
    /// Extents (or stripes) that cannot be read from what remains.
    pub unreadable: usize,
    /// Slabs this volume has stopped trusting.
    pub failed_slabs: Vec<SlabId>,
}

/// What a `resync` did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResyncReport {
    /// Legs rebuilt onto a fresh domain (data and parity).
    pub legs_rebuilt: usize,
    /// Legs added because the policy asks for more copies than existed.
    pub legs_added: usize,
    /// Legs dropped because the policy asks for fewer.
    pub legs_dropped: usize,
    /// Parity legs recomputed and rewritten (`verify`).
    pub parity_verified: usize,
    /// Extents or stripes that could not be recovered.
    pub unrecoverable: usize,
    /// Slabs no longer in the failed set.
    pub slabs_cleared: Vec<SlabId>,
    pub errors: Vec<String>,
}

/// Number of lock shards for extents/stripes of one volume.
const SHARDS: usize = 64;

/// `ThinVolume` wrapped with shared GEM and SlabRegistry references.
///
/// The handle owns Arc references to the GEM and registry, allowing
/// lock-free reads and serialized writes. Implements `BlockDevice`.
pub struct ThinVolumeHandle {
    inner: tokio::sync::Mutex<ThinVolume>,
    device_id: DeviceId,
    virtual_size: AtomicU64,
    id: VolumeId,
    slot_size: u64,
    gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
    placement: PlacementPolicy,
    redundancy: std::sync::RwLock<RedundancyPolicy>,
    /// Slabs a write (or read) has failed on. Persisted with the volume.
    failed: std::sync::RwLock<HashSet<SlabId>>,
    /// Per-extent (mirror) or per-stripe (parity) locks, sharded. Only a
    /// redundant volume takes these on the write path.
    shards: Vec<tokio::sync::Mutex<()>>,
    /// Stripes with a read-modify-write since the last flush (parity only).
    stripe_log: std::sync::RwLock<super::stripelog::StripeLog>,
    /// Sealed: refuses writes, discards and shrinks (#76).
    sealed: std::sync::atomic::AtomicBool,
}

impl ThinVolumeHandle {
    pub fn new(
        vol: ThinVolume,
        gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
        registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
        placement: PlacementPolicy,
    ) -> Self {
        Self::with_redundancy(vol, gem, registry, placement, RedundancyPolicy::none())
    }

    pub fn with_redundancy(
        vol: ThinVolume,
        gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
        registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
        placement: PlacementPolicy,
        redundancy: RedundancyPolicy,
    ) -> Self {
        let device_id = vol.device_id.clone();
        let virtual_size = AtomicU64::new(vol.virtual_size);
        let id = vol.id;
        let slot_size = vol.slot_size;
        ThinVolumeHandle {
            inner: tokio::sync::Mutex::new(vol),
            device_id,
            virtual_size,
            id,
            slot_size,
            gem,
            registry,
            placement,
            redundancy: std::sync::RwLock::new(redundancy),
            failed: std::sync::RwLock::new(HashSet::new()),
            shards: (0..SHARDS).map(|_| tokio::sync::Mutex::new(())).collect(),
            stripe_log: std::sync::RwLock::new(super::stripelog::StripeLog::none()),
            sealed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Which half of the node's mutable storage this volume allocates from.
    pub fn placement_role(&self) -> SlabRole {
        self.placement.role
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Relaxed)
    }

    /// Seal (or unseal) the volume. Sealing is a state, not a snapshot: the
    /// volume itself becomes what clones are taken from.
    pub fn set_sealed(&self, sealed: bool) {
        self.sealed.store(sealed, Ordering::Relaxed);
    }

    fn refuse_if_sealed(&self) -> DriveResult<()> {
        if self.is_sealed() {
            return Err(DriveError::Other(anyhow::anyhow!("{}", VolumeError::Sealed(self.id))));
        }
        Ok(())
    }

    /// Keep this volume's dirty-stripe log under `dir`. Returns what a
    /// previous run left dirty, which the caller should verify.
    pub fn use_stripe_log(&self, dir: &std::path::Path) -> Vec<u64> {
        let log = super::stripelog::StripeLog::at(dir, self.id.0);
        let left = log.load();
        *self.stripe_log.write().unwrap() = log;
        left
    }

    pub fn dirty_stripes(&self) -> Vec<u64> {
        self.stripe_log.read().unwrap().dirty()
    }

    /// Set the policy without the transition check — for a restripe that
    /// has already rebuilt the placement to match.
    pub fn force_redundancy(&self, policy: RedundancyPolicy) {
        *self.redundancy.write().unwrap() = policy;
    }

    /// Recompute and rewrite the parity of the given stripes — what a restart
    /// does with the stripes the log says were mid-write.
    pub async fn verify_stripes(&self, stripes: &[u64]) -> ResyncReport {
        let mut report = ResyncReport::default();
        let policy = self.redundancy();
        let Redundancy::Parity { data, parity } = policy.scheme else { return report };
        let width = data as usize;
        for &stripe in stripes {
            let _s = self.shard(stripe).lock().await;
            let members = match self.assemble_stripe(stripe, width).await {
                Ok(m) => m,
                Err(e) => {
                    report.unrecoverable += 1;
                    report.errors.push(format!("stripe {stripe}: {e}"));
                    continue;
                }
            };
            let group = { let gem = self.gem.read().await; gem.lookup_parity(self.id, stripe).cloned() };
            let Some(g) = group else { continue };
            if g.ref_count > 1 {
                // Shared with a clone: the stripe's parity belongs to both
                // and a recompute from *this* volume's members is only right
                // if they are the same — which they are, or the group would
                // have been copied on write. Recompute in place.
            }
            let refs: Vec<Option<&[u8]>> = members.iter().map(|m| Some(m.as_slice())).collect();
            let want = stripe::compute_parity(&refs, self.slot_size as usize, parity);
            for (i, leg) in g.legs.iter().enumerate() {
                if self.is_failed(leg.slab_id) {
                    continue;
                }
                match self.write_leg(*leg, 0, &want[i]).await {
                    Ok(()) => report.parity_verified += 1,
                    Err(e) => report.errors.push(format!("stripe {stripe} parity {i}: {e}")),
                }
            }
        }
        let _ = self.stripe_log.read().unwrap().clear();
        report
    }

    // ── Policy and trust ───────────────────────────────────────────────

    pub fn redundancy(&self) -> RedundancyPolicy {
        self.redundancy.read().unwrap().clone()
    }

    /// Change the policy. Only `none`/`mirror` → `mirror` is accepted here:
    /// it is applied by the next `resync`, which adds or drops legs. Going
    /// to or from parity would re-stripe every extent, which is a move, not
    /// a setting.
    pub fn set_redundancy(&self, policy: RedundancyPolicy) -> Result<(), VolumeError> {
        self.check_transition(&policy)?;
        *self.redundancy.write().unwrap() = policy;
        Ok(())
    }

    /// Whether `set_redundancy` would accept this policy.
    pub fn check_transition(&self, policy: &RedundancyPolicy) -> Result<(), VolumeError> {
        let current = self.redundancy();
        let ok = match (current.scheme, policy.scheme) {
            (Redundancy::None | Redundancy::Mirror { .. }, Redundancy::None | Redundancy::Mirror { .. }) => true,
            (a, b) => a == b,
        };
        if ok {
            Ok(())
        } else {
            Err(VolumeError::RestripeRequired { from: current.spelling(), to: policy.spelling() })
        }
    }

    pub fn failed_slabs(&self) -> Vec<SlabId> {
        let f = self.failed.read().unwrap();
        let mut v: Vec<SlabId> = f.iter().copied().collect();
        v.sort_by_key(|s| s.0);
        v
    }

    pub fn set_failed_slabs(&self, slabs: impl IntoIterator<Item = SlabId>) {
        *self.failed.write().unwrap() = slabs.into_iter().collect();
    }

    fn is_failed(&self, slab: SlabId) -> bool {
        self.failed.read().unwrap().contains(&slab)
    }

    /// Stop trusting a slab for this volume. Idempotent; logs the first time.
    fn mark_failed(&self, slab: SlabId, why: &str) {
        if self.failed.write().unwrap().insert(slab) {
            tracing::warn!(volume = %self.id, slab = %slab, "leg failed, slab marked failed for this volume: {why}");
        }
    }

    fn shard(&self, key: u64) -> &tokio::sync::Mutex<()> {
        &self.shards[(key % SHARDS as u64) as usize]
    }

    /// Lock key for an extent under the current policy: the stripe for a
    /// parity volume, the extent itself otherwise.
    fn lock_key(&self, vext: u64, policy: &RedundancyPolicy) -> u64 {
        match policy.scheme {
            Redundancy::Parity { data, .. } => vext / data as u64,
            _ => vext,
        }
    }

    // ── Sizing ─────────────────────────────────────────────────────────

    /// Resize the volume.
    ///
    /// Growing is instant — allocate-on-write handles new space.
    /// Shrinking frees slab slots beyond the new boundary.
    pub async fn resize(&self, new_size: u64) -> Result<(), VolumeError> {
        if new_size == 0 {
            return Err(VolumeError::InvalidSize("size must be > 0".to_string()));
        }

        let current = self.virtual_size.load(Ordering::Relaxed);
        if new_size == current {
            return Ok(());
        }
        if new_size < current && self.is_sealed() {
            return Err(VolumeError::Sealed(self.id));
        }

        let mut vol = self.inner.lock().await;

        if new_size < current {
            // Shrink: free slots beyond new boundary
            let max_vext_idx = new_size.div_ceil(self.slot_size);
            let to_remove: Vec<u64> = {
                let gem = self.gem.read().await;
                gem.volume_extents(&self.id)
                    .map(|iter| iter.filter(|(&idx, _)| idx >= max_vext_idx).map(|(&idx, _)| idx).collect())
                    .unwrap_or_default()
            };
            for vext_idx in to_remove {
                if let Err(e) = self.release_extent(vext_idx).await {
                    tracing::warn!(volume = %self.id, extent = vext_idx, "shrink could not release extent: {e}");
                }
            }
        }

        vol.virtual_size = new_size;
        self.virtual_size.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    pub fn volume_id(&self) -> VolumeId {
        self.id
    }

    pub async fn name(&self) -> String {
        self.inner.lock().await.name.clone()
    }

    /// Bytes of data mapped (one slot per extent, whatever the policy).
    pub async fn allocated(&self) -> u64 {
        let gem = self.gem.read().await;
        gem.get_volume_map(&self.id)
            .map(|m| m.len() as u64 * self.slot_size)
            .unwrap_or(0)
    }

    /// Bytes of physical slots this volume references, legs and parity
    /// included — what the policy actually costs.
    pub async fn physical(&self) -> u64 {
        let gem = self.gem.read().await;
        gem.get_volume_map(&self.id)
            .map(|m| m.all_legs().count() as u64 * self.slot_size)
            .unwrap_or(0)
    }

    pub async fn extent_count(&self) -> usize {
        let gem = self.gem.read().await;
        gem.get_volume_map(&self.id).map(|m| m.len()).unwrap_or(0)
    }

    /// Access the inner ThinVolume.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ThinVolume> {
        self.inner.lock().await
    }

    /// Get the shared GEM reference.
    pub fn gem(&self) -> &Arc<tokio::sync::RwLock<GlobalExtentMap>> {
        &self.gem
    }

    /// Get the shared SlabRegistry reference.
    pub fn registry(&self) -> &Arc<tokio::sync::RwLock<SlabRegistry>> {
        &self.registry
    }

    // ── Allocation ─────────────────────────────────────────────────────

    /// Domains this volume must stay off: the slabs it has stopped trusting.
    fn failed_domains(&self, registry: &SlabRegistry) -> Vec<FailureDomain> {
        self.failed.read().unwrap().iter().map(|s| registry.domain_of(s)).collect()
    }

    /// Allocate one slot for `(volume, vext_tag)` on a slab whose domain at
    /// `rung` differs from every one in `apart_from`, preferred tier first.
    /// The slot is reserved until the caller records or gives it back.
    async fn allocate_apart(
        &self,
        registry: &mut SlabRegistry,
        vext_tag: u64,
        apart_from: &[FailureDomain],
        rung: &str,
        generation: u64,
    ) -> DriveResult<Leg> {
        let mut tiers = vec![self.placement.preferred_tier];
        tiers.extend(self.placement.tier_fallback.iter().copied());
        for tier in tiers {
            // A slab that is full or collides is skipped by the registry; one
            // that then fails to allocate (a race) is simply tried past.
            let mut tried: Vec<SlabId> = Vec::new();
            loop {
                let mut taken: Vec<FailureDomain> = apart_from.to_vec();
                for t in &tried {
                    taken.push(registry.domain_of(t));
                }
                let Some(slab_id) =
                    registry.best_slab_for_tier_apart_from(tier, &taken, rung, self.placement.role)
                else {
                    break;
                };
                if self.is_failed(slab_id) {
                    tried.push(slab_id);
                    continue;
                }
                let Some(slab) = registry.get_mut(&slab_id) else {
                    tried.push(slab_id);
                    continue;
                };
                match slab.allocate_gen(self.id, vext_tag, generation).await {
                    Ok(slot_idx) => {
                        registry.reserve(slab_id, slot_idx);
                        return Ok(Leg::new(slab_id, slot_idx));
                    }
                    Err(_) => {
                        tried.push(slab_id);
                        continue;
                    }
                }
            }
        }
        Err(DriveError::NoSpace(format!(
            "no {} slab apart from {} domain(s) at rung '{rung}'",
            self.placement.role,
            apart_from.len()
        )))
    }

    /// Allocate a slot from the best available slab according to placement
    /// policy — the unreplicated path, kept lock-light.
    async fn allocate_slot(
        &self,
        registry: &mut SlabRegistry,
        vext_idx: u64,
        generation: u64,
    ) -> DriveResult<(SlabId, u32)> {
        let failed = self.failed_domains(registry);
        let leg = self.allocate_apart(registry, vext_idx, &failed, "drive", generation).await?;
        Ok((leg.slab_id, leg.slot_idx))
    }

    /// Allocate `copies` legs for one extent on distinct domains.
    async fn allocate_legs(
        &self,
        registry: &mut SlabRegistry,
        vext_idx: u64,
        policy: &RedundancyPolicy,
        generation: u64,
    ) -> DriveResult<Vec<Leg>> {
        let copies = policy.scheme.copies();
        let mut taken = self.failed_domains(registry);
        let mut legs = Vec::with_capacity(copies);
        for _ in 0..copies {
            match self.allocate_apart(registry, vext_idx, &taken, &policy.spread, generation).await {
                Ok(leg) => {
                    taken.push(registry.domain_of(&leg.slab_id));
                    legs.push(leg);
                }
                Err(e) => {
                    for l in &legs {
                        if let Some(s) = registry.get_mut(&l.slab_id) {
                            let _ = s.free(l.slot_idx).await;
                        }
                        registry.commit(l.slab_id, l.slot_idx);
                    }
                    // Still out of space, whatever the policy asked for: the
                    // kind survives the wrapping so the target can say so.
                    let msg = format!(
                        "cannot place {copies} legs on distinct '{}' domains: {e}",
                        policy.spread
                    );
                    return Err(match e {
                        DriveError::NoSpace(_) => DriveError::NoSpace(msg),
                        _ => DriveError::Other(anyhow::anyhow!(msg)),
                    });
                }
            }
        }
        Ok(legs)
    }

    async fn give_back(&self, legs: &[Leg]) {
        let mut reg = self.registry.write().await;
        for l in legs {
            if let Some(s) = reg.get_mut(&l.slab_id) {
                let _ = s.free(l.slot_idx).await;
            }
            reg.commit(l.slab_id, l.slot_idx);
        }
    }

    async fn commit_legs(&self, legs: &[Leg]) {
        let mut reg = self.registry.write().await;
        for l in legs {
            reg.commit(l.slab_id, l.slot_idx);
        }
    }

    // ── Leg I/O ────────────────────────────────────────────────────────

    async fn leg_io(&self, leg: Leg, off_in_slot: u64) -> DriveResult<(Arc<dyn BlockDevice>, u64)> {
        let reg = self.registry.read().await;
        let slab = reg.get(&leg.slab_id).ok_or_else(|| {
            DriveError::Other(anyhow::anyhow!("slab {} not attached", leg.slab_id.0))
        })?;
        slab.slot_device_and_offset(leg.slot_idx, off_in_slot)
    }

    async fn read_leg(&self, leg: Leg, off: u64, buf: &mut [u8]) -> DriveResult<()> {
        let (dev, phys) = self.leg_io(leg, off).await?;
        let n = dev.read(phys, buf).await?;
        if n != buf.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "short read: {n} of {} bytes from slab {} slot {}",
                buf.len(), leg.slab_id.0, leg.slot_idx
            )));
        }
        Ok(())
    }

    async fn write_leg(&self, leg: Leg, off: u64, buf: &[u8]) -> DriveResult<()> {
        let (dev, phys) = self.leg_io(leg, off).await?;
        let n = dev.write(phys, buf).await?;
        if n != buf.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "short write: {n} of {} bytes into slab {} slot {}",
                buf.len(), leg.slab_id.0, leg.slot_idx
            )));
        }
        Ok(())
    }

    /// The legs of a location that are worth trying, preferred one first:
    /// not on a failed slab, rotated by extent so mirrors share the reads.
    fn usable_legs(&self, loc: &ExtentLocation, vext: u64) -> Vec<Leg> {
        let mut legs: Vec<Leg> = loc.legs().filter(|l| !self.is_failed(l.slab_id)).collect();
        if legs.len() > 1 {
            let start = (vext % legs.len() as u64) as usize;
            legs.rotate_left(start);
        }
        legs
    }

    /// Read a range of an extent from whichever leg answers.
    async fn read_extent(&self, vext: u64, loc: &ExtentLocation, off: u64, buf: &mut [u8]) -> DriveResult<()> {
        let mut last: Option<DriveError> = None;
        for leg in self.usable_legs(loc, vext) {
            match self.read_leg(leg, off, buf).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.mark_failed(leg.slab_id, &e.to_string());
                    last = Some(e);
                }
            }
        }
        let policy = self.redundancy();
        if let Redundancy::Parity { data, .. } = policy.scheme {
            let stripe = vext / data as u64;
            let _s = self.shard(stripe).lock().await;
            let members = self.assemble_stripe(stripe, data as usize).await?;
            let i = (vext % data as u64) as usize;
            let src = &members[i][off as usize..off as usize + buf.len()];
            buf.copy_from_slice(src);
            return Ok(());
        }
        Err(last.unwrap_or_else(|| DriveError::Other(anyhow::anyhow!(
            "extent {vext} of volume {} has no readable leg", self.id
        ))))
    }

    /// Write a range to every usable leg. Succeeds if at least one leg took
    /// it; a leg that did not is marked failed so nothing reads it again.
    async fn write_legs(&self, vext: u64, loc: &ExtentLocation, off: u64, buf: &[u8]) -> DriveResult<()> {
        let legs = self.usable_legs(loc, vext);
        if legs.is_empty() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "extent {vext} of volume {} has no usable leg", self.id
            )));
        }
        let results = futures_util::future::join_all(legs.iter().map(|l| self.write_leg(*l, off, buf))).await;
        let mut ok = 0usize;
        let mut last: Option<DriveError> = None;
        for (leg, r) in legs.iter().zip(results) {
            match r {
                Ok(()) => ok += 1,
                Err(e) => {
                    self.mark_failed(leg.slab_id, &e.to_string());
                    last = Some(e);
                }
            }
        }
        if ok == 0 {
            return Err(last.unwrap());
        }
        Ok(())
    }

    /// Read a whole slot's worth of an extent, or `None` if no leg answers.
    async fn read_full(&self, vext: u64, loc: &ExtentLocation) -> Option<Vec<u8>> {
        let mut data = vec![0u8; self.slot_size as usize];
        for leg in self.usable_legs(loc, vext) {
            match self.read_leg(leg, 0, &mut data).await {
                Ok(()) => return Some(data),
                Err(e) => self.mark_failed(leg.slab_id, &e.to_string()),
            }
        }
        None
    }

    // ── Unreplicated and mirrored write paths ──────────────────────────

    /// Allocate a new slot and write data (allocate-on-write path).
    async fn allocate_and_write(
        &self,
        vext_idx: u64,
        off_in_slot: u64,
        buf: &[u8],
        policy: &RedundancyPolicy,
    ) -> DriveResult<()> {
        if policy.is_none() {
            // Allocate slot
            let (slab_id, slot_idx) = {
                let mut reg = self.registry.write().await;
                self.allocate_slot(&mut reg, vext_idx, 1).await?
            };

            // Insert into GEM
            {
                let mut gem = self.gem.write().await;
                gem.insert(self.id, vext_idx, ExtentLocation::new(slab_id, slot_idx));
            }
            // Mapped now, so the collector can see it is owned.
            self.registry.write().await.commit(slab_id, slot_idx);

            // Write data
            let (device, phys_offset) = {
                let reg = self.registry.read().await;
                let slab = reg.get(&slab_id).ok_or_else(|| {
                    DriveError::Other(anyhow::anyhow!("slab {} not found", slab_id.0))
                })?;
                slab.slot_device_and_offset(slot_idx, off_in_slot)?
            };
            device.write(phys_offset, buf).await?;
            return Ok(());
        }

        // Mirrored: every leg gets the whole slot, zero-filled around the
        // write, so the legs are identical from the first byte and what was
        // never written reads as zero from any of them.
        let legs = {
            let mut reg = self.registry.write().await;
            self.allocate_legs(&mut reg, vext_idx, policy, 1).await?
        };
        let mut full = vec![0u8; self.slot_size as usize];
        full[off_in_slot as usize..off_in_slot as usize + buf.len()].copy_from_slice(buf);
        let results = futures_util::future::join_all(legs.iter().map(|l| self.write_leg(*l, 0, &full))).await;
        let mut good = Vec::new();
        let mut bad = Vec::new();
        for (leg, r) in legs.iter().zip(results) {
            match r {
                Ok(()) => good.push(*leg),
                Err(e) => {
                    self.mark_failed(leg.slab_id, &e.to_string());
                    bad.push(*leg);
                }
            }
        }
        if good.is_empty() {
            self.give_back(&legs).await;
            return Err(DriveError::Other(anyhow::anyhow!(
                "no leg of extent {vext_idx} could be written"
            )));
        }
        self.give_back(&bad).await;
        {
            let mut gem = self.gem.write().await;
            gem.insert(self.id, vext_idx, ExtentLocation::with_legs(good[0], good[1..].to_vec()));
        }
        self.commit_legs(&good).await;
        Ok(())
    }

    /// Write into an extent this volume already owns exclusively.
    async fn write_in_place(
        &self,
        vext_idx: u64,
        loc: &ExtentLocation,
        off_in_slot: u64,
        buf: &[u8],
    ) -> DriveResult<()> {
        self.write_legs(vext_idx, loc, off_in_slot, buf).await
    }

    /// COW: copy old slot data to new slot(s), write new data, update GEM,
    /// dec_ref old.
    async fn cow_write(
        &self,
        vext_idx: u64,
        off_in_slot: u64,
        buf: &[u8],
        old_loc: &ExtentLocation,
        policy: &RedundancyPolicy,
    ) -> DriveResult<()> {
        // Read old slot data. A short copy here is silent data loss: the
        // tail of the slot the clone inherited would read back as whatever
        // the new slot already held. Refusing the write keeps the old
        // mapping, which still has the data.
        let mut data = self.read_full(vext_idx, old_loc).await.ok_or_else(|| {
            DriveError::Other(anyhow::anyhow!(
                "copy-on-write could not read extent {vext_idx} from any leg"
            ))
        })?;
        data[off_in_slot as usize..off_in_slot as usize + buf.len()].copy_from_slice(buf);

        // Allocate new slot(s)
        let generation = old_loc.generation + 1;
        let legs = {
            let mut reg = self.registry.write().await;
            if policy.is_none() {
                let (s, i) = self.allocate_slot(&mut reg, vext_idx, generation).await?;
                vec![Leg::new(s, i)]
            } else {
                self.allocate_legs(&mut reg, vext_idx, policy, generation).await?
            }
        };

        // Write old data with the new overlaid.
        //
        // Failing here leaves the slot allocated but never mapped — a genuine
        // orphan. Drop the reservation on the way out so the collector is
        // free to reclaim it, rather than pinning it for the process lifetime.
        let results = futures_util::future::join_all(legs.iter().map(|l| self.write_leg(*l, 0, &data))).await;
        let mut good = Vec::new();
        let mut bad = Vec::new();
        for (leg, r) in legs.iter().zip(results) {
            match r {
                Ok(()) => good.push(*leg),
                Err(e) => {
                    if !policy.is_none() {
                        self.mark_failed(leg.slab_id, &e.to_string());
                    }
                    bad.push((*leg, e));
                }
            }
        }
        if good.is_empty() {
            self.give_back(&legs).await;
            return Err(bad.into_iter().next().unwrap().1);
        }
        let bad: Vec<Leg> = bad.into_iter().map(|(l, _)| l).collect();
        self.give_back(&bad).await;

        // Update GEM
        {
            let mut gem = self.gem.write().await;
            let mut loc = ExtentLocation::with_legs(good[0], good[1..].to_vec());
            loc.generation = old_loc.generation + 1;
            gem.insert(self.id, vext_idx, loc);
        }

        // Dec ref on old slot(s)
        let mut synced = Vec::new();
        {
            let mut reg = self.registry.write().await;
            // The new slots are mapped now, so they no longer need protecting.
            for l in &good {
                reg.commit(l.slab_id, l.slot_idx);
            }
            for leg in old_loc.legs() {
                if let Some(slab) = reg.get_mut(&leg.slab_id) {
                    match slab.dec_ref(leg.slot_idx).await {
                        Ok(_) => synced.push((leg, slab.get_slot(leg.slot_idx).map(|s| s.ref_count).unwrap_or(0))),
                        Err(e) => tracing::warn!(
                            volume = %self.id, slot = leg.slot_idx,
                            "copy-on-write could not release the shared extent: {e}"
                        ),
                    }
                }
            }
        }
        self.sync_refs(&synced).await;

        Ok(())
    }

    /// After slots' share counts moved on disk, make the owning maps agree —
    /// so a source whose last clone diverged writes in place again rather
    /// than copying for nobody. Takes the GEM lock; the caller must not hold
    /// the registry.
    async fn sync_refs(&self, counts: &[(Leg, u32)]) {
        if counts.is_empty() {
            return;
        }
        let mut gem = self.gem.write().await;
        for (leg, count) in counts {
            if *count == 0 {
                continue;
            }
            if let Some((vol, tagged)) = gem.reverse_lookup(leg.slab_id, leg.slot_idx) {
                match super::gem::parse_parity_vext(tagged) {
                    Some((_, stripe)) => gem.set_parity_ref(vol, stripe, *count),
                    None => gem.set_extent_ref(vol, tagged, *count),
                }
            }
        }
    }

    /// Unmap an extent and release every slot it referenced. For a parity
    /// volume the stripe's parity is updated first. Caller holds whatever
    /// lock makes the mapping stable.
    async fn release_extent(&self, vext_idx: u64) -> DriveResult<()> {
        let policy = self.redundancy();
        if let Redundancy::Parity { data, parity } = policy.scheme {
            return self.release_parity_member(vext_idx, data as usize, parity, &policy).await;
        }
        let loc = {
            let gem = self.gem.read().await;
            gem.lookup(self.id, vext_idx).cloned()
        };
        let Some(loc) = loc else { return Ok(()) };
        {
            let mut gem = self.gem.write().await;
            gem.remove(self.id, vext_idx);
        }
        let mut reg = self.registry.write().await;
        for leg in loc.legs() {
            if let Some(slab) = reg.get_mut(&leg.slab_id) {
                if let Err(e) = slab.dec_ref(leg.slot_idx).await {
                    // The mapping is already gone, so a failure here
                    // strands the slot with no owner. Say so.
                    tracing::warn!(
                        volume = %self.id, slot = leg.slot_idx, extent = vext_idx,
                        "discard could not release extent: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    // ── Parity volumes ─────────────────────────────────────────────────

    /// Domains of every slot the stripe already uses (data members and
    /// parity legs) plus the volume's failed slabs.
    async fn stripe_domains(&self, stripe: u64, width: usize, except: Option<u64>) -> Vec<FailureDomain> {
        let gem = self.gem.read().await;
        let reg = self.registry.read().await;
        let mut taken = self.failed_domains(&reg);
        for vext in stripe * width as u64..(stripe + 1) * width as u64 {
            if Some(vext) == except {
                continue;
            }
            if let Some(loc) = gem.lookup(self.id, vext) {
                for leg in loc.legs() {
                    taken.push(reg.domain_of(&leg.slab_id));
                }
            }
        }
        if let Some(g) = gem.lookup_parity(self.id, stripe) {
            for leg in &g.legs {
                taken.push(reg.domain_of(&leg.slab_id));
            }
        }
        taken
    }

    /// Every member of a stripe as a full slot, reconstructing what cannot
    /// be read. Unallocated members are zeros.
    async fn assemble_stripe(&self, stripe: u64, width: usize) -> DriveResult<Vec<Vec<u8>>> {
        let (locs, group) = {
            let gem = self.gem.read().await;
            let locs: Vec<Option<ExtentLocation>> = (stripe * width as u64..(stripe + 1) * width as u64)
                .map(|v| gem.lookup(self.id, v).cloned())
                .collect();
            (locs, gem.lookup_parity(self.id, stripe).cloned())
        };
        let mut members: Vec<Option<Vec<u8>>> = Vec::with_capacity(width);
        for (i, loc) in locs.iter().enumerate() {
            match loc {
                None => members.push(Some(vec![0u8; self.slot_size as usize])),
                Some(l) => members.push(self.read_full(stripe * width as u64 + i as u64, l).await),
            }
        }
        if members.iter().all(|m| m.is_some()) {
            return Ok(members.into_iter().map(|m| m.unwrap()).collect());
        }
        let mut p: Option<Vec<u8>> = None;
        let mut q: Option<Vec<u8>> = None;
        if let Some(g) = &group {
            for (i, leg) in g.legs.iter().enumerate() {
                if self.is_failed(leg.slab_id) {
                    continue;
                }
                let mut buf = vec![0u8; self.slot_size as usize];
                match self.read_leg(*leg, 0, &mut buf).await {
                    Ok(()) => {
                        if i == 0 { p = Some(buf) } else { q = Some(buf) }
                    }
                    Err(e) => self.mark_failed(leg.slab_id, &e.to_string()),
                }
            }
        }
        stripe::reconstruct(&mut members, p.as_deref(), q.as_deref(), self.slot_size as usize)
            .map_err(|e| DriveError::Other(anyhow::anyhow!(
                "stripe {stripe} of volume {} is unrecoverable: {e}", self.id
            )))?;
        Ok(members.into_iter().map(|m| m.unwrap()).collect())
    }

    /// Write the parity legs of a stripe from scratch, to `legs`.
    async fn write_parity_full(&self, members: &[Vec<u8>], parity: u8, legs: &[Leg]) -> DriveResult<()> {
        let refs: Vec<Option<&[u8]>> = members.iter().map(|m| Some(m.as_slice())).collect();
        let bufs = stripe::compute_parity(&refs, self.slot_size as usize, parity);
        for (leg, buf) in legs.iter().zip(bufs.iter()) {
            self.write_leg(*leg, 0, buf).await?;
        }
        Ok(())
    }

    /// Give a stripe a parity group of its own: allocate legs apart from the
    /// members, compute from the current members, record it. Replaces (and
    /// releases) a shared group.
    async fn cow_parity_group(
        &self,
        stripe: u64,
        width: usize,
        parity: u8,
        policy: &RedundancyPolicy,
        old: Option<&ParityGroup>,
        members: &[Vec<u8>],
    ) -> DriveResult<()> {
        let taken = self.stripe_domains(stripe, width, None).await;
        let generation = old.map(|o| o.generation + 1).unwrap_or(1);
        let mut legs = Vec::with_capacity(parity as usize);
        {
            let mut reg = self.registry.write().await;
            let mut taken = taken;
            for i in 0..parity {
                match self.allocate_apart(&mut reg, parity_vext(i, stripe), &taken, &policy.spread, generation).await {
                    Ok(leg) => {
                        taken.push(reg.domain_of(&leg.slab_id));
                        legs.push(leg);
                    }
                    Err(e) => {
                        for l in &legs {
                            if let Some(s) = reg.get_mut(&l.slab_id) {
                                let _ = s.free(l.slot_idx).await;
                            }
                            reg.commit(l.slab_id, l.slot_idx);
                        }
                        return Err(DriveError::Other(anyhow::anyhow!(
                            "cannot place parity for stripe {stripe} apart from its members: {e}"
                        )));
                    }
                }
            }
        }
        if let Err(e) = self.write_parity_full(members, parity, &legs).await {
            self.give_back(&legs).await;
            return Err(e);
        }
        {
            let mut gem = self.gem.write().await;
            let mut g = ParityGroup::new(legs.clone(), width as u8);
            g.generation = generation;
            gem.insert_parity(self.id, stripe, g);
        }
        let mut synced = Vec::new();
        {
            let mut reg = self.registry.write().await;
            for l in &legs {
                reg.commit(l.slab_id, l.slot_idx);
            }
            if let Some(old) = old {
                for leg in &old.legs {
                    if let Some(slab) = reg.get_mut(&leg.slab_id) {
                        match slab.dec_ref(leg.slot_idx).await {
                            Ok(_) => synced.push((*leg, slab.get_slot(leg.slot_idx).map(|s| s.ref_count).unwrap_or(0))),
                            Err(e) => tracing::warn!(volume = %self.id, slot = leg.slot_idx, "could not release shared parity: {e}"),
                        }
                    }
                }
            }
        }
        self.sync_refs(&synced).await;
        Ok(())
    }

    /// Fold a member's change into the stripe's parity in place.
    async fn update_parity(&self, group: &ParityGroup, member_idx: usize, off: u64, delta: &[u8]) -> DriveResult<()> {
        let mut wrote = 0usize;
        for (i, leg) in group.legs.iter().enumerate() {
            if self.is_failed(leg.slab_id) {
                continue;
            }
            let mut cur = vec![0u8; delta.len()];
            let r = async {
                self.read_leg(*leg, off, &mut cur).await?;
                if i == 0 {
                    stripe::xor_into(&mut cur, delta);
                } else {
                    stripe::mul_xor_into(&mut cur, delta, stripe::gf_pow2(member_idx));
                }
                self.write_leg(*leg, off, &cur).await
            }
            .await;
            match r {
                Ok(()) => wrote += 1,
                Err(e) => self.mark_failed(leg.slab_id, &e.to_string()),
            }
        }
        if wrote == 0 && !group.legs.is_empty() {
            tracing::warn!(volume = %self.id, "stripe parity could not be updated on any leg — stripe is unprotected until resync");
        }
        Ok(())
    }

    /// The whole write path of a parity volume for one extent range, under
    /// the stripe lock.
    async fn write_parity_member(
        &self,
        vext: u64,
        off: u64,
        buf: &[u8],
        data: u8,
        parity: u8,
        policy: &RedundancyPolicy,
    ) -> DriveResult<()> {
        let width = data as usize;
        let stripe = vext / data as u64;
        let member_idx = (vext % data as u64) as usize;
        let _s = self.shard(stripe).lock().await;

        // The write hole, bounded: say which stripe is mid-write before it
        // is, so a restart verifies this one and not the whole volume.
        if let Err(e) = self.stripe_log.read().unwrap().mark(stripe) {
            tracing::warn!(volume = %self.id, stripe, "dirty-stripe log could not be written: {e}");
        }

        let (loc, group) = {
            let gem = self.gem.read().await;
            (gem.lookup(self.id, vext).cloned(), gem.lookup_parity(self.id, stripe).cloned())
        };

        // 1. The data member: old content over the written range, new slot
        //    when unmapped, shared, or unreadable in place.
        let range = off as usize..off as usize + buf.len();
        let (old_range, new_loc): (Vec<u8>, Option<ExtentLocation>) = match &loc {
            None => (vec![0u8; buf.len()], None),
            Some(l) if l.ref_count == 1 => {
                let mut old = vec![0u8; buf.len()];
                let leg = self.usable_legs(l, vext).into_iter().next();
                let read_ok = match leg {
                    Some(leg) => match self.read_leg(leg, off, &mut old).await {
                        Ok(()) => true,
                        Err(e) => {
                            self.mark_failed(leg.slab_id, &e.to_string());
                            false
                        }
                    },
                    None => false,
                };
                if read_ok {
                    (old, None)
                } else {
                    // Rebuild the member on the way through: reconstruct the
                    // full old slot, then write it whole to a fresh leg.
                    let members = self.assemble_stripe(stripe, width).await?;
                    let full = members[member_idx].clone();
                    let old = full[range.clone()].to_vec();
                    (old, Some(self.replace_member_slot(vext, stripe, width, policy, l, full, Some((off, buf))).await?))
                }
            }
            Some(l) => {
                // Shared with a clone: copy on write.
                let mut full = self.read_full(vext, l).await;
                if full.is_none() {
                    let members = self.assemble_stripe(stripe, width).await?;
                    full = Some(members[member_idx].clone());
                }
                let full = full.unwrap();
                let old = full[range.clone()].to_vec();
                (old, Some(self.replace_member_slot(vext, stripe, width, policy, l, full, Some((off, buf))).await?))
            }
        };

        match (&loc, &new_loc) {
            (Some(l), None) => self.write_legs(vext, l, off, buf).await?,
            (None, None) => {
                // First write: a fresh slot, zero-filled around the data.
                let taken = self.stripe_domains(stripe, width, Some(vext)).await;
                let leg = {
                    let mut reg = self.registry.write().await;
                    self.allocate_apart(&mut reg, vext, &taken, &policy.spread, 1).await.map_err(|e| {
                        DriveError::Other(anyhow::anyhow!(
                            "cannot place member {member_idx} of stripe {stripe} apart from the stripe: {e}"
                        ))
                    })?
                };
                let mut full = vec![0u8; self.slot_size as usize];
                full[range.clone()].copy_from_slice(buf);
                if let Err(e) = self.write_leg(leg, 0, &full).await {
                    self.give_back(&[leg]).await;
                    return Err(e);
                }
                {
                    let mut gem = self.gem.write().await;
                    gem.insert(self.id, vext, ExtentLocation::new(leg.slab_id, leg.slot_idx));
                }
                self.commit_legs(&[leg]).await;
            }
            _ => {} // replace_member_slot already wrote the data
        }

        // 2. The parity: a delta into a group this volume owns, or a fresh
        //    group when there is none or it is shared.
        let mut delta = old_range;
        stripe::xor_into(&mut delta, buf);
        match group {
            Some(g) if g.ref_count == 1 => self.update_parity(&g, member_idx, off, &delta).await,
            other => {
                let members = self.assemble_stripe(stripe, width).await?;
                self.cow_parity_group(stripe, width, parity, policy, other.as_ref(), &members).await
            }
        }
    }

    /// Put a member's full content into a fresh slot apart from the stripe,
    /// optionally with a range overlaid, and point the map at it (replacing
    /// the old location, whose slots are released).
    #[allow(clippy::too_many_arguments)]
    async fn replace_member_slot(
        &self,
        vext: u64,
        stripe: u64,
        width: usize,
        policy: &RedundancyPolicy,
        old: &ExtentLocation,
        mut full: Vec<u8>,
        overlay: Option<(u64, &[u8])>,
    ) -> DriveResult<ExtentLocation> {
        if let Some((off, buf)) = overlay {
            full[off as usize..off as usize + buf.len()].copy_from_slice(buf);
        }
        let taken = self.stripe_domains(stripe, width, Some(vext)).await;
        let leg = {
            let mut reg = self.registry.write().await;
            self.allocate_apart(&mut reg, vext, &taken, &policy.spread, old.generation + 1).await?
        };
        if let Err(e) = self.write_leg(leg, 0, &full).await {
            self.give_back(&[leg]).await;
            return Err(e);
        }
        let mut loc = ExtentLocation::new(leg.slab_id, leg.slot_idx);
        loc.generation = old.generation + 1;
        {
            let mut gem = self.gem.write().await;
            gem.insert(self.id, vext, loc.clone());
        }
        let mut synced = Vec::new();
        {
            let mut reg = self.registry.write().await;
            reg.commit(leg.slab_id, leg.slot_idx);
            for l in old.legs() {
                if let Some(slab) = reg.get_mut(&l.slab_id) {
                    match slab.dec_ref(l.slot_idx).await {
                        Ok(_) => synced.push((l, slab.get_slot(l.slot_idx).map(|s| s.ref_count).unwrap_or(0))),
                        Err(e) => tracing::warn!(volume = %self.id, slot = l.slot_idx, "could not release replaced member: {e}"),
                    }
                }
            }
        }
        self.sync_refs(&synced).await;
        Ok(loc)
    }

    /// Unmap one member of a parity volume, keeping the stripe's parity true.
    async fn release_parity_member(&self, vext: u64, width: usize, parity: u8, policy: &RedundancyPolicy) -> DriveResult<()> {
        let stripe = vext / width as u64;
        let member_idx = (vext % width as u64) as usize;
        let _s = self.shard(stripe).lock().await;
        let (loc, group) = {
            let gem = self.gem.read().await;
            (gem.lookup(self.id, vext).cloned(), gem.lookup_parity(self.id, stripe).cloned())
        };
        let Some(loc) = loc else { return Ok(()) };

        // The member's content is the delta back to zero.
        let members = self.assemble_stripe(stripe, width).await?;
        let old = members[member_idx].clone();

        {
            let mut gem = self.gem.write().await;
            gem.remove(self.id, vext);
        }
        {
            let mut reg = self.registry.write().await;
            for leg in loc.legs() {
                if let Some(slab) = reg.get_mut(&leg.slab_id) {
                    if let Err(e) = slab.dec_ref(leg.slot_idx).await {
                        tracing::warn!(volume = %self.id, slot = leg.slot_idx, extent = vext, "discard could not release extent: {e}");
                    }
                }
            }
        }

        let any_left = {
            let gem = self.gem.read().await;
            (stripe * width as u64..(stripe + 1) * width as u64).any(|v| gem.lookup(self.id, v).is_some())
        };
        match group {
            None => Ok(()),
            Some(g) if !any_left => {
                {
                    let mut gem = self.gem.write().await;
                    gem.remove_parity(self.id, stripe);
                }
                let mut reg = self.registry.write().await;
                for leg in &g.legs {
                    if let Some(slab) = reg.get_mut(&leg.slab_id) {
                        if let Err(e) = slab.dec_ref(leg.slot_idx).await {
                            tracing::warn!(volume = %self.id, slot = leg.slot_idx, "could not release parity of an empty stripe: {e}");
                        }
                    }
                }
                Ok(())
            }
            Some(g) if g.ref_count == 1 => self.update_parity(&g, member_idx, 0, &old).await,
            Some(g) => {
                let mut members = members;
                members[member_idx] = vec![0u8; self.slot_size as usize];
                self.cow_parity_group(stripe, width, parity, policy, Some(&g), &members).await
            }
        }
    }

    // ── Health and resync ──────────────────────────────────────────────

    /// What the policy asks for versus what is on trusted slabs.
    pub async fn health(&self) -> VolumeHealth {
        let policy = self.redundancy();
        let gem = self.gem.read().await;
        let reg = self.registry.read().await;
        let present = |leg: &Leg| reg.get(&leg.slab_id).is_some() && !self.is_failed(leg.slab_id);
        let mut h = VolumeHealth {
            state: HealthState::Healthy,
            redundancy: policy.spelling(),
            extents: 0,
            legs_expected: 0,
            legs_missing: 0,
            unreadable: 0,
            failed_slabs: self.failed_slabs(),
        };
        let Some(map) = gem.get_volume_map(&self.id) else { return h };
        h.extents = map.extents.len();
        match policy.scheme {
            Redundancy::Parity { data, parity } => {
                let mut stripes: HashMap<u64, usize> = HashMap::new();
                for (vext, loc) in &map.extents {
                    h.legs_expected += 1;
                    if !loc.legs().any(|l| present(&l)) {
                        h.legs_missing += 1;
                        *stripes.entry(vext / data as u64).or_default() += 1;
                    }
                }
                for (stripe, g) in &map.parity {
                    h.legs_expected += parity as usize;
                    let missing_parity = parity as usize - g.legs.iter().filter(|l| present(l)).count().min(parity as usize);
                    h.legs_missing += missing_parity;
                    let missing_data = stripes.get(stripe).copied().unwrap_or(0);
                    if missing_data > parity as usize - missing_parity {
                        h.unreadable += 1;
                    }
                }
                // Stripes with missing data and no group at all.
                for (stripe, missing_data) in &stripes {
                    if !map.parity.contains_key(stripe) && *missing_data > 0 {
                        h.unreadable += 1;
                    }
                }
            }
            _ => {
                let copies = policy.scheme.copies();
                for loc in map.extents.values() {
                    h.legs_expected += copies;
                    let ok = loc.legs().filter(|l| present(l)).count();
                    if ok == 0 {
                        h.unreadable += 1;
                    }
                    h.legs_missing += copies.saturating_sub(ok);
                }
            }
        }
        h.state = if h.unreadable > 0 {
            HealthState::Failed
        } else if h.legs_missing > 0 {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        };
        h
    }

    /// Rebuild every leg that is missing or on a failed slab onto a fresh
    /// domain, bring the leg count to what the policy asks, and clear the
    /// failed set of slabs nothing references any more. `verify` also
    /// recomputes and rewrites every stripe's parity.
    pub async fn resync(&self, verify: bool) -> ResyncReport {
        let policy = self.redundancy();
        let mut report = ResyncReport::default();
        match policy.scheme {
            Redundancy::Parity { data, parity } => {
                self.resync_parity(&policy, data as usize, parity, verify, &mut report).await
            }
            _ => self.resync_mirror(&policy, &mut report).await,
        }

        // A failed slab that no longer carries anything of ours is forgiven.
        let still_used: HashSet<SlabId> = {
            let gem = self.gem.read().await;
            gem.get_volume_map(&self.id)
                .map(|m| m.all_legs().map(|l| l.slab_id).collect())
                .unwrap_or_default()
        };
        let mut failed = self.failed.write().unwrap();
        let cleared: Vec<SlabId> = failed.iter().filter(|s| !still_used.contains(s)).copied().collect();
        for s in &cleared {
            failed.remove(s);
        }
        report.slabs_cleared = cleared;
        report
    }

    async fn resync_mirror(&self, policy: &RedundancyPolicy, report: &mut ResyncReport) {
        let copies = policy.scheme.copies();
        let extents: Vec<(u64, ExtentLocation)> = {
            let gem = self.gem.read().await;
            gem.volume_extents(&self.id)
                .map(|it| it.map(|(v, l)| (*v, l.clone())).collect())
                .unwrap_or_default()
        };
        let mut moves: HashMap<Leg, Leg> = HashMap::new();
        let mut adds: Vec<(Leg, Leg, u32)> = Vec::new();
        let mut drops: Vec<Leg> = Vec::new();

        for (vext, _) in extents {
            let _e = self.shard(vext).lock().await;
            // Re-read under the lock: a write may have moved it.
            let Some(loc) = ({ let gem = self.gem.read().await; gem.lookup(self.id, vext).cloned() }) else { continue };
            let (healthy, missing): (Vec<Leg>, Vec<Leg>) = {
                let reg = self.registry.read().await;
                loc.legs().partition(|l| reg.get(&l.slab_id).is_some() && !self.is_failed(l.slab_id))
            };
            if healthy.is_empty() {
                report.unrecoverable += 1;
                report.errors.push(format!("extent {vext}: no readable leg"));
                continue;
            }
            let want_new = copies.saturating_sub(healthy.len());
            if want_new == 0 {
                // Surplus legs beyond the policy — drop from the tail.
                let mut extra: Vec<Leg> = missing.clone();
                let mut hs = healthy.clone();
                while hs.len() > copies {
                    extra.push(hs.pop().unwrap());
                }
                drops.extend(extra);
                continue;
            }
            let mut data = vec![0u8; self.slot_size as usize];
            let mut got = false;
            for leg in &healthy {
                if self.read_leg(*leg, 0, &mut data).await.is_ok() {
                    got = true;
                    break;
                }
            }
            if !got {
                report.unrecoverable += 1;
                report.errors.push(format!("extent {vext}: every leg failed to read"));
                continue;
            }
            let mut taken: Vec<FailureDomain> = {
                let reg = self.registry.read().await;
                let mut t = self.failed_domains(&reg);
                t.extend(healthy.iter().map(|l| reg.domain_of(&l.slab_id)));
                t
            };
            let mut old_iter = missing.into_iter();
            for _ in 0..want_new {
                let new = {
                    let mut reg = self.registry.write().await;
                    match self.allocate_apart(&mut reg, vext, &taken, &policy.spread, loc.generation).await {
                        Ok(l) => {
                            taken.push(reg.domain_of(&l.slab_id));
                            l
                        }
                        Err(e) => {
                            report.errors.push(format!("extent {vext}: {e}"));
                            break;
                        }
                    }
                };
                if let Err(e) = self.write_leg(new, 0, &data).await {
                    self.give_back(&[new]).await;
                    report.errors.push(format!("extent {vext}: rebuilt leg failed to write: {e}"));
                    self.mark_failed(new.slab_id, &e.to_string());
                    continue;
                }
                // Carry the share count so the slot table agrees with the map.
                if loc.ref_count > 1 {
                    let mut reg = self.registry.write().await;
                    if let Some(s) = reg.get_mut(&new.slab_id) {
                        for _ in 1..loc.ref_count {
                            let _ = s.inc_ref(new.slot_idx).await;
                        }
                    }
                }
                match old_iter.next() {
                    Some(old) => {
                        moves.insert(old, new);
                        report.legs_rebuilt += 1;
                    }
                    None => {
                        adds.push((healthy[0], new, loc.ref_count));
                        report.legs_added += 1;
                    }
                }
            }
            // Extras still missing but not replaced (allocation failed) stay
            // listed; the volume remains degraded and says so.
        }

        // Publish: one sweep for moves, then adds and drops.
        {
            let mut gem = self.gem.write().await;
            gem.rewrite_legs(&moves);
            for (beside, new, _) in &adds {
                gem.add_leg_beside(*beside, *new);
            }
            for leg in &drops {
                gem.drop_leg_everywhere(*leg);
            }
        }
        let mut reg = self.registry.write().await;
        for (old, new) in &moves {
            reg.commit(new.slab_id, new.slot_idx);
            if let Some(s) = reg.get_mut(&old.slab_id) {
                let _ = s.free(old.slot_idx).await;
            }
        }
        for (_, new, _) in &adds {
            reg.commit(new.slab_id, new.slot_idx);
        }
        for leg in &drops {
            if let Some(s) = reg.get_mut(&leg.slab_id) {
                if s.free(leg.slot_idx).await.is_ok() {
                    report.legs_dropped += 1;
                }
            }
        }
    }

    async fn resync_parity(&self, policy: &RedundancyPolicy, width: usize, parity: u8, verify: bool, report: &mut ResyncReport) {
        let stripes: Vec<u64> = {
            let gem = self.gem.read().await;
            let Some(map) = gem.get_volume_map(&self.id) else { return };
            let mut s: HashSet<u64> = map.extents.keys().map(|v| v / width as u64).collect();
            s.extend(map.parity.keys().copied());
            let mut v: Vec<u64> = s.into_iter().collect();
            v.sort_unstable();
            v
        };
        for stripe in stripes {
            let _s = self.shard(stripe).lock().await;
            let members = match self.assemble_stripe(stripe, width).await {
                Ok(m) => m,
                Err(e) => {
                    report.unrecoverable += 1;
                    report.errors.push(format!("stripe {stripe}: {e}"));
                    continue;
                }
            };
            // Data legs on missing/failed slabs: rewrite onto fresh ones.
            let mut moves: HashMap<Leg, Leg> = HashMap::new();
            for (i, member) in members.iter().enumerate() {
                let vext = stripe * width as u64 + i as u64;
                let loc = { let gem = self.gem.read().await; gem.lookup(self.id, vext).cloned() };
                let Some(loc) = loc else { continue };
                let usable = {
                    let reg = self.registry.read().await;
                    loc.legs().any(|l| reg.get(&l.slab_id).is_some() && !self.is_failed(l.slab_id))
                };
                if usable {
                    continue;
                }
                let taken = self.stripe_domains(stripe, width, Some(vext)).await;
                let new = {
                    let mut reg = self.registry.write().await;
                    match self.allocate_apart(&mut reg, vext, &taken, &policy.spread, loc.generation).await {
                        Ok(l) => l,
                        Err(e) => {
                            report.errors.push(format!("stripe {stripe} member {i}: {e}"));
                            continue;
                        }
                    }
                };
                if let Err(e) = self.write_leg(new, 0, member).await {
                    self.give_back(&[new]).await;
                    report.errors.push(format!("stripe {stripe} member {i}: {e}"));
                    continue;
                }
                if loc.ref_count > 1 {
                    let mut reg = self.registry.write().await;
                    if let Some(s) = reg.get_mut(&new.slab_id) {
                        for _ in 1..loc.ref_count {
                            let _ = s.inc_ref(new.slot_idx).await;
                        }
                    }
                }
                moves.insert(loc.primary(), new);
                report.legs_rebuilt += 1;
            }
            // Parity legs: rebuild the missing, verify the rest if asked.
            let group = { let gem = self.gem.read().await; gem.lookup_parity(self.id, stripe).cloned() };
            let refs: Vec<Option<&[u8]>> = members.iter().map(|m| Some(m.as_slice())).collect();
            let want = stripe::compute_parity(&refs, self.slot_size as usize, parity);
            match group {
                None => {
                    // Members exist with no group at all: make one.
                    let any = { let gem = self.gem.read().await; (stripe * width as u64..(stripe + 1) * width as u64).any(|v| gem.lookup(self.id, v).is_some()) };
                    if any {
                        if let Err(e) = self.cow_parity_group(stripe, width, parity, policy, None, &members).await {
                            report.errors.push(format!("stripe {stripe}: parity could not be created: {e}"));
                        } else {
                            report.legs_rebuilt += parity as usize;
                        }
                    }
                }
                Some(g) => {
                    for (i, leg) in g.legs.iter().enumerate() {
                        let present = {
                            let reg = self.registry.read().await;
                            reg.get(&leg.slab_id).is_some() && !self.is_failed(leg.slab_id)
                        };
                        if present {
                            if verify {
                                match self.write_leg(*leg, 0, &want[i]).await {
                                    Ok(()) => report.parity_verified += 1,
                                    Err(e) => report.errors.push(format!("stripe {stripe} parity {i}: {e}")),
                                }
                            }
                            continue;
                        }
                        let taken = self.stripe_domains(stripe, width, None).await;
                        let new = {
                            let mut reg = self.registry.write().await;
                            match self.allocate_apart(&mut reg, parity_vext(i as u8, stripe), &taken, &policy.spread, g.generation).await {
                                Ok(l) => l,
                                Err(e) => {
                                    report.errors.push(format!("stripe {stripe} parity {i}: {e}"));
                                    continue;
                                }
                            }
                        };
                        if let Err(e) = self.write_leg(new, 0, &want[i]).await {
                            self.give_back(&[new]).await;
                            report.errors.push(format!("stripe {stripe} parity {i}: {e}"));
                            continue;
                        }
                        if g.ref_count > 1 {
                            let mut reg = self.registry.write().await;
                            if let Some(s) = reg.get_mut(&new.slab_id) {
                                for _ in 1..g.ref_count {
                                    let _ = s.inc_ref(new.slot_idx).await;
                                }
                            }
                        }
                        moves.insert(*leg, new);
                        report.legs_rebuilt += 1;
                    }
                    // A group rebuilt from slot tables carries no width;
                    // it has one now.
                    if g.data_width == 0 {
                        let mut gem = self.gem.write().await;
                        let mut ng = g.clone();
                        ng.data_width = width as u8;
                        gem.restore_parity(self.id, stripe, ng);
                    }
                }
            }
            {
                let mut gem = self.gem.write().await;
                gem.rewrite_legs(&moves);
            }
            let mut reg = self.registry.write().await;
            for (old, new) in &moves {
                reg.commit(new.slab_id, new.slot_idx);
                if let Some(s) = reg.get_mut(&old.slab_id) {
                    let _ = s.free(old.slot_idx).await;
                }
            }
        }
    }
}

#[async_trait]
impl BlockDevice for ThinVolumeHandle {
    fn id(&self) -> &DeviceId {
        &self.device_id
    }

    fn capacity_bytes(&self) -> u64 {
        self.virtual_size.load(Ordering::Relaxed)
    }

    fn block_size(&self) -> u32 {
        4096
    }

    fn optimal_io_size(&self) -> u32 {
        4096
    }

    /// Space comes back a whole slab slot at a time — `discard` only frees
    /// fully-covered slots, so a smaller discard reclaims nothing (#25).
    fn discard_granularity(&self) -> u32 {
        self.slot_size.min(u32::MAX as u64) as u32
    }

    fn device_type(&self) -> DriveType {
        DriveType::File
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let buf_len = buf.len() as u64;
        let mut bytes_read = 0u64;
        let mut pos = offset;

        while bytes_read < buf_len {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;
            let remaining_in_slot = self.slot_size - off_in_slot;
            let remaining_in_buf = buf_len - bytes_read;
            let to_read = remaining_in_slot.min(remaining_in_buf) as usize;

            let buf_start = bytes_read as usize;
            let buf_end = buf_start + to_read;

            // Look up extent in GEM
            let location = {
                let gem = self.gem.read().await;
                gem.lookup(self.id, vext_idx).cloned()
            };

            match location {
                Some(loc) => {
                    self.read_extent(vext_idx, &loc, off_in_slot, &mut buf[buf_start..buf_end]).await?;
                }
                None => {
                    // Unallocated — return zeros
                    buf[buf_start..buf_end].fill(0);
                }
            }

            bytes_read += to_read as u64;
            pos += to_read as u64;
        }

        Ok(bytes_read as usize)
    }

    /// Write, taking the per-volume lock only when the mapping must change.
    ///
    /// A steady-state write lands on an extent this volume already owns
    /// exclusively, which needs no serialisation — holding the volume lock for
    /// the whole call (as this used to) meant every write to a volume queued
    /// behind every other, no matter which extent it touched.
    ///
    /// Allocation and COW do change the mapping, so those re-check the extent
    /// under the lock before acting: two writers can both observe "unmapped"
    /// before either allocates, and without the re-check the second would
    /// allocate a duplicate slot and discard the first writer's data.
    ///
    /// A parity volume is serialised per stripe instead: its read-modify-
    /// write of parity is the thing that must not interleave, and two
    /// writers in different stripes never touch the same parity slot.
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.refuse_if_sealed()?;
        let buf_len = buf.len() as u64;
        let mut bytes_written = 0u64;
        let mut pos = offset;
        let policy = self.redundancy();

        while bytes_written < buf_len {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;
            let remaining_in_slot = self.slot_size - off_in_slot;
            let remaining_in_buf = buf_len - bytes_written;
            let to_write = remaining_in_slot.min(remaining_in_buf) as usize;

            let buf_start = bytes_written as usize;
            let buf_end = buf_start + to_write;
            let chunk = &buf[buf_start..buf_end];

            if let Redundancy::Parity { data, parity } = policy.scheme {
                self.write_parity_member(vext_idx, off_in_slot, chunk, data, parity, &policy).await?;
                bytes_written += to_write as u64;
                pos += to_write as u64;
                continue;
            }

            // A mirrored extent is serialised per extent so a resync copying
            // it cannot miss a write; an unreplicated one keeps the lock-free
            // steady state.
            let _shard = if policy.is_none() {
                None
            } else {
                Some(self.shard(self.lock_key(vext_idx, &policy)).lock().await)
            };

            // Look up existing extent in GEM
            let location = {
                let gem = self.gem.read().await;
                gem.lookup(self.id, vext_idx).cloned()
            };

            match location {
                // Exclusively owned: write straight through, no serialisation.
                Some(loc) if loc.ref_count == 1 => {
                    self.write_in_place(vext_idx, &loc, off_in_slot, chunk).await?;
                }
                // Shared, or not yet mapped — the mapping is about to change,
                // so serialise per volume and re-read it under the lock.
                _ => {
                    let _vol = self.inner.lock().await;
                    let fresh = {
                        let gem = self.gem.read().await;
                        gem.lookup(self.id, vext_idx).cloned()
                    };
                    match fresh {
                        Some(loc) if loc.ref_count > 1 => {
                            self.cow_write(vext_idx, off_in_slot, chunk, &loc, &policy).await?;
                        }
                        Some(loc) => {
                            // Another writer allocated it, or the sharer went
                            // away, while we waited for the lock.
                            self.write_in_place(vext_idx, &loc, off_in_slot, chunk).await?;
                        }
                        None => {
                            self.allocate_and_write(vext_idx, off_in_slot, chunk, &policy).await?;
                        }
                    }
                }
            }

            bytes_written += to_write as u64;
            pos += to_write as u64;
        }

        Ok(bytes_written as usize)
    }

    async fn flush(&self) -> DriveResult<()> {
        // Collect unique slab IDs for this volume, then flush their devices
        let slab_ids: Vec<SlabId> = {
            let gem = self.gem.read().await;
            gem.get_volume_map(&self.id)
                .map(|m| m.all_legs().map(|l| l.slab_id).collect::<HashSet<_>>().into_iter().collect())
                .unwrap_or_default()
        };

        {
            let reg = self.registry.read().await;
            for slab_id in slab_ids {
                if self.is_failed(slab_id) {
                    continue;
                }
                if let Some(slab) = reg.get(&slab_id) {
                    slab.device().flush().await?;
                }
            }
        }
        // Everything written so far is on the media, parity included: no
        // stripe is mid-write from the consumer's point of view.
        if let Err(e) = self.stripe_log.read().unwrap().clear() {
            tracing::warn!(volume = %self.id, "dirty-stripe log could not be cleared: {e}");
        }
        Ok(())
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.refuse_if_sealed()?;
        let policy = self.redundancy();
        // An unreplicated volume serialises against its own allocations with
        // the volume lock; a redundant one uses the extent/stripe shards,
        // which the write path takes *before* the volume lock — so taking
        // the volume lock here would invert the order.
        let _vol = if policy.is_none() { Some(self.inner.lock().await) } else { None };
        let mut pos = offset;
        let end = offset + len;

        while pos < end {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;

            // Only discard full slots
            if off_in_slot == 0 && (end - pos) >= self.slot_size {
                let _shard = if policy.is_none() || policy.scheme.is_parity() {
                    None
                } else {
                    Some(self.shard(vext_idx).lock().await)
                };
                self.release_extent(vext_idx).await?;
                drop(_shard);
            }

            let remaining = self.slot_size - off_in_slot;
            pos += remaining;
        }

        Ok(())
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: true, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::raid::{RaidArray, RaidLevel};

    async fn setup_test_volume(
        slot_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volume-test");
        std::fs::create_dir_all(&dir).unwrap();

        // Create 2 file devices for RAID 1
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

        // Format a slab on the RAID array
        let slab = Slab::format(backing, slot_size, StorageTier::Hot)
            .await
            .unwrap();

        let mut registry = SlabRegistry::new();
        registry.add(slab);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let gem = Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new()));

        let vol = ThinVolume::new("test-vol".to_string(), 128 * 1024 * 1024, slot_size);
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem,
            registry,
            PlacementPolicy::default(),
        ));

        (handle, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn write_allocates_and_read_returns_data() {
        let (handle, paths) = setup_test_volume(4096).await;

        let data = vec![0xAB_u8; 4096];
        let written = handle.write(0, &data).await.unwrap();
        assert_eq!(written, 4096);

        let mut buf = vec![0u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert_eq!(buf, data);

        assert_eq!(handle.extent_count().await, 1);
        assert!(handle.allocated().await > 0);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn read_unallocated_returns_zeros() {
        let (handle, paths) = setup_test_volume(4096).await;

        let mut buf = vec![0xFF_u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert!(buf.iter().all(|&b| b == 0));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn write_at_different_extents() {
        let (handle, paths) = setup_test_volume(4096).await;

        let data_a = vec![0xAA_u8; 4096];
        let data_b = vec![0xBB_u8; 4096];

        handle.write(0, &data_a).await.unwrap();
        handle.write(4096, &data_b).await.unwrap();

        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));

        handle.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        assert_eq!(handle.extent_count().await, 2);
        cleanup(&paths);
    }

    /// Concurrent writers to the same unmapped extent must not each allocate
    /// a slot — the second would leak one and discard the first writer's data.
    /// The narrowed write lock re-checks the mapping before allocating, and
    /// this is what pins that.
    #[tokio::test]
    async fn concurrent_first_writes_to_one_extent_allocate_once() {
        let (handle, paths) = setup_test_volume(4096).await;

        let free_before = handle.registry().read().await.total_free_slots();

        // Eight writers race on the same virtual extent.
        let mut tasks = Vec::new();
        for i in 0..8u8 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.write(0, &vec![0xB0 + i; 4096]).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert_eq!(handle.extent_count().await, 1, "exactly one extent mapped");
        assert_eq!(
            handle.registry().read().await.total_free_slots(),
            free_before - 1,
            "exactly one slot consumed"
        );

        // Whichever writer won, the extent holds one of their patterns whole.
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        let first = buf[0];
        assert!((0xB0..0xB8).contains(&first), "unexpected content {first:#x}");
        assert!(buf.iter().all(|&b| b == first), "extent must not be torn");

        cleanup(&paths);
    }

    /// Writes to different extents must proceed concurrently rather than
    /// queueing behind one another on a volume-wide lock.
    #[tokio::test]
    async fn concurrent_writes_to_distinct_extents_all_land() {
        let (handle, paths) = setup_test_volume(4096).await;

        let mut tasks = Vec::new();
        for i in 0..16u64 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.write(i * 4096, &vec![(0x40 + i) as u8; 4096]).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert_eq!(handle.extent_count().await, 16);
        for i in 0..16u64 {
            let mut buf = vec![0u8; 4096];
            handle.read(i * 4096, &mut buf).await.unwrap();
            assert!(
                buf.iter().all(|&b| b == (0x40 + i) as u8),
                "extent {i} content wrong"
            );
        }

        cleanup(&paths);
    }

    /// A single large write must allocate every extent it spans. Reported
    /// allocation stalling behind what was actually written is #33.
    #[tokio::test]
    async fn one_large_write_allocates_every_extent_it_spans() {
        let slot = 1024 * 1024;
        let (handle, paths) = setup_test_volume(slot).await;

        // 16 MB in a single call spans 16 slots.
        let data = vec![0xA7u8; 16 * slot as usize];
        let n = handle.write(0, &data).await.unwrap();
        assert_eq!(n, data.len());

        assert_eq!(handle.extent_count().await, 16, "one extent per slot spanned");
        assert_eq!(handle.allocated().await, 16 * slot, "allocated must track the write");

        // And it must read back whole.
        let mut buf = vec![0u8; data.len()];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xA7));

        cleanup(&paths);
    }

    /// Writing at a high offset must allocate that extent, not silently store
    /// data the map does not know about.
    #[tokio::test]
    async fn write_at_high_offset_allocates_and_reports() {
        let slot = 1024 * 1024;
        let (handle, paths) = setup_test_volume(slot).await;

        let off = 40 * slot;
        handle.write(off, &vec![0xBBu8; slot as usize]).await.unwrap();

        assert_eq!(handle.extent_count().await, 1);
        assert_eq!(handle.allocated().await, slot);

        let mut buf = vec![0u8; slot as usize];
        handle.read(off, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB), "data at a high offset must persist");

        cleanup(&paths);
    }

    #[tokio::test]
    async fn flush_works() {
        let (handle, paths) = setup_test_volume(4096).await;
        handle.write(0, &[0xCC_u8; 4096]).await.unwrap();
        handle.flush().await.unwrap();
        cleanup(&paths);
    }

    /// The #25 symptom: allocation must come back down when data is discarded,
    /// and the freed slots must return to the slab, not just leave the GEM.
    #[tokio::test]
    async fn discard_reclaims_extents_and_slab_slots() {
        let (handle, paths) = setup_test_volume(4096).await;

        let free_before = {
            let reg = handle.registry().read().await;
            reg.total_free_slots()
        };

        // Allocate four extents.
        for i in 0..4u64 {
            handle.write(i * 4096, &[0xAB_u8; 4096]).await.unwrap();
        }
        assert_eq!(handle.extent_count().await, 4);
        assert_eq!(handle.allocated().await, 4 * 4096);
        {
            let reg = handle.registry().read().await;
            assert_eq!(reg.total_free_slots(), free_before - 4);
        }

        // Discard the middle two.
        handle.discard(4096, 2 * 4096).await.unwrap();

        assert_eq!(handle.extent_count().await, 2);
        assert_eq!(handle.allocated().await, 2 * 4096);
        {
            let reg = handle.registry().read().await;
            assert_eq!(
                reg.total_free_slots(),
                free_before - 2,
                "discarded slots must return to the slab"
            );
        }

        // Discarded regions read back as zeros (we advertise LBPRZ).
        let mut buf = vec![0xFF_u8; 4096];
        handle.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0));

        // Untouched extents still hold their data.
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));

        cleanup(&paths);
    }

    /// A discard smaller than the reclaim granularity must not silently drop
    /// data — it frees nothing and the extent stays readable.
    #[tokio::test]
    async fn partial_discard_frees_nothing() {
        let (handle, paths) = setup_test_volume(65536).await;

        handle.write(0, &[0xCD_u8; 65536]).await.unwrap();
        assert_eq!(handle.extent_count().await, 1);
        assert_eq!(handle.discard_granularity(), 65536);

        // Half a slot — not enough to reclaim.
        handle.discard(0, 32768).await.unwrap();
        assert_eq!(handle.extent_count().await, 1);

        let mut buf = vec![0u8; 65536];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xCD), "partial discard must not lose data");

        cleanup(&paths);
    }
}

#[cfg(test)]
mod redundancy_tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::volume::gem::GlobalExtentMap;

    type Shared<T> = Arc<tokio::sync::RwLock<T>>;

    /// `n` slabs, each on its own file — so each is its own failure domain.
    async fn setup_slabs(n: usize, slot_size: u64) -> (Shared<GlobalExtentMap>, Shared<SlabRegistry>, Vec<SlabId>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-redundancy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let mut registry = SlabRegistry::new();
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        for i in 0..n {
            let path = dir.join(format!("{test_id}-slab-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, 8 * 1024 * 1024).await.unwrap();
            let slab = Slab::format(Arc::new(dev), slot_size, StorageTier::Hot).await.unwrap();
            ids.push(slab.slab_id());
            registry.add(slab);
            paths.push(path_str);
        }
        (
            Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new())),
            Arc::new(tokio::sync::RwLock::new(registry)),
            ids,
            paths,
        )
    }

    async fn add_slab(registry: &Shared<SlabRegistry>, paths: &mut Vec<String>, slot_size: u64) -> SlabId {
        let path = std::env::temp_dir()
            .join("stormblock-redundancy-test")
            .join(format!("{}-spare.bin", uuid::Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&path_str, 8 * 1024 * 1024).await.unwrap();
        let slab = Slab::format(Arc::new(dev), slot_size, StorageTier::Hot).await.unwrap();
        let id = slab.slab_id();
        registry.write().await.add(slab);
        paths.push(path_str);
        id
    }

    fn volume(gem: &Shared<GlobalExtentMap>, registry: &Shared<SlabRegistry>, policy: &str, slot_size: u64) -> Arc<ThinVolumeHandle> {
        let vol = ThinVolume::new("r".into(), 64 * 1024 * 1024, slot_size);
        Arc::new(ThinVolumeHandle::with_redundancy(
            vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
            RedundancyPolicy::parse(policy).unwrap(),
        ))
    }

    async fn raw(registry: &Shared<SlabRegistry>, leg: Leg, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let reg = registry.read().await;
        reg.get(&leg.slab_id).unwrap().read_slot(leg.slot_idx, 0, &mut buf).await.unwrap();
        buf
    }

    async fn loc(gem: &Shared<GlobalExtentMap>, id: VolumeId, vext: u64) -> ExtentLocation {
        gem.read().await.lookup(id, vext).unwrap().clone()
    }

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed)).collect()
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn mirror_places_two_legs_on_two_slabs() {
        let slot = 4096u64;
        let (gem, reg, _ids, paths) = setup_slabs(2, slot).await;
        let v = volume(&gem, &reg, "mirror:2", slot);
        let data = pattern(1, slot as usize);
        v.write(0, &data).await.unwrap();

        let l = loc(&gem, v.volume_id(), 0).await;
        assert_eq!(l.leg_count(), 2);
        let legs: Vec<Leg> = l.legs().collect();
        assert_ne!(legs[0].slab_id, legs[1].slab_id, "legs must be on different slabs");
        for leg in legs {
            assert_eq!(raw(&reg, leg, slot as usize).await, data, "both legs carry the data");
        }
        assert_eq!(v.physical().await, 2 * slot);
        assert_eq!(v.allocated().await, slot);
        assert_eq!(v.health().await.state, HealthState::Healthy);

        let mut back = vec![0u8; slot as usize];
        v.read(0, &mut back).await.unwrap();
        assert_eq!(back, data);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn mirror_zero_fills_the_rest_of_the_slot_on_every_leg() {
        let slot = 16384u64;
        let (gem, reg, _ids, paths) = setup_slabs(2, slot).await;
        let v = volume(&gem, &reg, "mirror", slot);
        v.write(4096, &[0xAB; 4096]).await.unwrap();
        let l = loc(&gem, v.volume_id(), 0).await;
        for leg in l.legs() {
            let r = raw(&reg, leg, slot as usize).await;
            assert!(r[..4096].iter().all(|&b| b == 0));
            assert!(r[4096..8192].iter().all(|&b| b == 0xAB));
            assert!(r[8192..].iter().all(|&b| b == 0));
        }
        let mut back = vec![0xFF; slot as usize];
        v.read(0, &mut back).await.unwrap();
        assert!(back[..4096].iter().all(|&b| b == 0));
        assert!(back[8192..].iter().all(|&b| b == 0));
        cleanup(&paths);
    }

    #[tokio::test]
    async fn mirror_refused_without_enough_domains() {
        let slot = 4096u64;
        let (gem, reg, _ids, paths) = setup_slabs(1, slot).await;
        assert_eq!(reg.read().await.distinct_domains_with_space("drive"), 1);
        let v = volume(&gem, &reg, "mirror:2", slot);
        let err = v.write(0, &[1u8; 4096]).await.unwrap_err();
        assert!(err.to_string().contains("distinct"), "{err}");
        assert_eq!(v.extent_count().await, 0, "nothing half-mapped");
        assert_eq!(reg.read().await.total_free_slots(), reg.read().await.total_slots(), "nothing leaked");
        cleanup(&paths);
    }

    #[tokio::test]
    async fn mirror_survives_a_lost_slab_and_resyncs() {
        let slot = 4096u64;
        let (gem, reg, ids, mut paths) = setup_slabs(2, slot).await;
        let v = volume(&gem, &reg, "mirror:2", slot);
        let datas: Vec<Vec<u8>> = (0..3).map(|i| pattern(10 + i, slot as usize)).collect();
        for (i, d) in datas.iter().enumerate() {
            v.write(i as u64 * slot, d).await.unwrap();
        }

        // The drive goes away.
        let lost = ids[0];
        reg.write().await.remove(&lost);

        for (i, d) in datas.iter().enumerate() {
            let mut back = vec![0u8; slot as usize];
            v.read(i as u64 * slot, &mut back).await.unwrap();
            assert_eq!(&back, d, "extent {i} still readable from the surviving leg");
        }
        let h = v.health().await;
        assert_eq!(h.state, HealthState::Degraded);
        assert_eq!(h.legs_missing, 3);
        assert_eq!(h.unreadable, 0);

        // A write while degraded lands on what is left.
        v.write(0, &pattern(99, slot as usize)).await.unwrap();

        // Replacement drive arrives; resync rebuilds onto it.
        let spare = add_slab(&reg, &mut paths, slot).await;
        let report = v.resync(false).await;
        assert_eq!(report.legs_rebuilt, 3, "{report:?}");
        assert_eq!(report.unrecoverable, 0);
        let h = v.health().await;
        assert_eq!(h.state, HealthState::Healthy, "{h:?}");
        for i in 0..3u64 {
            let l = loc(&gem, v.volume_id(), i).await;
            assert_eq!(l.leg_count(), 2);
            assert!(l.legs().all(|l| l.slab_id != lost));
            assert!(l.legs().any(|l| l.slab_id == spare));
            let want = if i == 0 { pattern(99, slot as usize) } else { datas[i as usize].clone() };
            for leg in l.legs() {
                assert_eq!(raw(&reg, leg, slot as usize).await, want, "leg content after resync");
            }
        }
        cleanup(&paths);
    }

    #[tokio::test]
    async fn failed_slab_is_skipped_until_resync_clears_it() {
        let slot = 4096u64;
        let (gem, reg, ids, mut paths) = setup_slabs(2, slot).await;
        let v = volume(&gem, &reg, "mirror:2", slot);
        v.write(0, &pattern(1, slot as usize)).await.unwrap();
        let before = loc(&gem, v.volume_id(), 0).await;

        // A write failed there (simulated): stop trusting it.
        let bad = ids[1];
        v.set_failed_slabs([bad]);
        assert_eq!(v.health().await.state, HealthState::Degraded);

        // Writes now go only to the good leg; the bad one goes stale.
        let newer = pattern(2, slot as usize);
        v.write(0, &newer).await.unwrap();
        let good = before.legs().find(|l| l.slab_id != bad).unwrap();
        let stale = before.legs().find(|l| l.slab_id == bad).unwrap();
        assert_eq!(raw(&reg, good, slot as usize).await, newer);
        assert_ne!(raw(&reg, stale, slot as usize).await, newer, "failed leg must not be written");
        let mut back = vec![0u8; slot as usize];
        v.read(0, &mut back).await.unwrap();
        assert_eq!(back, newer, "reads never touch the failed leg");

        // New extents avoid it too.
        let (_, _, _, _) = (&gem, &reg, &ids, &paths);
        let spare = add_slab(&reg, &mut paths, slot).await;
        v.write(slot, &pattern(3, slot as usize)).await.unwrap();
        let l1 = loc(&gem, v.volume_id(), 1).await;
        assert!(l1.legs().all(|l| l.slab_id != bad));
        assert!(l1.legs().any(|l| l.slab_id == spare));

        let report = v.resync(false).await;
        assert_eq!(report.legs_rebuilt, 1, "{report:?}");
        assert_eq!(report.slabs_cleared, vec![bad]);
        assert!(v.failed_slabs().is_empty());
        assert_eq!(v.health().await.state, HealthState::Healthy);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn parity_2p1_keeps_parity_true_and_reconstructs() {
        let slot = 4096u64;
        let (gem, reg, _ids, mut paths) = setup_slabs(3, slot).await;
        let v = volume(&gem, &reg, "raid5:2+1", slot);
        let a = pattern(1, slot as usize);
        let b = pattern(2, slot as usize);
        v.write(0, &a).await.unwrap();
        v.write(slot, &b).await.unwrap();

        let la = loc(&gem, v.volume_id(), 0).await;
        let lb = loc(&gem, v.volume_id(), 1).await;
        let g = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().clone();
        assert_eq!(g.legs.len(), 1);
        assert_eq!(g.data_width, 2);
        let mut slabs = vec![la.slab_id, lb.slab_id, g.legs[0].slab_id];
        slabs.sort_by_key(|s| s.0);
        slabs.dedup();
        assert_eq!(slabs.len(), 3, "data and parity on three different slabs");

        let xor = |x: &[u8], y: &[u8]| -> Vec<u8> { x.iter().zip(y).map(|(p, q)| p ^ q).collect() };
        assert_eq!(raw(&reg, g.legs[0], slot as usize).await, xor(&a, &b), "P = A ^ B");
        assert_eq!(v.physical().await, 3 * slot);

        // A partial overwrite is folded into parity read-modify-write.
        let mut a2 = a.clone();
        a2[1024..2048].copy_from_slice(&[0x5A; 1024]);
        v.write(1024, &[0x5A; 1024]).await.unwrap();
        assert_eq!(raw(&reg, la.primary(), slot as usize).await, a2);
        assert_eq!(raw(&reg, g.legs[0], slot as usize).await, xor(&a2, &b), "P follows the delta");

        // Lose the slab holding A: A comes back from B and P.
        reg.write().await.remove(&la.slab_id);
        let mut back = vec![0u8; slot as usize];
        v.read(0, &mut back).await.unwrap();
        assert_eq!(back, a2, "reconstructed from the stripe");
        let h = v.health().await;
        assert_eq!(h.state, HealthState::Degraded, "{h:?}");
        assert_eq!(h.unreadable, 0);

        // A write to the degraded member rebuilds it on the way through.
        let spare = add_slab(&reg, &mut paths, slot).await;
        v.write(0, &[0x77; 512]).await.unwrap();
        let la2 = loc(&gem, v.volume_id(), 0).await;
        assert_eq!(la2.slab_id, spare, "member rebuilt onto the spare");
        let mut a3 = a2.clone();
        a3[..512].copy_from_slice(&[0x77; 512]);
        assert_eq!(raw(&reg, la2.primary(), slot as usize).await, a3);
        assert_eq!(raw(&reg, g.legs[0], slot as usize).await, xor(&a3, &b));
        assert_eq!(v.health().await.state, HealthState::Healthy);

        // And a full resync with verify leaves everything consistent.
        let report = v.resync(true).await;
        assert_eq!(report.unrecoverable, 0, "{report:?}");
        assert_eq!(report.parity_verified, 1);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn parity_discard_updates_parity_and_frees_an_empty_stripe() {
        let slot = 4096u64;
        let (gem, reg, _ids, paths) = setup_slabs(3, slot).await;
        let free_before = reg.read().await.total_free_slots();
        let v = volume(&gem, &reg, "raid5:2+1", slot);
        let a = pattern(1, slot as usize);
        let b = pattern(2, slot as usize);
        v.write(0, &a).await.unwrap();
        v.write(slot, &b).await.unwrap();
        let g = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().clone();

        v.discard(slot, slot).await.unwrap();
        assert_eq!(raw(&reg, g.legs[0], slot as usize).await, a, "P = A ^ 0 after B is discarded");
        assert_eq!(v.extent_count().await, 1);

        v.discard(0, slot).await.unwrap();
        assert!(gem.read().await.lookup_parity(v.volume_id(), 0).is_none(), "empty stripe has no parity");
        assert_eq!(reg.read().await.total_free_slots(), free_before, "everything returned");
        cleanup(&paths);
    }

    #[tokio::test]
    async fn parity_clone_copies_its_own_parity_on_write() {
        let slot = 4096u64;
        let (gem, reg, _ids, paths) = setup_slabs(4, slot).await;
        let v = volume(&gem, &reg, "raid5:2+1", slot);
        let a = pattern(1, slot as usize);
        let b = pattern(2, slot as usize);
        v.write(0, &a).await.unwrap();
        v.write(slot, &b).await.unwrap();
        let src_parity = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().clone();

        let snap = {
            let mut g = gem.write().await;
            let mut r = reg.write().await;
            crate::volume::snapshot::create_snapshot(v.volume_id(), "snap", 64 * 1024 * 1024, slot, &mut g, &mut r)
                .await
                .unwrap()
        };
        let c = Arc::new(ThinVolumeHandle::with_redundancy(
            snap, gem.clone(), reg.clone(), PlacementPolicy::default(), v.redundancy(),
        ));
        assert_eq!(gem.read().await.lookup_parity(c.volume_id(), 0).unwrap().ref_count, 2);

        let cc = pattern(7, slot as usize);
        c.write(0, &cc).await.unwrap();

        let xor = |x: &[u8], y: &[u8]| -> Vec<u8> { x.iter().zip(y).map(|(p, q)| p ^ q).collect() };
        let cg = gem.read().await.lookup_parity(c.volume_id(), 0).unwrap().clone();
        assert_ne!(cg.legs, src_parity.legs, "the clone got its own parity");
        assert_eq!(cg.ref_count, 1);
        assert_eq!(raw(&reg, cg.legs[0], slot as usize).await, xor(&cc, &b));
        let sg = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().clone();
        assert_eq!(sg.legs, src_parity.legs);
        assert_eq!(sg.ref_count, 1, "source's parity is exclusive again");
        assert_eq!(raw(&reg, sg.legs[0], slot as usize).await, xor(&a, &b), "source parity untouched");

        let mut back = vec![0u8; slot as usize];
        v.read(0, &mut back).await.unwrap();
        assert_eq!(back, a);
        c.read(0, &mut back).await.unwrap();
        assert_eq!(back, cc);
        cleanup(&paths);
    }

    #[tokio::test]
    async fn raid6_reconstructs_two_lost_members_and_resyncs() {
        let slot = 4096u64;
        let (gem, reg, _ids, mut paths) = setup_slabs(5, slot).await;
        let v = volume(&gem, &reg, "raid6:3+2", slot);
        let datas: Vec<Vec<u8>> = (0..3).map(|i| pattern(20 + i, slot as usize)).collect();
        for (i, d) in datas.iter().enumerate() {
            v.write(i as u64 * slot, d).await.unwrap();
        }
        let g = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().clone();
        assert_eq!(g.legs.len(), 2);

        let l0 = loc(&gem, v.volume_id(), 0).await;
        let l1 = loc(&gem, v.volume_id(), 1).await;
        reg.write().await.remove(&l0.slab_id);
        reg.write().await.remove(&l1.slab_id);
        for (i, d) in datas.iter().enumerate() {
            let mut back = vec![0u8; slot as usize];
            v.read(i as u64 * slot, &mut back).await.unwrap();
            assert_eq!(&back, d, "member {i} with two members lost");
        }
        let h = v.health().await;
        assert_eq!(h.state, HealthState::Degraded, "{h:?}");

        add_slab(&reg, &mut paths, slot).await;
        add_slab(&reg, &mut paths, slot).await;
        let report = v.resync(false).await;
        assert_eq!(report.legs_rebuilt, 2, "{report:?}");
        assert_eq!(v.health().await.state, HealthState::Healthy);
        for (i, d) in datas.iter().enumerate() {
            let l = loc(&gem, v.volume_id(), i as u64).await;
            assert_eq!(raw(&reg, l.primary(), slot as usize).await, d.clone());
        }
        cleanup(&paths);
    }

    #[tokio::test]
    async fn slot_tables_rebuild_legs_and_parity() {
        let slot = 4096u64;
        let (gem, reg, _ids, paths) = setup_slabs(3, slot).await;
        let m = volume(&gem, &reg, "mirror:2", slot);
        m.write(0, &pattern(1, slot as usize)).await.unwrap();
        let p = volume(&gem, &reg, "raid5:2+1", slot);
        p.write(0, &pattern(2, slot as usize)).await.unwrap();
        p.write(slot, &pattern(3, slot as usize)).await.unwrap();

        let rebuilt = GlobalExtentMap::rebuild_from_slabs(reg.read().await.iter());
        let ml = rebuilt.lookup(m.volume_id(), 0).unwrap();
        assert_eq!(ml.leg_count(), 2);
        assert!(ml.same_slots(&loc(&gem, m.volume_id(), 0).await));
        let pg = rebuilt.lookup_parity(p.volume_id(), 0).unwrap();
        assert_eq!(pg.legs, gem.read().await.lookup_parity(p.volume_id(), 0).unwrap().legs);
        cleanup(&paths);
    }
}
