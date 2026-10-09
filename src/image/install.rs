//! An install keeps the node's data half (#311).
//!
//! The owner's rule for installs (2026-10-06): **an install touches the
//! system drive only, and only its system half.** The data slab and every data
//! volume on it are adopted, not recreated; the release's per-volume policy
//! (`/etc/stormblock/data-volumes`, #122) decides what happens to each data
//! volume it names; and a failure never falls back to a wipe — the install
//! stops and says why, and the data is untouched.
//!
//! The boot that installs runs from a fresh claim of the release (an
//! appliance clone), whose data half holds the release's own data volumes:
//! fresh clones of its blanks. The node's disk holds the node's. So two
//! volumes may answer to one name, and this decides which is the node's from
//! here on:
//!
//! | the name is … | and the release says | the node's volume | the release's |
//! |---|---|---|---|
//! | a golden in the release (sealed) | — | renamed `<name>@<old>` (its clones still name it by id) | keeps the name |
//! | a data volume the node has | keep (or nothing) | keeps the name, id and bytes | deleted (it was this boot's fresh clone) |
//! | | replace | renamed `<name>@<old>`, kept | keeps the name |
//! | | migrate `<hook>` | renamed `<name>@<old>`, kept; the migration listed | keeps the name |
//! | only in the release | — | — | keeps the name |
//! | only on the node (a PVC, a VM disk) | — | adopted as it is | — |
//! | the same id on both (the release the disk held, installed again) | — | keeps the name, id and bytes, whatever the policy | its copy deleted before the node's is adopted (#244) |
//!
//! Nothing of the node's is deleted. What the release keeps of its own moves
//! onto the node's data slab in the background (`FlowOver::data_flow`), as on
//! a fresh install (#285).
//!
//! **Checked before anything is written** ([`plan`]): the data slab's records
//! must read, and every leg of every volume they name must be in the data
//! half (the data slab or the bulk slab beside it). A data volume with an
//! extent in the system half — a clone sharing a golden's slot there — would
//! lose that extent when the system half is laid again; the install refuses
//! instead, before the system half is touched.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::partition::PartitionDevice;
use crate::drive::slab::{Slab, SlabId, SlabRole};
use crate::drive::BlockDevice;
use crate::image::stage::{generation_name, Migration, Policy};
use crate::pallet::gpt::Gpt;
use crate::volume::{VolumeId, VolumeManager};

/// What the node's data half holds, read before the install writes anything.
#[derive(Debug, Clone)]
pub struct Plan {
    pub data_slab: SlabId,
    pub bulk_slab: Option<SlabId>,
    /// Every volume the data half records: `(id, name, sealed)`.
    pub volumes: Vec<(VolumeId, String, bool)>,
    /// Volumes of the system half the release does not bring back that the
    /// node made (or that say nothing of where they came from): carried into
    /// the data half before the system half is laid again (#349).
    #[allow(dead_code)]
    pub carry: Vec<(VolumeId, String)>,
    /// The old release's own system volumes this release no longer names:
    /// dropped with the system half, as an install drops them (#349).
    pub dropped: Vec<String>,
    /// Release volumes the data half records with extents outside it (#369):
    /// system class, laid again with the system half — never kept as data,
    /// never a reason to refuse. Dropped from the data half after it is
    /// adopted.
    pub release_strays: Vec<(VolumeId, String)>,
}

/// What an install did with the node's data half, for the console, the
/// handover record and the node's release generations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The release installed (its root's `VERSION_ID`).
    pub version: String,
    /// The release the disk held before (`VERSION_ID`), or `previous`.
    pub previous: String,
    /// Every volume of the node's data half, adopted: how many.
    pub adopted: usize,
    /// Data volumes the node keeps by name (the release's fresh one deleted).
    #[serde(default)]
    pub kept: Vec<String>,
    /// The node's volumes renamed aside: `(name, now called)`.
    #[serde(default)]
    pub aside: Vec<(String, String)>,
    /// Migrations for stormupdate to run: the node's data is under
    /// `node_volume`, the release's under `volume`.
    #[serde(default)]
    pub migrations: Vec<InstallMigration>,
    /// Volumes the node made in the system half, carried into the data half
    /// (#349): a registry golden, held media, a volume made with no role.
    #[serde(default)]
    pub carried: Vec<String>,
    /// The old release's volumes the data half recorded with extents outside
    /// it (#369), dropped from it: they come back with the release.
    #[serde(default)]
    pub release_dropped: Vec<String>,
}

