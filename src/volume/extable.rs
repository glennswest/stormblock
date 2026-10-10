//! A volume's extent map, compact (#155).
//!
//! A `BTreeMap<u64, ExtentLocation>` cost ~100 B an extent: the 56-byte
//! location (a 16-byte slab uuid, a `Vec` for mirrors that are almost always
//! absent) plus a B-tree node's share. At 1 MiB extents that is ~100 GB per
//! PB written, in memory, on every node.
//!
//! Here an extent is 28 bytes (the slot a u64, #158): its offset in a chunk, the slab as an ordinal
//! into a process-wide table of slab ids, the slot, the share count and the
//! generation. Extents live in chunks of 64 consecutive virtual extents, one
//! B-tree entry per chunk, sorted inside it, so a volume written densely pays
//! ~29 B an extent and a sparse one pays the chunk's overhead only where it
//! has extents. Mirror legs are kept out of line, by extent, since most
//! extents have none.
//!
//! What callers see is unchanged in substance: `ExtentLocation`s, by value.
//! The on-disk record (`VolumeRecord.extents`) is still the B-tree, built by
//! [`ExtentTable::to_btree`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};

use crate::drive::slab::SlabId;

use super::gem::{ExtentLocation, Leg};

const CHUNK_SHIFT: u32 = 6;
const CHUNK_MASK: u64 = (1 << CHUNK_SHIFT) - 1;
/// Grow a chunk by this many entries at a time: a chunk's spare room stays
/// under a quarter of what a doubling `Vec` would leave.
const GROW: usize = 8;

/// Slab ids by ordinal, for the life of the process. A slab is a few per
/// drive; ordinals are never reused, so a map taken out of the extent map
/// (a deleted volume's) still decodes.
struct Interner {
    ids: Vec<SlabId>,
    ords: HashMap<SlabId, u32>,
}

fn interner() -> &'static RwLock<Interner> {
    static I: OnceLock<RwLock<Interner>> = OnceLock::new();
    I.get_or_init(|| RwLock::new(Interner { ids: Vec::new(), ords: HashMap::new() }))
}

/// The ordinal of a slab id, taking a new one the first time it is seen.
pub fn slab_ordinal(id: SlabId) -> u32 {
    if let Some(o) = interner().read().unwrap_or_else(|e| e.into_inner()).ords.get(&id) {
        return *o;
    }
    let mut w = interner().write().unwrap_or_else(|e| e.into_inner());
    if let Some(o) = w.ords.get(&id) {
        return *o;
    }
    let o = u32::try_from(w.ids.len()).expect("more than 4 G slab ids in one process");
    w.ids.push(id);
    w.ords.insert(id, o);
    o
}

/// The slab id of an ordinal [`slab_ordinal`] gave.
pub fn slab_of(ord: u32) -> SlabId {
    interner().read().unwrap_or_else(|e| e.into_inner()).ids[ord as usize]
}

/// One extent: 28 bytes.
#[derive(Clone, Copy)]
#[repr(C, packed(4))]
struct Packed {
    /// Position in the chunk (low bits of the virtual extent).
    off: u8,
    _pad: [u8; 3],
    slab: u32,
    slot: u64,
    refs: u32,
    generation: u64,
}

impl Packed {
    fn of(off: u8, loc: &ExtentLocation) -> Packed {
        Packed {
            off,
            _pad: [0; 3],
            slab: slab_ordinal(loc.slab_id),
            slot: loc.slot_idx,
            refs: loc.ref_count,
            generation: loc.generation,
        }
    }

    fn location(&self, mirrors: Vec<Leg>) -> ExtentLocation {
        let (slab, slot, refs, generation) = (self.slab, self.slot, self.refs, self.generation);
        ExtentLocation { slab_id: slab_of(slab), slot_idx: slot, ref_count: refs, generation, mirrors }
    }
}

/// Virtual extent → location, compact. See the module documentation.
#[derive(Clone, Default)]
pub struct ExtentTable {
    chunks: BTreeMap<u64, Vec<Packed>>,
    mirrors: HashMap<u64, Vec<Leg>>,
    len: usize,
}

impl std::fmt::Debug for ExtentTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

fn split(vext: u64) -> (u64, u8) {
    (vext >> CHUNK_SHIFT, (vext & CHUNK_MASK) as u8)
}

