//! Preformatted filesystem templates — mkfs once, clone forever.
//!
//! Format an empty filesystem into a volume, seal it as a snapshot, and every
//! consumer that needs a fresh filesystem gets a copy-on-write clone instead
//! of running mkfs. A clone starts at near-zero allocation and shares the
//! template's extents, so provisioning costs a snapshot rather than a format.
//!
//! Measured from the other side of the wire: formatting a 256 MiB ext4 over
//! NVMe/TCP takes ~20s (and took over 10 minutes before write pipelining).
//! Cloning a sealed template is effectively instant. For a consumer
//! provisioning volumes on a critical path — a pod start, a VM boot — that is
//! the whole difference.
//!
//! # Why this lives in the engine
//!
//! Formatting locally is memory-and-disk writes; formatting from a consumer is
//! the same work over a network transport, plus an attach and a detach. And
//! the consumer list is not one platform: RouterOS containers want journal-less
//! ext4, StormOS wants its own root layout, Proxmox VMs want disk images,
//! microVMs want a raw rootfs, x86 hosts want a journal. A packaging profile
//! should carry platform *choices*, not platform-independent capabilities.
//!
//! One piece can only live here: a **fresh filesystem UUID stamped at clone
//! time**. Every layer above clones *through* the engine, so a UUID stamped
//! anywhere else leaves clones sharing identity (stormblockmk#12).
//!
//! # Lifecycle
//!
//! ```text
//! create ──► format ──► seal ──► clone (from_template)
//! ```
//!
//! [`create`] does all of the first three in one call when `format_in_core` is
//! set, which is the normal path. The two-phase form — create a raw volume, let
//! an initiator format it over an export, then [`seal`] — stays available for
//! filesystems this engine cannot write itself.
//!
//! [`seal`] is the guard: it refuses a filesystem whose superblock is not in a
//! state a consumer can mount read-write. Checking only `VALID_FS` is what let
//! a template with `ERROR_FS` set and `RECOVER` pending seal cleanly, then fail
//! days later inside a container as `Read-only file system`
//! (stormblock-registry#10).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::volume::{VolumeId, VolumeManager};

use super::ext4;
use super::files::{self, SeedFile};

/// The volume manager, shared. Every entry point here locks it for as short a
/// window as the operation allows and **never across a format or a check** —
/// that is what lets many templates be built at once rather than one at a time.
pub type VmLock = tokio::sync::Mutex<VolumeManager>;
/// The template store, shared.
pub type StoreLock = tokio::sync::Mutex<TemplateStore>;

pub const TEMPLATES_FILE: &str = "fstemplates.json";

/// Filesystems this engine can lay down itself.
///
/// One formatter writes all three; the kind only decides which features are
/// turned on over the common base, exactly as `mke2fs -t` does. A local enum
/// rather than the formatter's own so the persisted store owns its encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    Ext2,
    Ext3,
    #[default]
    Ext4,
}

impl FsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsKind::Ext2 => "ext2",
            FsKind::Ext3 => "ext3",
            FsKind::Ext4 => "ext4",
        }
    }

    pub fn profile(&self) -> ext4::FsProfile {
        match self {
            FsKind::Ext2 => ext4::FsProfile::Ext2,
            FsKind::Ext3 => ext4::FsProfile::Ext3,
            FsKind::Ext4 => ext4::FsProfile::Ext4,
        }
    }
}

impl std::str::FromStr for FsKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ext2" => Ok(FsKind::Ext2),
            "ext3" => Ok(FsKind::Ext3),
            "ext4" => Ok(FsKind::Ext4),
            other => Err(format!(
                "unsupported filesystem '{other}' (this engine writes ext2, ext3 and ext4)"
            )),
        }
    }
}

impl std::fmt::Display for FsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateState {
    /// A raw volume waiting for something to lay a filesystem down on it.
    AwaitingFormat,
    /// A raw volume that is *already* a filesystem, because it was cloned
    /// from a parent template, waiting for its own content before it is
    /// sealed. There is nothing to format — that work was done once, in the
    /// parent — so this is a distinct state rather than a flavour of
    /// `AwaitingFormat`.
    AwaitingSeed,
    /// Sealed: `sealed_volume_id` is a clean snapshot, safe to clone.
    Ready,
}

impl TemplateState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TemplateState::AwaitingFormat => "awaiting_format",
            TemplateState::AwaitingSeed => "awaiting_seed",
            TemplateState::Ready => "ready",
        }
    }
}

/// What to build.
#[derive(Debug, Clone)]
pub struct TemplateSpec {
    pub name: String,
    pub fs: FsKind,
    pub size_bytes: u64,
    /// Journal on or off — **per template, not a build-time default**.
    /// `None` follows the filesystem kind (ext4 and ext3 have one, ext2 does
    /// not). RouterOS cannot replay a journal, so one that ever goes dirty
    /// there leaves the filesystem read-only permanently; a Linux host or a VM
    /// wants the crash consistency. Both variants coexist, told apart by name.
    pub journal: Option<bool>,
    /// Filesystem label baked into the template. Clones inherit it unless the
    /// clone asks for its own.
    pub label: String,
    /// A `mke2fs -O`-style feature list applied over the kind's defaults, e.g.
    /// `"^64bit"` or `"^metadata_csum,^flex_bg"` for a consumer that predates
    /// them. The engine's own vocabulary is the one every consumer already
    /// speaks, so it is passed through rather than re-invented as flags.
    pub features: Option<String>,
    /// Files written into the filesystem before it is sealed, so every clone
    /// inherits them for free — a skeleton `/etc`, a kernel cmdline, the
    /// `boot.toml` an initramfs reads. Written in userspace, with no mount.
    ///
    /// Only meaningful with `format_in_core`.
    pub seed: Vec<SeedFile>,
    /// Format and seal in this process. False leaves the template in
    /// `awaiting_format` for an initiator to format over an export.
    pub format_in_core: bool,
    /// Build this template's filesystem *from* another template's, rather
    /// than from a blank volume — `FROM` in the sense a container build means
    /// it.
    ///
    /// The raw volume becomes a copy-on-write clone of the parent's sealed
    /// snapshot, so it arrives already formatted and already carrying the
    /// parent's contents. Every block the parent contributed is then *shared*
    /// rather than stored a second time: snapshots clone an extent map and
    /// raise a refcount, so a runtime that five images have in common costs
    /// one copy, not five.
    ///
    /// The filesystem's shape is the parent's — kind, journal, features, block
    /// layout — because it *is* the parent's filesystem. Only `size_bytes` may
    /// differ, and only upwards.
    pub parent: Option<String>,
}

impl TemplateSpec {
    pub fn new(name: impl Into<String>, size_bytes: u64) -> Self {
        TemplateSpec {
            name: name.into(),
            fs: FsKind::Ext4,
            size_bytes,
            journal: None,
            label: String::new(),
            features: None,
            seed: Vec::new(),
            format_in_core: true,
            parent: None,
        }
    }

    /// Build this template on top of another — `FROM`, in a container build's
    /// sense. The parent's filesystem is inherited rather than rebuilt, and
    /// its blocks are shared rather than copied.
    pub fn from_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        // There is nothing to format: the filesystem arrives with the clone.
        self.format_in_core = false;
        self
    }
}

/// A clone minted ahead of time and waiting to be claimed.
///
/// Everything expensive about provisioning — the snapshot, the fresh
/// filesystem identity, the check — is done here, before anyone asks. What is
/// left at start time is a lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandingClone {
    pub volume_id: Uuid,
    /// The identity stamped when it was minted. A claimer never shares the
    /// template's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_uuid: Option<Uuid>,
    pub size_bytes: u64,
    /// Whether it was checked when it was minted, so a claim does not have to.
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsTemplate {
    pub id: Uuid,
    pub name: String,
    pub fs: FsKind,
    pub size_bytes: u64,
    /// Whether the filesystem carries a journal, as built.
    #[serde(default)]
    pub journal: bool,
    #[serde(default)]
    pub label: String,
    /// The `-O` list this template was built with, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<String>,
    /// Whether the filesystem uses 64-bit block numbers, as built.
    #[serde(default)]
    pub sixty_four_bit: bool,
    /// Whether it carries `metadata_csum`, and the seed that keeps a
    /// clone-time UUID stamp a single superblock write.
    #[serde(default)]
    pub metadata_csum: bool,
    #[serde(default)]
    pub csum_seed: bool,
    /// The UUID the template itself carries. Clones never keep it.
    #[serde(default)]
    pub fs_uuid: Option<Uuid>,
    pub state: TemplateState,
    /// The scratch volume that gets formatted, while it still exists.
    ///
    /// [`seal`] drops it: once the snapshot is taken, the sealed volume holds
    /// every extent (copy-on-write refcounts, so the snapshot does not depend
    /// on its origin) and the scratch copy is pure cost. Keeping it is what
    /// left 94 `-raw` volumes against 17 templates on one node (#47).
    #[serde(default)]
    pub raw_volume_id: Option<Uuid>,
    /// The clean snapshot clones descend from. Set at seal.
    pub sealed_volume_id: Option<Uuid>,
    /// How many clones have been minted from it.
    #[serde(default)]
    pub clones: u64,
    /// Paths seeded into the filesystem before sealing, for the record. The
    /// contents live in the filesystem, not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeded: Vec<String>,
    /// One clone, minted in advance, waiting for a claim (#55).
    ///
    /// **One, not a pool.** A second only helps when two starts of the same
    /// template collide, and the nodes this runs on are memory constrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing: Option<StandingClone>,
    /// A mint is in flight for this template.
    ///
    /// Not persisted: a crash mid-mint must not leave a template that can
    /// never replenish, and the orphan sweep collects what the interrupted
    /// mint left behind.
    #[serde(skip)]
    pub minting: bool,
    /// The template this one was built `FROM`, if any.
    ///
    /// Recorded for lineage, not for reads: a snapshot owns a complete extent
    /// map of its own, so nothing about reading this template's filesystem
    /// goes through its parent. Deleting the parent is safe for the same
    /// reason — the blocks are refcounted, not borrowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
}

impl FsTemplate {
    /// The volume clones are taken from — the sealed snapshot, never the raw
    /// volume (which may still be attached and changing).
    pub fn clone_source(&self) -> Option<VolumeId> {
        self.sealed_volume_id.map(VolumeId)
    }

    /// Every volume this template owns. Deleting the template must account for
    /// all of them — a template whose entry goes away while its volumes stay is
    /// space nothing can name afterwards (#47).
    pub fn volumes(&self) -> Vec<VolumeId> {
        [
            self.sealed_volume_id,
            self.raw_volume_id,
            self.standing.as_ref().map(|s| s.volume_id),
        ]
        .into_iter()
        .flatten()
        .map(VolumeId)
        .collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "fs": self.fs.as_str(),
            "size_bytes": self.size_bytes,
            "journal": self.journal,
            "features": self.features,
            "64bit": self.sixty_four_bit,
            "metadata_csum": self.metadata_csum,
            "metadata_csum_seed": self.csum_seed,
            "label": self.label,
            "fs_uuid": self.fs_uuid,
            "state": self.state.as_str(),
            "raw_volume_id": self.raw_volume_id,
            "sealed_volume_id": self.sealed_volume_id,
            // Whether a start of this template is a lookup or a mint (#55).
            "standing": self.standing.as_ref().map(|c| serde_json::json!({
                "volume_id": c.volume_id,
                "fs_uuid": c.fs_uuid,
                "size_bytes": c.size_bytes,
                "verified": c.verified,
            })),
            "clones": self.clones,
            "seeded": self.seeded,
            // Lineage, so a consumer can ask what was built on what. Without
            // it a template's parent is knowable only to the engine, and
            // "rebuild everything that sits on this base" is not a question
            // anything outside can answer.
            "parent_id": self.parent_id,
        })
    }
}

