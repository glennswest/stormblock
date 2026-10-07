//! Where the node's slabs are, for the open `/api/v1/health` (#322).
//!
//! stormcentral#353: a node passed the install stages "local boot" and
//! "fresh slab" while it ran entirely from a forge clone (#268), and nothing
//! it could read said so. Health now does, per half:
//!
//! ```json
//! "slabs": {"diskless": false, "system": "local", "data": "mixed",
//!           "items": [{"role": "system", "source": "local", "device": "/dev/sda@1048576", "volumes": 61},
//!                     {"role": "data", "source": "remote", "transport": "nvme-tcp", "volumes": 2}]}
//! ```
//!
//! A remote slab is never named beyond its transport. Health is open, and a
//! remote slab's URI carries what attaching it takes: forge's address, the
//! clone's subsystem NQN and the host NQN, and a boot host connects with no
//! secret (#210).
//!
//! **Never waits** (health is the discovery probe every booting node sends):
//! the registry and the extent map are read with `try_read`, the answer kept
//! for [`TTL`]. A node with no remote slab registered (forge, a node booted
//! from its own disk) is all local and nothing is counted.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::drive::slab::SlabId;
use crate::drive::DriveType;

/// How long an answer is reused.
pub const TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SlabReport {
    /// No volume has a leg on a local slab: the node runs from the network.
    pub diskless: bool,
    /// Where the system half's volumes are: `local`, `remote`, `mixed`
    /// (some of each, or a volume with legs on both — a flow-over under way),
    /// or `none`.
    pub system: &'static str,
    /// The same for the data half.
    pub data: &'static str,
    pub items: Vec<SlabItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SlabItem {
    pub role: &'static str,
    pub source: &'static str,
    /// A local slab's device (`/dev/sda@<offset>` for a partition).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// A remote slab's transport only (`nvme-tcp`, `iscsi`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<&'static str>,
    /// Volumes with a leg on it. Absent when nothing was counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<usize>,
}

/// Remote, and over what.
fn remote(t: DriveType) -> Option<&'static str> {
    match t {
        DriveType::NvmeTcp => Some("nvme-tcp"),
        DriveType::Iscsi => Some("iscsi"),
        _ => None,
    }
}

/// One slab, as the report needs it.
pub struct SlabFact {
    pub id: SlabId,
    pub data: bool,
    pub remote: Option<&'static str>,
    pub device: String,
}

/// The report from the slabs and, when counted, the slab set of each volume.
pub fn build(slabs: &[SlabFact], volumes: Option<&[HashSet<SlabId>]>) -> SlabReport {
    let role = |data: bool| if data { "data" } else { "system" };
    let items: Vec<SlabItem> = slabs
        .iter()
        .map(|s| SlabItem {
            role: role(s.data),
            source: if s.remote.is_some() { "remote" } else { "local" },
            device: s.remote.is_none().then(|| s.device.clone()),
            transport: s.remote,
            volumes: volumes.map(|vs| vs.iter().filter(|v| v.contains(&s.id)).count()),
        })
        .collect();
    let Some(vols) = volumes else {
        // Nothing remote: every half that has a slab is local.
        let half = |data: bool| if slabs.iter().any(|s| s.data == data) { "local" } else { "none" };
        return SlabReport {
            diskless: slabs.is_empty(),
            system: half(false),
            data: half(true),
            items,
        };
    };
    let fact = |id: &SlabId| slabs.iter().find(|s| s.id == *id);
    let half = |data: bool| {
        let (mut local, mut remote_) = (false, false);
        for v in vols {
            for id in v {
                if let Some(s) = fact(id).filter(|s| s.data == data) {
                    if s.remote.is_some() {
                        remote_ = true;
                    } else {
                        local = true;
                    }
                }
            }
        }
        match (local, remote_) {
            (true, true) => "mixed",
            (true, false) => "local",
            (false, true) => "remote",
            (false, false) => "none",
        }
    };
    let any_local = vols
        .iter()
        .any(|v| v.iter().any(|id| fact(id).is_some_and(|s| s.remote.is_none())));
    SlabReport { diskless: !any_local, system: half(false), data: half(true), items }
}

