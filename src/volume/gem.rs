//! Global Extent Map (GEM) — single source of truth for extent placement.
//!
//! The GEM tracks which slab slot(s) hold each volume's virtual extent.
//! It replaces both the ExtentAllocator's per-array bitmap and ThinVolume's
//! local extent_map with a unified, cross-slab index.
//!
//! An extent has one or more **legs**: the primary slot and, for a mirrored
//! volume, its mirrors — each on a distinct failure domain. A parity volume
//! keeps one leg per data extent and a **parity group** per stripe with the
//! P (and Q) legs. The reverse index covers every leg, so a slab's slot is
//! always traceable to what owns it.
//!
//! Recovery invariant: the GEM is reconstructable from slab slot tables.
//! Each slab's extent table is authoritative for its slots. The durable
//! record of an extent map is still the volume metadata file — the slot
//! tables are the fallback for when there is none.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabId;
use crate::volume::extent::VolumeId;

/// One physical slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Leg {
    pub slab_id: SlabId,
    pub slot_idx: u32,
}

impl Leg {
    pub fn new(slab_id: SlabId, slot_idx: u32) -> Self {
        Leg { slab_id, slot_idx }
    }
}

/// Location of a single extent in the slab mesh: its primary leg, and any
/// mirror legs a redundancy policy added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtentLocation {
    pub slab_id: SlabId,
    pub slot_idx: u32,
    pub ref_count: u32,
    pub generation: u64,
    /// Additional full copies, each on its own failure domain. Empty for
    /// an unreplicated volume.
    pub mirrors: Vec<Leg>,
}

impl ExtentLocation {
    /// A fresh, exclusively owned, unreplicated location.
    pub fn new(slab_id: SlabId, slot_idx: u32) -> Self {
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
    pub extents: BTreeMap<u64, ExtentLocation>,
    /// Stripe index → parity legs. Empty unless the volume has a parity policy.
    pub parity: BTreeMap<u64, ParityGroup>,
}

impl VolumeExtentMap {
    pub fn new() -> Self {
        VolumeExtentMap { extents: BTreeMap::new(), parity: BTreeMap::new() }
    }

    /// Number of mapped extents.
    pub fn len(&self) -> usize {
        self.extents.len()
    }

    /// Whether this map has no extents.
    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
    }

    /// Every slot this map references: data legs and parity legs alike.
    pub fn all_legs(&self) -> impl Iterator<Item = Leg> + '_ {
        self.extents
            .values()
            .flat_map(|l| l.legs())
            .chain(self.parity.values().flat_map(|g| g.legs.iter().copied()))
    }
}

/// Global Extent Map — tracks all extent locations across all volumes.
pub struct GlobalExtentMap {
    volumes: HashMap<VolumeId, VolumeExtentMap>,
    /// Every leg → the (volume, virtual extent) that owns it. Parity legs map
    /// to a `parity_vext`-tagged index.
    reverse: HashMap<(SlabId, u32), (VolumeId, u64)>,
}