/// A migration an install leaves for stormupdate (#122's `migrate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallMigration {
    /// The release's volume, under the plain name.
    pub volume: String,
    /// The node's data, renamed aside.
    pub node_volume: String,
    pub hook: String,
}

impl InstallMigration {
    /// As a generation's migration: the release's volume under the plain name
    /// (`staged` = `volume`), the node's in `node`.
    pub fn as_migration(&self) -> Migration {
        Migration {
            volume: self.volume.clone(),
            hook: self.hook.clone(),
            staged: self.volume.clone(),
            node: Some(self.node_volume.clone()),
        }
    }
}

/// The data and bulk partitions of a node layout, as slabs, and the system
/// slab (when it opens).
async fn halves(device: &Arc<dyn BlockDevice>) -> anyhow::Result<(Slab, Option<Slab>, Option<Slab>)> {
    let (data_i, system_i) = crate::image::local::node_layout(device)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the drive does not carry a node layout"))?;
    let gpt = Gpt::read(device).await.map_err(|e| anyhow::anyhow!("reading the table: {e}"))?;
    let part = |i: usize| -> anyhow::Result<Arc<dyn BlockDevice>> {
        let e = &gpt.entries[i];
        Ok(Arc::new(
            PartitionDevice::new(device.clone(), e.start_bytes(gpt.block_size), e.size_bytes(gpt.block_size))
                .map_err(|err| anyhow::anyhow!("partition {}: {err}", i + 1))?,
        ))
    };
    let data = Slab::open(part(data_i)?).await.map_err(|e| anyhow::anyhow!("the data slab will not open: {e}"))?;
    let bulk = match crate::image::local::bulk_partition(device).await {
        Some(i) => Some(Slab::open(part(i)?).await.map_err(|e| anyhow::anyhow!("the bulk slab will not open: {e}"))?),
        None => None,
    };
    let system = Slab::open(part(system_i)?).await.ok();
    Ok((data, bulk, system))
}

