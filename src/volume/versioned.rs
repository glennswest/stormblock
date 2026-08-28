//! Fence-free commits for StormFS (#50) — versioned-map CAS, atomic
//! multi-block write, pinned-version reads.
//!
//! # Why these three are one module
//!
//! The issue lists three primitives. Two of them are the same mechanism seen
//! from different sides, and the third falls out of machinery the engine
//! already has.
//!
//! **Versioned-map CAS** — *"swap the mapping for range R from version V to
//! V+1 only if it is still V"* — and **atomic multi-block write** —
//! *all-or-nothing across several extents* — are both [`commit`]. A writer
//! fills scratch extents wherever it likes, then asks for the target range to
//! be re-pointed at them. Because that swap moves extent *identity* rather
//! than bytes, it is cheap enough to do under one lock, validated in full
//! before anything is applied: the range either moves or does not. That is
//! the atomic multi-block write. Gating the same swap on a version is the
//! CAS, and it is what makes a stalled writer harmless rather than dangerous
//! — its data landed in extents nobody points at, and its swap fails the
//! check. No fencing round trip, and no correctness argument that depends on
//! clocks.
//!
//! **Pinned-version reads** are the engine's copy-on-write retention exposed.
//! A pin is a snapshot: taking one bumps the reference count on every extent
//! the volume holds, so a later commit that supersedes an extent decrements
//! it to one rather than freeing it, and the pinned reader keeps reading the
//! bytes it started with. No new retention machinery, and nothing for tier
//! migration to coordinate — a reader pinned to an older version keeps
//! reading the source chunks until it releases.
//!
//! # What makes a commit untearable
//!
//! Not a journal. The durable record of an extent map is the volume metadata
//! file, which [`crate::volume::MetadataStore`] writes whole, atomically, with
//! a checksum. A commit mutates the in-memory map under one lock and the map
//! is then persisted in one piece, so a crash finds either the whole swap or
//! none of it — never half. The slot table is re-pointed to match
//! ([`crate::drive::slab::Slab::reassign_slot`]) so the rebuild-from-slabs
//! fallback agrees with it.
//!
//! Versions live in their own file, and the **order the two are written in is
//! load-bearing**: versions first, then the map. A crash between them leaves
//! the version ahead of the map, so a writer holding the old version is told
//! it is stale, re-reads, and finds the old data — it retries. The other order
//! leaves the version *behind* a map that has already moved, and a writer
//! holding the old version would then commit over data that is already
//! committed. Version numbers have to be monotonic, not gapless, so burning
//! one is free and reusing one is not.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::slab_registry::SlabRegistry;

use super::extent::VolumeId;
use super::gem::GlobalExtentMap;

/// Version of every extent that has one. An extent with no entry is at
/// version 0, which is also what a never-committed range reads as.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionMap {
    #[serde(default)]
    versions: HashMap<VolumeId, BTreeMap<u64, u64>>,
}

impl VersionMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Version of one virtual extent.
    pub fn extent_version(&self, volume: VolumeId, vext: u64) -> u64 {
        self.versions
            .get(&volume)
            .and_then(|m| m.get(&vext))
            .copied()
            .unwrap_or(0)
    }

    /// Lowest and highest version across `[first, last)`.
    ///
    /// A range that has only ever been committed as a whole has one version
    /// throughout, and these are equal. They differ when a caller has
    /// committed overlapping ranges that do not line up, which is exactly the
    /// case a CAS must refuse rather than average over.
    pub fn range_versions(&self, volume: VolumeId, first: u64, last: u64) -> (u64, u64) {
        let mut lo = u64::MAX;
        let mut hi = 0;
        for vext in first..last {
            let v = self.extent_version(volume, vext);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if lo == u64::MAX {
            (0, 0)
        } else {
            (lo, hi)
        }
    }

    /// Highest version anywhere in a volume — what a pin is labelled with.
    pub fn volume_version(&self, volume: VolumeId) -> u64 {
        self.versions
            .get(&volume)
            .and_then(|m| m.values().max().copied())
            .unwrap_or(0)
    }

    /// Set every extent in `[first, last)` to `version`.
    pub fn set_range(&mut self, volume: VolumeId, first: u64, last: u64, version: u64) {
        let m = self.versions.entry(volume).or_default();
        for vext in first..last {
            m.insert(vext, version);
        }
    }

    /// Drop the versions for a range — the extents are gone.
    pub fn clear_range(&mut self, volume: VolumeId, first: u64, last: u64) {
        if let Some(m) = self.versions.get_mut(&volume) {
            for vext in first..last {
                m.remove(&vext);
            }
            if m.is_empty() {
                self.versions.remove(&volume);
            }
        }
    }

    /// Forget a deleted volume.
    pub fn forget(&mut self, volume: VolumeId) {
        self.versions.remove(&volume);
    }

    /// Number of volumes carrying versions.
    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }
}

