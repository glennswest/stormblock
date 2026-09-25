//! What each volume is, and whether something is using it (#138, #126).
//!
//! The owner, on the console: "Volumes are actually volumes related directly
//! to running containers/VMs" — and goldens, blanks and media are images, the
//! registry's view. Nothing about storage changes for that: a golden stays a
//! volume underneath. What changes is that the listing says, per volume,
//!
//! * **`kind`** — `volume`, `golden`, `blank`, `media`, `snapshot` or
//!   `template`, decided here once instead of by every tool's naming
//!   conventions;
//! * **`in_use`** and **`attachments`** — from what this engine is actually
//!   serving: exports, iSCSI LUNs, ublk devices (with where they are
//!   mounted), per-volume NVMe subsystems, the serving layer's wiring table,
//!   and the boot devices an adopting engine took over;
//! * **`consumer`** — who: the volume's owner (#115) when one was set, else
//!   the mount a ublk device carries.
//!
//! Gathered once per request, with each lock taken and released on its own —
//! never while the volume manager is held.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use uuid::Uuid;

use crate::mgmt::AppState;
use crate::volume::{VolumeId, VolumeManager};

/// One way a volume is being served right now.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Attachment {
    /// `ublk`, `nvme-tcp` or `iscsi`.
    pub transport: &'static str,
    /// `/dev/ublkbN`, for a ublk device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Where that device's filesystem is mounted, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounted_at: Option<String>,
    /// NQN or IQN of the target serving it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lun: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nsid: Option<u32>,
    /// A wiring row's state (`active`, `draining`, …), when it came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl Attachment {
    fn new(transport: &'static str) -> Self {
        Attachment {
            transport,
            device: None,
            mounted_at: None,
            target: None,
            port: None,
            lun: None,
            nsid: None,
            state: None,
        }
    }
}

/// Who is using a volume.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Consumer {
    /// `PersistentVolumeClaim`, `VirtualMachineInstance`, … as the owner was
    /// set; `Mount` when all that is known is where it is mounted.
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

/// What the listing needs to classify volumes, gathered once.
#[derive(Default)]
pub struct Context {
    attachments: HashMap<Uuid, Vec<Attachment>>,
    template_raw: HashSet<Uuid>,
    template_sealed: HashSet<Uuid>,
    snapshots: HashSet<Uuid>,
}

/// Per-volume fields the Volumes and Images views need.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_use: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumer: Option<Consumer>,
}

/// The kinds that are images — what the console's Images view shows and its
/// Volumes view leaves out.
pub const IMAGE_KINDS: [&str; 5] = ["golden", "blank", "media", "snapshot", "template"];

impl Context {
    pub async fn gather(state: &AppState) -> Context {
        let mut ctx = Context::default();
        let mut add = |v: Uuid, a: Attachment| {
            let list = ctx.attachments.entry(v).or_default();
            if !list.contains(&a) {
                list.push(a);
            }
        };

        for e in state.exports.read().await.iter() {
            let mut a = Attachment::new(match e.protocol {
                crate::mgmt::ExportProtocol::Iscsi => "iscsi",
                crate::mgmt::ExportProtocol::Nvmeof => "nvme-tcp",
            });
            a.target = Some(e.target_id.clone()).filter(|t| !t.is_empty());
            a.lun = e.lun_id;
            a.nsid = e.nsid;
            add(e.volume_id, a);
        }
        #[cfg(feature = "iscsi")]
        for (lun, entry) in state.lun_entries.read().await.iter() {
            if let crate::mgmt::LunBacking::Volume { volume_id } = entry.backing {
                let mut a = Attachment::new("iscsi");
                a.lun = Some(*lun);
                add(volume_id, a);
            }
        }
        let devices = state.ublk_exports.lock().await.devices();
        for (vol, dev) in devices {
            let Ok(v) = Uuid::parse_str(&vol) else { continue };
            let mut a = Attachment::new("ublk");
            a.mounted_at = crate::mgmt::ublk_export::mounted_at(&dev);
            a.device = Some(dev);
            add(v, a);
        }
        for (v, (nqn, port)) in state.nvme_portals.read().await.iter() {
            let mut a = Attachment::new("nvme-tcp");
            a.target = Some(nqn.clone());
            a.port = Some(*port);
            add(*v, a);
        }
        if let Some(serve) = state.serve.get() {
            for w in serve.wiring.lock().await.exports.iter() {
                let proto = format!("{:?}", w.protocol).to_lowercase();
                let mut a = Attachment::new(if proto.contains("iscsi") { "iscsi" } else { "nvme-tcp" });
                a.target = w.nqn.clone().or_else(|| Some(w.iqn.clone()).filter(|i| !i.is_empty()));
                a.port = Some(w.portal_port);
                a.lun = w.lun;
                a.state = Some(format!("{:?}", w.state).to_lowercase());
                add(w.volume_id, a);
            }
        }

        {
            let store = state.fstemplates.lock().await;
            for t in &store.templates {
                if let Some(r) = t.raw_volume_id {
                    ctx.template_raw.insert(r);
                }
                if let Some(s) = t.sealed_volume_id {
                    ctx.template_sealed.insert(s);
                }
            }
        }
        {
            let v1 = state.v1.lock().await;
            ctx.snapshots.extend(v1.snapshots.values().filter_map(|s| s.local_id));
            // Namespaces on the shared NVMe subsystem: what an attach through
            // `/api/v1/volumes/{id}/attach` or `/v1` hot-adds. Keyed by the
            // engine id for the first, by the `/v1` id for the second.
            let nqn = state.config.nvmeof.as_ref().map(|n| n.nqn.clone());
            let found: Vec<(Uuid, u32)> = v1
                .nvme_nsids
                .iter()
                .filter_map(|(key, nsid)| {
                    let engine = Uuid::parse_str(key)
                        .ok()
                        .or_else(|| v1.volumes.get(key).and_then(|r| r.local_id))?;
                    Some((engine, *nsid))
                })
                .collect();
            drop(v1);
            for (engine, nsid) in found {
                let mut a = Attachment::new("nvme-tcp");
                a.target = nqn.clone();
                a.nsid = Some(nsid);
                let list = ctx.attachments.entry(engine).or_default();
                if !list.contains(&a) {
                    list.push(a);
                }
            }
        }
        ctx
    }