/// Read the node's data half and check this install may keep it. Writes
/// nothing; an error here means the install must not go on (the data is
/// untouched, and the caller boots from the appliance).
///
/// `release` is every volume name the release being installed carries. The
/// system half is laid again, so a volume in it that the release does not
/// bring back is lost: one the node made there itself (a volume created with
/// no role on a node with both halves lands in the system half) is
/// application data. An unsealed volume in the system half that the release
/// does not name stops the install, named; a release's own previous
/// generation (`<name>@<version>`, #122) and sealed goldens do not.
pub async fn plan(device: &Arc<dyn BlockDevice>, release: &HashSet<String>) -> anyhow::Result<Plan> {
    let (data, bulk, system) = halves(device).await?;
    if !data.is_data() {
        anyhow::bail!("the partition typed as the data slab says it is a system slab");
    }
    let half: HashSet<SlabId> = std::iter::once(data.slab_id()).chain(bulk.as_ref().map(|b| b.slab_id())).collect();
    let mut volumes = Vec::new();
    let mut seen = HashSet::new();
    let mut outside: Vec<String> = Vec::new();
    let mut release_strays: Vec<(VolumeId, String)> = Vec::new();
    let mut carry_from_data: Vec<(VolumeId, String)> = Vec::new();
    let sys_id = system.as_ref().map(|s| s.slab_id());
    for slab in std::iter::once(&data).chain(bulk.as_ref()) {
        if !slab.has_metadata_region() {
            anyhow::bail!("slab {} keeps no volume records: what it holds cannot be told, so it is not installed over", slab.slab_id());
        }
        let doc = match crate::volume::metav2::read_slab(slab).await {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => anyhow::bail!("the records of slab {} do not read ({e}); not installing over them", slab.slab_id()),
        };
        for v in doc.volumes {
            if !seen.insert(v.id) {
                continue;
            }
            let legs = v
                .extents
                .values()
                .flat_map(|l| l.legs().collect::<Vec<_>>())
                .chain(v.parity.values().flat_map(|g| g.legs.clone()));
            let away: Vec<SlabId> = legs.map(|l| l.slab_id).filter(|s| !half.contains(s)).collect();
            if away.is_empty() {
                volumes.push((v.id, v.name, v.sealed));
                continue;
            }
            // The release's own (#369): cilium, coredns and the rest, laid
            // by an earlier install and recorded here too. System class:
            // the system half they live in is laid again, and they come
            // back with the release. A record written before origins were
            // (#349) says nothing: the release being installed naming it
            // (or its `.golden`) is what tells then.
            use crate::volume::metadata::Origin;
            let named = release.contains(&v.name)
                || v.name.strip_suffix(".golden").is_some_and(|b| release.contains(b));
            if v.origin == Origin::Release || (v.origin == Origin::Unmarked && named) {
                release_strays.push((v.id, v.name));
                continue;
            }
            // Anything else whose stray extents are all in this disk's own
            // system half loses nothing: it is carried into the data half
            // first (#349's carry), and kept.
            if sys_id.is_some_and(|sys| away.iter().all(|s| *s == sys)) {
                carry_from_data.push((v.id, v.name.clone()));
                volumes.push((v.id, v.name, v.sealed));
                continue;
            }
            outside.push(format!("{} ({} extent(s))", v.name, away.len()));
            volumes.push((v.id, v.name, v.sealed));
        }
    }
    if !outside.is_empty() {
        anyhow::bail!(
            "data volume(s) with extents outside the data half, which laying the system half again would lose: {}",
            outside.join(", ")
        );
    }
    // The system half: what laying it again would lose that the release does
    // not bring back.
    if let Some(sys) = system.as_ref().filter(|s| s.has_metadata_region()) {
        let doc = match crate::volume::metav2::read_slab(sys).await {
            Ok(d) => d,
            Err(e) => anyhow::bail!("the system half's records do not read ({e}): what it holds cannot be told"),
        };
        // Owner (#349): an install drops old system volumes, never a partner's
        // or a user's. What the old release laid and this one does not name
        // goes with the system half; what the node made — or what says
        // nothing of where it came from, recorded before origins were — is
        // carried into the data half, sealed or not.
        let mut carry = carry_from_data;
        let mut dropped = Vec::new();
        for v in doc.map(|d| d.volumes).unwrap_or_default() {
            if seen.contains(&v.id) || release.contains(&v.name) || v.name.contains('@') {
                continue;
            }
            match v.origin {
                crate::volume::metadata::Origin::Release => dropped.push(v.name),
                _ => carry.push((v.id, v.name)),
            }
        }
        carry.sort_by(|a, b| a.1.cmp(&b.1));
        dropped.sort();
        return Ok(Plan {
            data_slab: data.slab_id(),
            bulk_slab: bulk.as_ref().map(|b| b.slab_id()),
            volumes,
            carry,
            dropped,
            release_strays,
        });
    }
    Ok(Plan {
        data_slab: data.slab_id(),
        bulk_slab: bulk.as_ref().map(|b| b.slab_id()),
        volumes,
        carry: carry_from_data,
        dropped: Vec::new(),
        release_strays,
    })
}

/// Free slots in a node disk's data half, by slot size (data slab, bulk
/// slab) (#172).
pub async fn data_half_free(device: &Arc<dyn BlockDevice>) -> anyhow::Result<std::collections::BTreeMap<u64, u64>> {
    let (data, bulk, _) = halves(device).await?;
    let mut free: std::collections::BTreeMap<u64, u64> = Default::default();
    for s in std::iter::once(&data).chain(bulk.as_ref()) {
        *free.entry(s.slot_size()).or_default() += s.free_slots();
    }
    Ok(free)
}

