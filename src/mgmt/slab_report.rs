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

use serde::{Deserialize, Serialize};

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
    /// What this boot did with the machine's own disk (#344): taken, or the
    /// drive carrying slabs that was not taken and why, or none. A node that
    /// is `diskless` with its slabs on its own drive names the drive and the
    /// reason here, instead of a bare `remote`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_disk: Option<LocalDisk>,
    /// A system pair (#350): both drives here (`paired`), or one missing
    /// (`degraded`, never silent: every volume runs on one leg). Absent on a
    /// node that is not a pair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<PairState>,
    /// The boot came from a drive the system was moved off (#350): urgent,
    /// `{set_to, booted_from, message}`, until a boot comes from the new one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_order: Option<serde_json::Value>,
}

/// Where `/init` says the boot came from a drive the system moved off (#350).
pub const BOOT_ORDER_PATH: &str = "/run/stormblock/boot-order.json";

/// The boot-order message `/init` left, when it left one.
pub fn read_boot_order() -> Option<serde_json::Value> {
    std::fs::read(BOOT_ORDER_PATH).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// A system pair's state (#350), from the data slabs' headers.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PairState {
    /// `paired` or `degraded`.
    pub state: &'static str,
    /// The local data slab(s) and the partner each names.
    pub data_slab: String,
    pub partner: String,
    /// Why degraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The pair state from the registered data slabs' `(id, partner, partner here)`.
pub fn pair_state(pairs: &[(SlabId, SlabId, bool)]) -> Option<PairState> {
    let (ours, partner, here) = pairs.iter().find(|(_, _, h)| !*h).or_else(|| pairs.first())?;
    Some(PairState {
        state: if *here { "paired" } else { "degraded" },
        data_slab: ours.0.to_string(),
        partner: partner.0.to_string(),
        reason: (!*here).then(|| {
            format!(
                "the pair's other drive (data slab {}) is not here: every volume runs on this drive's leg alone; \
                 it is mirrored again when that drive is back or replaced (#350)",
                partner.0
            )
        }),
    })
}

/// The boot's verdict on the machine's own disk (#344), written to
/// [`LOCAL_DISK_PATH`] by the initramfs's survey and by `boot-local`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalDisk {
    /// `taken`, `refused` (a drive with slabs that was not taken), `failed`
    /// (taken, and `boot-local` could not use it), `ambiguous` (the system
    /// drive is not known: several drives could take it, none is named or
    /// holds this node, #351) or `none`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The drive's shelf (#347): `id` (the enclosure's logical id, null for
    /// none), `position` (`internal`, `front`, `rear`, `external`), `bay`,
    /// `identity` (the SES vendor and model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf: Option<serde_json::Value>,
    /// Who the drive is (#351): `wwn=..;serial=..;model=..;size=..`, as the
    /// survey read it and `boot-local` proved it before writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// `initramfs` (the survey) or `boot-local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The storage controllers the initramfs found before it decided (#345):
    /// `pci`, `id` (vendor:device), `class`, `driver` (null: none bound) and
    /// the `drives` under each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<serde_json::Value>,
    /// Every drive it found (#345): `name`, `model`, `serial`, `size_bytes`,
    /// `transport`, `controller`, `bay`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drives: Vec<serde_json::Value>,
}

/// Where the boot writes [`LocalDisk`]: `/run` survives the switch_root.
pub const LOCAL_DISK_PATH: &str = "/run/stormblock/local-disk.json";

fn local_disk_path() -> std::path::PathBuf {
    std::env::var_os("STORMBLOCK_LOCAL_DISK_REPORT")
        .map(Into::into)
        .unwrap_or_else(|| LOCAL_DISK_PATH.into())
}

/// The boot's verdict, if it wrote one.
pub fn read_local_disk() -> Option<LocalDisk> {
    let text = std::fs::read_to_string(local_disk_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record a new verdict, keeping the boot's inventory (#345): `boot-local`
/// says what it did with the disk without losing what the initramfs found.
pub fn update_local_disk(state: &str, drive: Option<&str>, reason: Option<String>, from: &str) {
    let old = read_local_disk();
    // The shelf the survey named, when it is the same drive (#347).
    let same = old.as_ref().filter(|o| o.drive.as_deref() == drive);
    let shelf = same.and_then(|o| o.shelf.clone());
    let identity = same.and_then(|o| o.identity.clone());
    write_local_disk(&LocalDisk {
        state: state.to_string(),
        drive: drive.map(str::to_string),
        reason,
        shelf,
        identity,
        from: Some(from.to_string()),
        controllers: old.as_ref().map(|o| o.controllers.clone()).unwrap_or_default(),
        drives: old.map(|o| o.drives).unwrap_or_default(),
    });
}

/// Write the verdict (atomically; a warning when it cannot be).
pub fn write_local_disk(note: &LocalDisk) {
    let path = local_disk_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(note).unwrap_or_default())?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        tracing::warn!("could not record the local disk's state in {}: {e}", path.display());
    }
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
            local_disk: None,
            pair: None,
            boot_order: None,
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
    SlabReport { diskless: !any_local, system: half(false), data: half(true), items, local_disk: None, pair: None, boot_order: None }
}