/// Errors the template lifecycle raises. Each maps to one HTTP status, so the
/// API layer never has to guess from a string.
#[derive(Debug)]
pub enum TemplateError {
    /// The name is already taken.
    Exists(String),
    NotFound(String),
    /// Wrong state for the operation (sealing a sealed template, cloning an
    /// unsealed one, purging one with descendants).
    Conflict(String),
    /// A volume that had to be thrown away could not be, so it is still there
    /// under a name the caller chose. Carries the id, because that is the only
    /// way anyone can clean it up: a clone is created under the *caller's* name
    /// (`pvc-web-1`, not `fstemplate-…`), so a leaked one is indistinguishable
    /// by name from a live consumer volume and no sweeper can safely find it
    /// (#48).
    Leaked { volume_id: Uuid, message: String },
    /// The filesystem on the volume is not in a state a consumer can mount.
    NotSealable(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::Exists(m) => write!(f, "{m}"),
            TemplateError::NotFound(m) => write!(f, "{m}"),
            TemplateError::Conflict(m) => write!(f, "{m}"),
            TemplateError::NotSealable(m) => write!(f, "{m}"),
            TemplateError::Invalid(m) => write!(f, "{m}"),
            TemplateError::Internal(m) => write!(f, "{m}"),
            TemplateError::Leaked { volume_id, message } => {
                write!(f, "{message}; volume {volume_id} is leaked")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

type Result<T> = std::result::Result<T, TemplateError>;

/// Templates on disk, so a restart does not lose what has been formatted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateStore {
    pub version: u32,
    pub templates: Vec<FsTemplate>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for TemplateStore {
    fn default() -> Self {
        TemplateStore { version: 1, templates: Vec::new(), path: None }
    }
}

impl TemplateStore {
    /// In-memory only — nothing survives a restart. Used by tests and by a
    /// node running without `--data-dir`.
    pub fn in_memory() -> Self {
        TemplateStore::default()
    }

    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(TEMPLATES_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<TemplateStore>(&raw) {
                Ok(mut s) => {
                    s.path = Some(path);
                    s
                }
                Err(e) => {
                    // Keep the bad file: a hand-inspectable corrupt store beats
                    // one that was silently overwritten.
                    let bak = path.with_extension("json.corrupt");
                    tracing::error!(
                        "corrupt {} ({e}) — preserved as {}",
                        path.display(),
                        bak.display()
                    );
                    let _ = std::fs::rename(&path, &bak);
                    TemplateStore { version: 1, templates: Vec::new(), path: Some(path) }
                }
            },
            Err(_) => TemplateStore { version: 1, templates: Vec::new(), path: Some(path) },
        }
    }

    pub fn get(&self, id: &Uuid) -> Option<&FsTemplate> {
        self.templates.iter().find(|t| &t.id == id)
    }

    pub fn get_mut(&mut self, id: &Uuid) -> Option<&mut FsTemplate> {
        self.templates.iter_mut().find(|t| &t.id == id)
    }

    /// Look up by id, or by name for callers that only know `ext4-nojournal`.
    pub fn find(&self, key: &str) -> Option<&FsTemplate> {
        key.parse::<Uuid>()
            .ok()
            .and_then(|id| self.get(&id))
            .or_else(|| self.templates.iter().find(|t| t.name == key))
    }

    pub fn by_name(&self, name: &str) -> Option<&FsTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    pub fn insert(&mut self, t: FsTemplate) {
        self.templates.push(t);
    }

    pub fn remove(&mut self, id: &Uuid) -> Option<FsTemplate> {
        let idx = self.templates.iter().position(|t| &t.id == id)?;
        Some(self.templates.remove(idx))
    }

    pub fn persist(&self) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let bytes = match serde_json::to_vec_pretty(self) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("failed to serialize fstemplates: {e}");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes)
            .and_then(|_| std::fs::rename(&tmp, path))
            .is_err()
        {
            tracing::warn!("failed to persist fstemplates to {}", path.display());
        }
    }
}

/// Throw away a volume the caller must not be handed, and say so truthfully.
///
/// The whole point is the return value. Discarding used to be `let _ = …`, so
/// when the delete failed the volume survived and the caller was told, in as
/// many words, that it had been discarded (#48). That is not a hypothetical
/// failure: `delete_volume` releases slots best-effort and still reports the
/// ones it could not release (#37).
///
/// Retried once, because most of those failure modes are transient-ish and a
/// second attempt costs nothing next to a volume nobody can ever find again.
async fn discard(vm: &VmLock, id: VolumeId) -> std::result::Result<(), String> {
    let first = match vm.lock().await.delete_volume(id).await {
        Ok(()) => return Ok(()),
        Err(e) => e.to_string(),
    };
    match vm.lock().await.delete_volume(id).await {
        Ok(()) => {
            tracing::warn!("discarding volume {id} succeeded on the second attempt (first: {first})");
            Ok(())
        }
        // A volume that is already gone is discarded, whatever the first error
        // said — this is the retry finding the state it wanted.
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => Ok(()),
        Err(second) => Err(format!("{first} (retry: {second})")),
    }
}

/// Discard a volume, folding a failed discard into the error the caller gets.
///
/// `context` says what went wrong first; the caller is never told the volume
/// was thrown away unless it actually was.
async fn discard_or_report(vm: &VmLock, id: VolumeId, context: String) -> TemplateError {
    match discard(vm, id).await {
        Ok(()) => TemplateError::Internal(format!("{context} — the volume was discarded")),
        Err(why) => {
            tracing::error!(
                volume = %id,
                "{context} — and it could NOT be discarded: {why}. The volume is leaked and \
                 carries a caller-chosen name, so nothing can find it automatically."
            );
            TemplateError::Leaked {
                volume_id: id.0,
                message: format!("{context} — and it could NOT be discarded: {why}"),
            }
        }
    }
}

/// Undo a half-built template: forget it, and delete every volume it made.
///
/// Every failure inside [`create`] goes through here. A create that does not
/// produce a usable template must leave nothing behind, because the caller's
/// next move is to try the same name again — and each attempt that leaves its
/// volumes standing costs the node two more of them for good (#47).
async fn rollback(vm: &VmLock, store: &StoreLock, id: &Uuid) {
    let volumes = {
        let mut s = store.lock().await;
        let volumes = s.get(id).map(|t| t.volumes()).unwrap_or_default();
        s.remove(id);
        s.persist();
        volumes
    };
    let mut m = vm.lock().await;
    for vol in volumes {
        if let Err(e) = m.delete_volume(vol).await {
            tracing::warn!("rolling back template {id}: volume {vol} not deleted: {e}");
        }
    }
}