/// The report now, without waiting: `None` when the registry or the extent
/// map is busy (the caller falls back to the last one).
pub fn snapshot(
    registry: &tokio::sync::RwLock<crate::drive::slab_registry::SlabRegistry>,
    gem: &tokio::sync::RwLock<crate::volume::gem::GlobalExtentMap>,
) -> Option<SlabReport> {
    let facts: Vec<SlabFact> = {
        let reg = registry.try_read().ok()?;
        let mut v: Vec<SlabFact> = reg
            .iter()
            .map(|(id, s)| SlabFact {
                id: *id,
                data: s.is_data(),
                remote: remote(s.device().device_type()),
                device: s.device().id().path.clone(),
            })
            .collect();
        v.sort_by(|a, b| (a.data, a.remote.is_some(), &a.device).cmp(&(b.data, b.remote.is_some(), &b.device)));
        v
    };
    if facts.iter().all(|f| f.remote.is_none()) {
        return Some(build(&facts, None));
    }
    let vols: Vec<HashSet<SlabId>> = {
        let g = gem.try_read().ok()?;
        let mut out = Vec::new();
        for id in g.resident_ids() {
            if let Some(m) = g.get_volume_map(&id) {
                out.push(m.all_legs().map(|l| l.slab_id).collect());
            }
        }
        // A map out of memory says which slabs it was on without loading.
        for id in g.cold_ids() {
            if let Some(c) = g.cold(&id) {
                out.push(c.slabs.iter().copied().collect());
            }
        }
        out
    };
    Some(build(&facts, Some(&vols)))
}

/// The cached report for health: fresh within [`TTL`], else made again if
/// nothing is busy, else the last one (or none yet).
pub fn for_health(state: &crate::mgmt::AppState) -> Option<SlabReport> {
    let mut cache = state.slab_report.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, r)) = cache.as_ref() {
        if at.elapsed() < TTL {
            return Some(r.clone());
        }
    }
    match snapshot(&state.slab_registry, &state.gem) {
        Some(r) => {
            *cache = Some((Instant::now(), r.clone()));
            Some(r)
        }
        None => cache.as_ref().map(|(_, r)| r.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slab(n: u8, data: bool, remote: bool) -> SlabFact {
        SlabFact {
            id: SlabId(uuid::Uuid::from_bytes([n; 16])),
            data,
            remote: remote.then_some("nvme-tcp"),
            device: if remote { format!("nvme-tcp://10.0.0.1:4420/nqn.x:host:server3@{n}") } else { format!("/dev/sda@{n}") },
        }
    }
    fn vols(sets: &[&[u8]]) -> Vec<HashSet<SlabId>> {
        sets.iter()
            .map(|s| s.iter().map(|n| SlabId(uuid::Uuid::from_bytes([*n; 16]))).collect())
            .collect()
    }

    /// #268's node: every slab is the forge clone's.
    #[test]
    fn a_node_running_from_its_forge_clone_is_diskless() {
        let s = [slab(1, false, true), slab(2, true, true)];
        let r = build(&s, Some(&vols(&[&[1], &[1], &[2]])));
        assert!(r.diskless);
        assert_eq!((r.system, r.data), ("remote", "remote"));
        assert!(r.items.iter().all(|i| i.device.is_none() && i.transport == Some("nvme-tcp")));
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("nqn") && !json.contains("10.0.0.1"), "a remote slab is never named: {json}");
    }

    /// Mid flow-over: the system half moved, the data half still on forge;
    /// a volume with legs on both is mixed. After it, the remote slabs are
    /// registered and empty: local.
    #[test]
    fn a_flow_over_reads_mixed_then_local() {
        let s = [slab(1, false, true), slab(2, true, true), slab(3, false, false), slab(4, true, false)];
        let mid = build(&s, Some(&vols(&[&[3], &[3, 1], &[2]])));
        assert!(!mid.diskless);
        assert_eq!((mid.system, mid.data), ("mixed", "remote"));
        let done = build(&s, Some(&vols(&[&[3], &[3], &[4]])));
        assert_eq!((done.system, done.data, done.diskless), ("local", "local", false));
        let remote_counts: Vec<_> = done.items.iter().filter(|i| i.source == "remote").map(|i| i.volumes).collect();
        assert_eq!(remote_counts, vec![Some(0), Some(0)]);
    }

    /// Nothing remote (forge, a disk-booted node): local, nothing counted.
    #[test]
    fn all_local_is_answered_without_counting() {
        let s = [slab(3, false, false), slab(4, true, false)];
        let r = build(&s, None);
        assert_eq!((r.system, r.data, r.diskless), ("local", "local", false));
        assert!(r.items.iter().all(|i| i.volumes.is_none() && i.device.is_some()));
        let none = build(&[], None);
        assert!(none.diskless);
        assert_eq!((none.system, none.data), ("none", "none"));
    }
}