    /// Every volume something is attached to — the one answer the listing
    /// and the delete guards share.
    pub fn attached(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.attachments.keys().copied()
    }

    /// What a volume is. The rule, in one place:
    ///
    /// * a template's scratch volume → `template`;
    /// * not sealed → `volume` — something writes to it;
    /// * a `/v1` snapshot (a Kubernetes VolumeSnapshot, #111) → `snapshot`;
    /// * a template's sealed volume, or a blank an image shipped → `blank`;
    /// * a whole-disk image or ISO (`fs.kind` gpt, mbr, iso9660) → `media`;
    /// * any other sealed volume → `golden`.
    pub fn kind(&self, vm: &VolumeManager, id: &VolumeId) -> &'static str {
        if self.template_raw.contains(&id.0) {
            return "template";
        }
        if !vm.is_sealed(id) {
            return "volume";
        }
        if self.snapshots.contains(&id.0) {
            return "snapshot";
        }
        if self.template_sealed.contains(&id.0) || vm.is_template(id) {
            return "blank";
        }
        if vm.fs_info(id).is_some_and(|f| matches!(f.kind.as_str(), "gpt" | "mbr" | "iso9660")) {
            return "media";
        }
        "golden"
    }

    pub fn usage(&self, vm: &VolumeManager, id: &VolumeId) -> Usage {
        let attachments = self.attachments.get(&id.0).cloned().unwrap_or_default();
        let consumer = match vm.owner(id) {
            Some(o) => Some(Consumer {
                kind: o.kind.clone(),
                namespace: o.namespace.clone(),
                name: o.name.clone(),
                uid: o.uid.clone(),
            }),
            None => attachments.iter().find_map(|a| a.mounted_at.clone()).map(|m| Consumer {
                kind: "Mount".into(),
                namespace: String::new(),
                name: m,
                uid: None,
            }),
        };
        Usage {
            kind: Some(self.kind(vm, id)),
            in_use: Some(!attachments.is_empty()),
            attachments,
            consumer,
        }
    }
}

/// Does a volume pass the listing's filters?
pub fn matches(u: &Usage, owned: bool, kind: Option<&str>, in_use: Option<bool>, unowned: bool) -> bool {
    let k = u.kind.unwrap_or("volume");
    let kind_ok = match kind {
        None | Some("all") => true,
        Some("image") => IMAGE_KINDS.contains(&k),
        Some(want) => want.split(',').any(|w| w.trim() == k),
    };
    kind_ok && in_use.is_none_or(|w| u.in_use == Some(w)) && (!unowned || !owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(kind: &'static str, in_use: bool) -> Usage {
        Usage { kind: Some(kind), in_use: Some(in_use), attachments: Vec::new(), consumer: None }
    }

    #[test]
    fn the_filters() {
        let v = usage("volume", true);
        let g = usage("golden", false);
        let t = usage("template", false);
        assert!(matches(&v, false, None, None, false), "no filter passes everything");
        assert!(matches(&g, false, Some("all"), None, false));
        assert!(matches(&g, false, Some("image"), None, false) && matches(&t, false, Some("image"), None, false));
        assert!(!matches(&v, false, Some("image"), None, false));
        assert!(matches(&g, false, Some("blank, golden"), None, false));
        assert!(!matches(&g, false, Some("volume"), None, false));
        assert!(matches(&v, false, None, Some(true), false) && !matches(&g, false, None, Some(true), false));
        assert!(!matches(&v, true, None, None, true), "owned is not unowned");
    }

    /// A template's scratch volume and a /v1 snapshot are images too, and
    /// the rule decides them before anything else could.
    #[tokio::test]
    async fn templates_and_snapshots_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.bin").to_string_lossy().to_string();
        let dev = crate::drive::filedev::FileDevice::open_with_capacity(&path, 8 << 20).await.unwrap();
        let mut vm = VolumeManager::new(4096);
        vm.add_slab(
            crate::drive::slab::Slab::format(std::sync::Arc::new(dev), 4096, crate::placement::topology::StorageTier::Hot)
                .await
                .unwrap(),
        )
        .await;
        let raw = vm.create_volume_any("raw", 1 << 20).await.unwrap();
        let snap = vm.create_volume_any("snap", 1 << 20).await.unwrap();
        vm.seal_volume(snap, None).await.unwrap();
        let mut ctx = Context::default();
        ctx.template_raw.insert(raw.0);
        ctx.snapshots.insert(snap.0);
        assert_eq!(ctx.kind(&vm, &raw), "template", "unsealed, and still not a volume anyone uses");
        assert_eq!(ctx.kind(&vm, &snap), "snapshot");
    }
}