/// Whether a data half with `free` slots (by slot size) can take `need`
/// more, with headroom (5 % and 64 slots) for the writes that come while it
/// moves (#172). `Some(why)` when it cannot.
pub fn short_of_room(
    need: &std::collections::BTreeMap<u64, u64>,
    free: &std::collections::BTreeMap<u64, u64>,
) -> Option<String> {
    let mut short = Vec::new();
    for (&size, &n) in need.iter().filter(|(_, n)| **n > 0) {
        let want = n + n / 20 + 64;
        let have = free.get(&size).copied().unwrap_or(0);
        if have < want {
            short.push(format!(
                "the release brings {n} slot(s) of {} into the data half, which has {have} free \
                 ({want} needed with headroom)",
                crate::mgmt::config::human_size(size)
            ));
        }
    }
    (!short.is_empty()).then(|| {
        format!(
            "{}; installed anyway the flow-over would stop on a full slab and, after a power cycle, \
             no new write on the node would land (#172). Free space on the data half or use a larger disk",
            short.join("; ")
        )
    })
}

#[cfg(test)]
mod room_tests {
    use super::short_of_room;
    use std::collections::BTreeMap;

    #[test]
    fn an_install_without_room_for_the_release_s_data_is_refused() {
        const MIB: u64 = 1 << 20;
        let m = |v: &[(u64, u64)]| v.iter().copied().collect::<BTreeMap<u64, u64>>();
        assert_eq!(short_of_room(&m(&[(MIB, 1000)]), &m(&[(MIB, 2000)])), None);
        // pvetest2: 13589 to bring, a full data half.
        let why = short_of_room(&m(&[(MIB, 13589)]), &m(&[(MIB, 120)])).unwrap();
        assert!(why.contains("13589") && why.contains("120 free"), "{why}");
        // Headroom: exactly the need is not enough.
        assert!(short_of_room(&m(&[(MIB, 1000)]), &m(&[(MIB, 1000)])).is_some());
        // Bulk slots are counted apart.
        assert!(short_of_room(&m(&[(MIB, 10), (8 * MIB, 500)]), &m(&[(MIB, 5000), (8 * MIB, 10)])).is_some());
        assert_eq!(short_of_room(&m(&[]), &m(&[])), None);
    }
}

/// A free name for the node's `name` set aside: `<name>@<previous>`, or with
/// `.2`, `.3`… when an earlier install already used it.
async fn aside_name(mgr: &VolumeManager, name: &str, previous: &str) -> String {
    let base = generation_name(name, previous);
    if mgr.find_volume(&base).await.is_none() {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}.{n}");
        if mgr.find_volume(&candidate).await.is_none() {
            return candidate;
        }
        n += 1;
    }
}

/// Rename the node's `name` aside and say so in the report.
async fn set_aside(
    mgr: &mut VolumeManager,
    report: &mut Report,
    id: VolumeId,
    name: &str,
    previous: &str,
) -> anyhow::Result<String> {
    let to = aside_name(mgr, name, previous).await;
    mgr.rename_volume(id, &to)
        .await
        .map_err(|e| anyhow::anyhow!("renaming the node's {name} aside: {e}"))?;
    report.aside.push((name.to_string(), to.clone()));
    Ok(to)
}

