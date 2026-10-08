//! Stage the next release on a running node, activate it, roll it back (#122).
//!
//! The owner's rules (2026-10-06 on #122): an update is done from the running
//! system, never by an install at boot; a release may mark each data volume
//! **keep**, **replace** or **migrate** (a hook the release ships); and the
//! boot after activating is decided by the appliance's assignment, which
//! stormupdate re-points to N+1 first (2A). An install keeps the data half
//! and applies the same policy file (#311, `image::install`).
//!
//! **Stage** reads N+1's published image (an `http://` URL, by `Range`, or any
//! device) and copies into the node's own slabs, with nothing of N touched:
//!
//! - every **golden** (sealed volume) under the id the release gave it — that
//!   id is what `slab holds` compares, so after activate the boot that claims
//!   N+1 finds the disk holds it, and a rollback that claims N finds N's
//!   goldens still there (#265). A golden is copied unsealed and sealed only
//!   when whole, and `slab holds` counts only sealed ones, so a stage cut
//!   short never reads as holding the release;
//! - every **clone** as a copy-on-write clone of its staged (or shared)
//!   parent, plus the extents it has of its own;
//! - a volume whose id the node already has (a golden N and N+1 share) is
//!   shared, not copied.
//!
//! Each is named `<name>@<version>`. A **data** volume the node already has by
//! name is kept unless the release marks it replace or migrate; one the node
//! does not have (a volume N+1 adds) is staged. N+1's boot pallet goes onto
//! the disk's boot area just below the active one.
//!
//! **Activate** renames: N's volumes that a staged one replaces become
//! `<name>@<N>`, the staged ones take the plain names, and N+1's boot pallet
//! goes on top. **Rollback** renames back and raises N's pallet. One previous
//! generation is kept; it is deleted when the next release is staged.
//!
//! What the release's policy says is read from its root volume,
//! `/etc/stormblock/data-volumes`, one volume a line:
//!
//! ```text
//! # volume   policy    [hook, a path in the release's root filesystem]
//! kubelet-data keep
//! fastetcd-data migrate /usr/libexec/stormcos/migrate-fastetcd
//! pod-logs   replace
//! ```
//!
//! An unlisted volume is **replace** in the system half and **keep** in the
//! data half. The engine runs nothing of the release's: a migration is listed
//! in the generation for stormupdate to run between stage and activate, with
//! the node's volume and the staged one both there by name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabRole;
use crate::drive::BlockDevice;
use crate::volume::{CreateOptions, VolumeId, VolumeManager};

/// The release's file that says what happens to each data volume.
pub const POLICY_FILE: &str = "/etc/stormblock/data-volumes";
/// The record of this node's release generations, in the data directory.
pub const GENERATIONS_FILE: &str = "release-generations.json";

/// What a release says happens to one of its volumes on an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "policy")]
pub enum Policy {
    /// The node's volume stays as it is.
    Keep,
    /// The release's volume replaces the node's; the node's is kept as the
    /// previous generation until the next stage.
    Replace,
    /// As replace, and `hook` (a path in the release's root filesystem)
    /// carries the node's data across. stormupdate runs it.
    Migrate { hook: String },
}

/// Parse a policy file. Unknown words are an error: a release that says
/// something the engine does not understand must not be half-applied.
pub fn parse_policy(text: &str) -> Result<HashMap<String, Policy>, String> {
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut w = line.split_whitespace();
        let (Some(vol), Some(p)) = (w.next(), w.next()) else {
            return Err(format!("{POLICY_FILE} line {}: expected `<volume> keep|replace|migrate [hook]`", n + 1));
        };
        let policy = match (p, w.next()) {
            ("keep", None) => Policy::Keep,
            ("replace", None) => Policy::Replace,
            ("migrate", Some(hook)) => Policy::Migrate { hook: hook.to_string() },
            ("migrate", None) => return Err(format!("{POLICY_FILE} line {}: migrate needs a hook", n + 1)),
            (other, _) => return Err(format!("{POLICY_FILE} line {}: {other:?} is not keep, replace or migrate", n + 1)),
        };
        out.insert(vol.to_string(), policy);
    }
    Ok(out)
}

/// A name with its generation: `<name>@<version>`.
pub fn generation_name(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// One staged (or activated, or kept) volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenVolume {
    pub id: VolumeId,
    /// The plain name it has (or takes) while its generation is current.
    pub name: String,
}