/// Where the writer staged the data it wants committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedRange {
    pub volume: VolumeId,
    pub offset: u64,
}

/// One compare-and-swap commit.
#[derive(Debug, Clone)]
pub struct CommitRequest {
    /// Volume whose map is being swapped.
    pub volume: VolumeId,
    /// Target range, slot-aligned.
    pub offset: u64,
    pub len: u64,
    pub slot_size: u64,
    /// The version the writer believes the range is at.
    pub expected_version: u64,
    /// The staged extents to swap in. `None` punches the range out
    /// atomically — an all-or-nothing truncate, rather than a write.
    pub staged: Option<StagedRange>,
}

/// Why a commit did not happen.
#[derive(Debug)]
pub enum CommitError {
    /// Somebody else got there first. The writer's data is in extents nobody
    /// points at, so nothing has been corrupted — re-read at `current` and
    /// try again.
    StaleVersion { expected: u64, current: u64 },
    /// The range spans extents at different versions, so no single expected
    /// version can describe it. Commit the pieces that were committed
    /// together.
    MixedVersions { low: u64, high: u64 },
    /// The staged range has a gap. Swapping it in would silently replace
    /// live data with zeros, so it is refused instead.
    StagedHole { offset: u64 },
    Unaligned {
        what: &'static str,
        value: u64,
        slot_size: u64,
    },
    Invalid(String),
    /// The swap itself failed part-way. The map is unchanged.
    Failed(String),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::StaleVersion { expected, current } => write!(
                f,
                "range is at version {current}, not {expected} — your data is still in the extents \
                 you staged and nothing points at them, so re-read at {current} and commit again"
            ),
            CommitError::MixedVersions { low, high } => write!(
                f,
                "range spans versions {low}..{high}, so no single expected version describes it"
            ),
            CommitError::StagedHole { offset } => write!(
                f,
                "nothing is mapped at staged offset {offset}: committing the range would replace \
                 live data with zeros"
            ),
            CommitError::Unaligned { what, value, slot_size } => write!(
                f,
                "{what} {value} is not a multiple of the slot size {slot_size}"
            ),
            CommitError::Invalid(m) => write!(f, "{m}"),
            CommitError::Failed(m) => write!(f, "commit failed, map unchanged: {m}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// What a commit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub version: u64,
    /// Extents whose mapping moved.
    pub extents_swapped: usize,
    /// Extents the target range held before, now dereferenced. A pinned
    /// reader keeps them alive; otherwise the space comes straight back.
    pub extents_released: usize,
    /// Slots the slab would not release. The swap still happened.
    pub failures: Vec<String>,
}

/// Swap a range's mapping, if it is still at the version the writer expects.
///
/// Everything is checked before anything is applied, and the whole thing runs
/// under the map and registry locks, so concurrent readers see the range
/// before or after — never part-way through. See the module docs for what
/// makes that hold across a crash as well as across a race.
pub async fn commit(
    versions: &mut VersionMap,
    gem: &Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: &Arc<tokio::sync::RwLock<SlabRegistry>>,
    req: &CommitRequest,
) -> Result<CommitOutcome, CommitError> {
    if req.len == 0 {
        return Err(CommitError::Invalid(
            "a commit must cover at least one slot".into(),
        ));
    }
    if req.offset % req.slot_size != 0 {
        return Err(CommitError::Unaligned {
            what: "offset",
            value: req.offset,
            slot_size: req.slot_size,
        });
    }
    if req.len % req.slot_size != 0 {
        return Err(CommitError::Unaligned {
            what: "length",
            value: req.len,
            slot_size: req.slot_size,
        });
    }
    if let Some(s) = &req.staged {
        if s.offset % req.slot_size != 0 {
            return Err(CommitError::Unaligned {
                what: "staged offset",
                value: s.offset,
                slot_size: req.slot_size,
            });
        }
        if s.volume == req.volume
            && s.offset < req.offset + req.len
            && req.offset < s.offset + req.len
        {
            return Err(CommitError::Invalid(
                "the staged range overlaps the range it is being committed into".into(),
            ));
        }
    }

    let first = req.offset / req.slot_size;
    let last = (req.offset + req.len) / req.slot_size;

    // The version check and the swap have to see the same map, so one lock
    // spans both. This is the only fence in the design, and it is local.
    let mut g = gem.write().await;
    let mut reg = registry.write().await;

    let (lo, hi) = versions.range_versions(req.volume, first, last);
    if lo != hi {
        return Err(CommitError::MixedVersions { low: lo, high: hi });
    }
    if lo != req.expected_version {
        return Err(CommitError::StaleVersion {
            expected: req.expected_version,
            current: lo,
        });
    }

    // Resolve every staged extent before touching anything: a gap found
    // half-way through would leave the range torn, which is the failure this
    // primitive exists to remove.
    let mut incoming = Vec::with_capacity((last - first) as usize);
    if let Some(s) = &req.staged {
        let staged_first = s.offset / req.slot_size;
        for i in 0..(last - first) {
            let src_vext = staged_first + i;
            match g.lookup(s.volume, src_vext) {
                Some(loc) => incoming.push((src_vext, loc.clone())),
                None => {
                    return Err(CommitError::StagedHole {
                        offset: src_vext * req.slot_size,
                    })
                }
            }
        }
    }

    // Everything below this line is bookkeeping that cannot fail in a way
    // that leaves the range half-swapped: the map operations are infallible,
    // and slot-table writes are reported but do not undo the map.
    let mut out = CommitOutcome {
        version: req.expected_version + 1,
        extents_swapped: 0,
        extents_released: 0,
        failures: Vec::new(),
    };

    for (i, vext) in (first..last).enumerate() {
        let displaced = g.remove(req.volume, vext);

        if let Some((src_vext, loc)) = incoming.get(i).cloned() {
            let staged_volume = req.staged.as_ref().map(|s| s.volume).unwrap_or(req.volume);
            g.remove(staged_volume, src_vext);

            // Re-point the slot itself, so the slot table names the address
            // the data now lives at rather than the scratch one.
            let mut moved = loc.clone();
            if let Some(slab) = reg.get_mut(&loc.slab_id) {
                match slab.reassign_slot(loc.slot_idx, req.volume, vext).await {
                    Ok(()) => moved.generation = loc.generation + 1,
                    Err(e) => out.failures.push(format!(
                        "slab {} slot {}: {e}",
                        loc.slab_id.0, loc.slot_idx
                    )),
                }
            }
            g.insert(req.volume, vext, moved);
            out.extents_swapped += 1;
        }

        // The old extent goes last: a pin holds a reference to it, so this
        // decrements rather than frees, and the pinned reader is untouched.
        if let Some(old) = displaced {
            out.extents_released += 1;
            for leg in old.legs() {
                if let Some(slab) = reg.get_mut(&leg.slab_id) {
                    if let Err(e) = slab.dec_ref(leg.slot_idx).await {
                        out.failures.push(format!(
                            "slab {} slot {}: {e}",
                            leg.slab_id.0, leg.slot_idx
                        ));
                    }
                }
            }
        }
    }

    // Including the punch-out case: a range with nothing mapped under it must
    // not read back as version 0, or a writer that saw it at V would be told
    // it is untouched and would commit over the truncate.
    versions.set_range(req.volume, first, last, out.version);

    Ok(out)
}

/// A reader's hold on a point-in-time image.
///
/// The `snapshot` is an ordinary volume: it can be exported and read through
/// the same path as any other, which is what keeps StormFS's rule that no
/// process sits in the data path. Releasing the pin deletes it, and the
/// extents it was keeping alive come back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub id: uuid::Uuid,
    /// The volume that was pinned.
    pub volume: VolumeId,
    /// The volume to read instead, for the duration of the pin.
    pub snapshot: VolumeId,
    /// Version the volume was at when the pin was taken.
    pub version: u64,
    pub created_unix: u64,
}