/// Adopt the node's data half into `mgr` (which holds the release's claimed
/// image) and settle each name as the module documents. `data`/`bulk` are the
/// slabs `update_system_slab` returned; `plan` was read before it ran.
pub async fn adopt(
    mgr: &mut VolumeManager,
    data: Slab,
    bulk: Option<Slab>,
    plan: &Plan,
    policy: &HashMap<String, Policy>,
    version: &str,
    previous: &str,
) -> anyhow::Result<Report> {
    if data.slab_id() != plan.data_slab {
        anyhow::bail!("the data slab changed between the plan and the install");
    }
    // The release's data volumes, by name, before the node's arrive.
    let mut release: HashMap<String, (VolumeId, bool)> = HashMap::new();
    for (id, name, _, _) in mgr.list_volumes().await {
        if mgr.volume_role(&id) == Some(SlabRole::Data) {
            release.insert(name, (id, mgr.is_sealed(&id)));
        }
    }

    // The same id on both sides (#244): the release the disk already held,
    // installed again — a root that would not come up, an install ticket
    // over the same release. One id is one volume, and adopting keeps the
    // record already known, the claim's fresh copy: the node's bytes would
    // be left unmapped, and freed by the next GC. So the claim's copy goes
    // first and the node's record is the one adopted. A sealed one (a
    // golden) is the same bytes on both sides and stays one volume. Two
    // volumes cannot share an id, so the policy cannot set the node's aside
    // here: re-installing the release it runs is a repair, and the node's
    // data is kept.
    let mut same_id: HashSet<VolumeId> = HashSet::new();
    for (node_id, name, sealed) in &plan.volumes {
        if *sealed || mgr.get_volume(node_id).is_none() || mgr.is_sealed(node_id) {
            continue;
        }
        mgr.delete_volume(*node_id)
            .await
            .map_err(|e| anyhow::anyhow!("the claim's copy of the node's {name} ({}) could not be dropped: {e}", node_id.0))?;
        same_id.insert(*node_id);
    }

    // A release stray the claim holds itself (same id) is the claim's; the
    // others are dropped once the data half is adopted (#369).
    let strays: Vec<(VolumeId, String)> = plan
        .release_strays
        .iter()
        .filter(|(id, _)| mgr.get_volume(id).is_none())
        .cloned()
        .collect();

    let mut found = vec![crate::drive::discover::FoundSlab { label: "data".into(), slab: data }];
    if let Some(b) = bulk {
        found.push(crate::drive::discover::FoundSlab { label: "bulk".into(), slab: b });
    }
    let adopted = mgr
        .adopt_slabs(found)
        .await
        .map_err(|e| anyhow::anyhow!("adopting the node's data half: {e}"))?;

    let mut report = Report {
        version: version.to_string(),
        previous: previous.to_string(),
        adopted: adopted.volumes.len() + adopted.already_known,
        ..Default::default()
    };
    for (id, name) in strays {
        if mgr.get_volume(&id).is_none() {
            continue;
        }
        match mgr.delete_volume(id).await {
            Ok(()) => {
                tracing::info!("install: {name} is the old release's (#369): laid again with the system half, its data-half record dropped");
                report.release_dropped.push(name);
            }
            Err(e) => tracing::warn!("install: the old release's {name} could not be dropped from the data half: {e}"),
        }
    }
    for (node_id, name, _) in &plan.volumes {
        if same_id.contains(node_id) {
            report.kept.push(name.clone());
            continue;
        }
        let Some(&(rel_id, rel_sealed)) = release.get(name) else { continue };
        if rel_id == *node_id {
            // One volume: the release's and the node's are the same id.
            continue;
        }
        if rel_sealed {
            // A golden of the release keeps its name: `slab holds` and the
            // release's clones go by it. The node's own is kept aside.
            set_aside(mgr, &mut report, *node_id, name, previous).await?;
            continue;
        }
        match policy.get(name).cloned().unwrap_or(Policy::Keep) {
            Policy::Keep => {
                // The node's stays. The release's was this boot's fresh
                // clone of its blank, and nothing has used it.
                if let Err(e) = mgr.delete_volume(rel_id).await {
                    let to = generation_name(name, version);
                    tracing::warn!("install: the release's fresh {name} could not be deleted ({e}); renamed {to}");
                    mgr.rename_volume(rel_id, &to)
                        .await
                        .map_err(|e| anyhow::anyhow!("setting the release's {name} aside: {e}"))?;
                }
                report.kept.push(name.clone());
            }
            Policy::Replace => {
                set_aside(mgr, &mut report, *node_id, name, previous).await?;
            }
            Policy::Migrate { hook } => {
                let node_volume = set_aside(mgr, &mut report, *node_id, name, previous).await?;
                report.migrations.push(InstallMigration { volume: name.clone(), node_volume, hook });
            }
        }
    }
    mgr.persist().await;
    Ok(report)
}

/// `VERSION_ID` from an os-release file's bytes.
pub fn version_id(os_release: &[u8]) -> Option<String> {
    String::from_utf8_lossy(os_release).lines().find_map(|l| {
        l.strip_prefix("VERSION_ID=").map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty())
    })
}

