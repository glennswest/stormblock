//! StormFS chunk lifecycle — allocate, deallocate, trim (#49).
//!
//! StormFS keeps its namespace and chunk map in a KV store and puts **no
//! process in the data path**: a client holding a chunk map issues I/O
//! straight to StormBlock. That only works if chunk lifecycle is a StormBlock
//! operation, which is what this module is.
//!
//! A **chunk** is a run of whole slab slots inside one volume, addressed as
//! `(volume, offset, len)` — the same `(volume, offset, len)` the client then
//! reads and writes over iSCSI or NVMe-oF. Whole slots, because a slot is the
//! unit that can be reclaimed and placed independently: the volume already
//! reports `slot_size` as its discard granularity, and a chunk smaller than
//! that could be handed out but never given back.
//!
//! Two properties drove the shape, both from the issue:
//!
//! 1. **Allocation is eager and tier-scoped.** StormFS owns *policy* — which
//!    tier a file belongs on — while StormBlock owns *placement* — which slab
//!    on that tier, and where in it. So `allocate` takes slots from
//!    `best_slab_for_tier` now and records the mapping now, rather than
//!    returning offsets and letting allocate-on-write fill them in later.
//!    Lazy allocation would place the chunk wherever the *volume's* placement
//!    policy pointed, which is not what the caller asked for, and would report
//!    space as available that a later write could fail to find.
//!
//! 2. **Deallocation is idempotent by construction.** The orphan sweeper can
//!    crash between freeing an extent and dropping its queue entry, so it will
//!    re-free. Freeing something already free is a success here, counted
//!    separately as `already_free` — the count is worth reporting, but making
//!    it an error would wedge the sweeper's queue permanently on one crash.
//!
//! Ownership is tracked per volume as a set of ranges, so a chunk that has
//! been trimmed to nothing is still *owned* and will not be handed to a second
//! caller. That is the difference between trim and deallocate: both free the
//! physical slots, only deallocate returns the address range to the pool.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::drive::DriveError;
use crate::placement::topology::StorageTier;

use super::extent::VolumeId;
use super::gem::{ExtentLocation, GlobalExtentMap};

/// A chunk, as StormFS addresses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkExtent {
    pub volume: VolumeId,
    pub offset: u64,
    pub len: u64,
}

impl ChunkExtent {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

/// What went wrong with a chunk operation.
#[derive(Debug)]
pub enum ChunkError {
    /// An offset or length that is not a whole number of slots. Rounding it
    /// silently would handed back a chunk that cannot be freed.
    Unaligned {
        what: &'static str,
        value: u64,
        slot_size: u64,
    },
    /// The request does not fit the volume's address space.
    OutOfRange {
        offset: u64,
        len: u64,
        virtual_size: u64,
    },
    /// Nothing sensible to do: zero-length chunk, zero count.
    Invalid(String),
    /// The volume has no free address space left for `count` chunks.
    NoAddressSpace { found: usize, wanted: usize },
    /// The requested tier is out of slots. Deliberately *not* satisfied from
    /// another tier: StormFS asked for a tier, and quietly giving it a slower
    /// one is worse than telling it there is no room.
    TierFull {
        tier: StorageTier,
        slots_needed: u64,
    },
    Drive(DriveError),
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Unaligned { what, value, slot_size } => write!(
                f,
                "{what} {value} is not a multiple of the slot size {slot_size}: a chunk is a whole \
                 number of slots, because a slot is the unit this volume can reclaim"
            ),
            ChunkError::OutOfRange { offset, len, virtual_size } => write!(
                f,
                "chunk {offset}..{} runs past the end of a {virtual_size}-byte volume",
                offset + len
            ),
            ChunkError::Invalid(m) => write!(f, "{m}"),
            ChunkError::NoAddressSpace { found, wanted } => write!(
                f,
                "only {found} of {wanted} chunks fit in the volume's free address space"
            ),
            ChunkError::TierFull { tier, slots_needed } => write!(
                f,
                "tier {tier} has no room for {slots_needed} more slots — allocation is \
                 tier-scoped, so this is not silently satisfied from another tier"
            ),
            ChunkError::Drive(e) => write!(f, "drive error: {e}"),
        }
    }
}