impl GlobalExtentMap {
    pub fn new() -> Self {
        GlobalExtentMap {
            volumes: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Insert or update an extent mapping.
    pub fn insert(
        &mut self,
        volume_id: VolumeId,
        vext_idx: u64,
        location: ExtentLocation,
    ) {
        // Remove old reverse entries if this virtual extent was already
        // mapped — but only the ones this extent owns: a clone re-mapping an
        // extent it shared must not take the source's slot out of the index.
        if let Some(vmap) = self.volumes.get(&volume_id) {
            if let Some(old_loc) = vmap.extents.get(&vext_idx) {
                for leg in old_loc.legs() {
                    let key = (leg.slab_id, leg.slot_idx);
                    if self.reverse.get(&key) == Some(&(volume_id, vext_idx)) {
                        self.reverse.remove(&key);
                    }
                }
            }
        }

        for leg in location.legs() {
            self.reverse.insert((leg.slab_id, leg.slot_idx), (volume_id, vext_idx));
        }

        self.volumes
            .entry(volume_id)
            .or_default()
            .extents
            .insert(vext_idx, location);
    }

    /// Insert a mapping recovered from persisted metadata.
    ///
    /// Unlike `insert`, this never displaces an existing reverse-index claim:
    /// shared COW slots map several (volume, vext) pairs onto one slot, and
    /// the reverse index keeps the slot-table owner (same convention as
    /// `clone_volume_map`).
    pub fn restore_mapping(
        &mut self,
        volume_id: VolumeId,
        vext_idx: u64,
        location: ExtentLocation,
    ) {
        for leg in location.legs() {
            self.reverse.entry((leg.slab_id, leg.slot_idx)).or_insert((volume_id, vext_idx));
        }
        self.volumes
            .entry(volume_id)
            .or_default()
            .extents
            .insert(vext_idx, location);
    }

    /// Record a stripe's parity legs.
    pub fn insert_parity(&mut self, volume_id: VolumeId, stripe: u64, group: ParityGroup) {
        if let Some(vmap) = self.volumes.get(&volume_id) {
            if let Some(old) = vmap.parity.get(&stripe) {
                for (i, leg) in old.legs.iter().enumerate() {
                    let key = (leg.slab_id, leg.slot_idx);
                    if self.reverse.get(&key) == Some(&(volume_id, parity_vext(i as u8, stripe))) {
                        self.reverse.remove(&key);
                    }
                }
            }
        }
        for (i, leg) in group.legs.iter().enumerate() {
            self.reverse
                .insert((leg.slab_id, leg.slot_idx), (volume_id, parity_vext(i as u8, stripe)));
        }
        self.volumes.entry(volume_id).or_default().parity.insert(stripe, group);
    }

    /// Restore a stripe's parity legs without displacing a reverse claim.
    pub fn restore_parity(&mut self, volume_id: VolumeId, stripe: u64, group: ParityGroup) {
        for (i, leg) in group.legs.iter().enumerate() {
            self.reverse
                .entry((leg.slab_id, leg.slot_idx))
                .or_insert((volume_id, parity_vext(i as u8, stripe)));
        }
        self.volumes.entry(volume_id).or_default().parity.insert(stripe, group);
    }

    pub fn lookup_parity(&self, volume_id: VolumeId, stripe: u64) -> Option<&ParityGroup> {
        self.volumes.get(&volume_id)?.parity.get(&stripe)
    }

    pub fn remove_parity(&mut self, volume_id: VolumeId, stripe: u64) -> Option<ParityGroup> {
        let vmap = self.volumes.get_mut(&volume_id)?;
        let g = vmap.parity.remove(&stripe)?;
        for leg in &g.legs {
            let key = (leg.slab_id, leg.slot_idx);
            if self.reverse.get(&key).map(|(v, _)| *v) == Some(volume_id) {
                self.reverse.remove(&key);
            }
        }
        if vmap.extents.is_empty() && vmap.parity.is_empty() {
            self.volumes.remove(&volume_id);
        }
        Some(g)
    }

    pub fn inc_parity_ref(&mut self, volume_id: VolumeId, stripe: u64) {
        if let Some(g) = self.volumes.get_mut(&volume_id).and_then(|m| m.parity.get_mut(&stripe)) {
            g.ref_count += 1;
        }
    }

    /// Record one more sharer of a single extent.
    ///
    /// The recorded count is what makes a write copy-on-write instead of
    /// landing in place, so re-sharing one extent must bump it on both sides
    /// exactly as cloning a whole map does.
    pub fn inc_extent_ref(&mut self, volume_id: VolumeId, vext_idx: u64) {
        if let Some(vmap) = self.volumes.get_mut(&volume_id) {
            if let Some(loc) = vmap.extents.get_mut(&vext_idx) {
                loc.ref_count += 1;
            }
        }
    }

    /// Set the recorded share count of an extent — after a slot's count
    /// moved on disk, so the map agrees on whether a write must copy.
    pub fn set_extent_ref(&mut self, volume_id: VolumeId, vext_idx: u64, ref_count: u32) {
        if let Some(loc) = self.volumes.get_mut(&volume_id).and_then(|m| m.extents.get_mut(&vext_idx)) {
            loc.ref_count = ref_count;
        }
    }

    pub fn set_parity_ref(&mut self, volume_id: VolumeId, stripe: u64, ref_count: u32) {
        if let Some(g) = self.volumes.get_mut(&volume_id).and_then(|m| m.parity.get_mut(&stripe)) {
            g.ref_count = ref_count;
        }
    }

    /// Look up where a volume's virtual extent lives.
    pub fn lookup(&self, volume_id: VolumeId, vext_idx: u64) -> Option<&ExtentLocation> {
        self.volumes
            .get(&volume_id)?
            .extents
            .get(&vext_idx)
    }

    /// Replace one leg of an extent with another slot (a leg moved or
    /// rebuilt), keeping the rest of the location as it is.
    pub fn replace_leg(
        &mut self,
        volume_id: VolumeId,
        vext_idx: u64,
        old: Leg,
        new: Leg,
    ) -> bool {
        let Some(loc) = self
            .volumes
            .get_mut(&volume_id)
            .and_then(|m| m.extents.get_mut(&vext_idx))
        else {
            return false;
        };
        if loc.primary() == old {
            loc.slab_id = new.slab_id;
            loc.slot_idx = new.slot_idx;
        } else if let Some(m) = loc.mirrors.iter_mut().find(|m| **m == old) {
            *m = new;
        } else {
            return false;
        }
        loc.generation += 1;
        if self.reverse.get(&(old.slab_id, old.slot_idx)) == Some(&(volume_id, vext_idx)) {
            self.reverse.remove(&(old.slab_id, old.slot_idx));
        }
        self.reverse.insert((new.slab_id, new.slot_idx), (volume_id, vext_idx));
        true
    }

    /// Add a mirror leg to an extent (a resync filling in a missing copy).
    pub fn add_leg(&mut self, volume_id: VolumeId, vext_idx: u64, leg: Leg) -> bool {
        let Some(loc) = self
            .volumes
            .get_mut(&volume_id)
            .and_then(|m| m.extents.get_mut(&vext_idx))
        else {
            return false;
        };
        if loc.legs().any(|l| l == leg) {
            return true;
        }
        loc.mirrors.push(leg);
        self.reverse.insert((leg.slab_id, leg.slot_idx), (volume_id, vext_idx));
        true
    }

    /// Drop a leg from an extent without touching the slot (the caller frees
    /// it, or it is already gone with its slab). Refuses to drop the last leg.
    pub fn drop_leg(&mut self, volume_id: VolumeId, vext_idx: u64, leg: Leg) -> bool {
        let Some(loc) = self
            .volumes
            .get_mut(&volume_id)
            .and_then(|m| m.extents.get_mut(&vext_idx))
        else {
            return false;
        };
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
        if self.reverse.get(&(leg.slab_id, leg.slot_idx)) == Some(&(volume_id, vext_idx)) {
            self.reverse.remove(&(leg.slab_id, leg.slot_idx));
        }
        true
    }

    /// Replace one parity leg of a stripe.
    pub fn replace_parity_leg(
        &mut self,
        volume_id: VolumeId,
        stripe: u64,
        old: Leg,
        new: Leg,
    ) -> bool {
        let Some(g) = self
            .volumes
            .get_mut(&volume_id)
            .and_then(|m| m.parity.get_mut(&stripe))
        else {
            return false;
        };
        let Some(i) = g.legs.iter().position(|l| *l == old) else { return false };
        g.legs[i] = new;
        g.generation += 1;
        if self.reverse.get(&(old.slab_id, old.slot_idx))
            == Some(&(volume_id, parity_vext(i as u8, stripe)))
        {
            self.reverse.remove(&(old.slab_id, old.slot_idx));
        }
        self.reverse
            .insert((new.slab_id, new.slot_idx), (volume_id, parity_vext(i as u8, stripe)));
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
    pub fn rewrite_legs(&mut self, moves: &HashMap<Leg, Leg>) -> usize {
        if moves.is_empty() {
            return 0;
        }
        let mut rewritten = 0usize;
        let mut reverse_updates: Vec<(Leg, Leg, VolumeId, u64)> = Vec::new();
        for (vol, vmap) in self.volumes.iter_mut() {
            for (vext, loc) in vmap.extents.iter_mut() {
                let primary = loc.primary();
                if let Some(new) = moves.get(&primary) {
                    loc.slab_id = new.slab_id;
                    loc.slot_idx = new.slot_idx;
                    loc.generation += 1;
                    rewritten += 1;
                    reverse_updates.push((primary, *new, *vol, *vext));
                }
                for m in loc.mirrors.iter_mut() {
                    if let Some(new) = moves.get(m) {
                        reverse_updates.push((*m, *new, *vol, *vext));
                        *m = *new;
                        rewritten += 1;
                    }
                }
            }
            for (stripe, g) in vmap.parity.iter_mut() {
                for (i, leg) in g.legs.iter_mut().enumerate() {
                    if let Some(new) = moves.get(leg) {
                        reverse_updates.push((*leg, *new, *vol, parity_vext(i as u8, *stripe)));
                        *leg = *new;
                        g.generation += 1;
                        rewritten += 1;
                    }
                }
            }
        }
        for (old, new, vol, vext) in reverse_updates {
            let owner = self.reverse.get(&(old.slab_id, old.slot_idx)).copied();
            if owner == Some((vol, vext)) {
                self.reverse.remove(&(old.slab_id, old.slot_idx));
                self.reverse.insert((new.slab_id, new.slot_idx), (vol, vext));
            } else {
                self.reverse.entry((new.slab_id, new.slot_idx)).or_insert((vol, vext));
            }
        }
        rewritten
    }

    /// Add `new` as a mirror leg beside `existing` in every map that names
    /// `existing` — the golden and every clone sharing the slot. Returns how
    /// many maps gained the leg.
    pub fn add_leg_beside(&mut self, existing: Leg, new: Leg) -> usize {
        let mut added = 0usize;
        let mut owner: Option<(VolumeId, u64)> = None;
        for (vol, vmap) in self.volumes.iter_mut() {
            for (vext, loc) in vmap.extents.iter_mut() {
                if loc.legs().any(|l| l == existing) && !loc.legs().any(|l| l == new) {
                    loc.mirrors.push(new);
                    added += 1;
                    if owner.is_none() {
                        owner = Some((*vol, *vext));
                    }
                }
            }
        }
        if let Some(o) = self.reverse.get(&(existing.slab_id, existing.slot_idx)).copied().or(owner) {
            self.reverse.entry((new.slab_id, new.slot_idx)).or_insert(o);
        }
        added
    }

    /// Drop `leg` from every map that names it, never leaving a location
    /// with no legs. Returns how many maps lost it.
    pub fn drop_leg_everywhere(&mut self, leg: Leg) -> usize {
        let mut dropped = 0usize;
        for vmap in self.volumes.values_mut() {
            for loc in vmap.extents.values_mut() {
                if loc.primary() == leg {
                    if loc.mirrors.is_empty() {
                        continue;
                    }
                    let next = loc.mirrors.remove(0);
                    loc.slab_id = next.slab_id;
                    loc.slot_idx = next.slot_idx;
                    dropped += 1;
                } else {
                    let before = loc.mirrors.len();
                    loc.mirrors.retain(|m| *m != leg);
                    if loc.mirrors.len() != before {
                        dropped += 1;
                    }
                }
            }
        }
        if dropped > 0 {
            self.reverse.remove(&(leg.slab_id, leg.slot_idx));
        }
        dropped
    }

    /// Remove an extent mapping. A reverse entry is dropped only when this
    /// volume owns it — a slot shared with a clone stays traceable to whoever
    /// allocated it.
    pub fn remove(&mut self, volume_id: VolumeId, vext_idx: u64) -> Option<ExtentLocation> {
        let vmap = self.volumes.get_mut(&volume_id)?;
        let loc = vmap.extents.remove(&vext_idx)?;
        for leg in loc.legs() {
            let key = (leg.slab_id, leg.slot_idx);
            if self.reverse.get(&key) == Some(&(volume_id, vext_idx)) {
                self.reverse.remove(&key);
            }
        }

        // Clean up empty volume map
        if vmap.extents.is_empty() && vmap.parity.is_empty() {
            self.volumes.remove(&volume_id);
        }
        Some(loc)
    }

    /// Remove all extents for a volume. Returns the removed extent map.
    pub fn remove_volume(&mut self, volume_id: VolumeId) -> Option<VolumeExtentMap> {
        let vmap = self.volumes.remove(&volume_id)?;
        for leg in vmap.all_legs() {
            let key = (leg.slab_id, leg.slot_idx);
            if self.reverse.get(&key).map(|(v, _)| *v) == Some(volume_id) {
                self.reverse.remove(&key);
            }
        }
        Some(vmap)
    }

    /// Get the volume extent map for a given volume.
    pub fn get_volume_map(&self, volume_id: &VolumeId) -> Option<&VolumeExtentMap> {
        self.volumes.get(volume_id)
    }

    /// Reverse lookup: given a slab+slot, find which volume+extent owns it.
    /// A parity slot answers with a `parity_vext`-tagged index.
    pub fn reverse_lookup(
        &self,
        slab_id: SlabId,
        slot_idx: u32,
    ) -> Option<(VolumeId, u64)> {
        self.reverse.get(&(slab_id, slot_idx)).copied()
    }

    /// Clone a volume's extent map for snapshot (bumps ref_count in the clone).
    pub fn clone_volume_map(
        &mut self,
        source_id: VolumeId,
        dest_id: VolumeId,
    ) -> Option<VolumeExtentMap> {
        let source_map = self.volumes.get(&source_id)?.clone();

        // Insert cloned mappings for the destination volume.
        // Note: ref_count updates in the actual slabs happen separately.
        let mut dest_map = VolumeExtentMap::new();
        for (&vext_idx, loc) in &source_map.extents {
            let mut new_loc = loc.clone();
            new_loc.ref_count += 1;
            dest_map.extents.insert(vext_idx, new_loc);

            // Note: reverse map still points to source — that's correct because
            // the slab slot is shared. The reverse map tracks the primary
            // owner. For COW, we track sharing via ref_count.
        }
        for (&stripe, g) in &source_map.parity {
            let mut ng = g.clone();
            ng.ref_count += 1;
            dest_map.parity.insert(stripe, ng);
        }

        // Also update the source's ref_counts in the GEM
        if let Some(src_map) = self.volumes.get_mut(&source_id) {
            for loc in src_map.extents.values_mut() {
                loc.ref_count += 1;
            }
            for g in src_map.parity.values_mut() {
                g.ref_count += 1;
            }
        }

        self.volumes.insert(dest_id, dest_map.clone());
        Some(dest_map)
    }

    /// Number of tracked volumes.
    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Total number of extent mappings across all volumes.
    pub fn total_extents(&self) -> usize {
        self.volumes.values().map(|v| v.extents.len()).sum()
    }

    /// Number of reverse index entries.
    pub fn reverse_entries(&self) -> usize {
        self.reverse.len()
    }

    /// List all volume IDs.
    pub fn volume_ids(&self) -> Vec<VolumeId> {
        self.volumes.keys().copied().collect()
    }

    /// Collect all data extents with a leg on a given slab (needed by
    /// evacuate_slab). Parity legs are listed by `slab_parity`.
    ///
    /// Iterates the reverse index, filters by `slab_id`, returns
    /// `(volume_id, vext_idx, location)` tuples.
    pub fn slab_extents(&self, slab_id: SlabId) -> Vec<(VolumeId, u64, ExtentLocation)> {
        self.reverse
            .iter()
            .filter(|((sid, _), (_, vext))| *sid == slab_id && vext & PARITY_TAG == 0)
            .filter_map(|((_, _), &(volume_id, vext_idx))| {
                let loc = self.volumes.get(&volume_id)?.extents.get(&vext_idx)?.clone();
                Some((volume_id, vext_idx, loc))
            })
            .collect()
    }

    /// Every parity group with a leg on a given slab:
    /// `(volume_id, stripe, group)`.
    pub fn slab_parity(&self, slab_id: SlabId) -> Vec<(VolumeId, u64, ParityGroup)> {
        self.reverse
            .iter()
            .filter(|((sid, _), (_, vext))| *sid == slab_id && vext & PARITY_TAG != 0)
            .filter_map(|((_, _), &(volume_id, tagged))| {
                let (_, stripe) = parse_parity_vext(tagged)?;
                let g = self.volumes.get(&volume_id)?.parity.get(&stripe)?.clone();
                Some((volume_id, stripe, g))
            })
            .collect()
    }

    /// Iterate over all extent locations for a volume.
    pub fn volume_extents(
        &self,
        volume_id: &VolumeId,
    ) -> Option<impl Iterator<Item = (&u64, &ExtentLocation)>> {
        self.volumes.get(volume_id).map(|v| v.extents.iter())
    }

    /// Rebuild the GEM from slab slot tables. This is the recovery path:
    /// scan all slabs, reconstruct the full extent map.
    ///
    /// The same (volume, extent) recorded in several slabs is a mirrored
    /// extent: the slot with the highest generation is the primary and the
    /// rest, at that generation, are its mirrors — a stale slot from an
    /// earlier copy-on-write carries a lower one. Parity slots are told
    /// apart by their tag.
    pub fn rebuild_from_slabs<'a>(
        slabs: impl Iterator<Item = (&'a SlabId, &'a super::super::drive::slab::Slab)>,
    ) -> Self {
        let mut gem = GlobalExtentMap::new();
        // (volume, vext) → [(generation, leg, ref_count)]
        let mut seen: HashMap<(VolumeId, u64), Vec<(u64, Leg, u32)>> = HashMap::new();
        let mut parity: HashMap<(VolumeId, u64), Vec<(u8, Leg, u32, u64)>> = HashMap::new();

        for (_, slab) in slabs {
            let cid = slab.slab_id();
            for slot_idx in 0..slab.total_slots() as u32 {
                if let Some(slot) = slab.get_slot(slot_idx) {
                    if slot.state != super::super::drive::slab::SlotState::Free {
                        let leg = Leg::new(cid, slot_idx);
                        match parse_parity_vext(slot.virtual_extent_idx) {
                            Some((pleg, stripe)) => parity
                                .entry((slot.volume_id, stripe))
                                .or_default()
                                .push((pleg, leg, slot.ref_count, slot.generation)),
                            None => seen
                                .entry((slot.volume_id, slot.virtual_extent_idx))
                                .or_default()
                                .push((slot.generation, leg, slot.ref_count)),
                        }
                    }
                }
            }
        }

        for ((vol, vext), mut legs) in seen {
            legs.sort_by(|a, b| b.0.cmp(&a.0));
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

    fn loc(slab_id: SlabId, slot_idx: u32) -> ExtentLocation {
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
        assert_eq!(*extents[0].0, 0);
        assert_eq!(*extents[1].0, 5);
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
        assert_eq!(gem.reverse_lookup(c3, 0), Some((vol, 0)));
        assert!(gem.slab_extents(c2).is_empty());
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
