//! Where a volume lives: slabs, drives, RAID partners, and the state of each
//! (#136, #114).
//!
//! `GET /api/v1/volumes` says what a volume is, what it costs and what it
//! descends from. This is the rest: which pieces of hardware hold it, so
//! "which volumes am I about to lose if this drive goes", "is this clone on
//! the same drive as its parent" and "where did it land" have answers outside
//! the engine.
//!
//! Derived from the extent map the engine already holds — every leg of every
//! extent names its slab — grouped by slab and then by drive. It is a walk of
//! the volume's map, O(extents), so the listing only does it when asked
//! (`?placement=true`); the single-volume GET always does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Serialize;

use super::slabs::DriveRef;
use crate::drive::slab::SlabId;
use crate::mgmt::AppState;
use crate::volume::{VolumeId, VolumeManager};

/// A volume's placement.
#[derive(Debug, Clone, Serialize)]
pub struct Placement {
    /// Each slab holding a leg of this volume.
    pub slabs: Vec<SlabPlacement>,
    /// The same, grouped by the drive the slabs are on.
    pub drives: Vec<DrivePlacement>,
    /// The redundancy picture: legs the policy asks for, legs missing,
    /// extents that cannot be read from what remains.
    pub legs: LegTotals,
    /// `none`, or `needed` while legs are missing — a `resync` rebuilds them.
    /// A resync is one synchronous call, so there is no progress to report
    /// while it runs; this says whether one is owed.
    pub rebuild: &'static str,
    /// Drive-level RAID arrays under this volume's slabs, with each member
    /// ("partner") and its state.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arrays: Vec<ArrayPlacement>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlabPlacement {
    pub id: String,
    pub role: String,
    pub tier: String,
    pub domain: String,
    pub drive: DriveRef,
    /// The node the drive is attached to: this node, or the host a fabric
    /// drive (`nvme-tcp://`, `iscsi://`) is served from.
    pub node: String,
    /// `ok`, `failed` (this volume stopped trusting it), `quarantined`
    /// (a health report took it out of placement), `draining`, or `missing`
    /// (a leg names a slab this node no longer has).
    pub state: &'static str,
    /// Progress of the drain moving legs off this slab, while one runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain: Option<DrainProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_id: Option<uuid::Uuid>,
    /// Data legs of this volume on the slab.
    pub legs: u64,
    /// Of those, legs shared with another volume (a clone and its golden):
    /// on this drive, but not this volume's alone.
    pub shared_legs: u64,
    /// Parity legs (RAID-5/6 policies).
    pub parity_legs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrainProgress {
    pub state: String,
    pub moved: u64,
    pub remaining: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrivePlacement {
    pub drive: DriveRef,
    pub node: String,
    pub slabs: usize,
    pub legs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegTotals {
    pub policy: String,
    pub health: String,
    pub extents: usize,
    pub expected: usize,
    pub missing: usize,
    pub unreadable: usize,
    pub failed_slabs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArrayPlacement {
    pub id: uuid::Uuid,
    pub level: String,
    pub members: Vec<ArrayMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArrayMember {
    pub index: usize,
    /// `active`, `degraded`, `spare`, `failed` or `rebuilding`.
    pub state: String,
    pub drive: DriveRef,
    pub node: String,
}

/// The node a drive belongs to: the host in a fabric URI, else this node.
fn node_of(path: &str, local: &str) -> String {
    match path.split_once("://") {
        Some((_, rest)) => rest
            .split(['/', '?'])
            .next()
            .map(|hp| hp.rsplit_once(':').map(|(h, _)| h).unwrap_or(hp))
            .unwrap_or(local)
            .trim_matches(['[', ']'])
            .to_string(),
        None => local.to_string(),
    }
}

/// This node's name, as the rest of the API reports it.
pub fn local_node(state: &AppState) -> String {
    state
        .config
        .management
        .node_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|h| h.trim().to_string()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

#[derive(Default)]
struct Count {
    legs: u64,
    shared: u64,
    parity: u64,
}

/// Where volume `id` lives, or `None` when there is no such volume.
///
/// Takes the volume manager the caller already holds, so a listing asks once.
pub async fn of_volume(state: &Arc<AppState>, vm: &VolumeManager, id: VolumeId) -> Option<Placement> {
    let handle = vm.get_volume_handle(&id)?;
    let health = handle.health().await;
    let local = local_node(state);

    // Legs per slab, from the map.
    let mut by_slab: HashMap<SlabId, Count> = HashMap::new();
    {
        let gem = vm.gem().read().await;
        if let Some(map) = gem.get_volume_map(&id) {
            for loc in map.extents.values() {
                let shared = loc.ref_count > 1;
                for leg in loc.legs() {
                    let c = by_slab.entry(leg.slab_id).or_default();
                    c.legs += 1;
                    if shared {
                        c.shared += 1;
                    }
                }
            }
            for group in map.parity.values() {
                for leg in &group.legs {
                    by_slab.entry(leg.slab_id).or_default().parity += 1;
                }
            }
        }
    }

    // Drains in progress, by the slabs they are moving.
    let mut draining: HashMap<SlabId, DrainProgress> = HashMap::new();
    {
        let drains = state.drains.read().await;
        for st in drains.all().await {
            if st.state != crate::drain::DrainState::Running {
                continue;
            }
            for s in &st.slabs {
                draining.insert(
                    *s,
                    DrainProgress {
                        state: "running".into(),
                        moved: st.moved,
                        remaining: st.remaining,
                        failed: st.failed,
                    },
                );
            }
        }
    }

    let failed: Vec<SlabId> = health.failed_slabs.clone();
    // Stable output: slabs in id order.
    let mut by_slab: Vec<(SlabId, Count)> = by_slab.into_iter().collect();
    by_slab.sort_by_key(|(id, _)| id.0);
    let mut slabs = Vec::with_capacity(by_slab.len());
    let mut array_ids = Vec::new();
    {
        let reg = vm.registry().read().await;
        for (sid, c) in &by_slab {
            let array_id = vm.array_of_slab(sid);
            if let Some(a) = array_id {
                if !array_ids.contains(&a) {
                    array_ids.push(a);
                }
            }
            let (role, tier, domain, drive, slot, present) = match reg.get(sid) {
                Some(slab) => (
                    slab.role().to_string(),
                    slab.tier().to_string(),
                    reg.domain_of(sid).to_string(),
                    DriveRef::of(slab.device()),
                    slab.slot_size(),
                    true,
                ),
                None => (
                    String::new(),
                    String::new(),
                    String::new(),
                    DriveRef { serial: String::new(), wwn: String::new(), model: String::new(), path: String::new() },
                    vm.slot_size(),
                    false,
                ),
            };
            let drain = draining.get(sid).cloned();
            let state_ = if !present {
                "missing"
            } else if failed.contains(sid) {
                "failed"
            } else if reg.is_quarantined(sid) {
                "quarantined"
            } else if drain.is_some() {
                "draining"
            } else {
                "ok"
            };
            let node = node_of(&drive.path, &local);
            slabs.push(SlabPlacement {
                id: sid.0.to_string(),
                role,
                tier,
                domain,
                node,
                drive,
                state: state_,
                drain,
                array_id: array_id.map(|a| a.0),
                legs: c.legs,
                shared_legs: c.shared,
                parity_legs: c.parity,
                bytes: (c.legs + c.parity) * slot,
            });
        }
    }

    // Per drive.
    let mut by_drive: BTreeMap<DriveRef, DrivePlacement> = BTreeMap::new();
    for s in &slabs {
        let e = by_drive.entry(s.drive.clone()).or_insert_with(|| DrivePlacement {
            drive: s.drive.clone(),
            node: s.node.clone(),
            slabs: 0,
            legs: 0,
            bytes: 0,
        });
        e.slabs += 1;
        e.legs += s.legs + s.parity_legs;
        e.bytes += s.bytes;
    }

    // RAID partners.
    let mut arrays = Vec::new();
    {
        let all = state.arrays.read().await;
        for a in array_ids {
            let Some(info) = all.get(&a) else { continue };
            let members = info
                .array
                .member_drives()
                .into_iter()
                .map(|(index, st, id)| {
                    let drive = DriveRef { serial: id.serial, wwn: id.wwn, model: id.model, path: id.path };
                    ArrayMember { index, state: st.to_string(), node: node_of(&drive.path, &local), drive }
                })
                .collect();
            arrays.push(ArrayPlacement { id: a.0, level: info.level.to_string(), members });
        }
    }

    Some(Placement {
        slabs,
        drives: by_drive.into_values().collect(),
        rebuild: if health.legs_missing > 0 { "needed" } else { "none" },
        legs: LegTotals {
            policy: health.redundancy.clone(),
            health: health.state.to_string(),
            extents: health.extents,
            expected: health.legs_expected,
            missing: health.legs_missing,
            unreadable: health.unreadable,
            failed_slabs: failed.iter().map(|s| s.0.to_string()).collect(),
        },
        arrays,
    })
}

#[cfg(test)]
mod tests {
    use super::node_of;

    #[test]
    fn a_fabric_drive_names_the_node_serving_it() {
        assert_eq!(node_of("/dev/sda", "r230"), "r230");
        assert_eq!(node_of("nvme-tcp://10.0.0.7:4420/nqn.x?nsid=1", "r230"), "10.0.0.7");
        assert_eq!(node_of("iscsi://shelf1.g8.lo:3260/iqn.x", "r230"), "shelf1.g8.lo");
        assert_eq!(node_of("nvme-tcp://[fd00::7]:4420/nqn.x", "r230"), "fd00::7");
    }
}