/// Create a template: a volume, a filesystem on it, and a sealed snapshot.
///
/// With `format_in_core` the whole lifecycle runs here and the returned
/// template is `Ready`. Without it, only the raw volume is created and the
/// caller must format it (over an export) and then call [`seal`].
pub async fn create(
    vm: &VmLock,
    store: &StoreLock,
    spec: &TemplateSpec,
) -> Result<FsTemplate> {
    if spec.name.trim().is_empty() {
        return Err(TemplateError::Invalid("name must not be empty".to_string()));
    }
    if spec.size_bytes == 0 {
        return Err(TemplateError::Invalid("size_bytes must be > 0".to_string()));
    }
    if store.lock().await.by_name(&spec.name).is_some() {
        return Err(TemplateError::Exists(format!("fstemplate {} already exists", spec.name)));
    }

    // `FROM` a parent, or from nothing.
    //
    // With a parent the raw volume is a copy-on-write clone of the parent's
    // sealed snapshot, which means it is already a filesystem: there is
    // nothing to mkfs, and the blocks the parent contributed are shared
    // rather than written again. Without one it is a blank volume that
    // something still has to format.
    let parent = match spec.parent.as_deref() {
        None => None,
        Some(key) => {
            let p = store
                .lock()
                .await
                .find(key)
                .cloned()
                .ok_or_else(|| TemplateError::NotFound(format!("parent fstemplate {key} not found")))?;
            if p.state != TemplateState::Ready {
                return Err(TemplateError::Conflict(format!(
                    "parent fstemplate {} is {} — seal it before building on it",
                    p.name,
                    p.state.as_str()
                )));
            }
            if spec.format_in_core {
                // Formatting over a parent would erase the very thing the
                // parent was for. Refuse rather than silently pick one.
                return Err(TemplateError::Invalid(
                    "format_in_core cannot be combined with parent: the parent's filesystem                      is the filesystem"
                        .to_string(),
                ));
            }
            if spec.size_bytes < p.size_bytes {
                return Err(TemplateError::Invalid(format!(
                    "size_bytes {} is smaller than parent {} ({}); a filesystem cannot be                      shrunk into a smaller volume",
                    spec.size_bytes, p.name, p.size_bytes
                )));
            }
            Some(p)
        }
    };

    let raw = match &parent {
        None => vm
            .lock()
            .await
            .create_volume_any(&format!("fstemplate-{}-raw", spec.name), spec.size_bytes)
            .await
            .map_err(|e| TemplateError::Internal(format!("creating template volume: {e}")))?,
        Some(p) => {
            let source = p.clone_source().ok_or_else(|| {
                TemplateError::Internal(format!("parent {} has no sealed snapshot", p.name))
            })?;
            let mut m = vm.lock().await;
            let id = m
                .create_snapshot(source, &format!("fstemplate-{}-raw", spec.name))
                .await
                .map_err(|e| TemplateError::Internal(format!("cloning parent {}: {e}", p.name)))?;
            if spec.size_bytes > p.size_bytes {
                m.resize_volume(id, spec.size_bytes).await.map_err(|e| {
                    TemplateError::Internal(format!("growing template volume: {e}"))
                })?;
            }
            drop(m);

            // Give it an identity of its own, now, before anything is written
            // to it and long before it is sealed.
            //
            // The clone arrives carrying the parent's filesystem UUID, so two
            // templates built on one parent would otherwise both claim it —
            // and with `metadata_csum` the UUID is the seed for every checksum
            // in the filesystem, so stamping it later means rewriting all of
            // them. `metadata_csum_seed` is what makes doing it here a single
            // superblock write instead.
            let dev = vm
                .lock()
                .await
                .get_volume(&id)
                .ok_or_else(|| TemplateError::Internal("cloned volume vanished".to_string()))?;
            let fresh = Uuid::new_v4();
            if let Err(e) = ext4::stamp_uuid(&dev, fresh, true).await {
                drop(dev);
                let _ = discard(vm, id).await;
                return Err(TemplateError::Internal(format!(
                    "stamping a fresh uuid on the clone of {}: {e}",
                    p.name
                )));
            }
            drop(dev);
            id
        }
    };

    // A template built on a parent *is* the parent's filesystem, so its shape
    // is inherited rather than restated: asking for ext2-with-no-journal on
    // top of an ext4 parent describes a filesystem that does not exist.
    let mut template = FsTemplate {
        standing: None,
        minting: false,
        id: Uuid::new_v4(),
        name: spec.name.clone(),
        fs: parent.as_ref().map(|p| p.fs).unwrap_or(spec.fs),
        size_bytes: spec.size_bytes,
        journal: match &parent {
            Some(p) => p.journal,
            None => spec.journal.unwrap_or(spec.fs != FsKind::Ext2),
        },
        label: match &parent {
            Some(p) if spec.label.is_empty() => p.label.clone(),
            _ => spec.label.clone(),
        },
        features: match &parent {
            Some(p) => p.features.clone(),
            None => spec.features.clone(),
        },
        sixty_four_bit: parent.as_ref().map(|p| p.sixty_four_bit).unwrap_or(false),
        metadata_csum: parent.as_ref().map(|p| p.metadata_csum).unwrap_or(false),
        csum_seed: parent.as_ref().map(|p| p.csum_seed).unwrap_or(false),
        // The clone carries the parent's filesystem UUID until something
        // stamps it. Two templates off one parent must not keep sharing it,
        // so `seal` gives this one an identity of its own.
        // Set below for a parent-built template, which was stamped at
        // creation; `seal` re-reads it off the filesystem either way.
        fs_uuid: None,
        state: match &parent {
            Some(_) => TemplateState::AwaitingSeed,
            None => TemplateState::AwaitingFormat,
        },
        raw_volume_id: Some(raw.0),
        sealed_volume_id: None,
        clones: 0,
        seeded: spec.seed.iter().map(|f| f.path.clone()).collect(),
        parent_id: parent.as_ref().map(|p| p.id),
    };

    // Claim the name before the long part. Two creates racing on one name
    // must not both format; the loser gives its volume straight back.
    {
        let mut s = store.lock().await;
        if s.by_name(&spec.name).is_some() {
            drop(s);
            // The loser gives its volume back — and if it cannot, that is the
            // error the caller gets, rather than a tidy "already exists" over
            // a volume nobody will ever look for.
            if let Err(why) = discard(vm, raw).await {
                tracing::error!(volume = %raw, "lost a race for the name {} and could not give the volume back: {why}", spec.name);
                return Err(TemplateError::Leaked {
                    volume_id: raw.0,
                    message: format!(
                        "fstemplate {} already exists, and this attempt's volume could NOT be \
                         discarded: {why}",
                        spec.name
                    ),
                });
            }
            return Err(TemplateError::Exists(format!(
                "fstemplate {} already exists",
                spec.name
            )));
        }
        s.insert(template.clone());
        s.persist();
    }

    if !spec.format_in_core {
        return Ok(template);
    }

    let dev = vm
        .lock()
        .await
        .get_volume(&raw)
        .ok_or_else(|| TemplateError::Internal("new template volume vanished".to_string()))?;

    let params = ext4::Ext4Params {
        profile: spec.fs.profile(),
        label: spec.label.clone(),
        uuid: Uuid::new_v4(),
        journal: spec.journal,
        features: spec.features.clone(),
        // The volume was created moments ago and has never been written, so
        // every unwritten block already reads back as zeros. This is what keeps
        // a template's allocation in kilobytes rather than the tens of
        // megabytes its inode tables describe.
        assume_blank: true,
        ..Default::default()
    };

    // **No lock is held across the format.** The formatter fans out across
    // block groups and thin volumes serialise only where a mapping changes, so
    // several templates format concurrently instead of queueing behind one
    // volume-manager mutex — which is the whole reason provisioning many at
    // once is not N times the cost of one.
    let format = ext4::format(&dev, &params).await;
    drop(dev);

    if let Err(e) = format {
        // Nothing half-formatted survives as a template.
        rollback(vm, store, &template.id).await;
        return Err(TemplateError::Internal(format!("formatting {}: {e}", spec.name)));
    }

    // Contents go in before the seal, so a clone inherits them without ever
    // writing them again. Also unlocked: this is I/O against one volume.
    if !spec.seed.is_empty() {
        let dev = vm
            .lock()
            .await
            .get_volume(&raw)
            .ok_or_else(|| TemplateError::Internal("template volume vanished".to_string()))?;
        let seeded = files::write_files(&dev, &spec.seed).await;
        drop(dev);
        if let Err(e) = seeded {
            rollback(vm, store, &template.id).await;
            return Err(TemplateError::Internal(format!(
                "seeding {}: {e}",
                spec.name
            )));
        }
    }

    template.fs_uuid = Some(params.uuid);
    {
        let mut s = store.lock().await;
        if let Some(t) = s.get_mut(&template.id) {
            t.fs_uuid = template.fs_uuid;
        }
        s.persist();
    }

    // Seal checks the filesystem, which is also the check on what was just
    // seeded into it. A template that will not seal is not a template: roll it
    // back rather than leave a formatted volume and a store entry the caller
    // cannot use and will not name again (#47).
    match seal(vm, store, &template.id, false).await {
        Ok(t) => Ok(t),
        Err(e) => {
            rollback(vm, store, &template.id).await;
            Err(e)
        }
    }
}

/// Seal a formatted template: verify the superblock, snapshot it, mark ready.
///
/// The verification is the point. A sealed template is the ancestor of every
/// clone that follows, so a superblock that says "errors recorded" or
/// "recovery pending" would be inherited by all of them — and a consumer that
/// cannot replay a journal (RouterOS) mounts every one of them read-only.
/// `force` skips the check for an operator who knows better; nothing else does.
pub async fn seal(vm: &VmLock, store: &StoreLock, id: &Uuid, force: bool) -> Result<FsTemplate> {
    let template = store
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| TemplateError::NotFound(format!("fstemplate {id} not found")))?;
    if template.state == TemplateState::Ready {
        return Err(TemplateError::Conflict(format!(
            "fstemplate {} is already sealed",
            template.name
        )));
    }

    let raw = template
        .raw_volume_id
        .map(VolumeId)
        .ok_or_else(|| {
            TemplateError::Conflict(format!(
                "fstemplate {} has no volume to seal",
                template.name
            ))
        })?;
    let dev = vm
        .lock()
        .await
        .get_volume(&raw)
        .ok_or_else(|| TemplateError::NotFound(format!("template volume {raw} is gone")))?;

    // Checking is I/O over the volume and nothing else, so it holds no lock:
    // an fsck of one template must not stall every other volume operation.
    let layout = match ext4::read_layout(&dev).await {
        Ok(l) => Some(l),
        Err(e) if force => {
            tracing::warn!("sealing {} with no readable filesystem: {e}", template.name);
            None
        }
        Err(e) => {
            return Err(TemplateError::NotSealable(format!(
                "no usable filesystem on the template volume: {e}"
            )))
        }
    };

    if layout.is_some() {
        match ext4::seal_blockers(&dev).await {
            Ok(blockers) if blockers.is_empty() => {}
            Ok(blockers) => {
                let why = blockers.iter().map(|b| b.to_string()).collect::<Vec<_>>().join("; ");
                if !force {
                    return Err(TemplateError::NotSealable(format!(
                        "refusing to seal {}: {why}",
                        template.name
                    )));
                }
                tracing::warn!("sealing {} despite: {why}", template.name);
            }
            Err(e) if force => tracing::warn!("sealing {} unchecked: {e}", template.name),
            Err(e) => {
                return Err(TemplateError::NotSealable(format!(
                    "checking the template filesystem: {e}"
                )))
            }
        }
    }
    drop(dev);

    let sealed = vm
        .lock()
        .await
        .create_snapshot(raw, &format!("fstemplate-{}-{}", template.fs, template.name))
        .await
        .map_err(|e| TemplateError::Internal(format!("sealing snapshot: {e}")))?;

    let mut s = store.lock().await;
    let t = s
        .get_mut(id)
        .ok_or_else(|| TemplateError::NotFound(format!("fstemplate {id} not found")))?;
    t.sealed_volume_id = Some(sealed.0);
    t.raw_volume_id = None;
    t.state = TemplateState::Ready;
    if let Some(l) = &layout {
        t.fs_uuid = Some(l.uuid);
        t.journal = l.has_journal;
        t.sixty_four_bit = l.sixty_four_bit;
        t.metadata_csum = l.metadata_csum;
        t.csum_seed = l.csum_seed;
        if t.label.is_empty() {
            t.label = l.label.clone();
        }
    }
    let out = t.clone();
    s.persist();
    drop(s);

    // The snapshot owns its extents outright — copy-on-write refcounts mean it
    // does not depend on the volume it was taken from — so the scratch volume
    // is now pure cost. Dropping it here is what keeps one template to one
    // volume instead of two (#47).
    if let Err(e) = vm.lock().await.delete_volume(raw).await {
        tracing::warn!(
            "fstemplate {}: scratch volume {raw} not deleted after sealing: {e}",
            out.name
        );
    }

    tracing::info!("fstemplate {} sealed as volume {}", out.name, sealed.0);
    Ok(out)
}

/// What a clone should look like.
#[derive(Debug, Clone, Default)]
pub struct CloneSpec {
    pub name: String,
    /// Grow the clone to this size. Never shrinks — the filesystem inside
    /// would not survive it.
    pub size_bytes: Option<u64>,
    /// Give the clone its own filesystem UUID. On by default: without it every
    /// clone of a template presents the same identity, which breaks
    /// mount-by-UUID and the blkid cache the moment two are on one host.
    pub stamp_uuid: bool,
    /// Also rewrite the backup superblocks. Costs one copy-on-write extent per
    /// backup group, so it is off by default — mount, `blkid` and `mount -U`
    /// all read the primary.
    pub stamp_backups: bool,
    /// Give the clone its own label.
    pub label: Option<String>,
    /// This clone is being minted to stand by, not handed to anyone yet.
    ///
    /// It is not counted against the template — `clones` means clones that went
    /// somewhere, and a pre-minted one that nobody has claimed has not — and it
    /// does not itself trigger a top-up, which is what would otherwise make
    /// minting recursive.
    pub standby: bool,
    /// Check the clone before handing it out. On by default: a clone that
    /// fails fsck is one that fails inside a container later, and the check is
    /// a read-only pass over metadata the stamp just touched. Turn it off for
    /// a provisioning path that has measured the cost and would rather not pay
    /// it per clone.
    pub verify: bool,
}

impl CloneSpec {
    pub fn new(name: impl Into<String>) -> Self {
        CloneSpec {
            name: name.into(),
            stamp_uuid: true,
            verify: true,
            ..Default::default()
        }
    }
}