/// A migration stormupdate runs: the hook, the node's volume and the
/// release's, both present by name between stage and activate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migration {
    pub volume: String,
    pub hook: String,
    /// The release's volume, staged: `<volume>@<version>` until activate.
    pub staged: String,
    /// Where the node's data is, when not under `volume`: after an install
    /// (#311) the release's volume has the plain name (`staged` = `volume`)
    /// and the node's is set aside as `<volume>@<previous>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// One release generation on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation {
    pub version: String,
    /// Every volume of the generation, in the order they were made (parents
    /// first): staged, or (for the previous generation) renamed aside.
    pub volumes: Vec<GenVolume>,
    /// Stage finished: every volume whole, goldens sealed.
    #[serde(default)]
    pub complete: bool,
    /// The boot pallet's manifest digest, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pallet: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<Migration>,
    /// Data volumes the node keeps, as the release asked (or left to default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// Where it was staged from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub at: u64,
}

/// The node's generations: what runs, what is staged, what it can go back to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generations {
    /// The release the node runs. `None` until stated (a stage's `current`)
    /// or set by an activate.
    #[serde(default)]
    pub current: Option<Generation>,
    #[serde(default)]
    pub staged: Option<Generation>,
    #[serde(default)]
    pub previous: Option<Generation>,
}

impl Generations {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(GENERATIONS_FILE)
    }

    pub fn load(data_dir: &Path) -> Generations {
        match std::fs::read(Self::path(data_dir)) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::error!("{}: unreadable ({e}); starting from no generations", Self::path(data_dir).display());
                Generations::default()
            }),
            Err(_) => Generations::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }
}

/// How far a stage has got, for its job's status.
#[derive(Debug, Default)]
pub struct Progress {
    pub volumes_done: AtomicU64,
    pub volumes_total: AtomicU64,
    pub bytes_copied: AtomicU64,
}

/// What one image volume becomes on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    /// Copied whole, under the release's id when it is a golden.
    Copy,
    /// A copy-on-write clone of its local parent plus its own extents.
    Clone { parent: VolumeId },
    /// The node already has this volume, by id.
    Shared,
    /// A data volume the node has by name and keeps.
    Kept,
}

/// One line of the plan, reported to the caller.
#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub name: String,
    pub id: VolumeId,
    pub role: String,
    pub sealed: bool,
    #[serde(flatten)]
    pub action: Action,
    /// The name it is staged under (`None`: not staged).
    pub staged_as: Option<String>,
    pub local_id: Option<VolumeId>,
    pub policy: Option<Policy>,
}

struct ImageVolume {
    id: VolumeId,
    name: String,
    size: u64,
    extent_size: u64,
    role: SlabRole,
    sealed: bool,
    parent: Option<VolumeId>,
    template: bool,
    lba: Option<u32>,
    fs: Option<crate::volume::metadata::FsInfo>,
    owner: Option<crate::volume::metadata::Owner>,
    /// Virtual extent → (slab, slot) of its primary leg.
    extents: BTreeMap<u64, (crate::drive::slab::SlabId, u64)>,
}

