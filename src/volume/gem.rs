//! Global Extent Map (GEM) — single source of truth for extent placement.
//!
//! The GEM tracks which slab slot(s) hold each volume's virtual extent.
//! It replaces both the ExtentAllocator's per-array bitmap and ThinVolume's
//! local extent_map with a unified, cross-slab index.
//!
//! An extent has one or more **legs**: the primary slot and, for a mirrored
//! volume, its mirrors — each on a distinct failure domain. A parity volume
//! keeps one leg per data extent and a **parity group** per stripe with the
//! P (and Q) legs. Who owns a slot is the slab's slot table's to say; what
//! is on a slab is found by walking the maps (#155).
//!
//! Recovery invariant: the GEM is reconstructable from slab slot tables.
//! Each slab's extent table is authoritative for its slots. The durable
//! record of an extent map is still the volume metadata file — the slot
//! tables are the fallback for when there is none.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabId;
use crate::volume::extent::VolumeId;

/// One physical slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Leg {
    pub slab_id: SlabId,
    pub slot_idx: u64,
}

impl Leg {
    pub fn new(slab_id: SlabId, slot_idx: u64) -> Self {
        Leg { slab_id, slot_idx }
    }
}

/// Location of a single extent in the slab mesh: its primary leg, and any
/// mirror legs a redundancy policy added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtentLocation {
    pub slab_id: SlabId,
    pub slot_idx: u64,
    pub ref_count: u32,
    pub generation: u64,
    /// Additional full copies, each on its own failure domain. Empty for
    /// an unreplicated volume.
    pub mirrors: Vec<Leg>,
}

impl ExtentLocation {
    /// A fresh, exclusively owned, unreplicated location.
    pub fn new(slab_id: SlabId, slot_idx: u64) -> Self {
        ExtentLocation { slab_id, slot_idx, ref_count: 1, generation: 1, mirrors: Vec::new() }
    }

    /// A fresh location with mirror legs.
    pub fn with_legs(primary: Leg, mirrors: Vec<Leg>) -> Self {
        ExtentLocation {
            slab_id: primary.slab_id,
            slot_idx: primary.slot_idx,
            ref_count: 1,
            generation: 1,
            mirrors,
        }
    }

    pub fn primary(&self) -> Leg {
        Leg { slab_id: self.slab_id, slot_idx: self.slot_idx }
    }

    /// Every leg, primary first.
    pub fn legs(&self) -> impl Iterator<Item = Leg> + '_ {
        std::iter::once(self.primary()).chain(self.mirrors.iter().copied())
    }

    pub fn leg_count(&self) -> usize {
        1 + self.mirrors.len()
    }

    pub fn leg_on(&self, slab_id: SlabId) -> Option<Leg> {
        self.legs().find(|l| l.slab_id == slab_id)
    }

    /// Same slots, whichever order the legs are in.
    pub fn same_slots(&self, other: &ExtentLocation) -> bool {
        if self.leg_count() != other.leg_count() {
            return false;
        }
        self.legs().all(|l| other.legs().any(|o| o == l))
    }
}

/// The P and Q legs of one stripe of a parity volume, with their own
/// reference count: a clone shares a stripe's parity until a copy-on-write
/// in that stripe moves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParityGroup {
    /// `legs[0]` is P, `legs[1]` (if any) is Q.
    pub legs: Vec<Leg>,
    pub ref_count: u32,
    pub generation: u64,
    /// Data extents per stripe, so anything holding the group alone can
    /// name the stripe's members: `stripe * data_width ..`.
    pub data_width: u8,
}

impl ParityGroup {
    pub fn new(legs: Vec<Leg>, data_width: u8) -> Self {
        ParityGroup { legs, ref_count: 1, generation: 1, data_width }
    }

    /// The virtual extents this stripe covers.
    pub fn members(&self, stripe: u64) -> std::ops::Range<u64> {
        let w = self.data_width.max(1) as u64;
        stripe * w..(stripe + 1) * w
    }
}

/// Bit set in a slot's recorded virtual extent index to say the slot holds
/// parity, not data. Bits 62..56 carry the parity leg (0 = P, 1 = Q); the
/// rest is the stripe index.
pub const PARITY_TAG: u64 = 1 << 63;
const PARITY_LEG_SHIFT: u32 = 56;
const PARITY_STRIPE_MASK: u64 = (1 << PARITY_LEG_SHIFT) - 1;

/// The virtual-extent value a parity slot records in the slot table.
pub fn parity_vext(leg: u8, stripe: u64) -> u64 {
    PARITY_TAG | ((leg as u64 & 0x7F) << PARITY_LEG_SHIFT) | (stripe & PARITY_STRIPE_MASK)
}

/// Decode a recorded virtual extent as `(parity leg, stripe)` if it is one.
pub fn parse_parity_vext(v: u64) -> Option<(u8, u64)> {
    if v & PARITY_TAG == 0 {
        return None;
    }
    Some((((v >> PARITY_LEG_SHIFT) & 0x7F) as u8, v & PARITY_STRIPE_MASK))
}

/// Per-volume extent map — virtual extent index to physical location(s).
#[derive(Debug, Clone, Default)]
pub struct VolumeExtentMap {
    /// Compact (#155): see [`super::extable`].
    pub extents: super::extable::ExtentTable,
    /// Stripe index → parity legs. Empty unless the volume has a parity policy.
    pub parity: BTreeMap<u64, ParityGroup>,
}

impl VolumeExtentMap {
    pub fn new() -> Self {
        VolumeExtentMap { extents: Default::default(), parity: BTreeMap::new() }
    }

    /// Number of mapped extents.
    pub fn len(&self) -> usize {
        self.extents.len()
    }

    /// Whether this map has no extents.
    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
    }

    /// Extents this volume is the only holder of — what it actually costs.
    ///
    /// A copy-on-write clone maps every extent of its parent on the day it is
    /// made and owns none of them: the slots are the parent's, shared in by
    /// reference, and the clone occupies nothing until it is written to. So a
    /// count of *mapped* extents is the volume's size, never its cost, and
    /// reporting that as "allocated" made a fresh clone of an 11 GB golden
    /// look like 11 GB of disk — which is the exact claim the whole design
    /// exists to falsify.
    ///
    /// `ref_count == 1` is the test: one holder, so freeing this volume frees
    /// the slot.
    pub fn exclusive(&self) -> usize {
        self.extents.ref_counts().filter(|r| *r <= 1).count()
    }

    /// Extents shared with at least one other volume — real bytes on the
    /// drive, but not this volume's to free.
    ///
    /// Worth reporting beside the exclusive count rather than hidden: a clone
    /// showing 0 allocated and nothing else says "empty", when what is true
    /// is "costs nothing yet, and reads 11 GB".
    pub fn shared(&self) -> usize {
        self.extents.ref_counts().filter(|r| *r > 1).count()
    }

    /// Physical legs of the extents this volume exclusively owns — what the
    /// redundancy policy costs on top.
    pub fn exclusive_legs(&self) -> usize {
        self.extents
            .values()
            .filter(|l| l.ref_count <= 1)
            .map(|l| l.leg_count())
            .sum::<usize>()
            + self.parity.values().filter(|g| g.ref_count <= 1).flat_map(|g| g.legs.iter()).count()
    }

    /// Every slot this map references: data legs and parity legs alike.
    pub fn all_legs(&self) -> impl Iterator<Item = Leg> + '_ {
        self.extents
            .values()
            .flat_map(|l| l.legs().collect::<Vec<_>>())
            .chain(self.parity.values().flat_map(|g| g.legs.iter().copied()))
    }
}

