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
            let away = legs.filter(|l| !half.contains(&l.slab_id)).count();
            if away > 0 {
                outside.push(format!("{} ({away} extent(s))", v.name));
            }
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
        let mut lost: Vec<String> = Vec::new();
        for v in doc.map(|d| d.volumes).unwrap_or_default() {
            if v.sealed || seen.contains(&v.id) || release.contains(&v.name) || v.name.contains('@') {
                continue;
            }
            lost.push(v.name);
        }
        if !lost.is_empty() {
            lost.sort();
            anyhow::bail!(
                "the system half holds {} volume(s) this release does not bring back, which laying it again \
                 would destroy: {} — move them to the data half (or delete them) and install again",
                lost.len(),
                lost.join(", ")
            );
        }
    }
    Ok(Plan { data_slab: data.slab_id(), bulk_slab: bulk.as_ref().map(|b| b.slab_id()), volumes })
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