/// The release image's volumes, read through a manager of its own that
/// nothing persists to anything but the image device's own memory.
async fn read_image(image: &Arc<dyn BlockDevice>) -> anyhow::Result<(VolumeManager, Vec<ImageVolume>)> {
    let found = crate::drive::discover::slabs_in_partitions(image).await;
    if found.is_empty() {
        anyhow::bail!("the image carries no slab");
    }
    let slot = found[0].slab.slot_size();
    let mut vm = VolumeManager::new(slot);
    vm.adopt_slabs(found).await.map_err(|e| anyhow::anyhow!("reading the image's volumes: {e}"))?;
    let mut out = Vec::new();
    for (id, name, size, _) in vm.list_volumes().await {
        crate::volume::gem::ensure_resident(vm.gem(), id).await?;
        let extents = {
            let g = vm.gem().read().await;
            g.volume_extents(&id)
                .map(|it| it.map(|(v, l)| (v, (l.slab_id, l.slot_idx))).collect())
                .unwrap_or_default()
        };
        let h = vm.get_volume_handle(&id).ok_or_else(|| anyhow::anyhow!("volume {id} has no handle"))?;
        out.push(ImageVolume {
            id,
            name,
            size,
            extent_size: h.extent_size(),
            role: vm.volume_role(&id).unwrap_or(SlabRole::System),
            sealed: vm.is_sealed(&id),
            parent: vm.parent(&id),
            template: vm.is_template(&id),
            lba: vm.lba(&id),
            fs: vm.fs_info(&id).cloned(),
            owner: vm.owner(&id).cloned(),
            extents,
        });
    }
    // Parents before their children.
    let ids: HashSet<VolumeId> = out.iter().map(|v| v.id).collect();
    let depth = |v: &ImageVolume, all: &[ImageVolume]| {
        let mut d = 0;
        let mut p = v.parent;
        while let Some(pid) = p.filter(|p| ids.contains(p)) {
            d += 1;
            p = all.iter().find(|x| x.id == pid).and_then(|x| x.parent);
            if d > 64 {
                break;
            }
        }
        d
    };
    let mut keyed: Vec<(usize, ImageVolume)> = Vec::new();
    let depths: Vec<usize> = out.iter().map(|v| depth(v, &out)).collect();
    for (v, d) in out.into_iter().zip(depths) {
        keyed.push((d, v));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
    Ok((vm, keyed.into_iter().map(|(_, v)| v).collect()))
}

/// The release's policy file, read from its root volume `root`.
pub(crate) async fn read_policy(image_vm: &VolumeManager, root: &str) -> anyhow::Result<HashMap<String, Policy>> {
    let Some(id) = image_vm.find_volume(root).await else {
        anyhow::bail!("the release has no root volume {root}");
    };
    let dev = image_vm.get_volume(&id).ok_or_else(|| anyhow::anyhow!("{root} has no handle"))?;
    match crate::fs::files::read_file(&dev, POLICY_FILE).await {
        Ok(bytes) => parse_policy(&String::from_utf8_lossy(&bytes)).map_err(|e| anyhow::anyhow!(e)),
        // No file: every volume takes its default.
        Err(e) if e.to_string().contains("not found") || e.to_string().contains("No such") => Ok(HashMap::new()),
        Err(e) => {
            tracing::info!("stage: {root}:{POLICY_FILE} not read ({e}); every volume takes its default");
            Ok(HashMap::new())
        }
    }
}

/// Delete a generation's volumes, children first. Volumes already gone are
/// skipped; one in use stops it.
pub async fn delete_generation(vm: &crate::lockwatch::TrackedMutex<VolumeManager>, gen: &Generation) -> anyhow::Result<usize> {
    let mut n = 0;
    let mine: HashSet<VolumeId> = gen.volumes.iter().map(|v| v.id).collect();
    for v in gen.volumes.iter().rev() {
        let mut m = vm.lock().await;
        if m.get_volume_handle(&v.id).is_none() {
            continue;
        }
        // Something outside the generation is cloned from it — a data
        // volume the node kept is a clone of the old release's golden. That
        // golden stays as long as its clone does.
        let others: Vec<VolumeId> = m.children(&v.id).into_iter().filter(|c| !mine.contains(c)).collect();
        if !others.is_empty() {
            tracing::info!("release {}: {} ({}) kept, {} volume(s) are cloned from it", gen.version, v.name, v.id, others.len());
            continue;
        }
        // A golden N and N+1 share is the current generation's too.
        m.delete_volume(v.id)
            .await
            .map_err(|e| anyhow::anyhow!("deleting {} ({}) of release {}: {e}", v.name, v.id, gen.version))?;
        n += 1;
    }
    vm.lock().await.persist().await;
    Ok(n)
}

/// Copy `extents` of `src` into `dst` (offsets in `extent_size`), and
/// discard on `dst` the extents in `clear`.
async fn copy_extents(
    src: &Arc<dyn BlockDevice>,
    dst: &Arc<dyn BlockDevice>,
    size: u64,
    extent_size: u64,
    extents: &[u64],
    clear: &[u64],
    progress: &Progress,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; extent_size as usize];
    for e in extents {
        let off = e * extent_size;
        if off >= size {
            continue;
        }
        let n = extent_size.min(size - off) as usize;
        src.read(off, &mut buf[..n]).await?;
        dst.write(off, &buf[..n]).await?;
        progress.bytes_copied.fetch_add(n as u64, Ordering::Relaxed);
    }
    for e in clear {
        let off = e * extent_size;
        if off < size {
            dst.discard(off, extent_size.min(size - off)).await?;
        }
    }
    dst.flush().await?;
    Ok(())
}

/// Options for [`stage`].
#[derive(Debug, Clone)]
pub struct StageOptions {
    pub version: String,
    /// The release's root volume, where its policy file is.
    pub root: String,
    /// What the image is, for the record.
    pub source: String,
}