impl std::error::Error for ChunkError {}

impl From<DriveError> for ChunkError {
    fn from(e: DriveError) -> Self {
        ChunkError::Drive(e)
    }
}

/// Which parts of which volumes StormFS has been given.
///
/// Ranges are slot-aligned, non-overlapping, and coalesced — this records
/// *what is spoken for*, not chunk identity. StormFS keeps chunk identity in
/// its own map; duplicating it here would be a second copy to disagree with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkMap {
    #[serde(default)]
    owned: HashMap<VolumeId, BTreeMap<u64, u64>>,
}

impl ChunkMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ranges owned in one volume, as `(offset, len)` in offset order.
    pub fn ranges(&self, volume: VolumeId) -> Vec<(u64, u64)> {
        self.owned
            .get(&volume)
            .map(|m| m.iter().map(|(&o, &l)| (o, l)).collect())
            .unwrap_or_default()
    }

    /// Volumes this map has handed out space in.
    pub fn volumes(&self) -> Vec<VolumeId> {
        self.owned.keys().copied().collect()
    }

    /// Total bytes spoken for in one volume.
    pub fn owned_bytes(&self, volume: VolumeId) -> u64 {
        self.owned
            .get(&volume)
            .map(|m| m.values().sum())
            .unwrap_or(0)
    }

    /// Whether `offset` falls inside a range this volume has handed out.
    pub fn is_owned(&self, volume: VolumeId, offset: u64) -> bool {
        let Some(m) = self.owned.get(&volume) else { return false };
        m.range(..=offset)
            .next_back()
            .is_some_and(|(&start, &len)| start + len > offset)
    }

    /// Record a range as handed out, merging with anything adjacent.
    pub fn claim(&mut self, volume: VolumeId, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let m = self.owned.entry(volume).or_default();
        let mut start = offset;
        let mut end = offset + len;

        // Absorb every range that touches or overlaps the new one. `..=end`
        // rather than `..end` so a range starting exactly where this one ends
        // is merged rather than left as a neighbour.
        let overlapping: Vec<u64> = m
            .range(..=end)
            .filter(|(&s, &l)| s + l >= start)
            .map(|(&s, _)| s)
            .collect();
        for s in overlapping {
            let l = m.remove(&s).unwrap_or(0);
            start = start.min(s);
            end = end.max(s + l);
        }
        m.insert(start, end - start);
    }

    /// Return a range to the pool, splitting whatever it cuts through.
    ///
    /// Returns the bytes that were actually owned. Releasing what is already
    /// free is not an error — it is the sweeper retrying after a crash.
    pub fn release(&mut self, volume: VolumeId, offset: u64, len: u64) -> u64 {
        if len == 0 {
            return 0;
        }
        let Some(m) = self.owned.get_mut(&volume) else { return 0 };
        let end = offset + len;

        let touched: Vec<(u64, u64)> = m
            .range(..end)
            .filter(|(&s, &l)| s + l > offset)
            .map(|(&s, &l)| (s, l))
            .collect();

        let mut released = 0;
        for (s, l) in touched {
            m.remove(&s);
            let e = s + l;
            released += e.min(end) - s.max(offset);
            if s < offset {
                m.insert(s, offset - s);
            }
            if e > end {
                m.insert(end, e - end);
            }
        }
        if m.is_empty() {
            self.owned.remove(&volume);
        }
        released
    }

    /// Forget a volume entirely — it has been deleted.
    pub fn forget(&mut self, volume: VolumeId) {
        self.owned.remove(&volume);
    }

    /// First-fit search for `count` free chunk offsets.
    ///
    /// A candidate is rejected if it overlaps a range already handed out *or*
    /// an extent the volume has mapped some other way — a volume can be
    /// written to directly as well as carved into chunks, and handing out an
    /// address that already holds data would let StormFS overwrite it.
    pub fn find_free(
        &self,
        volume: VolumeId,
        chunk_len: u64,
        count: usize,
        slot_size: u64,
        virtual_size: u64,
        is_mapped: impl Fn(u64) -> bool,
    ) -> Vec<u64> {
        let empty = BTreeMap::new();
        let owned = self.owned.get(&volume).unwrap_or(&empty);
        let mut out = Vec::with_capacity(count);
        let mut cursor = 0u64;

        'candidate: while out.len() < count && cursor + chunk_len <= virtual_size {
            // Sitting inside a range already handed out.
            if let Some((&s, &l)) = owned.range(..=cursor).next_back() {
                if s + l > cursor {
                    cursor = s + l;
                    continue;
                }
            }
            // A range starting inside the candidate.
            if let Some((&s, &l)) = owned.range(cursor..cursor + chunk_len).next() {
                cursor = s + l;
                continue;
            }
            // An extent mapped outside the chunk allocator's knowledge.
            let first = cursor / slot_size;
            let last = (cursor + chunk_len) / slot_size;
            for vext in first..last {
                if is_mapped(vext) {
                    cursor = (vext + 1) * slot_size;
                    continue 'candidate;
                }
            }
            out.push(cursor);
            cursor += chunk_len;
        }
        out
    }
}