/// What a clone came out as.
#[derive(Debug, Clone)]
pub struct CloneResult {
    pub volume_id: VolumeId,
    pub template_id: Uuid,
    pub fs_uuid: Option<Uuid>,
    pub size_bytes: u64,
    /// Whether this clone was checked before being handed out.
    pub verified: bool,
    /// Whether it came from the standing clone or was minted on the spot.
    ///
    /// Reported rather than inferred: a start that waits is correct, but a
    /// slow one should be explainable instead of mysterious (#55).
    pub from_standby: bool,
}

/// Mint a copy-on-write clone of a sealed template.
///
/// This is the provisioning path: a snapshot plus, at most, one 16-byte patch.
/// No mkfs, no attach, no round trip.
pub async fn clone_template(
    vm: &Arc<VmLock>,
    store: &Arc<StoreLock>,
    key: &str,
    spec: &CloneSpec,
) -> Result<CloneResult> {
    let template = store
        .lock()
        .await
        .find(key)
        .cloned()
        .ok_or_else(|| TemplateError::NotFound(format!("fstemplate {key} not found")))?;
    if template.state != TemplateState::Ready {
        return Err(TemplateError::Conflict(format!(
            "fstemplate {} is {} — seal it before cloning",
            template.name,
            template.state.as_str()
        )));
    }
    let source = template.clone_source().ok_or_else(|| {
        TemplateError::Internal(format!("fstemplate {} has no sealed snapshot", template.name))
    })?;
    if spec.name.trim().is_empty() {
        return Err(TemplateError::Invalid("clone name must not be empty".to_string()));
    }

    let (id, size) = {
        let mut m = vm.lock().await;
        let id = m
            .create_snapshot(source, &spec.name)
            .await
            .map_err(|e| TemplateError::Internal(format!("cloning template: {e}")))?;

        // A clone inherits the template's size; grow it if the caller asked
        // for more. Shrinking is never attempted — the filesystem inside would
        // not survive it.
        let mut size =
            m.get_volume(&id).map(|d| d.capacity_bytes()).unwrap_or(template.size_bytes);
        if let Some(want) = spec.size_bytes {
            if want > size {
                m.resize_volume(id, want)
                    .await
                    .map_err(|e| TemplateError::Internal(format!("growing clone: {e}")))?;
                size = want;
            }
        }
        (id, size)
    };

    // Fresh identity, with no lock held: this is a superblock write against
    // one volume, and thousands of clones may be minted at once.
    let mut fs_uuid = template.fs_uuid;
    if spec.stamp_uuid || spec.label.is_some() {
        let dev = vm
            .lock()
            .await
            .get_volume(&id)
            .ok_or_else(|| TemplateError::Internal("clone volume vanished".to_string()))?;
        if spec.stamp_uuid {
            let fresh = Uuid::new_v4();
            match ext4::stamp_uuid(&dev, fresh, spec.stamp_backups).await {
                Ok(_) => fs_uuid = Some(fresh),
                Err(e) => {
                    // Roll the clone back: handing out a volume that silently
                    // shares its template's identity is the bug this exists to
                    // prevent (stormblockmk#12).
                    drop(dev);
                    return Err(discard_or_report(
                        vm,
                        id,
                        format!("stamping a fresh filesystem UUID on clone {}: {e}", spec.name),
                    )
                    .await);
                }
            }
        }
        if let Some(label) = &spec.label {
            if let Err(e) = ext4::stamp_label(&dev, label).await {
                tracing::warn!("clone {}: could not set label: {e}", spec.name);
            }
        }
    }

    // Verify what is about to be handed out. A clone is a snapshot plus a
    // superblock write, and both are exactly the kind of thing that fails
    // quietly: the volume still reads, and the failure surfaces as a mount
    // problem on someone else's machine.
    if spec.verify {
        let dev = vm
            .lock()
            .await
            .get_volume(&id)
            .ok_or_else(|| TemplateError::Internal("clone volume vanished".to_string()))?;
        let verdict = ext4::check(&dev).await;
        drop(dev);
        let problem = match verdict {
            Ok(report) if report.is_clean() => None,
            Ok(report) => Some(
                report
                    .problems
                    .iter()
                    .map(|p| format!("{}: {}", p.code, p.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = problem {
            return Err(discard_or_report(
                vm,
                id,
                format!("clone {} did not check out ({why})", spec.name),
            )
            .await);
        }
    }

    {
        let mut s = store.lock().await;
        if let Some(t) = s.get_mut(&template.id) {
            // A standing clone is counted when it is claimed, not when it is
            // minted: `clones` answers "how many went somewhere".
            if !spec.standby {
                t.clones += 1;
            }
        }
        s.persist();
    }

    // A take is a take, whichever door it came through: if this template has
    // no clone waiting now, mint one behind the caller (#55). Cheap, because it
    // only fires when the field is empty — and it is what keeps the *next*
    // start fast rather than only the next claim.
    if !spec.standby
        && store.lock().await.get(&template.id).is_some_and(|t| t.standing.is_none())
    {
        replenish(vm, store, template.id);
    }

    Ok(CloneResult {
        volume_id: id,
        template_id: template.id,
        fs_uuid,
        size_bytes: size,
        verified: spec.verify,
        from_standby: false,
    })
}

// ------------------------------------------------------------- standing by

/// The name a standing clone is minted under.
///
/// There is no rename, so this is the name it keeps after a claim; a claimer
/// addresses it by volume id, which is what the attach path uses anyway.
fn standby_name(template: &FsTemplate) -> String {
    format!("standby-{}-{}", template.name, &Uuid::new_v4().simple().to_string()[..8])
}

/// What a claim wants that a pre-minted clone might not already be.
#[derive(Debug, Clone, Default)]
pub struct ClaimSpec {
    /// Grow the clone to this size. Never shrinks.
    pub size_bytes: Option<u64>,
    /// Give it a label. Costs one superblock write, so the fast path is the
    /// one that does not ask.
    pub label: Option<String>,
}

/// Make sure a sealed template has a clone standing by. Idempotent.
///
/// Returns the standing clone — the existing one if there already was one.
/// Minting is a snapshot, a stamp and a check, none of which depends on when
/// the start happens, so all of it belongs before the start rather than in it.
pub async fn ensure_standing(
    vm: &Arc<VmLock>,
    store: &Arc<StoreLock>,
    key: &str,
) -> Result<Option<StandingClone>> {
    // Claim the right to mint under the lock, so two callers arriving together
    // produce one clone rather than two — the second would be waste that
    // nothing ever collects, since only the template's field is a reference.
    let template = {
        let mut s = store.lock().await;
        let Some(t) = s.find(key).cloned() else {
            return Err(TemplateError::NotFound(format!("fstemplate {key} not found")));
        };
        if t.state != TemplateState::Ready {
            return Ok(None);
        }
        if let Some(standing) = &t.standing {
            return Ok(Some(standing.clone()));
        }
        if t.minting {
            return Ok(None);
        }
        if let Some(m) = s.get_mut(&t.id) {
            m.minting = true;
        }
        t
    };

    let mut spec = CloneSpec::new(standby_name(&template));
    spec.standby = true;
    let minted = clone_template(vm, store, &template.id.to_string(), &spec).await;

    let mut s = store.lock().await;
    if let Some(m) = s.get_mut(&template.id) {
        m.minting = false;
    }
    match minted {
        Ok(c) => {
            let standing = StandingClone {
                volume_id: c.volume_id.0,
                fs_uuid: c.fs_uuid,
                size_bytes: c.size_bytes,
                verified: c.verified,
            };
            // A claim may have arrived while this was minting and left with the
            // previous standing clone; either way the field is empty now, and
            // if it is not, this one would be the second — drop it rather than
            // strand it.
            match s.get_mut(&template.id) {
                Some(t) if t.standing.is_none() => {
                    t.standing = Some(standing.clone());
                    s.persist();
                    tracing::debug!(
                        "fstemplate {}: clone {} standing by",
                        template.name,
                        standing.volume_id
                    );
                    Ok(Some(standing))
                }
                _ => {
                    drop(s);
                    let mut m = vm.lock().await;
                    let _ = m.delete_volume(c.volume_id).await;
                    Ok(None)
                }
            }
        }
        Err(e) => {
            s.persist();
            Err(e)
        }
    }
}

/// What a template's fast path looks like right now.
#[derive(Debug, Clone, Serialize)]
pub struct StandingStatus {
    pub template_id: Uuid,
    pub name: String,
    pub state: &'static str,
    /// The clone waiting, if there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standing: Option<Uuid>,
    /// A mint is in flight, so this will resolve itself without help.
    pub minting: bool,
    /// Sealed, nothing waiting, nothing in flight: the next start of this
    /// template pays for a clone.
    pub needs_clone: bool,
}

/// Which templates would make a start wait, and which would not.
///
/// A *check*, separate from the fix: a supervisor that restarts the engine, or
/// watches it, should be able to ask whether the invariant holds without
/// causing work as a side effect of asking. [`ensure_standing_all`] is the fix.
pub fn standing_report(store: &TemplateStore) -> Vec<StandingStatus> {
    store
        .templates
        .iter()
        .map(|t| StandingStatus {
            template_id: t.id,
            name: t.name.clone(),
            state: t.state.as_str(),
            standing: t.standing.as_ref().map(|c| c.volume_id),
            minting: t.minting,
            // An unsealed template has no snapshot to clone from, so it is not
            // *missing* anything — it is simply not ready to have one.
            needs_clone: t.state == TemplateState::Ready
                && t.standing.is_none()
                && !t.minting,
        })
        .collect()
}

/// Just the ones that would make a start wait.
pub async fn standing_needed(store: &StoreLock) -> Vec<StandingStatus> {
    standing_report(&*store.lock().await)
        .into_iter()
        .filter(|s| s.needs_clone)
        .collect()
}

/// Give every sealed template a clone standing by.
///
/// Run at startup and after a seal. Failures are logged and skipped: a
/// template without a standing clone still works, it is only slower.
pub async fn ensure_standing_all(vm: &Arc<VmLock>, store: &Arc<StoreLock>) -> usize {
    let ready: Vec<(Uuid, String)> = {
        let s = store.lock().await;
        s.templates
            .iter()
            .filter(|t| t.state == TemplateState::Ready && t.standing.is_none())
            .map(|t| (t.id, t.name.clone()))
            .collect()
    };
    let mut minted = 0;
    for (id, name) in ready {
        match ensure_standing(vm, store, &id.to_string()).await {
            Ok(Some(_)) => minted += 1,
            Ok(None) => {}
            Err(e) => tracing::warn!("fstemplate {name}: could not mint a standing clone: {e}"),
        }
    }
    minted
}

/// Take the standing clone, and mint its replacement behind the caller.
///
/// The fast path is a lookup. When nothing is standing — the first claim, or
/// two starts colliding — this mints inline rather than refusing: a start that
/// waits beats a start that does not happen. `from_standby` says which it was.
pub async fn claim(
    vm: &Arc<VmLock>,
    store: &Arc<StoreLock>,
    key: &str,
    spec: &ClaimSpec,
) -> Result<CloneResult> {
    // Take it under the lock. This is the whole of "a claimed clone is never
    // handed out twice": two claims arriving together cannot both `take`, and
    // the loser mints its own.
    let (template, taken) = {
        let mut s = store.lock().await;
        let Some(t) = s.find(key).cloned() else {
            return Err(TemplateError::NotFound(format!("fstemplate {key} not found")));
        };
        let taken = s.get_mut(&t.id).and_then(|m| m.standing.take());
        if taken.is_some() {
            if let Some(m) = s.get_mut(&t.id) {
                m.clones += 1;
            }
            s.persist();
        }
        (t, taken)
    };

    if template.state != TemplateState::Ready {
        return Err(TemplateError::Conflict(format!(
            "fstemplate {} is {} — seal it before claiming",
            template.name,
            template.state.as_str()
        )));
    }

    let Some(standing) = taken else {
        tracing::info!(
            "fstemplate {}: nothing standing by, minting inline",
            template.name
        );
        let mut clone_spec = CloneSpec::new(standby_name(&template));
        clone_spec.size_bytes = spec.size_bytes;
        clone_spec.label = spec.label.clone();
        let result = clone_template(vm, store, key, &clone_spec).await?;
        replenish(vm, store, template.id);
        return Ok(result);
    };

    let mut size = standing.size_bytes;
    let volume = VolumeId(standing.volume_id);

    // Only what the caller asked for beyond what was pre-minted. A claim that
    // asks for nothing writes nothing.
    if let Some(want) = spec.size_bytes {
        if want > size {
            vm.lock()
                .await
                .resize_volume(volume, want)
                .await
                .map_err(|e| TemplateError::Internal(format!("growing claimed clone: {e}")))?;
            size = want;
        }
    }
    if let Some(label) = &spec.label {
        if let Some(dev) = vm.lock().await.get_volume(&volume) {
            if let Err(e) = ext4::stamp_label(&dev, label).await {
                tracing::warn!("claimed clone {}: could not set label: {e}", volume);
            }
        }
    }

    replenish(vm, store, template.id);

    Ok(CloneResult {
        volume_id: volume,
        template_id: template.id,
        fs_uuid: standing.fs_uuid,
        size_bytes: size,
        verified: standing.verified,
        from_standby: true,
    })
}

/// Mint the replacement behind the caller — spawned, never awaited, because a
/// claim's whole purpose is to not wait for one of these.
fn replenish(vm: &Arc<VmLock>, store: &Arc<StoreLock>, template_id: Uuid) {
    let vm = vm.clone();
    let store = store.clone();
    tokio::spawn(async move {
        if let Err(e) = ensure_standing(&vm, &store, &template_id.to_string()).await {
            tracing::warn!("fstemplate {template_id}: replacement clone not minted: {e}");
        }
    });
}

/// Remove a template, and by default the volumes it owns.
///
/// Clones are independent volumes: a snapshot holds its own refcounted
/// reference to every extent, so deleting the template it descends from takes
/// nothing away from it. That is why `force` is no longer required to purge a
/// template with descendants — it only silences the warning. Pass
/// `purge = false` to keep the volumes, which is how a node ends up with
/// template debris nothing can name (#47), so it is not the default.
pub async fn delete(
    vm: &VmLock,
    store: &StoreLock,
    id: &Uuid,
    purge: bool,
    force: bool,
) -> Result<Vec<Uuid>> {
    let template = store
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| TemplateError::NotFound(format!("fstemplate {id} not found")))?;
    if purge && template.clones > 0 && !force {
        tracing::warn!(
            "purging fstemplate {} with {} clone(s) descending from it — they keep their own \
             refcounted extents and are unaffected",
            template.name,
            template.clones
        );
    }

    let mut purged = Vec::new();
    if purge {
        let mut m = vm.lock().await;
        for vol in template.volumes() {
            match m.delete_volume(vol).await {
                Ok(()) => purged.push(vol.0),
                Err(e) => tracing::warn!("purging template volume {vol}: {e}"),
            }
        }
    }

    let mut s = store.lock().await;
    s.remove(id);
    s.persist();
    Ok(purged)
}

/// A volume that looks like it belonged to a template, and does not.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanVolume {
    pub volume_id: Uuid,
    pub name: String,
    pub size_bytes: u64,
    pub allocated_bytes: u64,
}

/// The prefix every volume this module creates carries: the scratch volume is
/// `fstemplate-{name}-raw`, the sealed snapshot `fstemplate-{fs}-{name}`.
/// Clones are named by whoever asked for them, so they never match.
const VOLUME_PREFIX: &str = "fstemplate-";

/// Volumes named like a template's, which no template in the store claims and
/// nothing is currently serving.
///
/// This is the reconciliation an instance needs *after* the fact. Before the
/// fixes above, a `DELETE` without `purge` removed the store entry and left
/// both volumes standing; nothing afterwards could tell those apart from live
/// ones by name (#47). Comparing against the store is what tells them apart.
///
/// `in_use` is a required argument rather than an optional guard, because the
/// consequence of forgetting it is deleting a volume something has attached
/// (#48). This module cannot compute it — export tables live in the management
/// layer — so the caller must, and being unable to omit it is the point.
pub async fn orphans(
    vm: &VmLock,
    store: &StoreLock,
    in_use: &std::collections::HashSet<Uuid>,
) -> Vec<OrphanVolume> {
    let claimed: std::collections::HashSet<Uuid> = {
        let s = store.lock().await;
        s.templates.iter().flat_map(|t| t.volumes()).map(|v| v.0).collect()
    };
    let volumes = vm.lock().await.list_volumes().await;
    volumes
        .into_iter()
        .filter(|(id, name, _, _)| {
            name.starts_with(VOLUME_PREFIX) && !claimed.contains(&id.0) && !in_use.contains(&id.0)
        })
        .map(|(id, name, size, allocated)| OrphanVolume {
            volume_id: id.0,
            name,
            size_bytes: size,
            allocated_bytes: allocated,
        })
        .collect()
}

/// Delete every volume [`orphans`] reports, and say what went.
///
/// A volume that will not delete is reported at `error` and left alone rather
/// than counted as reclaimed — the caller is told what actually went.
pub async fn reclaim_orphans(
    vm: &VmLock,
    store: &StoreLock,
    in_use: &std::collections::HashSet<Uuid>,
) -> Vec<OrphanVolume> {
    let found = orphans(vm, store, in_use).await;
    let mut reclaimed = Vec::with_capacity(found.len());
    for orphan in found {
        match discard(vm, VolumeId(orphan.volume_id)).await {
            Ok(()) => reclaimed.push(orphan),
            Err(why) => tracing::error!(
                "reclaiming orphaned template volume {} ({}) failed: {why} — it is still there",
                orphan.volume_id,
                orphan.name
            ),
        }
    }
    reclaimed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::raid::RaidArrayId;

    /// A node with one slab, and the two locks the lifecycle takes.
    async fn node(size: u64) -> (Arc<VmLock>, Arc<StoreLock>, String) {
        node_with_slots(size, 1024 * 1024).await
    }

    /// A node whose slab slots are `slot_size` bytes. The engine sizes slots by
    /// device and a real deployment gets 4 MiB, so the default 1 MiB here is
    /// the smaller, gentler case — anything that only goes wrong on a
    /// copy-on-write of a full-size slot needs this (#46).
    async fn node_with_slots(size: u64, slot_size: u64) -> (Arc<VmLock>, Arc<StoreLock>, String) {
        let dir = std::env::temp_dir().join("stormblock-fstemplate-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.slab", Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&p, size).await.unwrap();
        let mut vm = VolumeManager::new(slot_size);
        vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
        // Arc'd because the paths under test spawn: a take mints the
        // replacement behind the caller, and a task cannot borrow.
        (
            Arc::new(tokio::sync::Mutex::new(vm)),
            Arc::new(tokio::sync::Mutex::new(TemplateStore::in_memory())),
            p,
        )
    }

    async fn volume(vm: &VmLock, id: VolumeId) -> Arc<dyn BlockDevice> {
        vm.lock().await.get_volume(&id).expect("volume exists")
    }

    /// `FROM` a sealed template: the child is that filesystem, not a new one.
    ///
    /// This is the layering a container build does, expressed as volumes. The
    /// point is what it costs — the parent's blocks are shared through the
    /// extent map's refcounts, so a runtime five images have in common is
    /// stored once — and the property that makes it safe: a snapshot owns a
    /// complete map of its own, so nothing reads *through* the parent.
    #[tokio::test]
    async fn a_template_can_be_built_from_another() {
        let (vm, store, _path) = node(2 * 1024 * 1024 * 1024).await;

        let base = create(
            &vm,
            &store,
            &TemplateSpec {
                label: "base".to_string(),
                seed: vec![SeedFile {
                    path: "/runtime".to_string(),
                    contents: vec![0xab; 512 * 1024],
                }],
                ..TemplateSpec::new("base-runtime", 64 * 1024 * 1024)
            },
        )
        .await
        .unwrap();
        assert_eq!(base.state, TemplateState::Ready);

        let child = create(
            &vm,
            &store,
            &TemplateSpec::new("app", 64 * 1024 * 1024).from_parent("base-runtime"),
        )
        .await
        .unwrap();

        // Already a filesystem: there is nothing to format, so it is waiting
        // for content rather than for a mkfs.
        assert_eq!(child.state, TemplateState::AwaitingSeed);
        assert_eq!(child.parent_id, Some(base.id));
        assert_eq!(child.fs, base.fs, "the child is the parent's filesystem");
        assert_eq!(child.journal, base.journal);

        // The parent's contents came with it.
        let raw = volume(&vm, VolumeId(child.raw_volume_id.unwrap())).await;
        let got = crate::fs::files::read_file(&raw, "/runtime")
            .await
            .expect("the parent's file is present in the child");
        assert_eq!(got.len(), 512 * 1024);

        // …but not the parent's identity. Two children of one parent must not
        // both claim its filesystem UUID, and with metadata_csum that UUID
        // seeds every checksum, so it is stamped at creation rather than left
        // to be fixed up later.
        let layout = ext4::read_layout(&raw).await.unwrap();
        assert_ne!(
            Some(layout.uuid),
            base.fs_uuid,
            "the child kept its parent's filesystem uuid"
        );
        drop(raw);

        // And it seals like any other template.
        let sealed = seal(&vm, &store, &child.id, false).await.unwrap();
        assert_eq!(sealed.state, TemplateState::Ready);
        assert!(sealed.clone_source().is_some());
    }

    /// The chain a system is actually built as: scratch → runtime → app →
    /// config, each level `FROM` the last.
    ///
    /// Depth is the thing most likely to have an unnoticed limit, and here it
    /// is free: `create_snapshot` clones an extent map and raises a refcount,
    /// so every level owns a *complete* map and nothing reads through its
    /// parent. A four-deep chain is four independent filesystems that happen
    /// to share most of their blocks — not four levels of indirection.
    #[tokio::test]
    async fn layers_stack_to_any_depth() {
        let (vm, store, _path) = node(4 * 1024 * 1024 * 1024).await;

        let seed = |path: &str, byte: u8, len: usize| SeedFile {
            path: path.to_string(),
            contents: vec![byte; len],
        };

        // scratch → the runtime
        let runtime = create(
            &vm,
            &store,
            &TemplateSpec {
                seed: vec![seed("/stormd", 0xaa, 256 * 1024)],
                ..TemplateSpec::new("l-runtime", 64 * 1024 * 1024)
            },
        )
        .await
        .unwrap();

        // FROM the runtime → the app
        let app = create(
            &vm,
            &store,
            &TemplateSpec::new("l-app", 64 * 1024 * 1024).from_parent("l-runtime"),
        )
        .await
        .unwrap();
        {
            let dev = volume(&vm, VolumeId(app.raw_volume_id.unwrap())).await;
            crate::fs::files::write_files(&dev, &[seed("/app/netwatch", 0xbb, 128 * 1024)])
                .await
                .unwrap();
        }
        let app = seal(&vm, &store, &app.id, false).await.unwrap();

        // FROM the app → this deployment's config
        let deploy = create(
            &vm,
            &store,
            &TemplateSpec::new("l-deploy", 64 * 1024 * 1024).from_parent("l-app"),
        )
        .await
        .unwrap();
        {
            let dev = volume(&vm, VolumeId(deploy.raw_volume_id.unwrap())).await;
            crate::fs::files::write_files(&dev, &[seed("/etc/netwatch.toml", 0xcc, 512)])
                .await
                .unwrap();
        }
        let deploy = seal(&vm, &store, &deploy.id, false).await.unwrap();

        // Lineage is a chain, not a star.
        assert_eq!(app.parent_id, Some(runtime.id));
        assert_eq!(deploy.parent_id, Some(app.id));

        // Every level's contents accumulate, and the deepest has all three.
        let dev = volume(&vm, deploy.clone_source().unwrap()).await;
        for (path, len) in [
            ("/stormd", 256 * 1024),
            ("/app/netwatch", 128 * 1024),
            ("/etc/netwatch.toml", 512),
        ] {
            let got = crate::fs::files::read_file(&dev, path)
                .await
                .unwrap_or_else(|e| panic!("{path} missing at the deepest level: {e}"));
            assert_eq!(got.len(), len, "{path} truncated");
        }
        drop(dev);

        // And the middle of the chain is still itself: the app has the
        // runtime and its own binary, and not the deployment's config.
        let dev = volume(&vm, app.clone_source().unwrap()).await;
        assert!(crate::fs::files::read_file(&dev, "/stormd").await.is_ok());
        assert!(crate::fs::files::read_file(&dev, "/app/netwatch").await.is_ok());
        assert!(
            crate::fs::files::read_file(&dev, "/etc/netwatch.toml").await.is_err(),
            "a parent must not see what its child added"
        );
        drop(dev);

        // A clone of the deepest level is what a pod would get.
        let c = clone_template(&vm, &store, "l-deploy", &CloneSpec::new("l-pod"))
            .await
            .unwrap();
        let dev = volume(&vm, c.volume_id).await;
        assert!(crate::fs::files::read_file(&dev, "/stormd").await.is_ok());
        assert!(crate::fs::files::read_file(&dev, "/etc/netwatch.toml").await.is_ok());
    }

    /// A runtime clone is the stack, flattened, for free.
    ///
    /// This is the property the whole design rests on, so it is worth
    /// demonstrating rather than asserting. A pod does not mount a stack of
    /// layers and does not compose anything: `create_snapshot` hands it a
    /// **complete extent map** whose entries point straight at whichever slab
    /// slot holds each block, wherever in the chain that block was written.
    /// One map, direct references across the stack, no indirection at read
    /// time and no chain to walk however deep the chain was.
    ///
    /// Writes land only in the clone — copy-on-write against slots the
    /// goldens still share — so the stack underneath is untouched and the
    /// clone can simply be thrown away.
    ///
    /// The cost is a slot, not a copy: real slab usage grows by what the
    /// clone *writes*, not by what it can *see*. Measured here: a clone of a
    /// 9 MiB stack costs one slot — the clone stamping its own filesystem
    /// identity — and a 4 KiB write into it costs two more, the data slot and
    /// the metadata slot it dirties. Slot granularity, not byte count, is
    /// what a write actually costs.
    ///
    /// Two consequences worth stating plainly, because they are easy to get
    /// backwards:
    ///
    /// 1. **Depth has no read cost, at any depth.** The map is flat by
    ///    construction, so a twelve-level stack reads exactly as fast as a
    ///    one-level one. The reason to squash is space — each level carries
    ///    its own filesystem metadata and rounds its writes up to a slot —
    ///    and never latency. A clone can also be made *before* it is needed
    ///    and parked, since it costs a slot and no copying, which makes a
    ///    cold start a map lookup rather than any kind of build.
    /// 2. **A write never reaches the levels underneath.** Copy-on-write
    ///    takes fresh slab space and rewrites only this volume's map; the
    ///    goldens keep their refcounts and their bytes. Asserted below by
    ///    reading the golden back after the clone has written to it.
    #[tokio::test]
    async fn a_clone_flattens_the_stack_and_writes_only_to_itself() {
        let slot = 4 * 1024 * 1024;
        let (vm, store, _path) = node_with_slots(4 * 1024 * 1024 * 1024, slot).await;

        async fn used_slots(vm: &VmLock) -> u64 {
            let m = vm.lock().await;
            let reg = m.registry().read().await;
            reg.total_free_slots()
        }

        // runtime → app: two levels, real content in each.
        create(
            &vm,
            &store,
            &TemplateSpec {
                seed: vec![SeedFile {
                    path: "/stormd".to_string(),
                    contents: vec![0xaa; 8 * 1024 * 1024],
                }],
                ..TemplateSpec::new("flat-runtime", 128 * 1024 * 1024)
            },
        )
        .await
        .unwrap();

        let app = create(
            &vm,
            &store,
            &TemplateSpec::new("flat-app", 128 * 1024 * 1024).from_parent("flat-runtime"),
        )
        .await
        .unwrap();
        {
            let dev = volume(&vm, VolumeId(app.raw_volume_id.unwrap())).await;
            crate::fs::files::write_files(
                &dev,
                &[SeedFile { path: "/app".to_string(), contents: vec![0xbb; 1024 * 1024] }],
            )
            .await
            .unwrap();
        }
        seal(&vm, &store, &app.id, false).await.unwrap();

        // What a pod gets.
        let free_before = used_slots(&vm).await;
        let pod = clone_template(&vm, &store, "flat-app", &CloneSpec::new("flat-pod"))
            .await
            .unwrap();
        let free_after_clone = used_slots(&vm).await;

        // Cloning does not copy the stack. It copies a map, raises refcounts,
        // and pays only for stamping the clone's own filesystem identity —
        // one slot, against the 9 MiB of content the clone can now read,
        // which would be three slots at minimum if it were copied.
        let clone_cost = free_before - free_after_clone;
        assert!(
            clone_cost <= 1,
            "cloning cost {clone_cost} slots; it should share the stack, not copy it"
        );

        // And the clone sees the whole stack through that one map.
        let dev = volume(&vm, pod.volume_id).await;
        assert_eq!(
            crate::fs::files::read_file(&dev, "/stormd").await.unwrap().len(),
            8 * 1024 * 1024,
            "the runtime's file, written two levels down"
        );
        assert_eq!(
            crate::fs::files::read_file(&dev, "/app").await.unwrap().len(),
            1024 * 1024
        );

        // Writing touches only the clone.
        crate::fs::files::write_files(
            &dev,
            &[SeedFile { path: "/run/state".to_string(), contents: vec![0xdd; 4096] }],
        )
        .await
        .unwrap();
        drop(dev);
        let free_after_write = used_slots(&vm).await;
        assert!(
            free_after_write < free_after_clone,
            "the write consumed nothing; copy-on-write should have taken a slot"
        );

        // The golden underneath never saw it.
        let app_dev = volume(
            &vm,
            store.lock().await.by_name("flat-app").unwrap().clone_source().unwrap(),
        )
        .await;
        assert!(
            crate::fs::files::read_file(&app_dev, "/run/state").await.is_err(),
            "a pod's write reached the golden it was cloned from"
        );

        println!(
            "clone of a 9 MiB stack cost {clone_cost} slot(s); a 4 KiB write into it cost {} slot(s) of {} MiB",
            free_after_clone - free_after_write,
            slot / 1048576
        );
    }

    /// **These are not overlay layers.** Each level is a complete filesystem
    /// on a block device of its own; nothing is composed at mount time and
    /// nothing reads through a parent.
    ///
    /// The proof is deleting the parent. Under an overlay model a child is a
    /// diff and loses its lower layer with it. Here the child kept a complete
    /// extent map when it was cloned, and the blocks it shares are
    /// refcounted rather than borrowed — so the parent's *entry* goes and the
    /// child's contents do not.
    #[tokio::test]
    async fn a_child_survives_its_parent_being_deleted() {
        let (vm, store, _path) = node(2 * 1024 * 1024 * 1024).await;

        create(
            &vm,
            &store,
            &TemplateSpec {
                seed: vec![SeedFile {
                    path: "/stormd".to_string(),
                    contents: vec![0xaa; 512 * 1024],
                }],
                ..TemplateSpec::new("orphan-base", 64 * 1024 * 1024)
            },
        )
        .await
        .unwrap();

        let child = create(
            &vm,
            &store,
            &TemplateSpec::new("orphan-child", 64 * 1024 * 1024).from_parent("orphan-base"),
        )
        .await
        .unwrap();
        {
            let dev = volume(&vm, VolumeId(child.raw_volume_id.unwrap())).await;
            crate::fs::files::write_files(
                &dev,
                &[SeedFile { path: "/app".to_string(), contents: vec![0xbb; 64 * 1024] }],
            )
            .await
            .unwrap();
        }
        let child = seal(&vm, &store, &child.id, false).await.unwrap();

        // The parent goes entirely — entry and volumes.
        let base_id = store.lock().await.by_name("orphan-base").map(|t| t.id).unwrap();
        delete(&vm, &store, &base_id, true, true).await.unwrap();
        assert!(
            store.lock().await.by_name("orphan-base").is_none(),
            "the parent should be gone"
        );

        // The child is untouched: its own file, and the parent's.
        let dev = volume(&vm, child.clone_source().unwrap()).await;
        let inherited = crate::fs::files::read_file(&dev, "/stormd")
            .await
            .expect("the parent's file must survive the parent");
        assert_eq!(inherited.len(), 512 * 1024);
        assert_eq!(inherited[0], 0xaa, "and still be the right bytes");
        assert_eq!(
            crate::fs::files::read_file(&dev, "/app").await.unwrap().len(),
            64 * 1024
        );
        drop(dev);

        // And it still clones, which is what a pod actually needs.
        let c = clone_template(&vm, &store, "orphan-child", &CloneSpec::new("orphan-pod"))
            .await
            .unwrap();
        let dev = volume(&vm, c.volume_id).await;
        assert_eq!(
            crate::fs::files::read_file(&dev, "/stormd").await.unwrap().len(),
            512 * 1024
        );
    }

    /// Lineage has to be visible from outside, or "rebuild everything built
    /// on this base" is a question only the engine can answer — and the thing
    /// that wants to ask is not the engine.
    #[tokio::test]
    async fn a_childs_parent_is_visible_in_the_api() {
        let (vm, store, _path) = node(1024 * 1024 * 1024).await;
        let base = create(&vm, &store, &TemplateSpec::new("lineage-base", 64 * 1024 * 1024))
            .await
            .unwrap();
        let child = create(
            &vm,
            &store,
            &TemplateSpec::new("lineage-child", 64 * 1024 * 1024).from_parent("lineage-base"),
        )
        .await
        .unwrap();

        let j = child.json();
        assert_eq!(
            j.get("parent_id").and_then(|v| v.as_str()),
            Some(base.id.to_string().as_str()),
            "a child must name its parent in the API, not only on disk"
        );
        // A template with no parent says so, rather than omitting the field
        // and leaving a consumer to guess whether it was asked.
        assert!(base.json().get("parent_id").is_some());
        assert!(base.json()["parent_id"].is_null());
    }

    /// Formatting over a parent would destroy the thing the parent is for.
    #[tokio::test]
    async fn from_parent_refuses_to_also_format() {
        let (vm, store, _path) = node(1024 * 1024 * 1024).await;
        create(&vm, &store, &TemplateSpec::new("base2", 64 * 1024 * 1024)).await.unwrap();

        let mut spec = TemplateSpec::new("app2", 64 * 1024 * 1024).from_parent("base2");
        spec.format_in_core = true; // asking for both
        assert!(matches!(
            create(&vm, &store, &spec).await,
            Err(TemplateError::Invalid(_))
        ));
    }

    /// A parent that is not sealed has no snapshot to descend from.
    #[tokio::test]
    async fn from_parent_requires_a_sealed_parent() {
        let (vm, store, _path) = node(1024 * 1024 * 1024).await;
        let mut unsealed = TemplateSpec::new("half", 64 * 1024 * 1024);
        unsealed.format_in_core = false;
        create(&vm, &store, &unsealed).await.unwrap();

        assert!(matches!(
            create(&vm, &store, &TemplateSpec::new("app3", 64 * 1024 * 1024).from_parent("half"))
                .await,
            Err(TemplateError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn create_formats_seals_and_clones() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;

        let spec = TemplateSpec {
            label: "storm".to_string(),
            ..TemplateSpec::new("ext4-64m", 64 * 1024 * 1024)
        };
        let t = create(&vm, &store, &spec).await.unwrap();
        assert_eq!(t.state, TemplateState::Ready);
        assert_eq!(t.fs, FsKind::Ext4);

        // Thin volumes have 4 KiB sectors, and the kernel refuses to mount a
        // filesystem with smaller blocks than that (#40) — even though fsck is
        // perfectly happy with one.
        let sealed_dev = volume(&vm, t.clone_source().unwrap()).await;
        let layout = ext4::read_layout(&sealed_dev).await.unwrap();
        assert!(
            layout.block_size >= sealed_dev.block_size() as u64,
            "{}-byte blocks on a {}-byte-sector volume will not mount",
            layout.block_size,
            sealed_dev.block_size()
        );
        assert!(t.journal, "the ext4 profile carries one");
        assert!(t.metadata_csum && t.csum_seed, "and the checksums, seeded");
        let template_uuid = t.fs_uuid.expect("template carries a uuid");

        // A template costs the metadata it actually wrote, not the size of the
        // filesystem it describes.
        let sealed = vm.lock().await.get_volume_handle(&t.clone_source().unwrap()).unwrap();
        let allocated = sealed.allocated().await;
        assert!(allocated < 16 * 1024 * 1024, "template allocated {allocated} bytes");

        let clone = clone_template(&vm, &store, "ext4-64m", &CloneSpec::new("pvc-1"))
            .await
            .unwrap();
        let dev = volume(&vm, clone.volume_id).await;
        let l = ext4::read_layout(&dev).await.unwrap();
        assert_eq!(l.label, "storm", "clone inherits the template's label");
        assert!(l.clean);
        assert_ne!(l.uuid, template_uuid, "every clone needs its own identity");
        assert_eq!(Some(l.uuid), clone.fs_uuid);
        // The stamp must leave the filesystem checkable, not merely readable.
        assert!(ext4::check(&dev).await.unwrap().is_clean());
        assert_eq!(store.lock().await.by_name("ext4-64m").unwrap().clones, 1);

        // Two clones of one template must not collide either.
        let second =
            clone_template(&vm, &store, &clone.template_id.to_string(), &CloneSpec::new("pvc-2"))
                .await
                .unwrap();
        assert_ne!(second.fs_uuid, clone.fs_uuid);

        // ...and the template itself is untouched by either.
        let tdev = volume(&vm, t.clone_source().unwrap()).await;
        assert_eq!(ext4::read_layout(&tdev).await.unwrap().uuid, template_uuid);

        let _ = std::fs::remove_file(path);
    }

    /// The point of formatting inside the engine with a formatter that takes
    /// `&self`: several templates build at once. If any lock were held across
    /// a format they would queue, and the wall clock would be the sum.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn templates_format_concurrently() {
        let (vm, store, path) = node(4 * 1024 * 1024 * 1024).await;


        let mut tasks = Vec::new();
        for i in 0..4 {
            let vm = vm.clone();
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                create(
                    &vm,
                    &store,
                    &TemplateSpec::new(format!("parallel-{i}"), 128 * 1024 * 1024),
                )
                .await
            }));
        }

        let mut uuids = std::collections::HashSet::new();
        for task in tasks {
            let t = task.await.unwrap().expect("template built");
            assert_eq!(t.state, TemplateState::Ready);
            assert!(uuids.insert(t.fs_uuid.unwrap()), "templates share a UUID");
        }
        assert_eq!(store.lock().await.templates.len(), 4);

        // Each one is a real, checkable filesystem — concurrency must not have
        // let them write over each other.
        for i in 0..4 {
            let t = store.lock().await.by_name(&format!("parallel-{i}")).cloned().unwrap();
            let dev = volume(&vm, t.clone_source().unwrap()).await;
            let report = ext4::check(&dev).await.unwrap();
            assert!(report.is_clean(), "parallel-{i}: {:?}", report.problems);
        }

        let _ = std::fs::remove_file(path);
    }

    /// Clones are minted on a pod-start path, so they are minted at once too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clones_are_minted_concurrently_with_distinct_identity() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;

        create(&vm, &store, &TemplateSpec::new("golden", 64 * 1024 * 1024))
            .await
            .unwrap();

        let mut tasks = Vec::new();
        for i in 0..8 {
            let vm = vm.clone();
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                clone_template(&vm, &store, "golden", &CloneSpec::new(format!("pvc-{i}"))).await
            }));
        }

        let mut uuids = std::collections::HashSet::new();
        for task in tasks {
            let c = task.await.unwrap().expect("clone minted");
            assert!(uuids.insert(c.fs_uuid.unwrap()), "two clones share a UUID");
        }
        assert_eq!(store.lock().await.by_name("golden").unwrap().clones, 8);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn journal_and_features_are_per_template_choices() {
        let (vm, store, path) = node(4 * 1024 * 1024 * 1024).await;

        // The RouterOS shape and the Linux shape, coexisting by name.
        let plain = create(
            &vm,
            &store,
            &TemplateSpec {
                journal: Some(false),
                features: Some("^64bit,^metadata_csum".to_string()),
                ..TemplateSpec::new("ext4-nojournal-256m", 256 * 1024 * 1024)
            },
        )
        .await
        .unwrap();
        let journalled = create(
            &vm,
            &store,
            &TemplateSpec::new("ext4-journal-256m", 256 * 1024 * 1024),
        )
        .await
        .unwrap();

        for (t, want_journal) in [(&plain, false), (&journalled, true)] {
            let dev = volume(&vm, t.clone_source().unwrap()).await;
            let l = ext4::read_layout(&dev).await.unwrap();
            assert_eq!(l.has_journal, want_journal, "{}", t.name);
            assert!(!l.needs_recovery, "a fresh journal has nothing pending");
            assert!(l.clean);
            assert!(ext4::check(&dev).await.unwrap().is_clean(), "{}", t.name);
        }
        assert!(!plain.metadata_csum && !plain.sixty_four_bit);
        assert!(journalled.metadata_csum && journalled.sixty_four_bit);

        // ext2 and ext3 are the same formatter with different features.
        for (kind, journal) in [(FsKind::Ext2, false), (FsKind::Ext3, true)] {
            let t = create(
                &vm,
                &store,
                &TemplateSpec { fs: kind, ..TemplateSpec::new(kind.as_str(), 64 * 1024 * 1024) },
            )
            .await
            .unwrap();
            assert_eq!(t.journal, journal, "{}", kind.as_str());
            let dev = volume(&vm, t.clone_source().unwrap()).await;
            assert!(ext4::check(&dev).await.unwrap().is_clean(), "{}", kind.as_str());
        }

        let _ = std::fs::remove_file(path);
    }

    /// stormblock-registry#10: sealing must refuse a superblock a consumer
    /// would end up mounting read-only.
    #[tokio::test]
    async fn seal_refuses_a_dirty_filesystem() {
        let (vm, store, path) = node(1024 * 1024 * 1024).await;

        let spec = TemplateSpec {
            format_in_core: false,
            ..TemplateSpec::new("externally-formatted", 64 * 1024 * 1024)
        };
        let t = create(&vm, &store, &spec).await.unwrap();
        assert_eq!(t.state, TemplateState::AwaitingFormat);

        // Nothing formatted it yet: no filesystem, no seal.
        let err = seal(&vm, &store, &t.id, false).await.unwrap_err();
        assert!(matches!(err, TemplateError::NotSealable(_)), "{err}");

        // Format it the way an initiator would, then dirty the superblock the
        // way an unclean unmount does.
        let dev = volume(&vm, VolumeId(t.raw_volume_id.expect("awaiting format"))).await;
        ext4::format(&dev, &ext4::Ext4Params::default()).await.unwrap();
        dirty_superblock(&dev).await;

        let err = seal(&vm, &store, &t.id, false).await.unwrap_err();
        match err {
            TemplateError::NotSealable(m) => {
                assert!(m.contains("ERROR_FS"), "{m}");
                assert!(m.contains("RECOVER"), "{m}");
            }
            other => panic!("expected NotSealable, got {other}"),
        }
        assert_eq!(
            store.lock().await.get(&t.id).unwrap().state,
            TemplateState::AwaitingFormat
        );

        // force is the operator's escape hatch, and only that.
        let sealed = seal(&vm, &store, &t.id, true).await.unwrap();
        assert_eq!(sealed.state, TemplateState::Ready);

        let _ = std::fs::remove_file(path);
    }

    /// Set the flags an unclean unmount leaves behind, by hand.
    async fn dirty_superblock(dev: &Arc<dyn BlockDevice>) {
        use mkfs_ext4::features::IncompatFeatures;
        use mkfs_ext4::structs::superblock::state;
        let target = ext4::VolumeDevice::opaque(dev.clone());
        let mut fs = mkfs_ext4::fs::Filesystem::open(target).await.unwrap();
        let sb = fs.superblock_mut();
        sb.state = state::VALID_FS | state::ERROR_FS;
        sb.feature_incompat |= IncompatFeatures::RECOVER;
        fs.flush_superblock().await.unwrap();
    }

    /// A clone of a 32 MiB template checks out, on 4 MiB slots (#46).
    ///
    /// At this geometry the inode table runs from 4 KiB to ~2 MiB and the root
    /// directory's data block lands just past it — inside the *second* half of
    /// the first 4 MiB slot. The clone's UUID stamp writes the superblock,
    /// which copies that slot; a copy that stopped at 2 MiB left the inode
    /// table intact and the directory blocks reading back as zeros, which is
    /// exactly what the report described. Small slots never showed it, because
    /// the short transfer was capped at 2 MiB.
    #[tokio::test]
    async fn a_32mib_template_clones_clean_on_full_size_slots() {
        for slot in [4 * 1024 * 1024u64, 8 * 1024 * 1024] {
            let (vm, store, path) = node_with_slots(2 * 1024 * 1024 * 1024, slot).await;
            let t = create(&vm, &store, &TemplateSpec::new("ext4-32m", 32 * 1024 * 1024))
                .await
                .unwrap_or_else(|e| panic!("{slot}-byte slots: creating the template: {e}"));
            assert_eq!(t.state, TemplateState::Ready);

            // verify is on by default, so a clone that does not check out comes
            // back as an error rather than as a volume.
            let c = clone_template(&vm, &store, "ext4-32m", &CloneSpec::new("verify-32m"))
                .await
                .unwrap_or_else(|e| panic!("{slot}-byte slots: {e}"));
            assert!(c.verified);
            assert_ne!(c.fs_uuid, t.fs_uuid, "the clone carries its own identity");

            // And read the root directory back through the filesystem itself,
            // rather than trusting the check alone.
            let dev = volume(&vm, c.volume_id).await;
            let layout = ext4::read_layout(&dev).await.unwrap();
            assert_eq!(layout.uuid, c.fs_uuid.unwrap());

            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn clones_diverge_from_the_template_and_each_other() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();

        let a = clone_template(&vm, &store, "t", &CloneSpec::new("a")).await.unwrap();
        let b = clone_template(&vm, &store, "t", &CloneSpec::new("b")).await.unwrap();

        // Write into a data region of clone a; b and the template keep zeros.
        let dev_a = volume(&vm, a.volume_id).await;
        let payload = vec![0xA5u8; 4096];
        let off = 32 * 1024 * 1024;
        let mut done = 0;
        while done < payload.len() {
            done += dev_a.write(off + done as u64, &payload[done..]).await.unwrap();
        }

        let dev_b = volume(&vm, b.volume_id).await;
        let mut got = vec![0u8; 4096];
        dev_b.read(off, &mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0), "clone b saw clone a's write");

        let dev_t = volume(&vm, t.clone_source().unwrap()).await;
        dev_t.read(off, &mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0), "the template was written through");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn duplicate_names_and_unsealed_clones_are_refused() {
        let (vm, store, path) = node(1024 * 1024 * 1024).await;

        create(&vm, &store, &TemplateSpec::new("dup", 64 * 1024 * 1024)).await.unwrap();
        let err = create(&vm, &store, &TemplateSpec::new("dup", 64 * 1024 * 1024))
            .await
            .unwrap_err();
        assert!(matches!(err, TemplateError::Exists(_)), "{err}");

        let raw = create(
            &vm,
            &store,
            &TemplateSpec { format_in_core: false, ..TemplateSpec::new("raw", 64 * 1024 * 1024) },
        )
        .await
        .unwrap();
        let err = clone_template(&vm, &store, "raw", &CloneSpec::new("c")).await.unwrap_err();
        assert!(matches!(err, TemplateError::Conflict(_)), "{err}");
        assert!(clone_template(&vm, &store, "nope", &CloneSpec::new("c")).await.is_err());
        assert_eq!(raw.state, TemplateState::AwaitingFormat);

        let _ = std::fs::remove_file(path);
    }

    /// A sealed template owns exactly one volume, and deleting it takes that
    /// volume with it — clones and all (#47).
    #[tokio::test]
    async fn a_template_costs_one_volume_and_purges_it() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();

        // The scratch volume went at seal: the sealed snapshot holds its own
        // refcounted extents and does not need its origin.
        assert!(t.raw_volume_id.is_none(), "the scratch volume outlived the seal");
        let named: Vec<String> = vm
            .lock()
            .await
            .list_volumes()
            .await
            .into_iter()
            .map(|(_, n, _, _)| n)
            .filter(|n| n.starts_with("fstemplate-"))
            .collect();
        assert_eq!(named.len(), 1, "one template, one volume: {named:?}");

        let c = clone_template(&vm, &store, "t", &CloneSpec::new("c")).await.unwrap();

        // Descendants no longer block the purge — a snapshot keeps its own
        // reference to every extent, so the clone is untouched by this.
        let purged = delete(&vm, &store, &t.id, true, false).await.unwrap();
        assert_eq!(purged.len(), 1, "the sealed volume goes");
        assert!(store.lock().await.find("t").is_none());
        assert!(vm.lock().await.get_volume(&c.volume_id).is_some(), "the clone survived");
        let _ = std::fs::remove_file(path);
    }

    /// A clone that fails its verify is really gone before the caller is told
    /// it was discarded (#48).
    ///
    /// The clone carries the *caller's* name — `pvc-1`, not `fstemplate-…` —
    /// so one left behind is indistinguishable from a live consumer volume and
    /// no sweeper can ever find it. Being actually gone is the only guarantee
    /// available.
    #[tokio::test]
    async fn a_discarded_clone_is_really_gone_before_the_caller_is_told() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024)).await.unwrap();

        // Wreck the sealed template's filesystem so every clone of it fails
        // its own verify. The clone inherits the damage copy-on-write.
        {
            let sealed = volume(&vm, t.clone_source().unwrap()).await;
            let mut wreck = vec![0xFFu8; 8192];
            wreck[..1024].fill(0);
            sealed.write(0, &wreck).await.unwrap();
        }

        let before: Vec<String> = vm
            .lock()
            .await
            .list_volumes()
            .await
            .into_iter()
            .map(|(_, n, _, _)| n)
            .collect();

        let err = clone_template(&vm, &store, "t", &CloneSpec::new("pvc-1"))
            .await
            .expect_err("a clone of a wrecked template must not be handed out");

        // Whatever it says, it must not claim a discard that did not happen.
        match &err {
            TemplateError::Internal(m) => {
                assert!(m.contains("was discarded"), "{m}");
            }
            TemplateError::Leaked { volume_id, message } => {
                panic!("the clone leaked as {volume_id}: {message}");
            }
            other => panic!("unexpected error: {other}"),
        }

        // And it is gone — by name, which is the only handle anyone has.
        let after: Vec<String> = vm
            .lock()
            .await
            .list_volumes()
            .await
            .into_iter()
            .map(|(_, n, _, _)| n)
            .collect();
        assert_eq!(after.len(), before.len(), "the failed clone leaked: {after:?}");
        assert!(!after.contains(&"pvc-1".to_string()), "{after:?}");

        // It is also not something the orphan sweep could have cleaned up
        // afterwards, which is why it had to go now.
        let nothing = std::collections::HashSet::new();
        assert!(
            !orphans(&vm, &store, &nothing).await.iter().any(|o| o.name == "pvc-1"),
            "a caller-named clone is invisible to the template sweep by design"
        );

        let _ = std::fs::remove_file(path);
    }

    /// A leaked volume names itself, because nothing else can find it: a clone
    /// carries the caller's name, not the template prefix (#48).
    #[test]
    fn a_leak_error_carries_the_volume_id() {
        let id = Uuid::new_v4();
        let e = TemplateError::Leaked {
            volume_id: id,
            message: "clone pvc-1 did not check out (bad) — and it could NOT be discarded: busy"
                .to_string(),
        };
        let rendered = e.to_string();
        assert!(rendered.contains(&id.to_string()), "{rendered}");
        assert!(rendered.contains("could NOT be discarded"), "{rendered}");
        assert!(rendered.contains("leaked"), "{rendered}");
    }

    /// The sweep must never offer up something this node is serving (#48).
    #[tokio::test]
    async fn an_attached_orphan_is_not_reclaimable() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024)).await.unwrap();
        let sealed = t.sealed_volume_id.unwrap();

        // The state #47 leaves behind: the store forgets it, the volume stays.
        delete(&vm, &store, &t.id, false, false).await.unwrap();

        // With nothing attached it is debris, and reclaimable.
        let nothing = std::collections::HashSet::new();
        assert_eq!(orphans(&vm, &store, &nothing).await.len(), 1);

        // Attached, it is not — even though it is just as unclaimed.
        let attached: std::collections::HashSet<Uuid> = [sealed].into_iter().collect();
        assert!(
            orphans(&vm, &store, &attached).await.is_empty(),
            "an attached volume was offered for reclamation"
        );
        assert!(reclaim_orphans(&vm, &store, &attached).await.is_empty());
        assert!(
            vm.lock().await.get_volume(&VolumeId(sealed)).is_some(),
            "the attached volume survived the sweep"
        );

        let _ = std::fs::remove_file(path);
    }

    /// The state #47 found a node in: volumes named like a template's that no
    /// template claims, because a `DELETE` without `purge` walked away from them.
    #[tokio::test]
    async fn orphaned_template_volumes_are_found_and_reclaimed() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();
        // A clone, named by its consumer: never mistaken for template debris.
        clone_template(&vm, &store, "t", &CloneSpec::new("pvc-1")).await.unwrap();

        let nothing_attached = std::collections::HashSet::new();
        assert!(
            orphans(&vm, &store, &nothing_attached).await.is_empty(),
            "a live template is not debris"
        );

        // The old behaviour, by hand: forget the template, keep its volumes.
        delete(&vm, &store, &t.id, false, false).await.unwrap();

        let found = orphans(&vm, &store, &nothing_attached).await;
        assert_eq!(found.len(), 1, "the sealed volume is now unclaimed: {found:?}");

        let gone = reclaim_orphans(&vm, &store, &nothing_attached).await;
        assert_eq!(gone.len(), 1);
        assert!(orphans(&vm, &store, &nothing_attached).await.is_empty());
        assert!(
            vm.lock()
                .await
                .list_volumes()
                .await
                .iter()
                .any(|(_, n, _, _)| n == "pvc-1"),
            "reclaiming debris must not touch a clone"
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn templates_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("sb-fstemplate-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (vm, _unused, path) = node(1024 * 1024 * 1024).await;

        let id = {
            let store = Arc::new(tokio::sync::Mutex::new(TemplateStore::load(&dir)));
            create(&vm, &store, &TemplateSpec::new("persisted", 64 * 1024 * 1024))
                .await
                .unwrap()
                .id
        };

        let reloaded = TemplateStore::load(&dir);
        let t = reloaded.get(&id).expect("template survived");
        assert_eq!(t.name, "persisted");
        assert_eq!(t.state, TemplateState::Ready);
        assert!(t.fs_uuid.is_some());
        assert_eq!(t.fs, FsKind::Ext4);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