/// Global Extent Map — tracks all extent locations across all volumes.
///
/// Forward maps only (#155): "what is on this slab" is answered by walking
/// the maps when it is asked (drain, evacuation, flow-over), and "who owns
/// this slot" by the slab's slot table, which records the owner of every
/// slot. A resident reverse index cost ~120 B an extent for questions asked
/// a few times a day.
pub struct GlobalExtentMap {
    volumes: HashMap<VolumeId, VolumeExtentMap>,
    /// Maps not in memory (#158 stage C): kept in a format v2 store and
    /// loaded through the [`Pager`]. Every accessor that would read or change
    /// one panics (see [`check`](Self::check)): a missing map reads as "no
    /// extents", which serves zeros, allocates over the volume's data and
    /// lets GC free its slots.
    cold: HashMap<VolumeId, ColdMap>,
    pager: Option<Arc<dyn Pager>>,
    /// Walks in progress that hold no manager lock ([`pin_resident`]):
    /// nothing leaves memory while one runs.
    pins: Arc<std::sync::atomic::AtomicUsize>,
    /// Whether changes are recorded (#158): on while a format v2 metadata
    /// store takes them, off otherwise so nothing accumulates.
    track: bool,
    changes: std::sync::Mutex<Changes>,
}

/// Loads a map that is not in memory (#158 stage C).
#[async_trait::async_trait]
pub trait Pager: Send + Sync {
    async fn load(&self, id: VolumeId) -> std::io::Result<VolumeExtentMap>;
}

/// What is kept of a map that is not in memory.
#[derive(Debug, Clone)]
pub struct ColdMap {
    pub extents: usize,
    /// What a listing reports without loading it: `exclusive()`,
    /// `shared()`, `exclusive_legs()` as they were.
    pub exclusive: usize,
    pub shared: usize,
    pub exclusive_legs: usize,
    /// The slabs it has legs on.
    pub slabs: Vec<SlabId>,
}

/// Load `id`'s map if it is not in memory. A no-op for a resident (or
/// absent) map.
pub async fn ensure_resident(gem: &crate::lockwatch::TrackedRwLock<GlobalExtentMap>, id: VolumeId) -> std::io::Result<()> {
    let pager = {
        let g = gem.read().await;
        if !g.is_cold(&id) {
            return Ok(());
        }
        g.pager.clone().ok_or_else(|| std::io::Error::other(format!("volume {}: map not in memory and no pager", id.0)))?
    };
    let map = pager.load(id).await?;
    let mut g = gem.write().await;
    if g.is_cold(&id) {
        g.install(id, map);
    }
    Ok(())
}