impl ExtentTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn find(&self, vext: u64) -> Option<&Packed> {
        let (c, off) = split(vext);
        let chunk = self.chunks.get(&c)?;
        chunk.binary_search_by_key(&off, |p| p.off).ok().map(|i| &chunk[i])
    }

    fn mirrors_of(&self, vext: u64) -> Vec<Leg> {
        if self.mirrors.is_empty() {
            return Vec::new();
        }
        self.mirrors.get(&vext).cloned().unwrap_or_default()
    }

    pub fn get(&self, vext: &u64) -> Option<ExtentLocation> {
        self.find(*vext).map(|p| p.location(self.mirrors_of(*vext)))
    }

    pub fn contains_key(&self, vext: &u64) -> bool {
        self.find(*vext).is_some()
    }

    /// Map `vext` to `loc`; what it mapped to before, if anything.
    pub fn insert(&mut self, vext: u64, loc: ExtentLocation) -> Option<ExtentLocation> {
        let (c, off) = split(vext);
        let old_mirrors = if loc.mirrors.is_empty() {
            self.mirrors.remove(&vext)
        } else {
            self.mirrors.insert(vext, loc.mirrors.clone())
        };
        let packed = Packed::of(off, &loc);
        let chunk = self.chunks.entry(c).or_default();
        match chunk.binary_search_by_key(&off, |p| p.off) {
            Ok(i) => {
                let old = chunk[i];
                chunk[i] = packed;
                Some(old.location(old_mirrors.unwrap_or_default()))
            }
            Err(i) => {
                if chunk.len() == chunk.capacity() {
                    chunk.reserve_exact(GROW);
                }
                chunk.insert(i, packed);
                self.len += 1;
                None
            }
        }
    }

    pub fn remove(&mut self, vext: &u64) -> Option<ExtentLocation> {
        let (c, off) = split(*vext);
        let chunk = self.chunks.get_mut(&c)?;
        let i = chunk.binary_search_by_key(&off, |p| p.off).ok()?;
        let p = chunk.remove(i);
        if chunk.is_empty() {
            self.chunks.remove(&c);
        } else if chunk.capacity() - chunk.len() > 2 * GROW {
            chunk.shrink_to(chunk.len() + GROW);
        }
        self.len -= 1;
        let mirrors = self.mirrors.remove(vext).unwrap_or_default();
        Some(p.location(mirrors))
    }

    /// Change one extent's location in place. `None` when it is not mapped.
    pub fn update<R>(&mut self, vext: u64, f: impl FnOnce(&mut ExtentLocation) -> R) -> Option<R> {
        let mut loc = self.get(&vext)?;
        let r = f(&mut loc);
        self.insert(vext, loc);
        Some(r)
    }

    /// Change every extent in place; only the ones `f` changed are written
    /// back. `f` says whether it changed anything.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(u64, &mut ExtentLocation) -> bool) {
        let keys: Vec<u64> = self.keys().collect();
        for vext in keys {
            let Some(mut loc) = self.get(&vext) else { continue };
            if f(vext, &mut loc) {
                self.insert(vext, loc);
            }
        }
    }

    /// Add `n` to every extent's share count (a clone shares them all).
    pub fn add_refs(&mut self, n: u32) {
        for chunk in self.chunks.values_mut() {
            for p in chunk.iter_mut() {
                let r = p.refs;
                p.refs = r.saturating_add(n);
            }
        }
    }

    /// The same map with every virtual extent moved up by `base` extents
    /// (a golden gathered into a composed disk).
    pub fn shifted(&self, base: u64) -> ExtentTable {
        if base & CHUNK_MASK == 0 {
            let shift = base >> CHUNK_SHIFT;
            return ExtentTable {
                chunks: self.chunks.iter().map(|(c, v)| (c + shift, v.clone())).collect(),
                mirrors: self.mirrors.iter().map(|(v, m)| (v + base, m.clone())).collect(),
                len: self.len,
            };
        }
        self.iter().map(|(v, l)| (v + base, l)).collect()
    }

    /// Put every extent of `other` into this map (replacing any already
    /// there at the same virtual extent).
    pub fn extend_from(&mut self, other: &ExtentTable) {
        for (v, l) in other.iter() {
            self.insert(v, l);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, ExtentLocation)> + '_ {
        // One lookup of the slab table per change of slab, not per extent.
        let mut last: Option<(u32, SlabId)> = None;
        self.chunks.iter().flat_map(|(c, chunk)| chunk.iter().map(move |p| (c, *p))).map(move |(c, p)| {
            let vext = (c << CHUNK_SHIFT) | p.off as u64;
            let ord = p.slab;
            let slab_id = match last {
                Some((o, id)) if o == ord => id,
                _ => {
                    let id = slab_of(ord);
                    last = Some((ord, id));
                    id
                }
            };
            let (slot, refs, generation) = (p.slot, p.refs, p.generation);
            let mirrors = self.mirrors_of(vext);
            (vext, ExtentLocation { slab_id, slot_idx: slot, ref_count: refs, generation, mirrors })
        })
    }

    /// The mapped extents in `range`, in order: the chunks outside it are
    /// never visited (#300: a format zeroing terabytes of unmapped inode
    /// tables must not cost a lookup per extent).
    pub fn keys_in(&self, range: std::ops::Range<u64>) -> impl Iterator<Item = u64> + '_ {
        let (lo, hi) = (range.start, range.end);
        let first = lo >> CHUNK_SHIFT;
        let last = if hi == 0 { 0 } else { ((hi - 1) >> CHUNK_SHIFT) + 1 };
        self.chunks
            .range(first..last.max(first))
            .flat_map(|(c, chunk)| chunk.iter().map(move |p| (c << CHUNK_SHIFT) | p.off as u64))
            .filter(move |v| (lo..hi).contains(v))
    }

    pub fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.chunks.iter().flat_map(|(c, chunk)| chunk.iter().map(move |p| (c << CHUNK_SHIFT) | p.off as u64))
    }

    pub fn values(&self) -> impl Iterator<Item = ExtentLocation> + '_ {
        self.iter().map(|(_, l)| l)
    }

    /// Share counts only, without building locations.
    pub fn ref_counts(&self) -> impl Iterator<Item = u32> + '_ {
        self.chunks.values().flat_map(|c| c.iter().map(|p| p.refs))
    }

    /// The map as the on-disk record holds it.
    pub fn to_btree(&self) -> BTreeMap<u64, ExtentLocation> {
        self.iter().collect()
    }
}