/// One allocation request.
#[derive(Debug, Clone)]
pub struct AllocRequest {
    pub volume: VolumeId,
    pub virtual_size: u64,
    pub slot_size: u64,
    pub tier: StorageTier,
    /// Bytes per chunk. Must be a whole number of slots.
    pub chunk_len: u64,
    pub count: usize,
    /// Zero the chunk before handing it over.
    ///
    /// Defaults on at the API, and costs a full write of the chunk. Off is
    /// for a caller about to overwrite every byte anyway: without it, a
    /// freshly allocated chunk reads back whatever the slot's previous tenant
    /// left, except where the backing device zeroes on discard.
    pub zero: bool,
}

/// Hand out `count` chunks of `chunk_len` bytes on `tier`.
///
/// All-or-nothing: a request that cannot be satisfied in full frees whatever
/// it took on the way and returns the reason. A partially satisfied
/// allocation would leave StormFS to work out which of its chunks exist.
pub async fn allocate(
    map: &mut ChunkMap,
    gem: &Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: &Arc<tokio::sync::RwLock<SlabRegistry>>,
    req: &AllocRequest,
) -> Result<Vec<ChunkExtent>, ChunkError> {
    if req.count == 0 {
        return Err(ChunkError::Invalid("count must be at least 1".into()));
    }
    if req.chunk_len == 0 {
        return Err(ChunkError::Invalid("len must be at least one slot".into()));
    }
    if req.chunk_len % req.slot_size != 0 {
        return Err(ChunkError::Unaligned {
            what: "chunk length",
            value: req.chunk_len,
            slot_size: req.slot_size,
        });
    }

    let slots_per_chunk = req.chunk_len / req.slot_size;
    let slots_needed = slots_per_chunk * req.count as u64;

    // Pick addresses.
    let offsets = {
        let g = gem.read().await;
        map.find_free(
            req.volume,
            req.chunk_len,
            req.count,
            req.slot_size,
            req.virtual_size,
            |vext| g.lookup(req.volume, vext).is_some(),
        )
    };
    if offsets.len() < req.count {
        return Err(ChunkError::NoAddressSpace {
            found: offsets.len(),
            wanted: req.count,
        });
    }

    // Take every slot up front, under one lock. Checking free space and then
    // allocating in two steps would race another allocator into the same
    // slots; this way the tier either has room for the whole request or the
    // request never started.
    let mut taken: Vec<(u64, SlabId, u32)> = Vec::with_capacity(slots_needed as usize);
    {
        let mut reg = registry.write().await;
        for &offset in &offsets {
            for i in 0..slots_per_chunk {
                let vext = offset / req.slot_size + i;
                match take_slot_on_tier(&mut reg, req.tier, req.volume, vext).await {
                    Ok((slab_id, slot_idx)) => taken.push((vext, slab_id, slot_idx)),
                    Err(e) => {
                        rollback(&mut reg, &taken).await;
                        return Err(e);
                    }
                }
            }
        }
    }

    // Zero outside the registry lock: it is a full write of the chunk, and
    // holding the lock across it would stall every other writer on the node.
    if req.zero {
        let zeros = vec![0u8; req.slot_size as usize];
        for (_, slab_id, slot_idx) in &taken {
            let target = {
                let reg = registry.read().await;
                reg.get(slab_id)
                    .and_then(|s| s.slot_device_and_offset(*slot_idx, 0).ok())
            };
            let outcome = match target {
                Some((device, phys)) => device.write(phys, &zeros).await.map(|_| ()),
                None => Err(DriveError::Other(anyhow::anyhow!(
                    "slab {} vanished mid-allocation",
                    slab_id.0
                ))),
            };
            if let Err(e) = outcome {
                let mut reg = registry.write().await;
                rollback(&mut reg, &taken).await;
                return Err(ChunkError::Drive(e));
            }
        }
    }

    // Record the mappings, then drop the reservations that protected the
    // slots from collection while they were allocated but unmapped.
    {
        let mut g = gem.write().await;
        for (vext, slab_id, slot_idx) in &taken {
            g.insert(
                req.volume,
                *vext,
                ExtentLocation {
                    slab_id: *slab_id,
                    slot_idx: *slot_idx,
                    ref_count: 1,
                    generation: 1,
                },
            );
        }
    }
    {
        let mut reg = registry.write().await;
        for (_, slab_id, slot_idx) in &taken {
            reg.commit(*slab_id, *slot_idx);
        }
    }

    let mut chunks = Vec::with_capacity(offsets.len());
    for offset in offsets {
        map.claim(req.volume, offset, req.chunk_len);
        chunks.push(ChunkExtent {
            volume: req.volume,
            offset,
            len: req.chunk_len,
        });
    }
    Ok(chunks)
}