/// Carry the node's own volumes out of the system half into the data half
/// before the system half is laid again (#349): every extent of each one
/// still on the system slab moves to the data slab, its id, its sharing
/// (a slot every map names follows for all of them) and its record kept.
/// The data half then holds them, and `adopt` keeps them like any data
/// volume. Refused before anything moves when the data slab has no room.
pub async fn carry(device: &Arc<dyn BlockDevice>, plan: &Plan) -> anyhow::Result<Vec<String>> {
    if plan.carry.is_empty() {
        return Ok(Vec::new());
    }
    let (data, bulk, system) = halves(device).await?;
    let Some(system) = system else {
        anyhow::bail!("the system half does not open: its volumes cannot be carried");
    };
    let (data_id, system_id) = (data.slab_id(), system.slab_id());
    let mut mgr = VolumeManager::new(system.slot_size());
    let mut found = vec![
        crate::drive::discover::FoundSlab { label: "data".into(), slab: data },
        crate::drive::discover::FoundSlab { label: "system".into(), slab: system },
    ];
    if let Some(b) = bulk {
        found.push(crate::drive::discover::FoundSlab { label: "bulk".into(), slab: b });
    }
    mgr.adopt_slabs(found).await.map_err(|e| anyhow::anyhow!("opening the halves: {e}"))?;
    let gem = mgr.gem().clone();
    let registry = mgr.registry().clone();
    let _pin = crate::volume::gem::pin_resident(&gem).await.map_err(|e| anyhow::anyhow!("loading extent maps: {e}"))?;
    let ids: Vec<VolumeId> = plan.carry.iter().map(|(id, _)| *id).collect();
    // What moves, and whether it fits.
    let mut todo: Vec<(VolumeId, u64)> = Vec::new();
    {
        let g = gem.read().await;
        for id in &ids {
            let Some(m) = g.get_volume_map(id) else { continue };
            if m.parity.values().any(|grp| grp.legs.iter().any(|l| l.slab_id == system_id)) {
                anyhow::bail!("{} keeps parity in the system half: not carried automatically, move it by hand", id.0);
            }
            for (v, loc) in m.extents.iter() {
                if loc.leg_on(system_id).is_some() {
                    todo.push((*id, v));
                }
            }
        }
    }
    let free = registry.read().await.get(&data_id).map(|s| s.free_slots()).unwrap_or(0);
    if todo.len() as u64 > free {
        anyhow::bail!(
            "the data half has room for {free} extent(s) and the node's volumes in the system half need {}: \
             not installing over this disk (nothing moved); make room or move them by hand",
            todo.len()
        );
    }
    let engine = crate::placement::PlacementEngine::new();
    for (id, v) in todo {
        let mut g = gem.write().await;
        let mut reg = registry.write().await;
        // A slot another carried map shares has moved already.
        if g.lookup(id, v).is_none_or(|l| l.leg_on(system_id).is_none()) {
            continue;
        }
        engine
            .migrate_leg(&mut g, &mut reg, id, v, system_id, Some(data_id))
            .await
            .map_err(|e| anyhow::anyhow!("carrying {} extent {v}: {e}", id.0))?;
    }
    drop(_pin);
    mgr.keep_metadata_in_first(&[data_id]);
    mgr.persist_checked().await.map_err(|e| anyhow::anyhow!("recording the carried volumes: {e}"))?;
    Ok(plan.carry.iter().map(|(_, n)| n.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_id_reads_quoted_and_bare() {
        assert_eq!(version_id(b"NAME=x\nVERSION_ID=\"11.91\"\n").as_deref(), Some("11.91"));
        assert_eq!(version_id(b"VERSION_ID=11.90").as_deref(), Some("11.90"));
        assert_eq!(version_id(b"NAME=x\n"), None);
    }
}

/// The node's release generations after an install (#311): the release is
/// current, complete, with what it kept and what is left to migrate. Nothing
/// staged survives an install (it was staged against the release replaced),
/// and there is no previous one to roll back to: the system half it lived in
/// was laid again.
pub fn generations_after(report: &Report, at: u64) -> crate::image::stage::Generations {
    crate::image::stage::Generations {
        current: Some(crate::image::stage::Generation {
            version: report.version.clone(),
            volumes: Vec::new(),
            complete: true,
            pallet: None,
            migrations: report.migrations.iter().map(|m| m.as_migration()).collect(),
            kept: report.kept.clone(),
            source: Some("install".into()),
            at,
        }),
        staged: None,
        previous: None,
    }
}