impl FromIterator<(u64, ExtentLocation)> for ExtentTable {
    fn from_iter<I: IntoIterator<Item = (u64, ExtentLocation)>>(iter: I) -> Self {
        let mut t = ExtentTable::new();
        for (v, l) in iter {
            t.insert(v, l);
        }
        t
    }
}

impl From<&BTreeMap<u64, ExtentLocation>> for ExtentTable {
    fn from(m: &BTreeMap<u64, ExtentLocation>) -> Self {
        m.iter().map(|(v, l)| (*v, l.clone())).collect()
    }
}

impl<'a> IntoIterator for &'a ExtentTable {
    type Item = (u64, ExtentLocation);
    type IntoIter = Box<dyn Iterator<Item = (u64, ExtentLocation)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::SliceRandom;
    use rand::Rng;
    use uuid::Uuid;

    fn sid() -> SlabId {
        SlabId(Uuid::new_v4())
    }

    #[test]
    fn keys_in_a_range_are_the_mapped_ones_there_only() {
        let s = sid();
        let mut t = ExtentTable::new();
        for v in [0u64, 5, 63, 64, 65, 200, 1 << 40, (1 << 40) + 1] {
            t.insert(v, ExtentLocation::new(s, v));
        }
        let got = |r: std::ops::Range<u64>| t.keys_in(r).collect::<Vec<_>>();
        assert_eq!(got(0..u64::MAX), t.keys().collect::<Vec<_>>());
        assert_eq!(got(5..65), vec![5, 63, 64]);
        assert_eq!(got(64..65), vec![64]);
        assert_eq!(got(66..200), Vec::<u64>::new());
        assert_eq!(got(1..(1 << 41)), vec![5, 63, 64, 65, 200, 1 << 40, (1 << 40) + 1]);
        assert_eq!(got(7..7), Vec::<u64>::new());
    }

    #[test]
    fn an_entry_is_28_bytes() {
        assert_eq!(std::mem::size_of::<Packed>(), 28);
    }

    /// The table answers exactly what a B-tree would, under a random mix of
    /// inserts, replacements, removals and in-place updates.
    #[test]
    fn agrees_with_a_btree_under_random_operations() {
        let slabs: Vec<SlabId> = (0..5).map(|_| sid()).collect();
        let mut rng = rand::thread_rng();
        let mut t = ExtentTable::new();
        let mut b: BTreeMap<u64, ExtentLocation> = BTreeMap::new();
        for _ in 0..20_000 {
            let v = rng.gen_range(0..2_000u64) * if rng.gen_bool(0.1) { 1_000 } else { 1 };
            match rng.gen_range(0..4) {
                0 | 1 => {
                    let mut l = ExtentLocation::new(*slabs.choose(&mut rng).unwrap(), rng.gen());
                    l.ref_count = rng.gen_range(1..5);
                    l.generation = rng.gen();
                    if rng.gen_bool(0.2) {
                        l.mirrors = vec![Leg::new(*slabs.choose(&mut rng).unwrap(), rng.gen())];
                    }
                    assert_eq!(t.insert(v, l.clone()).map(|x| x.slot_idx), b.insert(v, l).map(|x| x.slot_idx));
                }
                2 => assert_eq!(t.remove(&v).map(|x| x.slot_idx), b.remove(&v).map(|x| x.slot_idx)),
                _ => {
                    let r = t.update(v, |l| l.ref_count += 1);
                    let rb = b.get_mut(&v).map(|l| l.ref_count += 1);
                    assert_eq!(r.is_some(), rb.is_some());
                }
            }
            assert_eq!(t.len(), b.len());
        }
        let got: Vec<_> = t.iter().map(|(v, l)| (v, l.slab_id, l.slot_idx, l.ref_count, l.generation, l.mirrors)).collect();
        let want: Vec<_> =
            b.iter().map(|(v, l)| (*v, l.slab_id, l.slot_idx, l.ref_count, l.generation, l.mirrors.clone())).collect();
        assert_eq!(got, want);
        assert_eq!(t.keys().collect::<Vec<_>>(), b.keys().copied().collect::<Vec<_>>());
        let back = t.to_btree();
        assert_eq!(back.len(), b.len());
        let shifted = t.shifted(64 * 7);
        assert_eq!(shifted.keys().collect::<Vec<_>>(), b.keys().map(|k| k + 448).collect::<Vec<_>>());
        let odd = t.shifted(5);
        assert_eq!(odd.get(&(b.keys().next().unwrap() + 5)).map(|l| l.slot_idx), b.values().next().map(|l| l.slot_idx));
    }
}