/// Take one slot on exactly `tier`.
async fn take_slot_on_tier(
    reg: &mut SlabRegistry,
    tier: StorageTier,
    volume: VolumeId,
    vext: u64,
) -> Result<(SlabId, u32), ChunkError> {
    // `best_slab_for_tier` already skips slabs with no free slots, so one
    // attempt is the whole story: a slab that reports room and then refuses
    // to give any is inconsistent with itself, and retrying would spin.
    let Some(slab_id) = reg.best_slab_for_tier(tier) else {
        return Err(ChunkError::TierFull { tier, slots_needed: 1 });
    };
    let Some(slab) = reg.get_mut(&slab_id) else {
        return Err(ChunkError::TierFull { tier, slots_needed: 1 });
    };
    match slab.allocate(volume, vext).await {
        Ok(slot_idx) => {
            reg.reserve(slab_id, slot_idx);
            Ok((slab_id, slot_idx))
        }
        Err(e) => {
            tracing::warn!(
                "slab {} reported free slots and then refused an allocation: {e}",
                slab_id.0
            );
            Err(ChunkError::TierFull { tier, slots_needed: 1 })
        }
    }
}

/// Give back slots taken by an allocation that then failed.
async fn rollback(reg: &mut SlabRegistry, taken: &[(u64, SlabId, u32)]) {
    for (_, slab_id, slot_idx) in taken {
        if let Some(slab) = reg.get_mut(slab_id) {
            if let Err(e) = slab.free(*slot_idx) .await {
                tracing::warn!(
                    "could not give back slot {slot_idx} on slab {} after a failed allocation: {e}",
                    slab_id.0
                );
            }
        }
        reg.commit(*slab_id, *slot_idx);
    }
}

/// What a free actually managed to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreeOutcome {
    /// Extents whose backing slots were released by this call.
    pub freed: usize,
    /// Extents that were already free. Not an error: the sweeper retries.
    pub already_free: usize,
    /// Bytes returned to the slabs.
    pub bytes_freed: u64,
    /// Slots the slab would not release, with the reason. The mapping is
    /// already gone by then, so these are stranded and worth reporting rather
    /// than folding into `already_free`.
    pub failures: Vec<String>,
}