/// The report now, without waiting: `None` when the registry or the extent
/// map is busy (the caller falls back to the last one).
pub fn snapshot(
    registry: &crate::lockwatch::TrackedRwLock<crate::drive::slab_registry::SlabRegistry>,
    gem: &crate::lockwatch::TrackedRwLock<crate::volume::gem::GlobalExtentMap>,
) -> Option<SlabReport> {
    let mut pairs: Vec<(SlabId, SlabId, bool)> = Vec::new();
    let facts: Vec<SlabFact> = {
        let reg = registry.try_read().ok()?;
        for (id, s) in reg.iter().filter(|(_, s)| s.is_data()) {
            if let Some(p) = s.partner() {
                pairs.push((*id, p, reg.get(&p).is_some_and(|q| q.is_data())));
            }
        }
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
    let pair = pair_state(&pairs);
    if facts.iter().all(|f| f.remote.is_none()) {
        let mut r = build(&facts, None);
        r.pair = pair;
        return Some(r);
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
    let mut r = build(&facts, Some(&vols));
    r.pair = pair;
    Some(r)
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
        Some(mut r) => {
            r.local_disk = read_local_disk();
            r.boot_order = read_boot_order();
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
    #[test]
    fn a_pair_with_its_other_drive_missing_is_degraded_and_says_so() {
        let (a, b) = (SlabId(uuid::Uuid::from_u128(1)), SlabId(uuid::Uuid::from_u128(2)));
        assert_eq!(pair_state(&[]), None, "not a pair");
        let ok = pair_state(&[(a, b, true), (b, a, true)]).unwrap();
        assert_eq!((ok.state, ok.reason.is_none()), ("paired", true));
        let deg = pair_state(&[(a, b, false)]).unwrap();
        assert_eq!(deg.state, "degraded");
        assert!(deg.reason.unwrap().contains(&b.0.to_string()));
        let j = serde_json::to_value(SlabReport { diskless: false, system: "local", data: "local", items: vec![], local_disk: None, pair: pair_state(&[(a, b, false)]), boot_order: None }).unwrap();
        assert_eq!(j["pair"]["state"], "degraded");
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

    /// #344: the boot's verdict on the machine's own disk is read back and
    /// carried in the report: a diskless node names the drive and the reason.
    #[test]
    fn the_local_disk_verdict_is_read_and_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("local-disk.json");
        // The process-wide override: only this module reads it.
        std::env::set_var("STORMBLOCK_LOCAL_DISK_REPORT", &path);
        assert_eq!(read_local_disk(), None, "no verdict written: none reported");
        // As the initramfs writes it.
        std::fs::write(
            &path,
            r#"{"state": "refused", "drive": "/dev/sda", "reason": "behind a SAS expander (a disk shelf, #273)", "from": "initramfs"}"#,
        )
        .unwrap();
        let note = read_local_disk().expect("the survey's verdict");
        assert_eq!((note.state.as_str(), note.drive.as_deref()), ("refused", Some("/dev/sda")));
        let mut r = build(&[slab(1, false, true), slab(2, true, true)], Some(&vols(&[&[1], &[2]])));
        r.local_disk = Some(note);
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["diskless"], true);
        assert_eq!(j["local_disk"]["drive"], "/dev/sda");
        assert!(j["local_disk"]["reason"].as_str().unwrap().contains("expander"));
        // As boot-local writes it.
        update_local_disk("failed", Some("/dev/sda"), Some("Input/output error".into()), "boot-local");
        assert_eq!(read_local_disk().unwrap().state, "failed");
        // #345: the initramfs's inventory is kept through boot-local's update,
        // and carried in the report.
        std::fs::write(
            &path,
            r#"{"state": "taken", "drive": "/dev/sda", "reason": null, "from": "initramfs",
               "shelf": {"id": "500056b3a1b2c3d4", "position": "front", "bay": "Slot 04", "identity": "DP BP13G+"},
               "controllers": [{"pci": "0000:01:00.0", "id": "1000:0097", "class": "0x010700", "driver": "mpt3sas", "drives": ["sda"]},
                               {"pci": "0000:00:1f.2", "id": "8086:a102", "class": "0x010601", "driver": null, "drives": []}],
               "drives": [{"name": "sda", "model": "WDC WD20EFAX-68F", "serial": "WD-WX11D28JFS6T", "size_bytes": 2000398934016,
                           "transport": "sas", "controller": "0000:01:00.0", "bay": "4"}]}"#,
        )
        .unwrap();
        update_local_disk("taken", Some("/dev/sda"), None, "boot-local");
        let note = read_local_disk().unwrap();
        assert_eq!((note.state.as_str(), note.controllers.len(), note.drives.len()), ("taken", 2, 1));
        let mut r = build(&[slab(1, false, false)], None);
        r.local_disk = Some(note);
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["local_disk"]["controllers"][0]["driver"], "mpt3sas");
        assert!(j["local_disk"]["controllers"][1]["driver"].is_null(), "an unbound controller is reported");
        assert_eq!(j["local_disk"]["drives"][0]["serial"], "WD-WX11D28JFS6T");
        assert_eq!(j["local_disk"]["shelf"]["position"], "front", "the survey's shelf is kept for the same drive (#347)");
        std::env::remove_var("STORMBLOCK_LOCAL_DISK_REPORT");
    }
}
