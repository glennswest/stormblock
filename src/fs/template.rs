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

pub const TEMPLATES_FILE: &str = "fstemplates.json";

/// Filesystems this engine can lay down itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    Ext4,
}

impl FsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsKind::Ext4 => "ext4",
        }
    }
}

impl std::str::FromStr for FsKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ext4" | "ext3" | "ext2" => Ok(FsKind::Ext4),
            other => Err(format!("unsupported filesystem '{other}' (this engine writes ext4)")),
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
    /// RouterOS cannot replay a journal, so one that ever goes dirty there
    /// leaves the filesystem read-only permanently; a Linux host or a VM wants
    /// the crash consistency. Both variants coexist, told apart by name.
    pub journal: bool,
    /// Filesystem label baked into the template. Clones inherit it unless the
    /// clone asks for its own.
    pub label: String,
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
            journal: false,
            label: String::new(),
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
    /// Whether the filesystem carries a journal.
    #[serde(default)]
    pub journal: bool,
    #[serde(default)]
    pub label: String,
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
    vm: &mut VolumeManager,
    store: &mut TemplateStore,
    spec: &TemplateSpec,
) -> Result<FsTemplate> {
    if spec.name.trim().is_empty() {
        return Err(TemplateError::Invalid("name must not be empty".to_string()));
    }
    if spec.size_bytes == 0 {
        return Err(TemplateError::Invalid("size_bytes must be > 0".to_string()));
    }
    if store.by_name(&spec.name).is_some() {
        return Err(TemplateError::Exists(format!("fstemplate {} already exists", spec.name)));
    }

    let raw = vm
        .create_volume_any(&format!("fstemplate-{}-raw", spec.name), spec.size_bytes)
        .await
        .map_err(|e| TemplateError::Internal(format!("creating template volume: {e}")))?;

    let mut template = FsTemplate {
        id: Uuid::new_v4(),
        name: spec.name.clone(),
        fs: spec.fs,
        size_bytes: spec.size_bytes,
        journal: spec.journal,
        label: spec.label.clone(),
        fs_uuid: None,
        state: TemplateState::AwaitingFormat,
        raw_volume_id: raw.0,
        sealed_volume_id: None,
        clones: 0,
    };

    if spec.format_in_core {
        let dev = vm
            .get_volume(&raw)
            .ok_or_else(|| TemplateError::Internal("new template volume vanished".to_string()))?;
        let params = ext4::Ext4Params {
            label: spec.label.clone(),
            uuid: Uuid::new_v4(),
            journal: spec.journal,
            // The volume was created moments ago and has never been written,
            // so every unwritten block already reads back as zeros. This is
            // what keeps a template's allocation in kilobytes.
            assume_blank: true,
            ..Default::default()
        };
        match spec.fs {
            FsKind::Ext4 => {
                ext4::format(&dev, &params)
                    .await
                    .map_err(|e| TemplateError::Internal(format!("formatting {}: {e}", spec.name)))?;
            }
        }
        template.fs_uuid = Some(params.uuid);
        drop(dev);

        store.insert(template);
        store.persist();
        let id = store.templates.last().expect("just inserted").id;
        return seal(vm, store, &id, false).await;
    }

    store.insert(template.clone());
    store.persist();
    Ok(template)
}

/// Seal a formatted template: verify the superblock, snapshot it, mark ready.
///
/// The verification is the point. A sealed template is the ancestor of every
/// clone that follows, so a superblock that says "errors recorded" or
/// "recovery pending" would be inherited by all of them — and a consumer that
/// cannot replay a journal (RouterOS) mounts every one of them read-only.
/// `force` skips the check for an operator who knows better; nothing else does.
pub async fn seal(
    vm: &mut VolumeManager,
    store: &mut TemplateStore,
    id: &Uuid,
    force: bool,
) -> Result<FsTemplate> {
    let template = store
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
    let dev = vm.get_volume(&raw).ok_or_else(|| {
        TemplateError::NotFound(format!("template volume {} is gone", template.raw_volume_id))
    })?;

    let layout = match ext4::read_at(&dev, ext4::SUPERBLOCK_OFFSET, ext4::SUPERBLOCK_LEN).await {
        Ok(sb) => match ext4::parse_superblock(&sb) {
            Ok(l) => Some(l),
            Err(e) if force => {
                tracing::warn!("sealing {} with no readable ext4 superblock: {e}", template.name);
                None
            }
            Err(e) => {
                return Err(TemplateError::NotSealable(format!(
                    "no usable ext4 filesystem on the template volume: {e}"
                )))
            }
        },
        Err(e) if force => {
            tracing::warn!("sealing {} without reading its superblock: {e}", template.name);
            None
        }
        Err(e) => {
            return Err(TemplateError::NotSealable(format!(
                "cannot read the template superblock: {e}"
            )))
        }
    };

    if let Some(l) = &layout {
        if let Err(why) = ext4::check_sealable(l) {
            if !force {
                return Err(TemplateError::NotSealable(format!(
                    "refusing to seal {}: {why}",
                    template.name
                )));
            }
            tracing::warn!("sealing {} despite: {why}", template.name);
        }
    }
    drop(dev);

    let sealed = vm
        .create_snapshot(raw, &format!("fstemplate-{}-{}", template.fs, template.name))
        .await
        .map_err(|e| TemplateError::Internal(format!("sealing snapshot: {e}")))?;

    let t = store
        .get_mut(id)
        .ok_or_else(|| TemplateError::NotFound(format!("fstemplate {id} not found")))?;
    t.sealed_volume_id = Some(sealed.0);
    t.state = TemplateState::Ready;
    if let Some(l) = &layout {
        t.fs_uuid = Some(l.uuid);
        t.journal = l.has_journal();
        if t.label.is_empty() {
            t.label = l.label.clone();
        }
    }
    let out = t.clone();
    store.persist();

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
}