/// Free the physical slots under a set of chunks.
///
/// With `release_ownership` the address range goes back to the pool too —
/// that is `deallocate`. Without it the range stays spoken for and only the
/// space comes back — that is `trim`, for punching a hole in a chunk StormFS
/// still holds a mapping for.
///
/// Only slots the range covers *completely* are freed, matching the volume's
/// discard granularity: a partial slot still holds bytes outside the range.
pub async fn free(
    map: &mut ChunkMap,
    gem: &Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: &Arc<tokio::sync::RwLock<SlabRegistry>>,
    extents: &[ChunkExtent],
    slot_size: u64,
    release_ownership: bool,
) -> Result<FreeOutcome, ChunkError> {
    let mut out = FreeOutcome::default();

    for ext in extents {
        if release_ownership {
            // Deallocate names a chunk, and a chunk is slot-aligned. A
            // request that is not is a caller bug worth reporting, since the
            // bytes outside the alignment would silently survive.
            if ext.offset % slot_size != 0 {
                return Err(ChunkError::Unaligned {
                    what: "offset",
                    value: ext.offset,
                    slot_size,
                });
            }
            if ext.len % slot_size != 0 {
                return Err(ChunkError::Unaligned {
                    what: "length",
                    value: ext.len,
                    slot_size,
                });
            }
        }
        if ext.len == 0 {
            continue;
        }

        // Whole slots inside the range only.
        let first = ext.offset.div_ceil(slot_size);
        let last = ext.end() / slot_size;

        let mut to_free: Vec<(u64, ExtentLocation)> = Vec::new();
        {
            let g = gem.read().await;
            for vext in first..last {
                match g.lookup(ext.volume, vext) {
                    Some(loc) => to_free.push((vext, loc.clone())),
                    None => out.already_free += 1,
                }
            }
        }

        if to_free.is_empty() {
            if release_ownership {
                map.release(ext.volume, ext.offset, ext.len);
            }
            continue;
        }

        {
            let mut g = gem.write().await;
            for (vext, _) in &to_free {
                g.remove(ext.volume, *vext);
            }
        }

        // Group by slab so a run of slots costs one batch rather than one
        // read-modify-write of the slot table each.
        let mut by_slab: HashMap<SlabId, Vec<u32>> = HashMap::new();
        for (_, loc) in &to_free {
            by_slab.entry(loc.slab_id).or_default().push(loc.slot_idx);
        }
        {
            let mut reg = registry.write().await;
            for (slab_id, slots) in by_slab {
                let Some(slab) = reg.get_mut(&slab_id) else {
                    out.failures
                        .push(format!("slab {} is not registered", slab_id.0));
                    continue;
                };
                match slab.dec_ref_batch(&slots).await {
                    Ok(outcome) => {
                        out.bytes_freed += outcome.freed as u64 * slot_size;
                        for (slot, why) in outcome.rejected {
                            out.failures
                                .push(format!("slab {} slot {slot}: {why}", slab_id.0));
                        }
                    }
                    Err(e) => out.failures.push(format!("slab {}: {e}", slab_id.0)),
                }
            }
        }

        out.freed += to_free.len();
        if release_ownership {
            map.release(ext.volume, ext.offset, ext.len);
        }
    }

    Ok(out)
}

/// One mapped extent, as `extent-map` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct MappedExtent {
    pub offset: u64,
    pub len: u64,
    pub slab: String,
    pub slot: u32,
    pub ref_count: u32,
    pub generation: u64,
    /// Whether this extent sits inside a range StormFS was given.
    ///
    /// `false` means the block layer holds data at an address the chunk
    /// allocator never handed out — which is exactly what `fsck` is looking
    /// for when it reconciles the two maps.
    pub owned: bool,
}

/// What the block layer actually holds for one volume.
#[derive(Debug, Clone, Serialize)]
pub struct ExtentMapReport {
    pub volume: String,
    pub slot_size: u64,
    pub virtual_size: u64,
    /// Ranges handed out, as `[offset, len]`.
    pub chunks: Vec<[u64; 2]>,
    pub owned_bytes: u64,
    pub extents: Vec<MappedExtent>,
    pub allocated_bytes: u64,
    /// Ranges handed out with nothing mapped under them — trimmed, or
    /// allocated and not yet written.
    pub unmapped_owned_bytes: u64,
}

