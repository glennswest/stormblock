//! A system pair (#350, owner 2026-10-08; decided 1B + 2A, 2026-10-17).
//!
//! Two drives, each laid as a whole node disk (its own boot area, system slab
//! and data slab), so either boots the node alone. The system half is the same
//! goldens on both, two identical lays; the data half, the part that changes,
//! is mirrored. Both are one mechanism here: every volume of the node is
//! `mirror:2` spread at `drive`, its second leg on the other drive (the
//! per-volume mirror of #142/#146, never a drive-level RAID). A sealed golden
//! is never written, so its two legs stay identical; they are compared once
//! when the pair is made (the "checked by digest" of the issue).
//!
//! The two data slabs name each other in their headers (`partner`, #402's
//! field), which is how `/init` finds either drive from the other. A drive
//! that is missing is a degraded mirror (named in health), and the pair is
//! made whole again by [`pair_up`] at the next start once it is back or
//! replaced.

use std::collections::HashSet;
use std::sync::Arc;

use crate::drive::slab::{SlabId, SlabRole};
use crate::lockwatch::TrackedMutex;
use crate::volume::redundancy::{Redundancy, RedundancyPolicy};
use crate::volume::thin::ResyncOptions;
use crate::volume::{VolumeId, VolumeManager};

/// The rung a pair's legs differ at: one leg on each drive.
pub const PAIR_RUNG: &str = "drive";

/// What making (or mending) a pair did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PairReport {
    /// Volumes whose policy became `mirror:2` here.
    pub policies_set: usize,
    /// Volumes resynced.
    pub volumes: usize,
    /// Legs written onto the other drive.
    pub legs_added: usize,
    pub bytes_copied: u64,
    /// Sealed volumes whose two legs were compared, and extents compared.
    pub sealed_checked: usize,
    pub extents_compared: usize,
    /// What did not come out whole: a volume and why.
    pub problems: Vec<String>,
}

impl PairReport {
    pub fn is_whole(&self) -> bool {
        self.problems.is_empty()
    }
}

/// The policy a pair gives a volume, when it has less: `mirror:2` at
/// [`PAIR_RUNG`]. A parity volume (already redundant across drives) and one
/// with more copies keep theirs.
fn pair_policy(current: &RedundancyPolicy) -> Option<RedundancyPolicy> {
    match current.scheme {
        Redundancy::None => Some(RedundancyPolicy::mirror(2).at(PAIR_RUNG)),
        Redundancy::Mirror { copies } if copies < 2 => Some(RedundancyPolicy::mirror(2).at(PAIR_RUNG)),
        _ => None,
    }
}

/// The local data slabs that name each other (#350): `(ours, partner)` for
/// each data slab registered whose header names a partner. A partner that is
/// not registered is a missing drive.
pub async fn pairs_registered(mgr: &VolumeManager) -> Vec<(SlabId, SlabId, bool)> {
    let reg = mgr.registry().read().await;
    reg.iter()
        .filter(|(_, s)| s.role() == SlabRole::Data)
        .filter_map(|(id, s)| s.partner().map(|p| (*id, p, reg.get(&p).is_some_and(|q| q.role() == SlabRole::Data))))
        .collect()
}

/// Whether this node is a system pair: a registered data slab names another
/// data slab as its partner (#402's split names a data slab from a *system*
/// slab, which is not a pair).
pub async fn is_pair(mgr: &VolumeManager) -> bool {
    !pairs_registered(mgr).await.is_empty()
}

