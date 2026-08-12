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

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::volume::{VolumeId, VolumeManager};

use super::ext4;

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
    /// Sealed: `sealed_volume_id` is a clean snapshot, safe to clone.
    Ready,
}

impl TemplateState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TemplateState::AwaitingFormat => "awaiting_format",
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
    /// Format and seal in this process. False leaves the template in
    /// `awaiting_format` for an initiator to format over an export.
    pub format_in_core: bool,
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
            format_in_core: true,
        }
    }
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
    /// The volume that gets formatted.
    pub raw_volume_id: Uuid,
    /// The clean snapshot clones descend from. Set at seal.
    pub sealed_volume_id: Option<Uuid>,
    /// How many clones have been minted from it.
    #[serde(default)]
    pub clones: u64,
}

impl FsTemplate {
    /// The volume clones are taken from — the sealed snapshot, never the raw
    /// volume (which may still be attached and changing).
    pub fn clone_source(&self) -> Option<VolumeId> {
        self.sealed_volume_id.map(VolumeId)
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
            "clones": self.clones,
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

    let raw = vm
        .lock()
        .await
        .create_volume_any(&format!("fstemplate-{}-raw", spec.name), spec.size_bytes)
        .await
        .map_err(|e| TemplateError::Internal(format!("creating template volume: {e}")))?;

    let mut template = FsTemplate {
        id: Uuid::new_v4(),
        name: spec.name.clone(),
        fs: spec.fs,
        size_bytes: spec.size_bytes,
        journal: spec.journal.unwrap_or(spec.fs != FsKind::Ext2),
        label: spec.label.clone(),
        features: spec.features.clone(),
        sixty_four_bit: false,
        metadata_csum: false,
        csum_seed: false,
        fs_uuid: None,
        state: TemplateState::AwaitingFormat,
        raw_volume_id: raw.0,
        sealed_volume_id: None,
        clones: 0,
    };

    // Claim the name before the long part. Two creates racing on one name
    // must not both format; the loser gives its volume straight back.
    {
        let mut s = store.lock().await;
        if s.by_name(&spec.name).is_some() {
            drop(s);
            let _ = vm.lock().await.delete_volume(raw).await;
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
        let mut s = store.lock().await;
        s.remove(&template.id);
        s.persist();
        drop(s);
        let _ = vm.lock().await.delete_volume(raw).await;
        return Err(TemplateError::Internal(format!("formatting {}: {e}", spec.name)));
    }

    template.fs_uuid = Some(params.uuid);
    {
        let mut s = store.lock().await;
        if let Some(t) = s.get_mut(&template.id) {
            t.fs_uuid = template.fs_uuid;
        }
        s.persist();
    }

    seal(vm, store, &template.id, false).await
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

    let raw = VolumeId(template.raw_volume_id);
    let dev = vm.lock().await.get_volume(&raw).ok_or_else(|| {
        TemplateError::NotFound(format!("template volume {} is gone", template.raw_volume_id))
    })?;

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
}

/// Mint a copy-on-write clone of a sealed template.
///
/// This is the provisioning path: a snapshot plus, at most, one 16-byte patch.
/// No mkfs, no attach, no round trip.
pub async fn clone_template(
    vm: &VmLock,
    store: &StoreLock,
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
                    let _ = vm.lock().await.delete_volume(id).await;
                    return Err(TemplateError::Internal(format!(
                        "stamping a fresh filesystem UUID on the clone: {e}"
                    )));
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
            let _ = vm.lock().await.delete_volume(id).await;
            return Err(TemplateError::Internal(format!(
                "clone {} did not check out and was discarded: {why}",
                spec.name
            )));
        }
    }

    {
        let mut s = store.lock().await;
        if let Some(t) = s.get_mut(&template.id) {
            t.clones += 1;
        }
        s.persist();
    }

    Ok(CloneResult {
        volume_id: id,
        template_id: template.id,
        fs_uuid,
        size_bytes: size,
        verified: spec.verify,
    })
}

/// Remove a template. `purge` also deletes its volumes.
///
/// Clones are independent volumes — copy-on-write means deleting the template
/// does not take their data with it — but a template with descendants is
/// usually still wanted, so purging one requires `force`.
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
        return Err(TemplateError::Conflict(format!(
            "fstemplate {} has {} clone(s) descending from it — purge with force=true only if \
             you know they are gone",
            template.name, template.clones
        )));
    }

    let mut purged = Vec::new();
    if purge {
        let mut m = vm.lock().await;
        for vol in [template.sealed_volume_id, Some(template.raw_volume_id)].into_iter().flatten() {
            match m.delete_volume(VolumeId(vol)).await {
                Ok(()) => purged.push(vol),
                Err(e) => tracing::warn!("purging template volume {vol}: {e}"),
            }
        }
    }

    let mut s = store.lock().await;
    s.remove(id);
    s.persist();
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::raid::RaidArrayId;

    /// A node with one slab, and the two locks the lifecycle takes.
    async fn node(size: u64) -> (VmLock, StoreLock, String) {
        let dir = std::env::temp_dir().join("stormblock-fstemplate-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.slab", Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&p, size).await.unwrap();
        let mut vm = VolumeManager::new(1024 * 1024);
        vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
        (
            tokio::sync::Mutex::new(vm),
            tokio::sync::Mutex::new(TemplateStore::in_memory()),
            p,
        )
    }

    async fn volume(vm: &VmLock, id: VolumeId) -> Arc<dyn BlockDevice> {
        vm.lock().await.get_volume(&id).expect("volume exists")
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
        let vm = Arc::new(vm);
        let store = Arc::new(store);

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
        let vm = Arc::new(vm);
        let store = Arc::new(store);
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
        let dev = volume(&vm, VolumeId(t.raw_volume_id)).await;
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

    #[tokio::test]
    async fn purge_needs_force_once_clones_exist() {
        let (vm, store, path) = node(2 * 1024 * 1024 * 1024).await;
        let t = create(&vm, &store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();
        clone_template(&vm, &store, "t", &CloneSpec::new("c")).await.unwrap();

        let err = delete(&vm, &store, &t.id, true, false).await.unwrap_err();
        assert!(matches!(err, TemplateError::Conflict(_)), "{err}");

        let purged = delete(&vm, &store, &t.id, true, true).await.unwrap();
        assert_eq!(purged.len(), 2, "raw and sealed volumes both go");
        assert!(store.lock().await.find("t").is_none());

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn templates_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("sb-fstemplate-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (vm, _unused, path) = node(1024 * 1024 * 1024).await;

        let id = {
            let store = tokio::sync::Mutex::new(TemplateStore::load(&dir));
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
