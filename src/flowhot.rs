//! What the node reads from the appliance while a flow-over runs (#411):
//! the extents that move first.
//!
//! A node boots from forge's clone and moves onto its own disk in the
//! background. The extents it reads meanwhile — the boot path, the root
//! filesystem, the running images — are the ones whose every read is a round
//! trip to forge, so the flow-over moves them before the cold rest, and the
//! node stops depending on forge for what matters sooner.
//!
//! Every read of a volume extent whose slot is on a flow-over's source is
//! counted, per slot. Off (one atomic load) on a node with no flow-over. The
//! initramfs engine (`boot-local`) serves the boot itself and stands down
//! before the flow-over starts in its successor (`adopt-ublk`), so it leaves
//! its counts in `/run/stormblock/hot-extents.json`, which the successor
//! reads (#411: the boot path is what the boot read).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, RwLock};

use crate::drive::slab::SlabId;
use crate::volume::gem::{ExtentLocation, Leg};

/// Where the initramfs engine leaves what it counted.
pub const HANDOVER_FILE: &str = "/run/stormblock/hot-extents.json";

/// Slots counted at most (a flow-over's sources hold tens of thousands).
const MAX: usize = 1 << 20;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SOURCES: RwLock<Vec<SlabId>> = RwLock::new(Vec::new());
static HITS: Mutex<Option<HashMap<Leg, u32>>> = Mutex::new(None);
/// Bumped with every new hot slot: when the flow-over re-orders.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The flow-over's sources: reads of slots on them are counted from now
/// (empty: counting stops).
pub fn set_sources(sources: &[SlabId]) {
    let mut s = SOURCES.write().unwrap_or_else(|e| e.into_inner());
    s.clear();
    s.extend_from_slice(sources);
    ACTIVE.store(!s.is_empty(), Relaxed);
}

/// A read of `loc`: counted when one of its legs is on a source.
pub fn note(loc: &ExtentLocation) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let sources = SOURCES.read().unwrap_or_else(|e| e.into_inner());
    let Some(leg) = loc.legs().find(|l| sources.contains(&l.slab_id)) else { return };
    drop(sources);
    count(leg, 1);
}

fn count(leg: Leg, n: u32) {
    let mut h = HITS.lock().unwrap_or_else(|e| e.into_inner());
    let map = h.get_or_insert_with(HashMap::new);
    if let Some(c) = map.get_mut(&leg) {
        *c = c.saturating_add(n);
        return;
    }
    if map.len() < MAX {
        map.insert(leg, n);
        GENERATION.fetch_add(1, Relaxed);
    }
}

/// How often each slot was read from a source.
pub fn hits() -> HashMap<Leg, u32> {
    HITS.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// Changes whenever a slot becomes hot.
pub fn generation() -> u64 {
    GENERATION.load(Relaxed)
}

/// Slots read, hottest first: what the boot read, for its successor.
pub fn save(path: &Path) -> std::io::Result<usize> {
    let mut v: Vec<(Leg, u32)> = hits().into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(&v).map_err(std::io::Error::other)?;
    crate::serve::wiring::write_atomic(path, &bytes).map_err(std::io::Error::other)?;
    Ok(v.len())
}

/// Take what the boot counted (its file), added to what this engine counts.
pub fn load(path: &Path) -> usize {
    let Ok(bytes) = std::fs::read(path) else { return 0 };
    let Ok(v) = serde_json::from_slice::<Vec<(Leg, u32)>>(&bytes) else {
        tracing::warn!("{}: unreadable; the flow-over goes in disk order", path.display());
        return 0;
    };
    let n = v.len();
    for (leg, c) in v {
        count(leg, c);
    }
    n
}

/// `items` (whose third field is a source slot) with the hot ones first,
/// hottest first, the rest in the order they came.
pub fn hot_first<A, B>(items: &mut std::collections::VecDeque<(A, B, Leg)>, hits: &HashMap<Leg, u32>) -> usize {
    if hits.is_empty() {
        return 0;
    }
    let s = items.make_contiguous();
    s.sort_by_key(|(_, _, leg)| std::cmp::Reverse(hits.get(leg).copied().unwrap_or(0)));
    let hot: HashSet<Leg> = s.iter().filter(|(_, _, l)| hits.contains_key(l)).map(|(_, _, l)| *l).collect();
    hot.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_slots_go_first_hottest_first_and_the_rest_keep_their_order() {
        let s = SlabId(uuid::Uuid::new_v4());
        let leg = |i| Leg::new(s, i);
        let mut items: std::collections::VecDeque<(u8, u8, Leg)> = (0..8).map(|i| (0, 0, leg(i))).collect();
        let hits: HashMap<Leg, u32> = [(leg(5), 3), (leg(2), 9)].into();
        assert_eq!(hot_first(&mut items, &hits), 2);
        let order: Vec<u64> = items.iter().map(|(_, _, l)| l.slot_idx).collect();
        assert_eq!(order, vec![2, 5, 0, 1, 3, 4, 6, 7]);
    }
}