/// Make the pair whole, holding the manager only to read what it needs and
/// to write the records (#364): every volume `mirror:2`, resynced onto the
/// other drive; the sealed ones' two legs compared. Safe on a live node: a
/// resync takes each extent's lock as I/O does (#146).
pub async fn pair_up_unlocked(vm: &TrackedMutex<VolumeManager>) -> PairReport {
    let mut report = PairReport::default();
    let (handles, sealed, gem, registry) = {
        let m = vm.lock().await;
        let ids = m.volume_ids();
        let handles: Vec<(VolumeId, Arc<crate::volume::thin::ThinVolumeHandle>)> =
            ids.iter().filter_map(|id| m.get_volume_handle(id).map(|h| (*id, h))).collect();
        let sealed: HashSet<VolumeId> = ids.iter().filter(|id| m.is_sealed(id)).copied().collect();
        (handles, sealed, m.gem().clone(), m.registry().clone())
    };
    for (id, h) in &handles {
        if let Some(p) = pair_policy(&h.redundancy()) {
            match h.set_redundancy(p) {
                Ok(()) => report.policies_set += 1,
                Err(e) => report.problems.push(format!("{}: {e}", id.0)),
            }
        }
    }
    let mut owed = Vec::new();
    for (id, h) in &handles {
        if h.redundancy().scheme.is_parity() {
            continue;
        }
        let mut r = h.resync_with(&ResyncOptions::default()).await;
        report.volumes += 1;
        report.legs_added += r.legs_added + r.legs_rebuilt;
        report.bytes_copied += r.bytes_copied;
        if r.unrecoverable > 0 || !r.errors.is_empty() {
            report.problems.push(format!(
                "{}: {} extent(s) not mirrored{}",
                id.0,
                r.unrecoverable,
                r.errors.first().map(|e| format!(" ({e})")).unwrap_or_default()
            ));
        }
        owed.push((h.clone(), std::mem::take(&mut r.owed)));
    }
    // The replaced slots only once the maps that stopped naming them are on
    // disk (#366).
    match VolumeManager::persist_detached_checked(vm).await {
        Ok(()) => {
            for (h, o) in owed {
                h.release_slots(&o).await;
            }
        }
        Err(e) => report.problems.push(format!("the pair's records were not written: {e}")),
    }
    compare_sealed(&gem, &registry, &sealed, &mut report).await;
    report
}

/// [`pair_up_unlocked`] for a manager no one else holds (boot-local, tests).
pub async fn pair_up(mgr: VolumeManager) -> (VolumeManager, PairReport) {
    let vm = TrackedMutex::new(mgr);
    let r = pair_up_unlocked(&vm).await;
    (vm.into_inner(), r)
}

/// Every sealed volume's extents with two legs: both legs read and compared
/// (#350: the system half "checked by digest" on both drives).
async fn compare_sealed(
    gem: &Arc<crate::lockwatch::TrackedRwLock<crate::volume::gem::GlobalExtentMap>>,
    registry: &Arc<crate::lockwatch::TrackedRwLock<crate::drive::slab_registry::SlabRegistry>>,
    sealed: &HashSet<VolumeId>,
    report: &mut PairReport,
) {
    let _pin = match crate::volume::gem::pin_resident(gem).await {
        Ok(p) => p,
        Err(e) => {
            report.problems.push(format!("loading extent maps to compare the goldens: {e}"));
            return;
        }
    };
    for id in sealed {
        let locs: Vec<(u64, Vec<crate::volume::gem::Leg>)> = {
            let g = gem.read().await;
            match g.get_volume_map(id) {
                Some(m) => m.extents.iter().map(|(v, l)| (v, l.legs().collect())).collect(),
                None => continue,
            }
        };
        report.sealed_checked += 1;
        for (vext, legs) in locs {
            if legs.len() < 2 {
                report.problems.push(format!("{} extent {vext}: one leg only", id.0));
                continue;
            }
            let mut first: Option<Vec<u8>> = None;
            for leg in &legs {
                let reg = registry.read().await;
                let Some(slab) = reg.get(&leg.slab_id) else {
                    report.problems.push(format!("{} extent {vext}: slab {} not here", id.0, leg.slab_id.0));
                    break;
                };
                let mut buf = vec![0u8; slab.slot_size() as usize];
                if let Err(e) = slab.read_slot(leg.slot_idx, 0, &mut buf).await {
                    report.problems.push(format!("{} extent {vext}: reading slab {}: {e}", id.0, leg.slab_id.0));
                    break;
                }
                drop(reg);
                match &first {
                    None => first = Some(buf),
                    Some(f) if *f != buf => {
                        report.problems.push(format!("{} extent {vext}: the legs differ", id.0));
                        break;
                    }
                    Some(_) => {}
                }
            }
            report.extents_compared += 1;
        }
    }
}