/// Stage the release in `image` onto the node `vm` manages (see the module
/// documentation). Returns the generation (complete) and the plan. The
/// caller saves the generation, before (incomplete) and after.
pub async fn stage(
    vm: &crate::lockwatch::TrackedMutex<VolumeManager>,
    image: Arc<dyn BlockDevice>,
    opts: &StageOptions,
    progress: &Progress,
    mut on_volume: impl FnMut(&Generation),
) -> anyhow::Result<(Generation, Vec<PlanItem>)> {
    let (image_vm, vols) = read_image(&image).await?;
    let policy = read_policy(&image_vm, &opts.root).await?;
    let mut gen = Generation {
        version: opts.version.clone(),
        volumes: Vec::new(),
        complete: false,
        pallet: None,
        migrations: Vec::new(),
        kept: Vec::new(),
        source: Some(opts.source.clone()),
        at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    let (local_ids, local_names): (HashSet<VolumeId>, HashSet<String>) = {
        let m = vm.lock().await;
        let l = m.list_volumes().await;
        (l.iter().map(|v| v.0).collect(), l.into_iter().map(|v| v.1).collect())
    };
    progress.volumes_total.store(vols.len() as u64, Ordering::Relaxed);

    // Image id → the node's volume it is, once staged or shared.
    let mut local_of: HashMap<VolumeId, VolumeId> = HashMap::new();
    let mut plan = Vec::new();
    for v in &vols {
        let pol = policy.get(&v.name).cloned();
        let mut item = PlanItem {
            name: v.name.clone(),
            id: v.id,
            role: v.role.to_string(),
            sealed: v.sealed,
            action: Action::Copy,
            staged_as: None,
            local_id: None,
            policy: pol.clone(),
        };
        if local_ids.contains(&v.id) {
            item.action = Action::Shared;
            item.local_id = Some(v.id);
            local_of.insert(v.id, v.id);
            plan.push(item);
            progress.volumes_done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let effective = pol.clone().unwrap_or(match v.role {
            SlabRole::Data => Policy::Keep,
            SlabRole::System => Policy::Replace,
        });
        // A golden is always staged: it is the release, and what `slab
        // holds` asks for. Keep applies to what the node runs on.
        if !v.sealed && effective == Policy::Keep && local_names.contains(&v.name) {
            item.action = Action::Kept;
            gen.kept.push(v.name.clone());
            plan.push(item);
            progress.volumes_done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let staged_name = generation_name(&v.name, &opts.version);
        let src = image_vm.get_volume(&v.id).ok_or_else(|| anyhow::anyhow!("image volume {} has no handle", v.name))?;
        let parent_local = v.parent.and_then(|p| local_of.get(&p).copied());
        let parent_extents = v.parent.and_then(|p| vols.iter().find(|x| x.id == p)).map(|p| &p.extents);
        let (id, copy, clear) = match (parent_local, parent_extents, v.sealed) {
            // A clone (not a golden: a golden keeps the release's id, which
            // a snapshot cannot give it) of a parent that is here.
            (Some(lp), Some(pext), false) => {
                let id = vm.lock().await.create_snapshot_deferred(lp, &staged_name).await.map_err(|e| anyhow::anyhow!("{staged_name}: {e}"))?;
                item.action = Action::Clone { parent: lp };
                let copy: Vec<u64> = v.extents.iter().filter(|(e, l)| pext.get(e) != Some(l)).map(|(e, _)| *e).collect();
                let clear: Vec<u64> = pext.keys().filter(|e| !v.extents.contains_key(e)).copied().collect();
                (id, copy, clear)
            }
            _ => {
                let opts = CreateOptions {
                    role: Some(v.role),
                    extent_size: Some(v.extent_size),
                    id: v.sealed.then_some(v.id),
                    ..Default::default()
                };
                let id = vm.lock().await.create_volume_with(&staged_name, v.size, opts).await.map_err(|e| anyhow::anyhow!("{staged_name}: {e}"))?;
                (id, v.extents.keys().copied().collect(), Vec::new())
            }
        };
        // A staged copy of the release is the release's (#349).
        vm.lock().await.set_origin(id, crate::volume::metadata::Origin::Release);
        gen.volumes.push(GenVolume { id, name: v.name.clone() });
        on_volume(&gen);
        let dst = vm.lock().await.get_volume(&id).ok_or_else(|| anyhow::anyhow!("{staged_name} has no handle"))?;
        copy_extents(&src, &dst, v.size, v.extent_size, &copy, &clear, progress).await?;
        {
            let mut m = vm.lock().await;
            if let Some(lba) = v.lba {
                let _ = m.set_lba(id, lba).await;
            }
            if v.template {
                m.mark_template(id);
            }
            if let Some(o) = &v.owner {
                let _ = m.set_owner(id, Some(o.clone())).await;
            }
            if v.sealed {
                m.seal_volume(id, v.fs.clone()).await.map_err(|e| anyhow::anyhow!("sealing {staged_name}: {e}"))?;
            }
            m.persist().await;
        }
        if let Policy::Migrate { hook } = &effective {
            if local_names.contains(&v.name) {
                gen.migrations.push(Migration { volume: v.name.clone(), hook: hook.clone(), staged: staged_name.clone(), node: None });
            }
        }
        item.staged_as = Some(staged_name);
        item.local_id = Some(id);
        local_of.insert(v.id, id);
        plan.push(item);
        progress.volumes_done.fetch_add(1, Ordering::Relaxed);
    }
    gen.complete = true;
    Ok((gen, plan))
}

/// The renames an activate makes: `(id, from, to)`, the node's volumes aside
/// first, then the staged ones onto the plain names.
pub async fn activation_renames(
    vm: &VolumeManager,
    staged: &Generation,
    current_version: &str,
) -> Result<(Vec<(VolumeId, String, String)>, Vec<GenVolume>), String> {
    let list = vm.list_volumes().await;
    let by_name: HashMap<String, VolumeId> = list.iter().map(|v| (v.1.clone(), v.0)).collect();
    let staged_ids: HashSet<VolumeId> = staged.volumes.iter().map(|v| v.id).collect();
    let mut aside = Vec::new();
    let mut moved = Vec::new();
    let mut onto = Vec::new();
    for v in &staged.volumes {
        let staged_name = generation_name(&v.name, &staged.version);
        match by_name.get(&staged_name) {
            Some(id) if *id == v.id => {}
            _ => return Err(format!("staged volume {staged_name} ({}) is not here", v.id)),
        }
        if let Some(old) = by_name.get(&v.name).filter(|id| !staged_ids.contains(id)) {
            let to = generation_name(&v.name, current_version);
            if by_name.contains_key(&to) {
                return Err(format!("{to} already exists: the generation before {current_version} was not removed"));
            }
            aside.push((*old, v.name.clone(), to));
            moved.push(GenVolume { id: *old, name: v.name.clone() });
        }
        onto.push((v.id, staged_name, v.name.clone()));
    }
    aside.extend(onto);
    Ok((aside, moved))
}

/// Apply renames, in order. On a failure the ones made are undone.
pub async fn apply_renames(vm: &mut VolumeManager, renames: &[(VolumeId, String, String)]) -> Result<(), String> {
    for (i, (id, from, to)) in renames.iter().enumerate() {
        if let Err(e) = vm.rename_volume(*id, to).await {
            for (id, from, _) in renames[..i].iter().rev() {
                let _ = vm.rename_volume(*id, from).await;
            }
            return Err(format!("renaming {from} to {to}: {e}"));
        }
    }
    vm.persist().await;
    Ok(())
}

/// The renames a rollback makes: the current generation's volumes aside as
/// `<name>@<current>`, the previous generation's back onto the plain names.
pub fn rollback_renames(current: &Generation, previous: &Generation) -> Vec<(VolumeId, String, String)> {
    let mut out: Vec<(VolumeId, String, String)> = current
        .volumes
        .iter()
        .map(|v| (v.id, v.name.clone(), generation_name(&v.name, &current.version)))
        .collect();
    out.extend(
        previous
            .volumes
            .iter()
            .map(|v| (v.id, generation_name(&v.name, &previous.version), v.name.clone())),
    );
    out
}

pub fn hex(d: &[u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_file_parses_and_refuses_what_it_does_not_know() {
        let p = parse_policy("# c\nkubelet-data keep\n\nfastetcd-data migrate /usr/libexec/m  # why\npod-logs replace\n").unwrap();
        assert_eq!(p["kubelet-data"], Policy::Keep);
        assert_eq!(p["pod-logs"], Policy::Replace);
        assert_eq!(p["fastetcd-data"], Policy::Migrate { hook: "/usr/libexec/m".into() });
        assert!(parse_policy("x migrate").is_err());
        assert!(parse_policy("x wipe").is_err());
        assert!(parse_policy("x").is_err());
    }

    #[test]
    fn digests_round_trip_as_hex() {
        let d = [0xabu8; 32];
        assert_eq!(unhex(&hex(&d)), Some(d));
        assert_eq!(unhex("xyz"), None);
    }
}