/// Every pin this node is holding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinTable {
    #[serde(default)]
    pins: HashMap<uuid::Uuid, Pin>,
}

impl PinTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, pin: Pin) {
        self.pins.insert(pin.id, pin);
    }

    pub fn get(&self, id: &uuid::Uuid) -> Option<&Pin> {
        self.pins.get(id)
    }

    pub fn remove(&mut self, id: &uuid::Uuid) -> Option<Pin> {
        self.pins.remove(id)
    }

    pub fn list(&self) -> Vec<Pin> {
        let mut v: Vec<Pin> = self.pins.values().cloned().collect();
        v.sort_by_key(|p| p.created_unix);
        v
    }

    /// Pins held on one volume.
    pub fn for_volume(&self, volume: VolumeId) -> Vec<Pin> {
        self.pins
            .values()
            .filter(|p| p.volume == volume)
            .cloned()
            .collect()
    }

    /// Whether any pin reads through this snapshot volume — asked before
    /// deleting a volume, since deleting a pin's snapshot out from under a
    /// reader is what the pin exists to prevent.
    pub fn is_pinned_snapshot(&self, volume: VolumeId) -> bool {
        self.pins.values().any(|p| p.snapshot == volume)
    }

    pub fn len(&self) -> usize {
        self.pins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::drive::BlockDevice;
    use crate::placement::topology::StorageTier;
    use crate::volume::chunk::{self, AllocRequest, ChunkMap};

    const SLOT: u64 = 4096;

    fn vol() -> VolumeId {
        VolumeId::new()
    }

    #[test]
    fn an_uncommitted_range_is_version_zero() {
        let v = VersionMap::new();
        assert_eq!(v.extent_version(vol(), 0), 0);
        assert_eq!(v.range_versions(vol(), 0, 8), (0, 0));
    }

    #[test]
    fn set_range_makes_a_range_uniform() {
        let mut v = VersionMap::new();
        let id = vol();
        v.set_range(id, 0, 4, 7);
        assert_eq!(v.range_versions(id, 0, 4), (7, 7));
        assert_eq!(v.volume_version(id), 7);
        // Outside the range is untouched.
        assert_eq!(v.range_versions(id, 0, 5), (0, 7));
    }

    #[test]
    fn clear_range_drops_versions() {
        let mut v = VersionMap::new();
        let id = vol();
        v.set_range(id, 0, 4, 3);
        v.clear_range(id, 0, 4);
        assert!(v.is_empty());
    }

    #[test]
    fn pins_are_listed_and_found_by_snapshot() {
        let mut t = PinTable::new();
        let volume = vol();
        let snapshot = vol();
        let id = uuid::Uuid::new_v4();
        t.insert(Pin { id, volume, snapshot, version: 4, created_unix: 1 });

        assert_eq!(t.len(), 1);
        assert_eq!(t.for_volume(volume).len(), 1);
        assert!(t.is_pinned_snapshot(snapshot));
        assert!(!t.is_pinned_snapshot(volume));
        assert!(t.remove(&id).is_some());
        assert!(t.is_empty());
    }

    // ---- commits against a real slab ------------------------------------

    struct Rig {
        gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
        registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
        path: String,
    }

    async fn rig() -> Rig {
        let dir = std::env::temp_dir().join("stormblock-versioned-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("v-{}.bin", uuid::Uuid::new_v4().simple()));
        let path = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);

        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(&path, 8 * 1024 * 1024)
                .await
                .unwrap(),
        );
        let slab = Slab::format(dev, SLOT, StorageTier::Hot).await.unwrap();
        let mut reg = SlabRegistry::new();
        reg.add(slab);

        Rig {
            gem: Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new())),
            registry: Arc::new(tokio::sync::RwLock::new(reg)),
            path,
        }
    }

    impl Rig {
        /// Allocate a chunk at the next free address and fill it with `byte`.
        async fn staged(&self, map: &mut ChunkMap, volume: VolumeId, slots: u64, byte: u8) -> u64 {
            let chunks = chunk::allocate(
                map,
                &self.gem,
                &self.registry,
                &AllocRequest {
                    volume,
                    virtual_size: 16 * 1024 * 1024,
                    slot_size: SLOT,
                    tier: StorageTier::Hot,
                    chunk_len: SLOT * slots,
                    count: 1,
                    zero: false,
                },
            )
            .await
            .unwrap();
            let offset = chunks[0].offset;
            for i in 0..slots {
                self.fill(volume, offset / SLOT + i, byte).await;
            }
            offset
        }

        async fn fill(&self, volume: VolumeId, vext: u64, byte: u8) {
            let loc = self.gem.read().await.lookup(volume, vext).cloned().unwrap();
            let reg = self.registry.read().await;
            reg.get(&loc.slab_id)
                .unwrap()
                .write_slot(loc.slot_idx, 0, &vec![byte; SLOT as usize])
                .await
                .unwrap();
        }

        async fn byte_at(&self, volume: VolumeId, vext: u64) -> Option<u8> {
            let loc = self.gem.read().await.lookup(volume, vext).cloned()?;
            let reg = self.registry.read().await;
            let mut buf = vec![0u8; SLOT as usize];
            reg.get(&loc.slab_id)
                .unwrap()
                .read_slot(loc.slot_idx, 0, &mut buf)
                .await
                .unwrap();
            Some(buf[0])
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn req(volume: VolumeId, offset: u64, slots: u64, expected: u64, staged: Option<u64>) -> CommitRequest {
        CommitRequest {
            volume,
            offset,
            len: SLOT * slots,
            slot_size: SLOT,
            expected_version: expected,
            staged: staged.map(|o| StagedRange { volume, offset: o }),
        }
    }

    #[tokio::test]
    async fn a_commit_swaps_the_range_and_bumps_the_version() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        // Live data at 0, new data staged further along.
        let live = r.staged(&mut map, v, 2, 0x11).await;
        let staged = r.staged(&mut map, v, 2, 0x22).await;
        assert_eq!(live, 0);

        let out = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 2, 0, Some(staged)))
            .await
            .unwrap();

        assert_eq!(out.version, 1);
        assert_eq!(out.extents_swapped, 2);
        assert_eq!(out.extents_released, 2);
        assert!(out.failures.is_empty(), "{:?}", out.failures);

        // The target now reads the staged bytes...
        assert_eq!(r.byte_at(v, 0).await, Some(0x22));
        assert_eq!(r.byte_at(v, 1).await, Some(0x22));
        // ...and the staging address is empty, its extents having moved.
        assert!(r.gem.read().await.lookup(v, staged / SLOT).is_none());
        assert_eq!(versions.range_versions(v, 0, 2), (1, 1));

        r.cleanup();
    }

    /// The point of the CAS: a writer that stalled long enough to lose its
    /// lease is harmless. Its swap fails, and it has corrupted nothing.
    #[tokio::test]
    async fn a_stale_writer_is_refused_and_changes_nothing() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        let live = r.staged(&mut map, v, 2, 0x11).await;
        let quick = r.staged(&mut map, v, 2, 0x22).await;
        let slow = r.staged(&mut map, v, 2, 0x33).await;

        // The quick writer commits first.
        commit(&mut versions, &r.gem, &r.registry, &req(v, live, 2, 0, Some(quick)))
            .await
            .unwrap();

        // The slow one still believes the range is at version 0.
        let err = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 2, 0, Some(slow)))
            .await
            .unwrap_err();
        match err {
            CommitError::StaleVersion { expected, current } => {
                assert_eq!(expected, 0);
                assert_eq!(current, 1);
            }
            other => panic!("expected a stale-version refusal, got {other}"),
        }

        // The winner's data stands, and the loser's is still where it put it.
        assert_eq!(r.byte_at(v, 0).await, Some(0x22));
        assert_eq!(r.byte_at(v, slow / SLOT).await, Some(0x33));
        assert_eq!(versions.range_versions(v, 0, 2), (1, 1));

        // And it can simply retry at the version it was told.
        let out = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 2, 1, Some(slow)))
            .await
            .unwrap();
        assert_eq!(out.version, 2);
        assert_eq!(r.byte_at(v, 0).await, Some(0x33));

        r.cleanup();
    }

    /// All-or-nothing: a staged range with a gap in it is refused outright
    /// rather than leaving the target part new and part old.
    #[tokio::test]
    async fn a_gap_in_the_staged_range_leaves_the_target_untouched() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        let live = r.staged(&mut map, v, 4, 0x11).await;
        // Stage only two slots, then claim a four-slot commit from there.
        let staged = r.staged(&mut map, v, 2, 0x22).await;

        let err = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 4, 0, Some(staged)))
            .await
            .unwrap_err();
        assert!(matches!(err, CommitError::StagedHole { .. }), "got {err}");

        // Every one of the four target extents still holds the old data —
        // the swap did not start.
        for vext in 0..4 {
            assert_eq!(r.byte_at(v, vext).await, Some(0x11), "extent {vext} was torn");
        }
        assert_eq!(versions.range_versions(v, 0, 4), (0, 0));

        r.cleanup();
    }

    /// A commit with nothing staged punches the range out atomically — the
    /// truncate case — and the space comes back.
    #[tokio::test]
    async fn a_commit_with_nothing_staged_punches_the_range_out() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        let free_before = r.registry.read().await.total_free_slots();
        let live = r.staged(&mut map, v, 3, 0x11).await;

        let out = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 3, 0, None))
            .await
            .unwrap();
        assert_eq!(out.extents_swapped, 0);
        assert_eq!(out.extents_released, 3);
        assert_eq!(out.version, 1);

        assert!(r.byte_at(v, 0).await.is_none(), "the range must be unmapped");
        assert_eq!(
            r.registry.read().await.total_free_slots(),
            free_before,
            "a punched-out range returns its slots"
        );
        // Still at version 1, not back to 0: a writer that saw version 0
        // must not be told the range is untouched.
        assert_eq!(versions.range_versions(v, 0, 3), (1, 1));

        r.cleanup();
    }

    /// Committing into the range you staged into would free the extents it is
    /// swapping in. Refused.
    #[tokio::test]
    async fn a_self_overlapping_commit_is_refused() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        r.staged(&mut map, v, 4, 0x11).await;
        let err = commit(&mut versions, &r.gem, &r.registry, &req(v, 0, 4, 0, Some(SLOT * 2)))
            .await
            .unwrap_err();
        assert!(matches!(err, CommitError::Invalid(_)), "got {err}");

        r.cleanup();
    }

    #[tokio::test]
    async fn a_range_spanning_two_versions_cannot_be_described_by_one() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let mut versions = VersionMap::new();
        let v = vol();

        let live = r.staged(&mut map, v, 4, 0x11).await;
        let staged = r.staged(&mut map, v, 2, 0x22).await;

        // Commit the first half only.
        commit(&mut versions, &r.gem, &r.registry, &req(v, live, 2, 0, Some(staged)))
            .await
            .unwrap();

        // Now the four-slot range is half at 1 and half at 0.
        let more = r.staged(&mut map, v, 4, 0x33).await;
        let err = commit(&mut versions, &r.gem, &r.registry, &req(v, live, 4, 1, Some(more)))
            .await
            .unwrap_err();
        match err {
            CommitError::MixedVersions { low, high } => {
                assert_eq!((low, high), (0, 1));
            }
            other => panic!("expected mixed versions, got {other}"),
        }

        r.cleanup();
    }

    #[tokio::test]
    async fn unaligned_commits_are_refused() {
        let r = rig().await;
        let mut versions = VersionMap::new();
        let v = vol();

        let mut bad = req(v, SLOT / 2, 1, 0, None);
        let err = commit(&mut versions, &r.gem, &r.registry, &bad).await.unwrap_err();
        assert!(matches!(err, CommitError::Unaligned { what: "offset", .. }), "got {err}");

        bad = req(v, 0, 1, 0, None);
        bad.len = SLOT + 1;
        let err = commit(&mut versions, &r.gem, &r.registry, &bad).await.unwrap_err();
        assert!(matches!(err, CommitError::Unaligned { what: "length", .. }), "got {err}");

        r.cleanup();
    }

    /// A commit is validated before it is applied, so two of them racing on
    /// one range leave it whole: one wins, the other is told it is stale.
    #[tokio::test]
    async fn concurrent_commits_on_one_range_do_not_tear_it() {
        let r = rig().await;
        let mut map = ChunkMap::new();
        let versions = Arc::new(tokio::sync::Mutex::new(VersionMap::new()));
        let v = vol();

        let live = r.staged(&mut map, v, 4, 0x11).await;
        let mut staged = Vec::new();
        for byte in [0xA0u8, 0xB0, 0xC0, 0xD0] {
            staged.push(r.staged(&mut map, v, 4, byte).await);
        }

        let gem = r.gem.clone();
        let registry = r.registry.clone();
        let mut tasks = Vec::new();
        for offset in staged {
            let (versions, gem, registry) = (versions.clone(), gem.clone(), registry.clone());
            tasks.push(tokio::spawn(async move {
                let mut guard = versions.lock().await;
                commit(&mut guard, &gem, &registry, &req(v, live, 4, 0, Some(offset))).await
            }));
        }

        let mut winners = 0;
        for t in tasks {
            if t.await.unwrap().is_ok() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "exactly one commit may win the CAS");

        // Whichever won, all four extents carry its bytes — none are mixed.
        let first = r.byte_at(v, 0).await.unwrap();
        for vext in 1..4 {
            assert_eq!(r.byte_at(v, vext).await, Some(first), "range was torn");
        }
        assert_eq!(versions.lock().await.range_versions(v, 0, 4), (1, 1));

        r.cleanup();
    }
}