impl CloneSpec {
    pub fn new(name: impl Into<String>) -> Self {
        CloneSpec { name: name.into(), stamp_uuid: true, ..Default::default() }
    }
}

/// What a clone came out as.
#[derive(Debug, Clone)]
pub struct CloneResult {
    pub volume_id: VolumeId,
    pub template_id: Uuid,
    pub fs_uuid: Option<Uuid>,
    pub size_bytes: u64,
}

/// Mint a copy-on-write clone of a sealed template.
///
/// This is the provisioning path: a snapshot plus, at most, one 16-byte patch.
/// No mkfs, no attach, no round trip.
pub async fn clone_template(
    vm: &mut VolumeManager,
    store: &mut TemplateStore,
    key: &str,
    spec: &CloneSpec,
) -> Result<CloneResult> {
    let template = store
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

    let id = vm
        .create_snapshot(source, &spec.name)
        .await
        .map_err(|e| TemplateError::Internal(format!("cloning template: {e}")))?;

    // A clone inherits the template's size; grow it if the caller asked for
    // more. Shrinking is never attempted.
    let mut size = vm.get_volume(&id).map(|d| d.capacity_bytes()).unwrap_or(template.size_bytes);
    if let Some(want) = spec.size_bytes {
        if want > size {
            vm.resize_volume(id, want)
                .await
                .map_err(|e| TemplateError::Internal(format!("growing clone: {e}")))?;
            size = want;
        }
    }

    // Fresh identity. This is the step that cannot live above the engine:
    // every consumer clones *through* here, so a UUID stamped anywhere else
    // would miss the clones that consumer never touches (stormblockmk#12).
    let mut fs_uuid = template.fs_uuid;
    if spec.stamp_uuid || spec.label.is_some() {
        let dev = vm
            .get_volume(&id)
            .ok_or_else(|| TemplateError::Internal("clone volume vanished".to_string()))?;
        if spec.stamp_uuid {
            let fresh = Uuid::new_v4();
            match ext4::stamp_uuid(&dev, fresh, spec.stamp_backups).await {
                Ok(_) => fs_uuid = Some(fresh),
                Err(e) => {
                    // Roll the clone back: handing out a volume that silently
                    // shares its template's identity is the bug this exists to
                    // prevent.
                    drop(dev);
                    let _ = vm.delete_volume(id).await;
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

    if let Some(t) = store.get_mut(&template.id) {
        t.clones += 1;
    }
    store.persist();

    Ok(CloneResult { volume_id: id, template_id: template.id, fs_uuid, size_bytes: size })
}

/// Remove a template. `purge` also deletes its volumes.
///
/// Clones are independent volumes — copy-on-write means deleting the template
/// does not take their data with it — but a template with descendants is
/// usually still wanted, so purging one requires `force`.
pub async fn delete(
    vm: &mut VolumeManager,
    store: &mut TemplateStore,
    id: &Uuid,
    purge: bool,
    force: bool,
) -> Result<Vec<Uuid>> {
    let template = store
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
        for vol in [template.sealed_volume_id, Some(template.raw_volume_id)].into_iter().flatten() {
            match vm.delete_volume(VolumeId(vol)).await {
                Ok(()) => purged.push(vol),
                Err(e) => tracing::warn!("purging template volume {vol}: {e}"),
            }
        }
    }

    store.remove(id);
    store.persist();
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::raid::RaidArrayId;

    async fn manager(size: u64) -> (VolumeManager, String) {
        let dir = std::env::temp_dir().join("stormblock-fstemplate-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.slab", Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&p, size).await.unwrap();
        let mut vm = VolumeManager::new(1024 * 1024);
        vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
        (vm, p)
    }

    #[tokio::test]
    async fn create_formats_seals_and_clones() {
        let (mut vm, path) = manager(1024 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();

        let spec = TemplateSpec {
            label: "storm".to_string(),
            ..TemplateSpec::new("ext4-nojournal-64m", 64 * 1024 * 1024)
        };
        let t = create(&mut vm, &mut store, &spec).await.unwrap();
        assert_eq!(t.state, TemplateState::Ready);
        assert!(t.sealed_volume_id.is_some());
        let template_uuid = t.fs_uuid.expect("template carries a uuid");

        // A template costs the metadata it actually wrote, not the size of the
        // filesystem it describes.
        let sealed = vm.get_volume_handle(&t.clone_source().unwrap()).unwrap();
        assert!(
            sealed.allocated() .await < 8 * 1024 * 1024,
            "template allocated {} bytes",
            sealed.allocated().await
        );

        let clone = clone_template(&mut vm, &mut store, "ext4-nojournal-64m", &CloneSpec::new("pvc-1"))
            .await
            .unwrap();
        let dev = vm.get_volume(&clone.volume_id).unwrap();
        let sb = ext4::read_at(&dev, ext4::SUPERBLOCK_OFFSET, ext4::SUPERBLOCK_LEN).await.unwrap();
        let l = ext4::parse_superblock(&sb).unwrap();
        assert_eq!(l.label, "storm", "clone inherits the template's label");
        assert!(l.clean);
        assert_ne!(l.uuid, template_uuid, "every clone needs its own identity");
        assert_eq!(Some(l.uuid), clone.fs_uuid);
        assert_eq!(store.by_name("ext4-nojournal-64m").unwrap().clones, 1);

        // Two clones of one template must not collide either.
        let second = clone_template(&mut vm, &mut store, &clone.template_id.to_string(), &CloneSpec::new("pvc-2"))
            .await
            .unwrap();
        assert_ne!(second.fs_uuid, clone.fs_uuid);

        // ...and the template itself is untouched by either.
        let tdev = vm.get_volume(&t.clone_source().unwrap()).unwrap();
        let tsb = ext4::read_at(&tdev, ext4::SUPERBLOCK_OFFSET, ext4::SUPERBLOCK_LEN).await.unwrap();
        assert_eq!(ext4::parse_superblock(&tsb).unwrap().uuid, template_uuid);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn journal_is_a_per_template_choice() {
        let (mut vm, path) = manager(2 * 1024 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();

        // Both variants coexist, told apart by name — RouterOS needs the
        // journal-less one, a Linux host wants the other.
        let plain = create(
            &mut vm,
            &mut store,
            &TemplateSpec::new("ext4-nojournal-256m", 256 * 1024 * 1024),
        )
        .await
        .unwrap();
        let journalled = create(
            &mut vm,
            &mut store,
            &TemplateSpec { journal: true, ..TemplateSpec::new("ext4-journal-256m", 256 * 1024 * 1024) },
        )
        .await
        .unwrap();

        for (t, want_journal) in [(&plain, false), (&journalled, true)] {
            let dev = vm.get_volume(&t.clone_source().unwrap()).unwrap();
            let sb = ext4::read_at(&dev, ext4::SUPERBLOCK_OFFSET, ext4::SUPERBLOCK_LEN).await.unwrap();
            let l = ext4::parse_superblock(&sb).unwrap();
            assert_eq!(l.has_journal(), want_journal, "{}", t.name);
            assert!(!l.needs_recovery(), "a fresh journal has nothing pending");
            assert!(l.clean);
        }

        let _ = std::fs::remove_file(path);
    }

    /// stormblock-registry#10: sealing must refuse a superblock a consumer
    /// would end up mounting read-only.
    #[tokio::test]
    async fn seal_refuses_a_dirty_filesystem() {
        let (mut vm, path) = manager(512 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();

        let spec = TemplateSpec {
            format_in_core: false,
            ..TemplateSpec::new("externally-formatted", 64 * 1024 * 1024)
        };
        let t = create(&mut vm, &mut store, &spec).await.unwrap();
        assert_eq!(t.state, TemplateState::AwaitingFormat);

        // Nothing formatted it yet: no superblock, no seal.
        let err = seal(&mut vm, &mut store, &t.id, false).await.unwrap_err();
        assert!(matches!(err, TemplateError::NotSealable(_)), "{err}");

        // Format it the way an initiator would, then dirty the superblock the
        // way an unclean RouterOS unmount does.
        let dev = vm.get_volume(&VolumeId(t.raw_volume_id)).unwrap();
        ext4::format(&dev, &ext4::Ext4Params { journal: true, ..Default::default() })
            .await
            .unwrap();
        let mut sb = ext4::read_at(&dev, ext4::SUPERBLOCK_OFFSET, ext4::SUPERBLOCK_LEN).await.unwrap();
        sb[0x3A..0x3C].copy_from_slice(&(ext4::STATE_VALID_FS | ext4::STATE_ERROR_FS).to_le_bytes());
        sb[0x60..0x64].copy_from_slice(&0x0004u32.to_le_bytes()); // RECOVER
        ext4::write_at(&dev, ext4::SUPERBLOCK_OFFSET, &sb).await.unwrap();
        drop(dev);

        let err = seal(&mut vm, &mut store, &t.id, false).await.unwrap_err();
        match err {
            TemplateError::NotSealable(m) => {
                assert!(m.contains("ERROR_FS"), "{m}");
                assert!(m.contains("RECOVER"), "{m}");
            }
            other => panic!("expected NotSealable, got {other}"),
        }
        assert_eq!(store.get(&t.id).unwrap().state, TemplateState::AwaitingFormat);

        // force is the operator's escape hatch, and only that.
        let sealed = seal(&mut vm, &mut store, &t.id, true).await.unwrap();
        assert_eq!(sealed.state, TemplateState::Ready);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn clones_diverge_from_the_template_and_each_other() {
        let (mut vm, path) = manager(1024 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();
        let t = create(&mut vm, &mut store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();

        let a = clone_template(&mut vm, &mut store, "t", &CloneSpec::new("a")).await.unwrap();
        let b = clone_template(&mut vm, &mut store, "t", &CloneSpec::new("b")).await.unwrap();

        // Write into a data region of clone a; b and the template keep zeros.
        let dev_a = vm.get_volume(&a.volume_id).unwrap();
        let payload = vec![0xA5u8; 4096];
        let off = 32 * 1024 * 1024;
        ext4::write_all(&dev_a, off, &payload).await.unwrap();

        let dev_b = vm.get_volume(&b.volume_id).unwrap();
        let mut got = vec![0u8; 4096];
        dev_b.read(off, &mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0), "clone b saw clone a's write");

        let dev_t = vm.get_volume(&t.clone_source().unwrap()).unwrap();
        dev_t.read(off, &mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0), "the template was written through");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn duplicate_names_and_unsealed_clones_are_refused() {
        let (mut vm, path) = manager(512 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();

        create(&mut vm, &mut store, &TemplateSpec::new("dup", 64 * 1024 * 1024)).await.unwrap();
        let err = create(&mut vm, &mut store, &TemplateSpec::new("dup", 64 * 1024 * 1024))
            .await
            .unwrap_err();
        assert!(matches!(err, TemplateError::Exists(_)), "{err}");

        let raw = create(
            &mut vm,
            &mut store,
            &TemplateSpec { format_in_core: false, ..TemplateSpec::new("raw", 64 * 1024 * 1024) },
        )
        .await
        .unwrap();
        let err = clone_template(&mut vm, &mut store, "raw", &CloneSpec::new("c"))
            .await
            .unwrap_err();
        assert!(matches!(err, TemplateError::Conflict(_)), "{err}");
        assert!(clone_template(&mut vm, &mut store, "nope", &CloneSpec::new("c")).await.is_err());
        assert_eq!(raw.state, TemplateState::AwaitingFormat);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn purge_needs_force_once_clones_exist() {
        let (mut vm, path) = manager(1024 * 1024 * 1024).await;
        let mut store = TemplateStore::in_memory();
        let t = create(&mut vm, &mut store, &TemplateSpec::new("t", 64 * 1024 * 1024))
            .await
            .unwrap();
        clone_template(&mut vm, &mut store, "t", &CloneSpec::new("c")).await.unwrap();

        let err = delete(&mut vm, &mut store, &t.id, true, false).await.unwrap_err();
        assert!(matches!(err, TemplateError::Conflict(_)), "{err}");

        let purged = delete(&mut vm, &mut store, &t.id, true, true).await.unwrap();
        assert_eq!(purged.len(), 2, "raw and sealed volumes both go");
        assert!(store.find("t").is_none());

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn templates_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("sb-fstemplate-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut vm, path) = manager(512 * 1024 * 1024).await;

        let id = {
            let mut store = TemplateStore::load(&dir);
            let t = create(&mut vm, &mut store, &TemplateSpec::new("persisted", 64 * 1024 * 1024))
                .await
                .unwrap();
            t.id
        };

        let reloaded = TemplateStore::load(&dir);
        let t = reloaded.get(&id).expect("template survived");
        assert_eq!(t.name, "persisted");
        assert_eq!(t.state, TemplateState::Ready);
        assert!(t.fs_uuid.is_some());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