/// While held, no map leaves memory (#158 stage C).
pub struct Pin(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Pin {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Every map in memory, kept there while the pin is held: what a walk of
/// every map that holds no manager lock takes first (a flow-over, GC, a
/// drain, a rebuild). Pinned before loading, so nothing leaves between.
pub async fn pin_resident(gem: &crate::lockwatch::TrackedRwLock<GlobalExtentMap>) -> std::io::Result<Pin> {
    let pin = gem.read().await.pin();
    ensure_all_resident(gem).await?;
    Ok(pin)
}

/// Load every map not in memory: what a walk of every map needs.
pub async fn ensure_all_resident(gem: &crate::lockwatch::TrackedRwLock<GlobalExtentMap>) -> std::io::Result<()> {
    let ids = gem.read().await.cold_ids();
    for id in ids {
        ensure_resident(gem, id).await?;
    }
    Ok(())
}

/// What changed in the maps since the changes were last taken (#157, #158):
/// what a format v2 persist writes, where v1 wrote every map whole.
#[derive(Debug, Default)]
pub struct Changes {
    /// Volumes whose map changed as a whole (cloned from, renamed, removed,
    /// absorbed): written again entirely.
    pub whole: std::collections::HashSet<VolumeId>,
    pub extents: HashMap<VolumeId, std::collections::HashSet<u64>>,
    pub parity: HashMap<VolumeId, std::collections::HashSet<u64>>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.whole.is_empty() && self.extents.is_empty() && self.parity.is_empty()
    }
    /// Every volume named.
    pub fn volumes(&self) -> std::collections::HashSet<VolumeId> {
        self.whole.iter().chain(self.extents.keys()).chain(self.parity.keys()).copied().collect()
    }
}

impl GlobalExtentMap {
    pub fn new() -> Self {
        GlobalExtentMap {
            volumes: HashMap::new(),
            cold: HashMap::new(),
            pager: None,
            pins: Default::default(),
            track: false,
            changes: Default::default(),
        }
    }

    /// Panic if `id`'s map is not in memory. See the note on `cold`.
    #[track_caller]
    fn check(&self, id: &VolumeId) {
        if self.cold.contains_key(id) {
            panic!(
                "volume {}'s extent map is not in memory: this path must load it first (ensure_resident, #158)",
                id.0
            );
        }
    }

    /// Panic if any map is not in memory: a walk of every map would miss it.
    #[track_caller]
    fn check_all(&self) {
        if let Some(id) = self.cold.keys().next() {
            panic!(
                "{} extent map(s) not in memory (first: volume {}): a walk of every map must load them \
                 first (ensure_all_resident, #158)",
                self.cold.len(),
                id.0
            );
        }
    }

    /// See [`pin_resident`].
    pub fn pin(&self) -> Pin {
        self.pins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Pin(self.pins.clone())
    }

    pub fn set_pager(&mut self, pager: Option<Arc<dyn Pager>>) {
        self.pager = pager;
    }

    pub fn pager(&self) -> Option<Arc<dyn Pager>> {
        self.pager.clone()
    }

    pub fn is_cold(&self, id: &VolumeId) -> bool {
        self.cold.contains_key(id)
    }

    pub fn cold_ids(&self) -> Vec<VolumeId> {
        self.cold.keys().copied().collect()
    }

    pub fn cold(&self, id: &VolumeId) -> Option<&ColdMap> {
        self.cold.get(id)
    }

    /// The volumes whose maps are in memory.
    pub fn resident_ids(&self) -> Vec<VolumeId> {
        self.volumes.keys().copied().collect()
    }

    /// Whether `id` has changes not yet taken by a persist.
    pub fn has_changes(&self, id: &VolumeId) -> bool {
        let c = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        c.whole.contains(id) || c.extents.contains_key(id) || c.parity.contains_key(id)
    }

    /// Take `id`'s map out of memory. Refused (false) while it has changes no
    /// persist has taken; the caller makes sure what was taken is written.
    pub fn evict(&mut self, id: VolumeId) -> bool {
        if self.pins.load(std::sync::atomic::Ordering::SeqCst) > 0 || self.has_changes(&id) || self.cold.contains_key(&id) {
            return false;
        }
        let Some(map) = self.volumes.remove(&id) else { return false };
        let mut slabs: Vec<SlabId> = map.all_legs().map(|l| l.slab_id).collect();
        slabs.sort_by_key(|s| s.0);
        slabs.dedup();
        self.cold.insert(
            id,
            ColdMap {
                extents: map.extents.len(),
                exclusive: map.exclusive(),
                shared: map.shared(),
                exclusive_legs: map.exclusive_legs(),
                slabs,
            },
        );
        true
    }

    /// Put a loaded map back. Not a change: it is what the store holds.
    pub fn install(&mut self, id: VolumeId, map: VolumeExtentMap) {
        self.cold.remove(&id);
        if !map.is_empty() {
            self.volumes.insert(id, map);
        }
    }

    /// Record changes from now on (or stop, dropping what was recorded).
    pub fn track_changes(&mut self, on: bool) {
        self.track = on;
        if !on {
            *self.changes.get_mut().unwrap_or_else(|e| e.into_inner()) = Changes::default();
        }
    }

    pub fn tracking(&self) -> bool {
        self.track
    }

    /// What changed since the last call (empty when not tracking).
    pub fn take_changes(&self) -> Changes {
        std::mem::take(&mut *self.changes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn touch(&mut self, v: VolumeId, vext: u64) {
        if self.track {
            self.changes.get_mut().unwrap_or_else(|e| e.into_inner()).extents.entry(v).or_default().insert(vext);
        }
    }

    fn touch_parity(&mut self, v: VolumeId, stripe: u64) {
        if self.track {
            self.changes.get_mut().unwrap_or_else(|e| e.into_inner()).parity.entry(v).or_default().insert(stripe);
        }
    }

    fn touch_whole(&mut self, v: VolumeId) {
        if self.track {
            self.changes.get_mut().unwrap_or_else(|e| e.into_inner()).whole.insert(v);
        }
    }

    /// Insert or update an extent mapping.
    #[track_caller]
    pub fn insert(&mut self, volume_id: VolumeId, vext_idx: u64, location: ExtentLocation) {
        self.check(&volume_id);
        self.volumes.entry(volume_id).or_default().extents.insert(vext_idx, location);
        self.touch(volume_id, vext_idx);
    }

    /// Insert a mapping recovered from persisted metadata.
    pub fn restore_mapping(&mut self, volume_id: VolumeId, vext_idx: u64, location: ExtentLocation) {
        self.insert(volume_id, vext_idx, location);
    }

    /// Take the maps of `ids` out of `other` — a map rebuilt from slabs just
    /// adopted. Anything already here for those volumes is replaced.
    pub fn absorb(&mut self, mut other: GlobalExtentMap, ids: &std::collections::HashSet<VolumeId>) {
        for id in ids {
            if let Some(m) = other.volumes.remove(id) {
                self.cold.remove(id);
                self.volumes.insert(*id, m);
                self.touch_whole(*id);
            }
        }
    }

    /// Record a stripe's parity legs.
    #[track_caller]
    pub fn insert_parity(&mut self, volume_id: VolumeId, stripe: u64, group: ParityGroup) {
        self.check(&volume_id);
        self.volumes.entry(volume_id).or_default().parity.insert(stripe, group);
        self.touch_parity(volume_id, stripe);
    }

    /// Restore a stripe's parity legs.
    pub fn restore_parity(&mut self, volume_id: VolumeId, stripe: u64, group: ParityGroup) {
        self.insert_parity(volume_id, stripe, group);
    }

    #[track_caller]
    pub fn lookup_parity(&self, volume_id: VolumeId, stripe: u64) -> Option<&ParityGroup> {
        self.check(&volume_id);
        self.volumes.get(&volume_id)?.parity.get(&stripe)
    }

    #[track_caller]
    pub fn remove_parity(&mut self, volume_id: VolumeId, stripe: u64) -> Option<ParityGroup> {
        self.check(&volume_id);
        let vmap = self.volumes.get_mut(&volume_id)?;
        let g = vmap.parity.remove(&stripe)?;
        if vmap.extents.is_empty() && vmap.parity.is_empty() {
            self.volumes.remove(&volume_id);
        }
        self.touch_parity(volume_id, stripe);
        Some(g)
    }

    #[track_caller]
    pub fn inc_parity_ref(&mut self, volume_id: VolumeId, stripe: u64) {
        self.check(&volume_id);
        if let Some(g) = self.volumes.get_mut(&volume_id).and_then(|m| m.parity.get_mut(&stripe)) {
            g.ref_count += 1;
            self.touch_parity(volume_id, stripe);
        }
    }

    /// Record one more sharer of a single extent.
    ///
    /// The recorded count is what makes a write copy-on-write instead of
    /// landing in place, so re-sharing one extent must bump it on both sides
    /// exactly as cloning a whole map does.
    #[track_caller]
    pub fn inc_extent_ref(&mut self, volume_id: VolumeId, vext_idx: u64) {
        self.check(&volume_id);
        if let Some(m) = self.volumes.get_mut(&volume_id) {
            if m.extents.update(vext_idx, |loc| loc.ref_count += 1).is_some() {
                self.touch(volume_id, vext_idx);
            }
        }
    }

    /// Set the recorded share count of an extent — after a slot's count
    /// moved on disk, so the map agrees on whether a write must copy.
    #[track_caller]
    pub fn set_extent_ref(&mut self, volume_id: VolumeId, vext_idx: u64, ref_count: u32) {
        self.check(&volume_id);
        if let Some(m) = self.volumes.get_mut(&volume_id) {
            if m.extents.update(vext_idx, |loc| loc.ref_count = ref_count).is_some() {
                self.touch(volume_id, vext_idx);
            }
        }
    }

    #[track_caller]
    pub fn set_parity_ref(&mut self, volume_id: VolumeId, stripe: u64, ref_count: u32) {
        self.check(&volume_id);
        if let Some(g) = self.volumes.get_mut(&volume_id).and_then(|m| m.parity.get_mut(&stripe)) {
            g.ref_count = ref_count;
            self.touch_parity(volume_id, stripe);
        }
    }

    /// Set the share count the slot table's owner of `leg` records, when the
    /// owner's map still names that leg (#155: the slot table says who owns a
    /// slot; the maps say what they reference). `tagged` is the owner's
    /// extent as the slot records it: a parity slot's is `parity_vext`.
    pub fn set_owner_ref(&mut self, owner: VolumeId, tagged: u64, leg: Leg, ref_count: u32) {
        if self.cold.contains_key(&owner) {
            // A map not in memory keeps the count it had (#158). This only
            // ever lowers a count (after a copy-on-write gave a share back),
            // and a count too high costs a needless copy, never data — the
            // same reason `raise_shares` only raises.
            return;
        }
        match parse_parity_vext(tagged) {
            Some((_, stripe)) => {
                if let Some(g) = self.volumes.get_mut(&owner).and_then(|m| m.parity.get_mut(&stripe)) {
                    if g.legs.contains(&leg) {
                        g.ref_count = ref_count;
                        self.touch_parity(owner, stripe);
                    }
                }
            }
            None => {
                if let Some(m) = self.volumes.get_mut(&owner) {
                    let hit = m.extents.update(tagged, |loc| {
                        if loc.legs().any(|l| l == leg) {
                            loc.ref_count = ref_count;
                            return true;
                        }
                        false
                    });
                    if hit == Some(true) {
                        self.touch(owner, tagged);
                    }
                }
            }
        }
    }

    /// Look up where a volume's virtual extent lives.
    #[track_caller]
    pub fn lookup(&self, volume_id: VolumeId, vext_idx: u64) -> Option<ExtentLocation> {
        self.check(&volume_id);
        self.volumes.get(&volume_id)?.extents.get(&vext_idx)
    }

    /// Replace one leg of an extent with another slot (a leg moved or
    /// rebuilt), keeping the rest of the location as it is.
    #[track_caller]
    pub fn replace_leg(&mut self, volume_id: VolumeId, vext_idx: u64, old: Leg, new: Leg) -> bool {
        self.check(&volume_id);
        let Some(m) = self.volumes.get_mut(&volume_id) else { return false };
        let done = m
            .extents
            .update(vext_idx, |loc| {
                if loc.primary() == old {
                    loc.slab_id = new.slab_id;
                    loc.slot_idx = new.slot_idx;
                } else if let Some(m) = loc.mirrors.iter_mut().find(|m| **m == old) {
                    *m = new;
                } else {
                    return false;
                }
                loc.generation += 1;
                true
            })
            .unwrap_or(false);
        if done {
            self.touch(volume_id, vext_idx);
        }
        done
    }

    /// Add a mirror leg to an extent (a resync filling in a missing copy).
    #[track_caller]
    pub fn add_leg(&mut self, volume_id: VolumeId, vext_idx: u64, leg: Leg) -> bool {
        self.check(&volume_id);
        let Some(m) = self.volumes.get_mut(&volume_id) else { return false };
        let done = m
            .extents
            .update(vext_idx, |loc| {
                if !loc.legs().any(|l| l == leg) {
                    loc.mirrors.push(leg);
                }
            })
            .is_some();
        if done {
            self.touch(volume_id, vext_idx);
        }
        done
    }

    /// Drop a leg from an extent without touching the slot (the caller frees
    /// it, or it is already gone with its slab). Refuses to drop the last leg.
    #[track_caller]
    pub fn drop_leg(&mut self, volume_id: VolumeId, vext_idx: u64, leg: Leg) -> bool {
        self.check(&volume_id);
        let Some(m) = self.volumes.get_mut(&volume_id) else { return false };
        let done = m
            .extents
            .update(vext_idx, |loc| {
                if loc.primary() == leg {
                    let Some(next) = loc.mirrors.first().copied() else { return false };
                    loc.mirrors.remove(0);
                    loc.slab_id = next.slab_id;
                    loc.slot_idx = next.slot_idx;
                } else {
                    let before = loc.mirrors.len();
                    loc.mirrors.retain(|m| *m != leg);
                    if loc.mirrors.len() == before {
                        return false;
                    }
                }
                true
            })
            .unwrap_or(false);
        if done {
            self.touch(volume_id, vext_idx);
        }
        done
    }

    /// Replace one parity leg of a stripe.
    #[track_caller]
    pub fn replace_parity_leg(&mut self, volume_id: VolumeId, stripe: u64, old: Leg, new: Leg) -> bool {
        self.check(&volume_id);
        let Some(g) = self.volumes.get_mut(&volume_id).and_then(|m| m.parity.get_mut(&stripe)) else {
            return false;
        };
        let Some(i) = g.legs.iter().position(|l| *l == old) else { return false };
        g.legs[i] = new;
        g.generation += 1;
        self.touch_parity(volume_id, stripe);
        true
    }

    /// Rewrite every reference to the legs in `moves` — in every volume's
    /// extents and parity groups — to their replacements.
    ///
    /// A slot shared by a golden and its clones is one physical slot named
    /// from several maps; when a resync rebuilds that leg onto a fresh slab,
    /// every map that named the old slot must name the new one, or the clones
    /// keep pointing at a slab that is gone. One sweep, however many legs
    /// moved. Returns how many references were rewritten.
    #[track_caller]
    pub fn rewrite_legs(&mut self, moves: &HashMap<Leg, Leg>) -> usize {
        self.check_all();
        if moves.is_empty() {
            return 0;
        }
        let mut rewritten = 0usize;
        let mut touched: Vec<(VolumeId, u64)> = Vec::new();
        let mut touched_parity: Vec<(VolumeId, u64)> = Vec::new();
        let track = self.track;
        for (vid, vmap) in self.volumes.iter_mut() {
            vmap.extents.for_each_mut(|vext, loc| {
                let before = rewritten;
                if let Some(new) = moves.get(&loc.primary()) {
                    loc.slab_id = new.slab_id;
                    loc.slot_idx = new.slot_idx;
                    loc.generation += 1;
                    rewritten += 1;
                }
                for m in loc.mirrors.iter_mut() {
                    if let Some(new) = moves.get(m) {
                        *m = *new;
                        rewritten += 1;
                    }
                }
                if track && rewritten != before {
                    touched.push((*vid, vext));
                }
                rewritten != before
            });
            for (stripe, g) in vmap.parity.iter_mut() {
                for leg in g.legs.iter_mut() {
                    if let Some(new) = moves.get(leg) {
                        *leg = *new;
                        g.generation += 1;
                        rewritten += 1;
                        if track {
                            touched_parity.push((*vid, *stripe));
                        }
                    }
                }
            }
        }
        for (v, e) in touched {
            self.touch(v, e);
        }
        for (v, s) in touched_parity {
            self.touch_parity(v, s);
        }
        rewritten
    }

    /// Add `new` as a mirror leg beside `existing` in every map that names
    /// `existing` — the golden and every clone sharing the slot. Returns how
    /// many maps gained the leg.
    #[track_caller]
    pub fn add_leg_beside(&mut self, existing: Leg, new: Leg) -> usize {
        self.check_all();
        let mut added = 0usize;
        let mut touched: Vec<(VolumeId, u64)> = Vec::new();
        for (vid, vmap) in self.volumes.iter_mut() {
            vmap.extents.for_each_mut(|vext, loc| {
                if loc.legs().any(|l| l == existing) && !loc.legs().any(|l| l == new) {
                    loc.mirrors.push(new);
                    added += 1;
                    touched.push((*vid, vext));
                    return true;
                }
                false
            });
        }
        for (v, e) in touched {
            self.touch(v, e);
        }
        added
    }

    /// Drop `leg` from every map that names it, never leaving a location
    /// with no legs. Returns how many maps lost it.
    #[track_caller]
    pub fn drop_leg_everywhere(&mut self, leg: Leg) -> usize {
        self.check_all();
        let mut dropped = 0usize;
        let mut touched: Vec<(VolumeId, u64)> = Vec::new();
        for (vid, vmap) in self.volumes.iter_mut() {
            vmap.extents.for_each_mut(|vext, loc| {
                if loc.primary() == leg {
                    if loc.mirrors.is_empty() {
                        return false;
                    }
                    let next = loc.mirrors.remove(0);
                    loc.slab_id = next.slab_id;
                    loc.slot_idx = next.slot_idx;
                    dropped += 1;
                    touched.push((*vid, vext));
                    true
                } else {
                    let before = loc.mirrors.len();
                    loc.mirrors.retain(|m| *m != leg);
                    if loc.mirrors.len() != before {
                        dropped += 1;
                        touched.push((*vid, vext));
                        return true;
                    }
                    false
                }
            });
        }
        for (v, e) in touched {
            self.touch(v, e);
        }
        dropped
    }

    /// Remove an extent mapping.
    #[track_caller]
    pub fn remove(&mut self, volume_id: VolumeId, vext_idx: u64) -> Option<ExtentLocation> {
        self.check(&volume_id);
        let vmap = self.volumes.get_mut(&volume_id)?;
        let loc = vmap.extents.remove(&vext_idx)?;
        if vmap.extents.is_empty() && vmap.parity.is_empty() {
            self.volumes.remove(&volume_id);
        }
        self.touch(volume_id, vext_idx);
        Some(loc)
    }

    /// Give one volume's whole map to another id — a restripe built the new
    /// placement under a scratch id and the real volume now takes it.
    /// Whatever `to` had is returned to the caller to release.
    #[track_caller]
    pub fn rename_volume(&mut self, from: VolumeId, to: VolumeId) -> Option<VolumeExtentMap> {
        self.check(&to);
        self.check(&from);
        let map = self.volumes.remove(&from)?;
        let old = self.volumes.remove(&to);
        self.volumes.insert(to, map);
        self.touch_whole(from);
        self.touch_whole(to);
        old
    }

    /// Remove all extents for a volume. Returns the removed extent map.
    #[track_caller]
    pub fn remove_volume(&mut self, volume_id: VolumeId) -> Option<VolumeExtentMap> {
        self.check(&volume_id);
        self.touch_whole(volume_id);
        self.volumes.remove(&volume_id)
    }

    /// Get the volume extent map for a given volume.
    #[track_caller]
    pub fn get_volume_map(&self, volume_id: &VolumeId) -> Option<&VolumeExtentMap> {
        self.check(volume_id);
        self.volumes.get(volume_id)
    }

    /// Some extent that references a slot, by walking the maps: the first
    /// found. A parity slot answers with a `parity_vext`-tagged index. For
    /// tests and diagnostics; who *owns* a slot is the slot table's to say.
    #[track_caller]
    pub fn reverse_lookup(&self, slab_id: SlabId, slot_idx: u64) -> Option<(VolumeId, u64)> {
        self.check_all();
        let leg = Leg::new(slab_id, slot_idx);
        for (vol, vmap) in &self.volumes {
            for (vext, loc) in &vmap.extents {
                if loc.legs().any(|l| l == leg) {
                    return Some((*vol, vext));
                }
            }
            for (stripe, g) in &vmap.parity {
                if let Some(i) = g.legs.iter().position(|l| *l == leg) {
                    return Some((*vol, parity_vext(i as u8, *stripe)));
                }
            }
        }
        None
    }

    /// Clone a volume's extent map for snapshot (bumps ref_count in the clone).
    #[track_caller]
    pub fn clone_volume_map(&mut self, source_id: VolumeId, dest_id: VolumeId) -> Option<VolumeExtentMap> {
        self.check(&dest_id);
        self.check(&source_id);
        let source_map = self.volumes.get(&source_id)?.clone();

        // Insert cloned mappings for the destination volume.
        // Note: ref_count updates in the actual slabs happen separately.
        let mut dest_map = VolumeExtentMap::new();
        dest_map.extents = source_map.extents.clone();
        dest_map.extents.add_refs(1);
        for (&stripe, g) in &source_map.parity {
            let mut ng = g.clone();
            ng.ref_count += 1;
            dest_map.parity.insert(stripe, ng);
        }

        // Also update the source's ref_counts in the GEM
        if let Some(src_map) = self.volumes.get_mut(&source_id) {
            src_map.extents.add_refs(1);
            for g in src_map.parity.values_mut() {
                g.ref_count += 1;
            }
        }

        self.volumes.insert(dest_id, dest_map.clone());
        self.touch_whole(source_id);
        self.touch_whole(dest_id);
        Some(dest_map)
    }

    /// Share one volume's extents into another, offset by `dest_base_vext`.
    ///
    /// `clone_volume_map` is this with a single source and no offset. A
    /// composed volume needs several sources at several offsets, which is the
    /// difference between "a copy of that golden" and "a disk made of those
    /// goldens".
    ///
    /// Returns the legs whose slab ref counts the caller must raise. The GEM's
    /// own counts are raised here, on both sides: the destination's copy
    /// because it now shares, and the source's because it is now shared.
    #[track_caller]
    pub fn gather_into(&mut self, source_id: VolumeId, dest_id: VolumeId, dest_base_vext: u64) -> Vec<Leg> {
        self.check(&dest_id);
        self.check(&source_id);
        let Some(source_map) = self.volumes.get(&source_id).cloned() else {
            // A golden nothing has written yet contributes no extents, and a
            // volume made of it is legitimately empty there. Not an error.
            return Vec::new();
        };

        let mut legs = Vec::new();
        {
            let dest_map = self.volumes.entry(dest_id).or_default();
            let mut shared = source_map.extents.shifted(dest_base_vext);
            shared.add_refs(1);
            for loc in shared.values() {
                legs.extend(loc.legs());
            }
            dest_map.extents.extend_from(&shared);
            for (&stripe, g) in &source_map.parity {
                let mut shared = g.clone();
                shared.ref_count += 1;
                dest_map.parity.insert(dest_base_vext + stripe, shared);
            }
        }

        if let Some(src_map) = self.volumes.get_mut(&source_id) {
            src_map.extents.add_refs(1);
            for g in src_map.parity.values_mut() {
                g.ref_count += 1;
            }
        }
        self.touch_whole(source_id);
        self.touch_whole(dest_id);

        legs
    }

    /// Share `source_id`'s extents into `dest_id`, each at the virtual extent
    /// `place` names for it (#362): what [`gather_into`](Self::gather_into)
    /// does with an offset, for a destination where the source's extents are
    /// not one contiguous run. Every mapped source extent must be in `place`.
    /// `None`, changing nothing, for a source with parity groups: a stripe
    /// cannot be renumbered extent by extent.
    pub fn gather_remapped(
        &mut self,
        source_id: VolumeId,
        dest_id: VolumeId,
        place: &std::collections::HashMap<u64, u64>,
    ) -> Option<Vec<Leg>> {
        self.check(&dest_id);
        self.check(&source_id);
        let Some(source_map) = self.volumes.get(&source_id).cloned() else {
            return Some(Vec::new());
        };
        if !source_map.parity.is_empty() {
            return None;
        }
        let mut shared = super::extable::ExtentTable::new();
        for (vext, loc) in source_map.extents.iter() {
            let at = *place.get(&vext).expect("every mapped extent is placed");
            shared.insert(at, loc);
        }
        shared.add_refs(1);
        let mut legs = Vec::new();
        for loc in shared.values() {
            legs.extend(loc.legs());
        }
        self.volumes.entry(dest_id).or_default().extents.extend_from(&shared);
        if let Some(src_map) = self.volumes.get_mut(&source_id) {
            src_map.extents.add_refs(1);
        }
        self.touch_whole(source_id);
        self.touch_whole(dest_id);
        Some(legs)
    }

    /// Number of tracked volumes.
    pub fn volume_count(&self) -> usize {
        self.volumes.len() + self.cold.len()
    }

    /// Total number of extent mappings across all volumes.
    pub fn total_extents(&self) -> usize {
        self.volumes.values().map(|v| v.extents.len()).sum::<usize>() + self.cold.values().map(|c| c.extents).sum::<usize>()
    }

    /// Number of distinct slots the maps reference (a walk).
    #[track_caller]
    pub fn reverse_entries(&self) -> usize {
        self.check_all();
        let mut seen = std::collections::HashSet::new();
        for vmap in self.volumes.values() {
            for leg in vmap.all_legs() {
                seen.insert(leg);
            }
        }
        seen.len()
    }

    /// List all volume IDs.
    #[track_caller]
    pub fn volume_ids(&self) -> Vec<VolumeId> {
        self.check_all();
        self.volumes.keys().copied().collect()
    }

    /// Every data extent with a leg on a given slab, one per slot: a slot
    /// several maps share (a golden and its clones) is listed once, under
    /// whichever map was walked first. Moving it with `move_slot` rewrites
    /// every map that names it. Parity legs are listed by `slab_parity`.
    ///
    /// A walk of every map (#155): callers take the list once per pass, not
    /// once per extent they move.
    #[track_caller]
    pub fn slab_extents(&self, slab_id: SlabId) -> Vec<(VolumeId, u64, ExtentLocation)> {
        self.check_all();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (vol, vmap) in &self.volumes {
            for (vext, loc) in &vmap.extents {
                if let Some(leg) = loc.leg_on(slab_id) {
                    if seen.insert(leg.slot_idx) {
                        out.push((*vol, vext, loc));
                    }
                }
            }
        }
        out
    }

    /// Every parity group with a leg on a given slab, one per slot:
    /// `(volume_id, stripe, group)`.
    #[track_caller]
    pub fn slab_parity(&self, slab_id: SlabId) -> Vec<(VolumeId, u64, ParityGroup)> {
        self.check_all();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (vol, vmap) in &self.volumes {
            for (stripe, g) in &vmap.parity {
                if let Some(leg) = g.legs.iter().find(|l| l.slab_id == slab_id) {
                    if seen.insert(leg.slot_idx) {
                        out.push((*vol, *stripe, g.clone()));
                    }
                }
            }
        }
        out
    }

    /// Whether any map references a leg on `slab_id` (stops at the first).
    pub fn slab_in_use(&self, slab_id: SlabId) -> bool {
        if self.cold.values().any(|c| c.slabs.contains(&slab_id)) {
            return true;
        }
        self.volumes.values().any(|m| m.all_legs().any(|l| l.slab_id == slab_id))
    }

    /// Iterate over all extent locations for a volume.
    #[track_caller]
    pub fn volume_extents(&self, volume_id: &VolumeId) -> Option<impl Iterator<Item = (u64, ExtentLocation)> + '_> {
        self.check(volume_id);
        self.volumes.get(volume_id).map(|v| v.extents.iter())
    }

    /// Rebuild the GEM from slab slot tables. This is the recovery path:
    /// read every slab's table (once, #155), reconstruct the full extent map.
    ///
    /// The same (volume, extent) recorded in several slabs is a mirrored
    /// extent: the slot with the highest generation is the primary and the
    /// rest, at that generation, are its mirrors — a stale slot from an
    /// earlier copy-on-write carries a lower one. Parity slots are told
    /// apart by their tag.
    pub async fn rebuild_from_slabs<'a>(
        slabs: impl Iterator<Item = (&'a SlabId, &'a super::super::drive::slab::Slab)>,
    ) -> super::super::drive::DriveResult<Self> {
        let view = SlotView::read(slabs.map(|(_, s)| s.view_source()).collect()).await?;
        Ok(Self::rebuild_from_view(&view))
    }

    /// [`rebuild_from_slabs`](Self::rebuild_from_slabs) from tables already read.
    pub fn rebuild_from_view(view: &SlotView) -> Self {
        let mut gem = GlobalExtentMap::new();
        // (volume, vext) → [(generation, leg, ref_count)]
        type Seen = HashMap<(VolumeId, u64), Vec<(u64, Leg, u32)>>;
        // (volume, stripe) → [(parity leg, leg, ref_count, generation)]
        type SeenParity = HashMap<(VolumeId, u64), Vec<(u8, Leg, u32, u64)>>;
        let mut seen: Seen = HashMap::new();
        let mut parity: SeenParity = HashMap::new();

        for (leg, slot) in view.iter() {
            if slot.state.is_owned() {
                match parse_parity_vext(slot.virtual_extent_idx) {
                    Some((pleg, stripe)) => parity
                        .entry((slot.volume_id, stripe))
                        .or_default()
                        .push((pleg, *leg, slot.ref_count, slot.generation)),
                    None => seen
                        .entry((slot.volume_id, slot.virtual_extent_idx))
                        .or_default()
                        .push((slot.generation, *leg, slot.ref_count)),
                }
            }
        }

        for ((vol, vext), mut legs) in seen {
            legs.sort_by_key(|l| (std::cmp::Reverse(l.0), l.1.slab_id.0, l.1.slot_idx));
            let (gen, primary, ref_count) = legs[0];
            let mirrors = legs[1..]
                .iter()
                .filter(|(g, _, _)| *g == gen)
                .map(|(_, l, _)| *l)
                .collect();
            gem.insert(vol, vext, ExtentLocation {
                slab_id: primary.slab_id,
                slot_idx: primary.slot_idx,
                ref_count,
                generation: gen,
                mirrors,
            });
        }
        for ((vol, stripe), mut legs) in parity {
            legs.sort_by_key(|(pleg, _, _, _)| *pleg);
            let ref_count = legs[0].2;
            let generation = legs[0].3;
            let legs = legs.into_iter().map(|(_, l, _, _)| l).collect();
            // The slot table does not record the stripe width; a rebuilt
            // group is repaired from the volume record, which does.
            gem.insert_parity(vol, stripe, ParityGroup { legs, ref_count, generation, data_width: 0 });
        }

        gem
    }
}

/// Every slot in use on some slabs, each slab's table read once (#155):
/// what restore rebuilds and reconciles against, held for the restore and
/// dropped. The engine keeps no per-slot record otherwise.
#[derive(Default)]
pub struct SlotView {
    slots: HashMap<Leg, super::super::drive::slab::Slot>,
}

impl SlotView {
    /// Read the tables `sources` name, with no lock held.
    pub async fn read(sources: Vec<super::super::drive::slab::ViewSource>) -> super::super::drive::DriveResult<SlotView> {
        let mut slots = HashMap::new();
        for src in sources {
            for (idx, slot) in src.read().await? {
                slots.insert(Leg::new(src.slab, idx), slot);
            }
        }
        Ok(SlotView { slots })
    }

    /// The entry of an allocated (or erasing) slot; `None` for a free one.
    pub fn get(&self, leg: Leg) -> Option<&super::super::drive::slab::Slot> {
        self.slots.get(&leg)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Leg, &super::super::drive::slab::Slot)> {
        self.slots.iter()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl Default for GlobalExtentMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn cid() -> SlabId {
        SlabId(Uuid::new_v4())
    }

    fn loc(slab_id: SlabId, slot_idx: u64) -> ExtentLocation {
        ExtentLocation::new(slab_id, slot_idx)
    }

    #[test]
    fn insert_and_lookup() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 42));
        gem.insert(vol, 1, loc(c, 43));

        let l0 = gem.lookup(vol, 0).unwrap();
        assert_eq!(l0.slab_id, c);
        assert_eq!(l0.slot_idx, 42);

        let l1 = gem.lookup(vol, 1).unwrap();
        assert_eq!(l1.slot_idx, 43);

        assert!(gem.lookup(vol, 999).is_none());
        assert_eq!(gem.total_extents(), 2);
    }

    #[test]
    fn remove_extent() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 10));
        gem.insert(vol, 1, loc(c, 11));

        let removed = gem.remove(vol, 0).unwrap();
        assert_eq!(removed.slot_idx, 10);
        assert!(gem.lookup(vol, 0).is_none());
        assert!(gem.lookup(vol, 1).is_some());
        assert_eq!(gem.total_extents(), 1);
    }

    #[test]
    fn remove_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 0));
        gem.insert(vol, 1, loc(c, 1));
        gem.insert(vol, 2, loc(c, 2));

        let map = gem.remove_volume(vol).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(gem.volume_count(), 0);
        assert_eq!(gem.reverse_entries(), 0);
    }

    #[test]
    fn reverse_lookup() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 5, loc(c, 99));

        let (v, idx) = gem.reverse_lookup(c, 99).unwrap();
        assert_eq!(v, vol);
        assert_eq!(idx, 5);

        assert!(gem.reverse_lookup(c, 0).is_none());
    }

    #[test]
    fn reverse_index_consistency() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c1 = cid();
        let c2 = cid();

        gem.insert(vol, 0, loc(c1, 0));
        assert!(gem.reverse_lookup(c1, 0).is_some());

        // Move extent to different slab
        gem.insert(vol, 0, loc(c2, 5));
        assert!(gem.reverse_lookup(c1, 0).is_none());
        assert_eq!(gem.reverse_lookup(c2, 5).unwrap(), (vol, 0));
    }

    #[test]
    fn multi_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol_a = VolumeId::new();
        let vol_b = VolumeId::new();
        let c = cid();

        gem.insert(vol_a, 0, loc(c, 0));
        gem.insert(vol_a, 1, loc(c, 1));
        gem.insert(vol_b, 0, loc(c, 2));
        gem.insert(vol_b, 1, loc(c, 3));

        assert_eq!(gem.volume_count(), 2);
        assert_eq!(gem.total_extents(), 4);

        assert_eq!(gem.lookup(vol_a, 0).unwrap().slot_idx, 0);
        assert_eq!(gem.lookup(vol_b, 0).unwrap().slot_idx, 2);
    }

    #[test]
    fn multi_slab_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c1 = cid();
        let c2 = cid();

        // Volume spreads across two slabs
        gem.insert(vol, 0, loc(c1, 0));
        gem.insert(vol, 1, loc(c2, 0));
        gem.insert(vol, 2, loc(c1, 1));

        assert_eq!(gem.lookup(vol, 0).unwrap().slab_id, c1);
        assert_eq!(gem.lookup(vol, 1).unwrap().slab_id, c2);
        assert_eq!(gem.lookup(vol, 2).unwrap().slab_id, c1);
    }

    #[test]
    fn clone_volume_map_for_snapshot() {
        let mut gem = GlobalExtentMap::new();
        let source = VolumeId::new();
        let snap = VolumeId::new();
        let c = cid();

        gem.insert(source, 0, loc(c, 10));
        gem.insert(source, 1, loc(c, 11));

        let cloned = gem.clone_volume_map(source, snap).unwrap();
        assert_eq!(cloned.len(), 2);

        // Both volumes now point to the same slots
        let src_loc = gem.lookup(source, 0).unwrap();
        let snap_loc = gem.lookup(snap, 0).unwrap();
        assert_eq!(src_loc.slot_idx, snap_loc.slot_idx);

        // Ref counts bumped
        assert_eq!(src_loc.ref_count, 2);
        assert_eq!(snap_loc.ref_count, 2);

        assert_eq!(gem.volume_count(), 2);
    }

    #[test]
    fn volume_ids_and_extents() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 0));
        gem.insert(vol, 5, loc(c, 5));

        let ids = gem.volume_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vol);

        let extents: Vec<_> = gem.volume_extents(&vol).unwrap().collect();
        assert_eq!(extents.len(), 2);
        assert_eq!(extents[0].0, 0);
        assert_eq!(extents[1].0, 5);
    }

    #[test]
    fn slab_extents_filter() {
        let mut gem = GlobalExtentMap::new();
        let vol_a = VolumeId::new();
        let vol_b = VolumeId::new();
        let c1 = cid();
        let c2 = cid();

        gem.insert(vol_a, 0, loc(c1, 0));
        gem.insert(vol_a, 1, loc(c2, 0));
        gem.insert(vol_b, 0, loc(c1, 1));
        gem.insert(vol_b, 1, loc(c1, 2));

        let on_c1 = gem.slab_extents(c1);
        assert_eq!(on_c1.len(), 3);
        for (_, _, l) in &on_c1 {
            assert_eq!(l.slab_id, c1);
        }

        let on_c2 = gem.slab_extents(c2);
        assert_eq!(on_c2.len(), 1);
        assert_eq!(on_c2[0].0, vol_a);
        assert_eq!(on_c2[0].1, 1);

        let c3 = cid();
        assert!(gem.slab_extents(c3).is_empty());
    }


    #[test]
    fn legs_cover_the_reverse_index() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let (c1, c2) = (cid(), cid());
        let loc = ExtentLocation::with_legs(Leg::new(c1, 4), vec![Leg::new(c2, 9)]);
        gem.insert(vol, 0, loc);
        assert_eq!(gem.reverse_lookup(c1, 4), Some((vol, 0)));
        assert_eq!(gem.reverse_lookup(c2, 9), Some((vol, 0)));
        assert_eq!(gem.reverse_entries(), 2);
        // Both slabs list the extent.
        assert_eq!(gem.slab_extents(c1).len(), 1);
        assert_eq!(gem.slab_extents(c2).len(), 1);

        // Replacing a leg moves only that reverse entry.
        let c3 = cid();
        assert!(gem.replace_leg(vol, 0, Leg::new(c2, 9), Leg::new(c3, 1)));
        assert!(gem.reverse_lookup(c2, 9).is_none());
        assert_eq!(gem.reverse_lookup(c3, 1), Some((vol, 0)));
        assert_eq!(gem.lookup(vol, 0).unwrap().mirrors, vec![Leg::new(c3, 1)]);

        // Dropping the primary promotes a mirror; the last leg cannot go.
        assert!(gem.drop_leg(vol, 0, Leg::new(c1, 4)));
        assert_eq!(gem.lookup(vol, 0).unwrap().primary(), Leg::new(c3, 1));
        assert!(!gem.drop_leg(vol, 0, Leg::new(c3, 1)));

        gem.remove(vol, 0);
        assert_eq!(gem.reverse_entries(), 0);
    }

    #[test]
    fn parity_groups_are_tagged_and_shared_on_clone() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let (c1, c2, cp) = (cid(), cid(), cid());
        gem.insert(vol, 0, loc(c1, 0));
        gem.insert(vol, 1, loc(c2, 0));
        gem.insert_parity(vol, 0, ParityGroup::new(vec![Leg::new(cp, 7)], 2));

        let (v, tagged) = gem.reverse_lookup(cp, 7).unwrap();
        assert_eq!(v, vol);
        assert_eq!(parse_parity_vext(tagged), Some((0, 0)));
        assert!(gem.slab_extents(cp).is_empty(), "parity is not a data extent");
        assert_eq!(gem.slab_parity(cp).len(), 1);

        let snap = VolumeId::new();
        gem.clone_volume_map(vol, snap);
        assert_eq!(gem.lookup_parity(vol, 0).unwrap().ref_count, 2);
        assert_eq!(gem.lookup_parity(snap, 0).unwrap().ref_count, 2);

        let removed = gem.remove_volume(snap).unwrap();
        assert_eq!(removed.all_legs().count(), 3);
        // The source still owns the reverse entry.
        assert!(gem.reverse_lookup(cp, 7).is_some());
    }

    #[test]
    fn rewrite_legs_reaches_every_map_that_shares_a_slot() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let snap = VolumeId::new();
        let (c1, c2, c3) = (cid(), cid(), cid());
        gem.insert(vol, 0, ExtentLocation::with_legs(Leg::new(c1, 0), vec![Leg::new(c2, 0)]));
        gem.insert_parity(vol, 0, ParityGroup::new(vec![Leg::new(c2, 1)], 2));
        gem.clone_volume_map(vol, snap);

        let mut moves = HashMap::new();
        moves.insert(Leg::new(c2, 0), Leg::new(c3, 0));
        moves.insert(Leg::new(c2, 1), Leg::new(c3, 1));
        let n = gem.rewrite_legs(&moves);
        assert_eq!(n, 4, "two maps, two legs each");
        for v in [vol, snap] {
            assert_eq!(gem.lookup(v, 0).unwrap().mirrors, vec![Leg::new(c3, 0)]);
            assert_eq!(gem.lookup_parity(v, 0).unwrap().legs, vec![Leg::new(c3, 1)]);
        }
        assert!(gem.reverse_lookup(c2, 0).is_none());
        assert!(matches!(gem.reverse_lookup(c3, 0), Some((v, 0)) if v == vol || v == snap));
        assert!(gem.slab_extents(c2).is_empty());
        assert_eq!(gem.slab_extents(c3).len(), 1, "one entry per slot, however many maps share it");
    }

    #[test]
    fn parity_vext_round_trips() {
        let v = parity_vext(1, 123_456);
        assert_eq!(parse_parity_vext(v), Some((1, 123_456)));
        assert_eq!(parse_parity_vext(123_456), None);
    }

    #[test]
    fn empty_gem() {
        let gem = GlobalExtentMap::new();
        assert_eq!(gem.volume_count(), 0);
        assert_eq!(gem.total_extents(), 0);
        assert_eq!(gem.reverse_entries(), 0);
        assert!(gem.lookup(VolumeId::new(), 0).is_none());
    }
}