/// Report the mapping for one volume, for `fsck` to reconcile against.
pub async fn extent_map(
    map: &ChunkMap,
    gem: &Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    volume: VolumeId,
    slot_size: u64,
    virtual_size: u64,
) -> ExtentMapReport {
    let g = gem.read().await;
    let mut extents: Vec<MappedExtent> = g
        .volume_extents(&volume)
        .map(|iter| {
            iter.map(|(&vext, loc)| {
                let offset = vext * slot_size;
                MappedExtent {
                    offset,
                    len: slot_size,
                    slab: loc.slab_id.0.to_string(),
                    slot: loc.slot_idx,
                    ref_count: loc.ref_count,
                    generation: loc.generation,
                    owned: map.is_owned(volume, offset),
                }
            })
            .collect()
        })
        .unwrap_or_default();
    extents.sort_by_key(|e| e.offset);

    let owned_bytes = map.owned_bytes(volume);
    let mapped_owned: u64 = extents.iter().filter(|e| e.owned).map(|e| e.len).sum();

    ExtentMapReport {
        volume: volume.0.to_string(),
        slot_size,
        virtual_size,
        chunks: map.ranges(volume).into_iter().map(|(o, l)| [o, l]).collect(),
        owned_bytes,
        allocated_bytes: extents.len() as u64 * slot_size,
        unmapped_owned_bytes: owned_bytes.saturating_sub(mapped_owned),
        extents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol() -> VolumeId {
        VolumeId::new()
    }

    #[test]
    fn claim_merges_adjacent_ranges() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 0, 1024);
        m.claim(v, 1024, 1024);
        assert_eq!(m.ranges(v), vec![(0, 2048)], "adjacent ranges must coalesce");

        m.claim(v, 4096, 1024);
        assert_eq!(m.ranges(v), vec![(0, 2048), (4096, 1024)]);

        // Bridging the gap merges all three into one.
        m.claim(v, 2048, 2048);
        assert_eq!(m.ranges(v), vec![(0, 5120)]);
    }

    #[test]
    fn release_splits_and_is_idempotent() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 0, 8192);

        // Punch out the middle.
        assert_eq!(m.release(v, 2048, 2048), 2048);
        assert_eq!(m.ranges(v), vec![(0, 2048), (4096, 4096)]);

        // Releasing it again is a no-op, not an error — the sweeper retries.
        assert_eq!(m.release(v, 2048, 2048), 0);
        assert_eq!(m.ranges(v), vec![(0, 2048), (4096, 4096)]);

        // A release spanning several ranges reports only what was owned.
        assert_eq!(m.release(v, 0, 8192), 6144);
        assert!(m.ranges(v).is_empty());
    }

    #[test]
    fn is_owned_covers_the_interior() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 4096, 4096);
        assert!(!m.is_owned(v, 0));
        assert!(m.is_owned(v, 4096));
        assert!(m.is_owned(v, 8191));
        assert!(!m.is_owned(v, 8192));
    }

    #[test]
    fn find_free_skips_owned_ranges() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 0, 4096);

        let found = m.find_free(v, 4096, 2, 1024, 32768, |_| false);
        assert_eq!(found, vec![4096, 8192]);
    }

    #[test]
    fn find_free_skips_extents_mapped_outside_the_allocator() {
        let m = ChunkMap::new();
        let v = vol();
        // Slot 2 holds data written directly to the volume.
        let found = m.find_free(v, 2048, 2, 1024, 32768, |vext| vext == 2);
        // The candidate at 0 covers slots 0-1 and is fine; the next candidate
        // starts after slot 2 rather than overlapping it.
        assert_eq!(found, vec![0, 3072]);
    }

    #[test]
    fn find_free_stops_at_the_end_of_the_volume() {
        let m = ChunkMap::new();
        let v = vol();
        let found = m.find_free(v, 4096, 10, 1024, 8192, |_| false);
        assert_eq!(found, vec![0, 4096], "only two chunks fit");
    }

    #[test]
    fn forget_drops_a_deleted_volume() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 0, 4096);
        m.forget(v);
        assert!(m.ranges(v).is_empty());
        assert!(m.volumes().is_empty());
    }

    // ---- against a real slab -------------------------------------------

    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::drive::BlockDevice;

    const SLOT: u64 = 4096;

    /// A registry with one slab on `tier`, plus the file backing it.
    async fn rig(
        tier: StorageTier,
        bytes: u64,
    ) -> (
        Arc<tokio::sync::RwLock<GlobalExtentMap>>,
        Arc<tokio::sync::RwLock<SlabRegistry>>,
        String,
    ) {
        let dir = std::env::temp_dir().join("stormblock-chunk-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("chunk-{}.bin", uuid::Uuid::new_v4().simple()));
        let path = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);

        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(&path, bytes).await.unwrap(),
        );
        let slab = Slab::format(dev, SLOT, tier).await.unwrap();
        let mut reg = SlabRegistry::new();
        reg.add(slab);

        (
            Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new())),
            Arc::new(tokio::sync::RwLock::new(reg)),
            path,
        )
    }

    fn request(volume: VolumeId, tier: StorageTier, chunk_len: u64, count: usize) -> AllocRequest {
        AllocRequest {
            volume,
            virtual_size: 64 * 1024 * 1024,
            slot_size: SLOT,
            tier,
            chunk_len,
            count,
            zero: false,
        }
    }

    #[tokio::test]
    async fn allocate_maps_slots_on_the_requested_tier() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let free_before = reg.read().await.total_free_slots();
        let chunks = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT * 4, 2))
            .await
            .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[1].offset, SLOT * 4);

        // Eager: the mapping exists now, not at first write.
        let g = gem.read().await;
        for vext in 0..8 {
            assert!(g.lookup(v, vext).is_some(), "extent {vext} must be mapped");
        }
        drop(g);
        assert_eq!(reg.read().await.total_free_slots(), free_before - 8);
        assert_eq!(map.owned_bytes(v), SLOT * 8);

        let _ = std::fs::remove_file(&path);
    }

    /// StormFS owns which tier. A tier with no room is a refusal, not a
    /// quiet demotion to a slower one.
    #[tokio::test]
    async fn a_tier_with_no_slabs_is_refused_rather_than_substituted() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let err = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Cold, SLOT, 1))
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkError::TierFull { .. }), "got {err}");
        assert_eq!(map.owned_bytes(v), 0);
        assert!(gem.read().await.get_volume_map(&v).is_none());

        let _ = std::fs::remove_file(&path);
    }

    /// A request that cannot be met in full leaves nothing behind — otherwise
    /// StormFS has to work out which of its chunks exist.
    #[tokio::test]
    async fn a_request_that_does_not_fit_takes_nothing() {
        // Small slab: a handful of slots once the header and slot table are
        // accounted for.
        let (gem, reg, path) = rig(StorageTier::Hot, 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let free_before = reg.read().await.total_free_slots();
        let want = free_before as usize + 4;

        let err = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT, want))
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkError::TierFull { .. }), "got {err}");

        assert_eq!(
            reg.read().await.total_free_slots(),
            free_before,
            "every slot taken on the way must go back"
        );
        assert_eq!(reg.read().await.in_flight_count(), 0, "no reservation may leak");
        assert_eq!(map.owned_bytes(v), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn deallocate_frees_and_re_freeing_is_a_success() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let free_before = reg.read().await.total_free_slots();
        let chunks = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT * 2, 1))
            .await
            .unwrap();

        let out = free(&mut map, &gem, &reg, &chunks, SLOT, true).await.unwrap();
        assert_eq!(out.freed, 2);
        assert_eq!(out.already_free, 0);
        assert_eq!(out.bytes_freed, SLOT * 2);
        assert!(out.failures.is_empty());
        assert_eq!(reg.read().await.total_free_slots(), free_before);
        assert_eq!(map.owned_bytes(v), 0);

        // The sweeper crashed before dropping its queue entry, so it retries.
        let again = free(&mut map, &gem, &reg, &chunks, SLOT, true).await.unwrap();
        assert_eq!(again.freed, 0);
        assert_eq!(again.already_free, 2, "already free is a success, not an error");
        assert!(again.failures.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    /// Trim gives the space back and keeps the address range, so a later
    /// allocate cannot hand the same offsets to somebody else.
    #[tokio::test]
    async fn trim_returns_space_but_not_the_address_range() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let free_before = reg.read().await.total_free_slots();
        let chunks = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT * 4, 1))
            .await
            .unwrap();

        let out = free(&mut map, &gem, &reg, &chunks, SLOT, false).await.unwrap();
        assert_eq!(out.bytes_freed, SLOT * 4);
        assert_eq!(
            reg.read().await.total_free_slots(),
            free_before,
            "trim must return the slots"
        );
        assert_eq!(
            map.owned_bytes(v),
            SLOT * 4,
            "trim must not return the address range"
        );

        // So the next allocation goes after it, not on top of it.
        let next = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT, 1))
            .await
            .unwrap();
        assert_eq!(next[0].offset, SLOT * 4);

        let _ = std::fs::remove_file(&path);
    }

    /// A partial trim frees nothing: the slot still holds bytes outside the
    /// range, and freeing it would lose them.
    #[tokio::test]
    async fn a_trim_inside_one_slot_frees_nothing() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        let chunks = allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT, 1))
            .await
            .unwrap();
        let free_after_alloc = reg.read().await.total_free_slots();

        let half = ChunkExtent {
            volume: v,
            offset: chunks[0].offset,
            len: SLOT / 2,
        };
        let out = free(&mut map, &gem, &reg, &[half], SLOT, false).await.unwrap();
        assert_eq!(out.freed, 0);
        assert_eq!(out.bytes_freed, 0);
        assert_eq!(reg.read().await.total_free_slots(), free_after_alloc);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_zeroed_chunk_does_not_carry_the_previous_tenants_bytes() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let first = vol();

        // Fill a chunk, then give it back.
        let chunks = allocate(&mut map, &gem, &reg, &request(first, StorageTier::Hot, SLOT, 1))
            .await
            .unwrap();
        let (slab_id, slot_idx) = {
            let g = gem.read().await;
            let loc = g.lookup(first, 0).unwrap();
            (loc.slab_id, loc.slot_idx)
        };
        {
            let r = reg.read().await;
            let slab = r.get(&slab_id).unwrap();
            slab.write_slot(slot_idx, 0, &vec![0xAB_u8; SLOT as usize])
                .await
                .unwrap();
        }
        free(&mut map, &gem, &reg, &chunks, SLOT, true).await.unwrap();

        // A second tenant takes the same slot back, asking for it zeroed.
        let second = vol();
        let mut req = request(second, StorageTier::Hot, SLOT, 1);
        req.zero = true;
        allocate(&mut map, &gem, &reg, &req).await.unwrap();

        let (slab_id, slot_idx) = {
            let g = gem.read().await;
            let loc = g.lookup(second, 0).unwrap();
            (loc.slab_id, loc.slot_idx)
        };
        let mut buf = vec![0xFF_u8; SLOT as usize];
        {
            let r = reg.read().await;
            r.get(&slab_id)
                .unwrap()
                .read_slot(slot_idx, 0, &mut buf)
                .await
                .unwrap();
        }
        assert!(
            buf.iter().all(|&b| b == 0),
            "a zeroed chunk must not read back the previous tenant's data"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn extent_map_flags_data_the_allocator_never_handed_out() {
        let (gem, reg, path) = rig(StorageTier::Hot, 4 * 1024 * 1024).await;
        let mut map = ChunkMap::new();
        let v = vol();

        allocate(&mut map, &gem, &reg, &request(v, StorageTier::Hot, SLOT * 2, 1))
            .await
            .unwrap();

        // Something wrote straight to the volume, past the chunk.
        {
            let mut g = gem.write().await;
            g.insert(
                v,
                9,
                ExtentLocation {
                    slab_id: reg.read().await.iter().next().map(|(id, _)| *id).unwrap(),
                    slot_idx: 99,
                    ref_count: 1,
                    generation: 1,
                },
            );
        }

        let report = extent_map(&map, &gem, v, SLOT, 64 * 1024 * 1024).await;
        assert_eq!(report.extents.len(), 3);
        assert_eq!(report.chunks, vec![[0, SLOT * 2]]);
        let stray: Vec<_> = report.extents.iter().filter(|e| !e.owned).collect();
        assert_eq!(stray.len(), 1, "fsck must be able to see the stray extent");
        assert_eq!(stray[0].offset, 9 * SLOT);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chunk_map_survives_a_round_trip() {
        let mut m = ChunkMap::new();
        let v = vol();
        m.claim(v, 0, 4096);
        m.claim(v, 8192, 4096);

        let json = serde_json::to_string(&m).unwrap();
        let back: ChunkMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ranges(v), vec![(0, 4096), (8192, 4096)]);
    }
}
