//! Volume manager — thin provisioning, COW snapshots, slab-based allocation.
//!
//! The `VolumeManager` coordinates thin volumes on top of slab-backed storage.
//! Each `ThinVolume` implements `BlockDevice`, so target protocols
//! (NVMe-oF, iSCSI) see volumes as plain block devices.

#[cfg(feature = "stormfs-data")]
pub mod chunk;
pub mod extent;
pub mod fence;
pub mod gem;
pub mod metadata;
pub mod metav2;
mod persist_v2;
pub mod redundancy;
pub mod thin;
pub mod snapshot;
pub mod compose;
pub mod disk;
pub mod stripe;
pub mod stripelog;
#[cfg(feature = "stormfs-data")]
pub mod versioned;
pub mod gc;
pub mod extable;
pub mod erase;
pub mod holds;
pub mod pressure;
pub mod relocate;
pub mod synonym;
pub mod throttle;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::drive::slab::{Slab, SlabId, SlabRole};
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::topology::StorageTier;
use crate::raid::RaidArrayId;

pub use extent::{ExtentAllocator, VolumeId, DEFAULT_EXTENT_SIZE};
pub use metadata::{Access, FsInfo, MetadataStore, Retention};
pub use synonym::{Synonym, SynonymError, SynonymStore, Target as SynonymTarget};
pub use thin::{Lba, ThinVolume, ThinVolumeHandle, VolumeError, PlacementPolicy, VolumeHealth, HealthState, ResyncReport, ResyncOptions, ResyncCheckpoint};
pub use redundancy::{Redundancy, RedundancyPolicy};

/// What a restripe did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestripeReport {
    pub extents_copied: usize,
    pub slots_released: usize,
    pub redundancy: String,
}

/// Everything a volume is created with beyond a name and a size.
///
/// `placement.role` is not read here: the role a volume is created in comes
/// from `role`, so that "the caller did not say" is distinguishable from
/// "the caller said system" (#93).
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub redundancy: RedundancyPolicy,
    pub placement: PlacementPolicy,
    /// Which half of the node's storage to place in. `None` asks the node:
    /// system where it has a system slab, otherwise the role it does have.
    pub role: Option<SlabRole>,
    /// The size of the volume's extents (#156), fixed for its life. `None`:
    /// [`BULK_EXTENT`] for a volume of [`BULK_FROM`] or more where the node
    /// has a pool of that size in the role, else the node's default.
    pub extent_size: Option<u64>,
    /// The volume's id, when it must be a given one: a release's golden
    /// staged on a node keeps the id the release gave it, which is what
    /// `slab holds` compares (#122). `None`: a fresh id.
    pub id: Option<VolumeId>,
}

/// The bulk extent size (#156, owner 2026-10-05: 8 MiB rather than 64).
pub const BULK_EXTENT: u64 = 8 << 20;
/// Volumes this large or larger are bulk when nothing says otherwise.
pub const BULK_FROM: u64 = 64 << 30;

impl CreateOptions {
    pub fn redundant(policy: RedundancyPolicy) -> Self {
        CreateOptions { redundancy: policy, ..Default::default() }
    }

    /// Place this volume in the node's data slabs — identity and state,
    /// which no install may reformat (#88).
    pub fn in_role(mut self, role: SlabRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Every extent on this slab (#150). The role is the slab's.
    pub fn pinned_to(slab: SlabId) -> Self {
        CreateOptions {
            placement: PlacementPolicy { pinned: Some(slab), ..Default::default() },
            ..Default::default()
        }
    }

    /// Place in `role` when one was named, and let the node decide otherwise.
    pub fn in_role_opt(mut self, role: Option<SlabRole>) -> Self {
        self.role = role;
        self
    }

    /// Extents of this size (#156); `None` lets the node choose.
    pub fn with_extent_size(mut self, size: Option<u64>) -> Self {
        self.extent_size = size;
        self
    }
}
pub use gem::GlobalExtentMap;

/// Default slot size for slabs created via add_backing_device.
pub const DEFAULT_SLOT_SIZE: u64 = crate::drive::slab::DEFAULT_SLOT_SIZE;

/// What moving a volume between tiers did.
#[derive(Debug, Default)]
pub struct RetierReport {
    /// Extents nothing else referenced: relocated, and the space given back.
    pub moved: usize,
    /// Extents shared with another volume: copied, the original left in place.
    pub copied: usize,
    /// Extents already on the destination.
    pub already: usize,
    pub failed: usize,
    pub destination: SlabId,
}

/// What an adoption found and what it did with it.
#[derive(Debug, Default)]
pub struct AdoptReport {
    /// Slabs newly attached: id, the partition they were found in, role.
    pub slabs: Vec<(SlabId, String, String)>,
    /// Volumes now addressable: id, name, virtual size.
    pub volumes: Vec<(VolumeId, String, u64)>,
    /// Slabs this engine already had.
    pub already_attached: usize,
    /// Volumes this engine already knew.
    pub already_known: usize,
}

/// Manages volumes, slab allocation, and snapshots.
/// Which of a volume's failed legs came back when it was asked to try again.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClearFailedReport {
    pub cleared: Vec<String>,
    pub still_failed: Vec<String>,
}

/// How much room one slab's volume record needs against what it has.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetadataPressure {
    pub slab_id: String,
    pub volumes: usize,
    pub needed_bytes: u64,
    pub capacity_bytes: u64,
    pub fits: bool,
}

pub struct VolumeManager {
    gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
    volumes: HashMap<VolumeId, Arc<ThinVolumeHandle>>,
    /// Legacy mapping: array_id → slab_id (for backward compat with callers
    /// that pass array_id to create_volume).
    array_slabs: HashMap<RaidArrayId, SlabId>,
    slot_size: u64,
    metadata_store: Option<MetadataStore>,
    /// Slabs that keep this manager's `volumes.dat` inside themselves. Set
    /// where there is no filesystem to keep it in — an image, an appliance
    /// disk — so the slab is the whole record of what it holds.
    ///
    /// Plural because a node's mutable storage is two partitions, not one.
    /// Each slab records **its own** volumes: the data slab's copy has to
    /// survive the system slab being replaced wholesale, so it cannot live
    /// in the system slab, and a single merged copy in either one would be
    /// exactly the coupling an install is supposed to break (#88).
    metadata_slabs: Vec<SlabId>,
    /// What each volume is for: kept, or thrown away. Held here rather than
    /// on the volume handle because it is a fact about the data, not about
    /// the I/O path, and every consumer of a handle would otherwise have to
    /// carry it around to be able to ask.
    retentions: HashMap<VolumeId, Retention>,
    /// Lineage: which volume each one was cloned from (#76).
    parents: HashMap<VolumeId, VolumeId>,
    /// Volumes that are blanks to clone from rather than volumes to run.
    ///
    /// Held with the volumes rather than in a separate registry on purpose:
    /// the slab is already the record of what it holds, which is why it
    /// carries its own `volumes.dat`. A node that attaches a slab should know
    /// what is in it without a filesystem to look the answer up in (#100).
    templates: std::collections::HashSet<VolumeId>,
    /// What is known about the filesystem on each volume.
    fs_info: HashMap<VolumeId, FsInfo>,
    /// What each volume belongs to, for the volumes anything has said (#115).
    owners: HashMap<VolumeId, crate::volume::metadata::Owner>,
    /// Why the last attempt to write this manager's record failed, if it did.
    ///
    /// A background persist cannot fail the call that triggered it — the
    /// volume is already created, the write already acknowledged. What it
    /// must not do is let that pass unremarked: a node whose record is not
    /// being written is a node that will come back missing volumes, and the
    /// only warning of it was a line in a log. Kept here so the condition is
    /// a thing the API can be asked about rather than something to notice.
    durability: Arc<std::sync::Mutex<Option<String>>>,
    /// Bumped every time the volume metadata is persisted — which is every
    /// time a volume, its lineage or its placement changes. A mirror asks
    /// "has anything changed since N" instead of re-reading everything (#136).
    generation: std::sync::atomic::AtomicU64,
    /// A flow-over in progress: the local system slab, and the slabs being
    /// emptied into it (#258). The destination records every volume with a
    /// leg on a source, moved or not, so a disk whose flow-over is cut short
    /// still names everything the node boots — and the next boot finishes the
    /// flow-over from a fresh clone (#171) instead of finding volumes missing
    /// and re-installing.
    flowing_into: std::sync::Mutex<Vec<(SlabId, Vec<SlabId>)>>,
    /// What serves each volume as a device right now (#267): a held volume
    /// is not deleted, whichever path asks.
    holds: holds::ServeHolds,
    /// The generation of the newest records written, held while they are
    /// written (#269): two persists may snapshot in one order and finish in
    /// the other, and an older snapshot must never be written over a newer.
    records_written: Arc<tokio::sync::Mutex<u64>>,
    /// What each metadata slab's copy last held (a hash of the encoded
    /// records), so an unchanged copy is not written and flushed again
    /// (#269): the flow-over persists after every extent, and most of its
    /// persists change one slab's records, not both.
    records_on_slab: Arc<std::sync::Mutex<HashMap<SlabId, u64>>>,
    /// The format v2 stores this manager writes, and what each holds (#158).
    v2: Arc<std::sync::Mutex<persist_v2::V2State>>,
}

/// What a persist writes: taken from the manager in memory, written with no
/// manager lock held (#269).
struct Records {
    generation: u64,
    /// `volumes.dat` in the data directory, unless writing it would replace a
    /// record of real storage with an empty one.
    store: Option<(MetadataStore, metadata::VolumeMetadata)>,
    /// Each v1 metadata slab's own copy, encoded.
    slabs: Vec<(SlabId, Result<Vec<u8>, String>)>,
    /// What changed, for each format v2 store (#158).
    v2: Option<persist_v2::V2Records>,
}

impl VolumeManager {
    /// Create a new VolumeManager.
    ///
    /// `slot_size` is the slab slot size (typically 1 MB for production,
    /// smaller values like 4096 for tests).
    pub fn new(slot_size: u64) -> Self {
        VolumeManager {
            gem: Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new())),
            registry: Arc::new(tokio::sync::RwLock::new(SlabRegistry::new())),
            volumes: HashMap::new(),
            array_slabs: HashMap::new(),
            slot_size,
            metadata_store: None,
            metadata_slabs: Vec::new(),
            retentions: HashMap::new(),
            parents: HashMap::new(),
            templates: std::collections::HashSet::new(),
            fs_info: HashMap::new(),
            owners: HashMap::new(),
            durability: Arc::new(std::sync::Mutex::new(None)),
            generation: std::sync::atomic::AtomicU64::new(1),
            flowing_into: std::sync::Mutex::new(Vec::new()),
            holds: Default::default(),
            records_written: Default::default(),
            records_on_slab: Default::default(),
            v2: Default::default(),
        }
    }

    /// Create a VolumeManager with on-disk metadata persistence.
    pub fn with_data_dir(slot_size: u64, data_dir: PathBuf) -> std::io::Result<Self> {
        let store = MetadataStore::new(data_dir)?;
        Ok(VolumeManager {
            gem: Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new())),
            registry: Arc::new(tokio::sync::RwLock::new(SlabRegistry::new())),
            volumes: HashMap::new(),
            array_slabs: HashMap::new(),
            slot_size,
            metadata_store: Some(store),
            metadata_slabs: Vec::new(),
            retentions: HashMap::new(),
            parents: HashMap::new(),
            templates: std::collections::HashSet::new(),
            fs_info: HashMap::new(),
            owners: HashMap::new(),
            durability: Arc::new(std::sync::Mutex::new(None)),
            generation: std::sync::atomic::AtomicU64::new(1),
            flowing_into: std::sync::Mutex::new(Vec::new()),
            holds: Default::default(),
            records_written: Default::default(),
            records_on_slab: Default::default(),
            v2: Default::default(),
        })
    }

    /// The holds that keep a served volume from being deleted (#267). Give
    /// a clone to whatever serves volumes as devices.
    pub fn holds(&self) -> holds::ServeHolds {
        self.holds.clone()
    }

    // ── Lineage, sealing, filesystem identity (#76) ────────────────────

    /// Mark a volume as a blank to clone from.
    pub fn mark_template(&mut self, id: VolumeId) {
        self.templates.insert(id);
    }

    /// Is this a blank to clone from?
    pub fn is_template(&self, id: &VolumeId) -> bool {
        self.templates.contains(id)
    }

    /// Every blank this node can clone from, smallest first.
    ///
    /// Derived from what is attached rather than read from a registry: a
    /// blank added to an image is a template on the next boot with nothing to
    /// register and nothing to keep in sync.
    pub async fn templates(&self) -> Vec<(VolumeId, String, u64)> {
        let mut out: Vec<(VolumeId, String, u64)> = Vec::new();
        for id in &self.templates {
            if let Some(h) = self.volumes.get(id) {
                out.push((*id, h.name().await, h.capacity_bytes()));
            }
        }
        out.sort_by_key(|(_, _, size)| *size);
        out
    }

    /// The volume this one was cloned from.
    pub fn parent(&self, id: &VolumeId) -> Option<VolumeId> {
        self.parents.get(id).copied()
    }

    /// Every volume cloned directly from `id`.
    pub fn children(&self, id: &VolumeId) -> Vec<VolumeId> {
        let mut v: Vec<VolumeId> = self.parents.iter().filter(|(_, p)| *p == id).map(|(c, _)| *c).collect();
        v.sort_by_key(|c| c.0);
        v
    }

    /// `id`, its parent, its parent's parent, … oldest last. Stops at a cycle
    /// or a parent that no longer exists (a deleted golden leaves the link
    /// as a record of where the data came from).
    pub fn lineage(&self, id: &VolumeId) -> Vec<VolumeId> {
        let mut out = vec![*id];
        let mut cur = *id;
        while let Some(p) = self.parents.get(&cur) {
            if out.contains(p) || out.len() > 1024 {
                break;
            }
            out.push(*p);
            cur = *p;
        }
        out
    }

    pub fn is_sealed(&self, id: &VolumeId) -> bool {
        self.volumes.get(id).map(|h| h.is_sealed()).unwrap_or(false)
    }

    /// Seal a volume: from now on it takes no writes and is what clones are
    /// taken from. `fs`, when given, records what is on it.
    pub async fn seal_volume(&mut self, id: VolumeId, fs: Option<FsInfo>) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        handle.set_sealed(true);
        if let Some(fs) = fs {
            self.fs_info.insert(id, fs);
        }
        self.persist().await;
        Ok(())
    }

    /// The logical block size a volume is presented at (#228).
    pub fn lba(&self, id: &VolumeId) -> Option<u32> {
        self.volumes.get(id).map(|h| h.lba())
    }

    /// Present a volume at another logical block size (512 or 4096, #228).
    ///
    /// The size a GPT and a FAT were laid in is a property of their bytes, so
    /// this is said once, by whoever laid them — `compose/disk` or a create —
    /// and clones inherit it. An initiator already attached keeps what it was
    /// told at connect; the caller refuses a change on a volume being served.
    pub async fn set_lba(&mut self, id: VolumeId, bs: u32) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        if !handle.set_lba(bs) {
            return Err(VolumeError::InvalidSize(format!(
                "a volume is presented at {} or {} bytes per LBA, not {bs}",
                thin::Lba::BOOT,
                thin::Lba::DEFAULT
            )));
        }
        self.persist().await;
        Ok(())
    }

    /// Reopen a sealed volume for writes. Exists for an operator undoing a
    /// mistake; clones already taken keep their own extents either way.
    ///
    /// Unsealing does not make a volume writable on its own: a volume whose
    /// access is read-only stays read-only, because those are two different
    /// statements and undoing one is not consent to undo the other.
    pub async fn unseal_volume(&mut self, id: VolumeId) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        handle.set_sealed(false);
        self.persist().await;
        Ok(())
    }

    /// What a volume's access is set to. Say nothing about sealing — ask
    /// [`writable`](Self::writable) for whether a write would land.
    pub fn access(&self, id: &VolumeId) -> Option<Access> {
        self.volumes.get(id).map(|h| h.access())
    }

    /// Whether a write would be taken: not sealed and not read-only.
    pub fn writable(&self, id: &VolumeId) -> bool {
        self.volumes.get(id).map(|h| h.writable()).unwrap_or(false)
    }

    /// Set a volume read-only or read-write, at any point in its life.
    ///
    /// The lever a consumer needs and sealing is not: sealing declares a
    /// volume to be the master copy clones descend from, which is a one-way
    /// statement about what it *is*. This is a setting on an ordinary
    /// clone — published read-only, handed to a rescue guest read-only,
    /// opened again when it is that volume's turn to be written — and it
    /// moves both ways. Persisted, so it survives a restart.
    pub async fn set_access(&mut self, id: VolumeId, access: Access) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        handle.set_access(access);
        self.persist().await;
        Ok(())
    }

    pub fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo> {
        self.fs_info.get(id)
    }

    /// What this volume belongs to, when anything has said.
    pub fn owner(&self, id: &VolumeId) -> Option<&crate::volume::metadata::Owner> {
        self.owners.get(id)
    }

    /// Record what a volume belongs to, or forget it with `None` (#115).
    ///
    /// Settable after the fact and not only at create time, because the
    /// volumes that most need an owner are the ones that already exist: a
    /// node's data volumes were cloned at boot by `stormpump`, before there
    /// was an apiserver to own them, and adopting them is how the Kubernetes
    /// view is rebuilt from the storage rather than the other way round.
    pub async fn set_owner(
        &mut self,
        id: VolumeId,
        owner: Option<crate::volume::metadata::Owner>,
    ) -> Result<(), VolumeError> {
        if !self.volumes.contains_key(&id) {
            return Err(VolumeError::VolumeNotFound(id));
        }
        match owner {
            Some(o) => {
                self.owners.insert(id, o);
            }
            None => {
                self.owners.remove(&id);
            }
        }
        self.persist().await;
        Ok(())
    }

    /// Every volume nothing claims — the orphan question, answerable at last.
    ///
    /// Templates and sealed volumes are excluded: a blank is owned by the
    /// node and a golden is owned by whatever clones descend from it, and
    /// neither is a candidate for cleanup on the strength of having no owner.
    pub fn unowned(&self) -> Vec<VolumeId> {
        let mut v: Vec<VolumeId> = self
            .volumes
            .keys()
            .filter(|id| !self.owners.contains_key(id) && !self.templates.contains(id))
            .copied()
            .collect();
        v.sort_by_key(|id| id.0);
        v
    }

    pub async fn set_fs_info(&mut self, id: VolumeId, fs: Option<FsInfo>) -> Result<(), VolumeError> {
        self.set_fs_info_deferred(id, fs)?;
        self.persist().await;
        Ok(())
    }

    /// [`set_fs_info`](Self::set_fs_info) without the persist, for a caller
    /// that persists once at the end of a larger step — a mint is a snapshot
    /// and a record, and two metadata writes where one would do is most of
    /// what it cost (#137).
    pub fn set_fs_info_deferred(&mut self, id: VolumeId, fs: Option<FsInfo>) -> Result<(), VolumeError> {
        if !self.volumes.contains_key(&id) {
            return Err(VolumeError::VolumeNotFound(id));
        }
        match fs {
            Some(f) => {
                self.fs_info.insert(id, f);
            }
            None => {
                self.fs_info.remove(&id);
            }
        }
        Ok(())
    }

    /// Record the filesystem UUID a stamp just wrote.
    pub async fn set_fs_uuid(&mut self, id: VolumeId, uuid: uuid::Uuid) -> Result<(), VolumeError> {
        if !self.volumes.contains_key(&id) {
            return Err(VolumeError::VolumeNotFound(id));
        }
        if let Some(f) = self.fs_info.get_mut(&id) {
            f.uuid = Some(uuid);
        }
        self.persist().await;
        Ok(())
    }

    /// A volume by id or by name.
    pub async fn find_volume(&self, key: &str) -> Option<VolumeId> {
        if let Ok(u) = key.parse::<uuid::Uuid>() {
            if self.volumes.contains_key(&VolumeId(u)) {
                return Some(VolumeId(u));
            }
        }
        for (id, h) in &self.volumes {
            if h.name().await == key {
                return Some(*id);
            }
        }
        None
    }

    /// Keep volume metadata inside `slab_id` instead of (or as well as) a
    /// data directory.
    ///
    /// This is what makes a slab self-describing: the extent maps, the volume
    /// names and the sizes travel with the storage rather than beside it.
    /// An image has nowhere else to put them — there is no filesystem in the
    /// picture until the volume this record names has been exported.
    pub fn persist_to_slab(&mut self, slab_id: SlabId) {
        self.metadata_slabs = vec![slab_id];
    }

    /// The same, for a node whose storage is more than one slab. Each slab
    /// is given the volumes that live on it and nothing else.
    pub fn persist_to_slabs(&mut self, slab_ids: Vec<SlabId>) {
        self.metadata_slabs = slab_ids;
    }

    /// Keep metadata in these slabs as well, ahead of the ones already named.
    ///
    /// First, because order is a decision: a volume with no extents yet is
    /// recorded in the first metadata slab of its own role. On a node that has
    /// just laid its own disk, that slab has to be the local one. Otherwise the
    /// record lives only on an appliance clone that the next boot will not
    /// attach.
    pub fn keep_metadata_in_first(&mut self, slab_ids: &[SlabId]) {
        let mut next: Vec<SlabId> = slab_ids.to_vec();
        next.extend(self.metadata_slabs.iter().copied().filter(|s| !slab_ids.contains(s)));
        self.metadata_slabs = next;
    }

    /// A flow-over empties `sources` into `dest` (#258): from now on `dest`'s
    /// record names every volume with a leg on a source as well as its own.
    ///
    /// Until the flow-over reaches it, a golden lives wholly on the
    /// appliance's clone, so the local disk's record left it out — and a
    /// power cut then left a disk that "is missing N mounted volume(s)", which
    /// the initramfs read as not this node's and installed over (11.63 on
    /// server3). Recorded, the extents on the absent clone are what the next
    /// boot fetches from a fresh one (#171).
    pub fn record_flow_over(&self, dest: SlabId, sources: Vec<SlabId>) {
        // `RECORD_FLOW_OFF_258=1`: the old behaviour, for showing the test
        // fails without it.
        if std::env::var_os("RECORD_FLOW_OFF_258").is_some() {
            return;
        }
        // One entry per destination: the system half and, since #285, the
        // data half flow at once, each into its own local slab.
        let mut f = self.flowing_into.lock().unwrap();
        f.retain(|(d, _)| *d != dest);
        f.push((dest, sources));
    }

    /// Which slab, if any, this manager writes its metadata into. The first
    /// of them where there are several.
    pub fn metadata_slab(&self) -> Option<SlabId> {
        self.metadata_slabs.first().copied()
    }

    /// Every slab this manager writes a copy of its metadata into.
    pub fn metadata_slabs(&self) -> &[SlabId] {
        &self.metadata_slabs
    }

    /// Whether this slab carries part of the manager's own record of itself
    /// — what a delete guard has to ask before removing a drive.
    pub fn is_metadata_slab(&self, id: &SlabId) -> bool {
        self.metadata_slabs.contains(id)
    }

    /// Register a RAID array as a backing device for volumes.
    ///
    /// Formats a slab on the device and registers it in the slab registry.
    /// The `array_id` is kept for backward compatibility with callers that
    /// reference arrays by ID.
    pub async fn add_backing_device(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
    ) {
        let slab = match Slab::format(device, self.slot_size, StorageTier::Hot).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to format slab on array {array_id}: {e}");
                return;
            }
        };
        let slab_id = slab.slab_id();
        {
            let mut reg = self.registry.write().await;
            reg.add(slab);
        }
        self.array_slabs.insert(array_id, slab_id);
        tracing::info!("Registered array {array_id} as slab {}", slab_id.0);
    }

    /// Give a volume another name (#122: a staged release's `<name>@<v>`
    /// becomes `<name>`, and the one it replaces `<name>@<N>`). Refused when
    /// another volume has the name. The device, its exports and attachments
    /// are by id and are not touched; the next boot resolves the new name.
    pub async fn rename_volume(&mut self, id: VolumeId, name: &str) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        if name.is_empty() {
            return Err(VolumeError::AllocatorError("a volume needs a name".into()));
        }
        for (other, h) in &self.volumes {
            if *other != id && h.name().await == name {
                return Err(VolumeError::AllocatorError(format!("volume {other} is already called {name}")));
            }
        }
        handle.lock().await.name = name.to_string();
        Ok(())
    }

    /// Register a pre-formatted slab directly.
    pub async fn add_slab(&mut self, slab: Slab) {
        let id = slab.slab_id();
        let mut reg = self.registry.write().await;
        reg.add(slab);
        tracing::info!("Registered slab {}", id.0);
    }

    /// Attach an **existing** slab-formatted device without reformatting.
    ///
    /// Counterpart to `add_backing_device` for the reboot / boot-artifact
    /// path: opens the slab (header + slot table) from the device and
    /// registers it under `array_id`, so `restore()` can resolve volumes
    /// that reference that array. Errors instead of logging — the caller
    /// (initramfs, artifact consumer) must know the attach failed.
    pub async fn open_backing_device(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
    ) -> Result<(), VolumeError> {
        let slab = Slab::open(device).await.map_err(VolumeError::Drive)?;
        self.attach_slab(array_id, slab).await
    }

    /// Register an already-open slab under `array_id`.
    ///
    /// The half of `open_backing_device` that does not re-read the device —
    /// a caller that opened the slab to read its embedded metadata already
    /// holds it, and opening it twice would read the whole slot table again.
    pub async fn attach_slab(
        &mut self,
        array_id: RaidArrayId,
        slab: Slab,
    ) -> Result<(), VolumeError> {
        // Another size is another pool (#156), where the records can say
        // each volume's size (format 2).
        if slab.slot_size() != self.slot_size && !self.records_any_size().await {
            return Err(VolumeError::InvalidSize(format!(
                "slab slot size {} does not match manager slot size {}",
                slab.slot_size(),
                self.slot_size,
            )));
        }
        let slab_id = slab.slab_id();
        {
            let mut reg = self.registry.write().await;
            reg.add(slab);
        }
        self.array_slabs.insert(array_id, slab_id);
        tracing::info!("Opened array {array_id} as existing slab {}", slab_id.0);
        Ok(())
    }

    /// Create a new thin volume on a specific RAID array.
    ///
    /// The `array_id` parameter maps to a slab for placement preference.
    /// The volume can allocate from any slab if the preferred one is full.
    pub async fn create_volume(
        &mut self,
        name: &str,
        virtual_size: u64,
        array_id: RaidArrayId,
    ) -> Result<VolumeId, VolumeError> {
        let Some(slab) = self.array_slabs.get(&array_id).copied() else {
            return Err(VolumeError::AllocatorError(
                format!("no backing device for array {array_id}")
            ));
        };
        self.create_volume_with(name, virtual_size, CreateOptions::pinned_to(slab)).await
    }

    /// The extent size a new volume gets (#156): what was asked, else bulk for
    /// a volume of [`BULK_FROM`] or more where the role has a bulk pool, else
    /// the node's default, else the smallest size the role has. Sizes other
    /// than the default are recorded only by metadata format 2, so they need
    /// every metadata store to be v2 or to become v2 at its next persist.
    async fn choose_extent_size(
        &self,
        role: SlabRole,
        pinned: Option<SlabId>,
        virtual_size: u64,
        asked: Option<u64>,
    ) -> Result<u64, VolumeError> {
        let (sizes, pin_size) = {
            let reg = self.registry.read().await;
            (reg.sizes_in_role(role), pinned.and_then(|p| reg.get(&p).map(|s| s.slot_size())))
        };
        let v2_ok = self.records_any_size().await;
        if let Some(a) = asked {
            if a < 4096 || !a.is_power_of_two() {
                return Err(VolumeError::InvalidSize(format!("extent size {a}: a power of two of 4 KiB or more")));
            }
            if pin_size.is_some_and(|p| p != a) || (pin_size.is_none() && !sizes.is_empty() && !sizes.contains(&a)) {
                return Err(VolumeError::InvalidSize(format!(
                    "no {role} slab with {a}-byte slots to place it in (this node has {sizes:?})"
                )));
            }
            if a != self.slot_size && !v2_ok {
                return Err(VolumeError::InvalidSize(format!(
                    "{a}-byte extents are recorded only by metadata format 2 ([metadata] format = 2)"
                )));
            }
            return Ok(a);
        }
        if let Some(p) = pin_size {
            return Ok(p);
        }
        if v2_ok && virtual_size >= BULK_FROM && sizes.contains(&BULK_EXTENT) {
            return Ok(BULK_EXTENT);
        }
        if sizes.is_empty() || sizes.contains(&self.slot_size) || !v2_ok {
            return Ok(self.slot_size);
        }
        Ok(sizes[0])
    }

    /// Whether every place this manager records volumes can record a
    /// volume's own extent size (#156): metadata format 2, or format 2 the
    /// default (a v1 metadata slab migrates at the next persist).
    async fn records_any_size(&self) -> bool {
        if crate::drive::slab::default_format() == crate::drive::slab::SLAB_VERSION_2 {
            return true;
        }
        if self.metadata_slabs.is_empty() && self.metadata_store.is_none() {
            return true;
        }
        (self.metadata_store.is_none() || self.dir_v2()) && !self.v1_sinks().await
    }

    /// The slab an array's storage is, when the array has one here.
    pub fn array_slab(&self, array_id: &RaidArrayId) -> Option<SlabId> {
        self.array_slabs.get(array_id).copied()
    }

    /// The volumes on an array's slab: pinned to it, or with any leg there.
    pub async fn volumes_on_slab(&self, slab: SlabId) -> Vec<(VolumeId, String, bool)> {
        let on: HashSet<VolumeId> = {
            let gem = self.gem.read().await;
            let mut on: HashSet<VolumeId> = gem
                .resident_ids()
                .into_iter()
                .filter(|v| gem.get_volume_map(v).map(|m| m.all_legs().any(|l| l.slab_id == slab)).unwrap_or(false))
                .collect();
            // A map not in memory says which slabs it is on (#158).
            on.extend(gem.cold_ids().into_iter().filter(|v| gem.cold(v).is_some_and(|c| c.slabs.contains(&slab))));
            on
        };
        let mut out = Vec::new();
        for (id, h) in &self.volumes {
            let pinned = h.pinned_slab() == Some(slab);
            if pinned || on.contains(id) {
                out.push((*id, h.name().await, pinned));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// An array that is one consumer's storage (#150): its slab is formatted
    /// in the data role, *dedicated* (nothing unpinned allocates on it), and
    /// with a metadata region of its own that carries the records of the
    /// volumes pinned to it — so a head that reassembles the same members can
    /// adopt the slab and find its volumes. Returns the slab.
    pub async fn add_dedicated_array(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
    ) -> Result<SlabId, VolumeError> {
        self.add_array_slab(array_id, device, true).await
    }

    /// Format an array's slab in the data role with a metadata region of its
    /// own — dedicated (#150), or in the general pool, which is what a RAID
    /// set on a shelf is (#252): volumes are allocated onto it like onto any
    /// drive, and the slab carries their records, so a restart that
    /// reassembles the set finds them on it.
    pub async fn add_array_slab(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
        dedicated: bool,
    ) -> Result<SlabId, VolumeError> {
        let cap = device.capacity_bytes();
        let mut fmt = crate::drive::slab::SlabFormat::new(self.slot_size, StorageTier::Hot)
            .with_role(SlabRole::Data)
            .with_auto_metadata(cap);
        if dedicated {
            fmt = fmt.dedicated();
        }
        let slab = Slab::format_with(device, fmt)
            .await
            .map_err(|e| VolumeError::AllocatorError(format!("formatting the slab on array {array_id}: {e}")))?;
        let slab_id = slab.slab_id();
        self.registry.write().await.add(slab);
        self.array_slabs.insert(array_id, slab_id);
        if !self.metadata_slabs.contains(&slab_id) {
            self.metadata_slabs.push(slab_id);
        }
        tracing::info!(
            "array {array_id} is {} slab {}",
            if dedicated { "dedicated" } else { "general-pool" },
            slab_id.0
        );
        self.persist().await;
        Ok(slab_id)
    }

    /// Forget an array's slab: refused while any volume is pinned to it or
    /// has a leg there, since removing it would take their data (#150).
    pub async fn remove_array(&mut self, array_id: &RaidArrayId) -> Result<(), VolumeError> {
        let Some(slab) = self.array_slabs.get(array_id).copied() else {
            return Ok(());
        };
        let on = self.volumes_on_slab(slab).await;
        if !on.is_empty() {
            let names: Vec<String> = on.iter().map(|(_, n, _)| n.clone()).collect();
            return Err(VolumeError::AllocatorError(format!(
                "array {array_id} holds {} volume(s): {}",
                on.len(),
                names.join(", ")
            )));
        }
        self.registry.write().await.remove(&slab);
        self.array_slabs.remove(array_id);
        self.metadata_slabs.retain(|s| *s != slab);
        self.persist().await;
        Ok(())
    }

    /// Create a new thin volume without binding it to a specific array.
    ///
    /// Slab placement happens at write time via the registry, so the volume
    /// can allocate from any registered slab. Used by the /v1 management
    /// surface where placement is expressed in nodes, not arrays.
    pub async fn create_volume_any(
        &mut self,
        name: &str,
        virtual_size: u64,
    ) -> Result<VolumeId, VolumeError> {
        self.create_volume_with(name, virtual_size, CreateOptions::default()).await
    }

    /// Create a volume with a redundancy policy.
    ///
    /// The policy is a boundary: a node that cannot put every leg of an
    /// extent on a distinct domain at the policy's rung refuses the volume
    /// now, rather than the first write finding out. Thin sizing is not
    /// checked — a volume larger than any one drive is the normal case,
    /// since each extent picks its own slabs.
    pub async fn create_volume_with(
        &mut self,
        name: &str,
        virtual_size: u64,
        opts: CreateOptions,
    ) -> Result<VolumeId, VolumeError> {
        // The role is settled here, once, and written onto the handle: an
        // unspecified role means "wherever this node keeps volumes", which is
        // the system slabs on a node that has them and the data slabs on a
        // node that does not (#93).
        if let Some(pin) = opts.placement.pinned {
            if !opts.redundancy.is_none() {
                return Err(VolumeError::AllocatorError(format!(
                    "a volume pinned to one slab cannot be {}: its redundancy is the array's",
                    opts.redundancy.spelling()
                )));
            }
            if self.registry.read().await.get(&pin).is_none() {
                return Err(VolumeError::AllocatorError(format!("slab {} is not attached", pin.0)));
            }
        }
        let role = match (opts.placement.pinned, opts.role) {
            // A pinned volume is in its slab's half, whatever else was said.
            (Some(pin), _) => self.registry.read().await.role_of(&pin),
            (None, Some(r)) => r,
            (None, None) => self.registry.read().await.default_role(opts.extent_size.unwrap_or(0)),
        };
        let extent_size = self.choose_extent_size(role, opts.placement.pinned, virtual_size, opts.extent_size).await?;
        let needed = opts.redundancy.scheme.width();
        if needed > 1 {
            let available = self
                .registry
                .read()
                .await
                .distinct_domains_with_space_in_role(&opts.redundancy.spread, role, extent_size);
            if available < needed {
                return Err(VolumeError::InsufficientDomains {
                    policy: opts.redundancy.spelling(),
                    needed,
                    available,
                });
            }
        }
        let placement = PlacementPolicy { role, ..opts.placement };
        let vol = match opts.id {
            Some(id) if self.volumes.contains_key(&id) => {
                return Err(VolumeError::AllocatorError(format!("volume {id} already exists")));
            }
            Some(id) => ThinVolume::restore(id, name.to_string(), virtual_size, extent_size),
            None => ThinVolume::new(name.to_string(), virtual_size, extent_size),
        };
        let id = vol.id();
        let parity = opts.redundancy.scheme.is_parity();
        let handle = Arc::new(ThinVolumeHandle::with_redundancy(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            placement,
            opts.redundancy,
        ));
        if let (Some(store), true) = (&self.metadata_store, parity) {
            handle.use_stripe_log(store.dir());
        }
        self.volumes.insert(id, handle);
        self.persist().await;
        Ok(id)
    }

    /// Take on the slabs already present on a drive, and the volumes they
    /// describe, without writing anything.
    ///
    /// An appliance is handed a whole-disk image and serves it. The goldens
    /// inside it are volumes in a slab in one of its partitions, and until
    /// they are attached the engine serving that image cannot name them —
    /// which is why an image's contents could only be reached by booting a
    /// node from it. Adoption opens them where they lie.
    ///
    /// **Nothing is written.** The slabs are opened, their volume records read
    /// and their mappings restored into the live map. A volume already known
    /// is left exactly as it is: adopting a drive twice, or adopting one whose
    /// volumes another drive already provided, changes nothing.
    ///
    /// **A slab whose slot size disagrees with this manager's is refused.**
    /// That mismatch is not a detail — the volume layer divides by one and the
    /// slab addresses by the other, so every extent would be written across
    /// its neighbours. It is the defect that corrupted the serving path, and
    /// it is not going to be reintroduced through the back door.
    ///
    /// Adoption lasts for this run. It is a runtime action against a drive the
    /// engine has open, not a change to what the engine is configured to hold.
    pub async fn adopt_slabs(
        &mut self,
        found: Vec<crate::drive::discover::FoundSlab>,
    ) -> Result<AdoptReport, VolumeError> {
        let mut report = AdoptReport::default();
        if found.is_empty() {
            return Ok(report);
        }

        let mut metadata_slabs = Vec::new();
        // Slabs whose own metadata region should carry the record from now on.
        let mut adopted_meta: Vec<SlabId> = Vec::new();
        let any_size = self.records_any_size().await;
        {
            let mut reg = self.registry.write().await;
            for f in found {
                let id = f.slab.slab_id();
                if f.slab.slot_size() != self.slot_size && !any_size {
                    return Err(VolumeError::InvalidSize(format!(
                        "slab {} in {} has {}-byte slots and this engine addresses \
                         {}-byte extents: adopting it would write every extent \
                         across its neighbours",
                        id.0, f.label, f.slab.slot_size(), self.slot_size
                    )));
                }
                if reg.get(&id).is_some() {
                    report.already_attached += 1;
                    continue;
                }
                if f.slab.has_metadata_region() {
                    metadata_slabs.push(id);
                }
                adopted_meta.push(id);
                report.slabs.push((id, f.label, f.slab.role().to_string()));
                reg.add(f.slab);
            }
        }

        // The records live in the slab, which is the only place they can live
        // for storage that arrived as a file: there is no data directory
        // belonging to an image.
        // Each record with the role of the slab it was read from: a volume
        // with no extents yet is recorded in a metadata slab of its own role,
        // and that is the only place its role is written down (#141).
        let mut records: Vec<(metadata::VolumeRecord, SlabRole)> = Vec::new();
        let mut arrays_found: Vec<(RaidArrayId, SlabId)> = Vec::new();
        {
            let reg = self.registry.read().await;
            let mut seen: HashSet<VolumeId> = HashSet::new();
            for slab_id in &metadata_slabs {
                let Some(slab) = reg.get(slab_id) else { continue };
                let home = slab.role();
                let doc = match metav2::read_slab(slab).await {
                    Ok(Some(d)) => d,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(slab = %slab_id, "adopt: metadata unreadable: {e}");
                        continue;
                    }
                };
                // The arrays a slab says it is (#150): what lets a volume
                // pinned to an array find that array's slab on a new head.
                for a in &doc.arrays {
                    arrays_found.push((a.array_id, *slab_id));
                }
                for v in doc.volumes {
                    if seen.insert(v.id) {
                        records.push((v, home));
                    }
                }
            }
        }
        for (array, slab) in arrays_found {
            self.array_slabs.entry(array).or_insert(slab);
        }

        // The slot tables of what was just adopted, reconciled with the
        // records the way `restore` does (#171). The records are rewritten
        // only now and then; every allocation since is in a slot table, and
        // mapping from the records alone lost it — a volume came back holding
        // what it held at its last record, not what was flushed since.
        // Each adopted slab's table read once, with no lock held (#155).
        let view = {
            let sources = {
                let reg = self.registry.read().await;
                reg.iter().filter(|(id, _)| adopted_meta.contains(id)).map(|(_, s)| s.view_source()).collect()
            };
            gem::SlotView::read(sources).await.map_err(VolumeError::Drive)?
        };
        let mut rebuilt = GlobalExtentMap::rebuild_from_view(&view);
        let parent_of: HashMap<VolumeId, VolumeId> =
            records.iter().filter_map(|(v, _)| v.parent.map(|p| (v.id, p))).collect();
        let live: HashSet<VolumeId> =
            records.iter().map(|(v, _)| v.id).chain(self.volumes.keys().copied()).collect();
        let lineage = |mut id: VolumeId| -> HashSet<VolumeId> {
            let mut set = HashSet::new();
            while set.insert(id) && set.len() < 256 {
                match parent_of.get(&id) {
                    Some(p) => id = *p,
                    None => break,
                }
            }
            set
        };
        let mut absorbed: HashSet<VolumeId> = HashSet::new();

        for (vrec, home) in records {
            if self.volumes.contains_key(&vrec.id) {
                report.already_known += 1;
                continue;
            }

            // Only what these drives can serve: a leg on a slab that is not
            // attached is dropped (and said so) by the reconciliation.
            {
                let reg = self.registry.read().await;
                reconcile_record(&reg, &view, &mut rebuilt, &vrec, &lineage(vrec.id), &live);
                for (stripe, g) in &vrec.parity {
                    rebuilt.insert_parity(vrec.id, *stripe, g.clone());
                }
            }
            absorbed.insert(vrec.id);

            // Where a volume already lives is what it is — the same rule
            // `restore` uses. A volume adopted out of a data slab that came
            // back as `system` could not allocate into the slab it is sitting
            // in: every later write would look for a system slab and find
            // none.
            let role = {
                let reg = self.registry.read().await;
                vrec.extents
                    .values()
                    .next()
                    .map(|loc| reg.role_of(&loc.slab_id))
                    .unwrap_or(home)
            };
            let extent_size = restored_extent_size(&*self.registry.read().await, &vrec, self.slot_size)?;
            let vol = ThinVolume::restore(vrec.id, vrec.name.clone(), vrec.virtual_size, extent_size);
            let handle = Arc::new(ThinVolumeHandle::with_redundancy(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy {
                    role,
                    pinned: vrec.array_id.and_then(|a| self.array_slabs.get(&a).copied()),
                    ..Default::default()
                },
                vrec.redundancy.clone(),
            ));
            handle.set_failed_slabs(vrec.failed_slabs.iter().copied());
            handle.set_sealed(vrec.sealed);
            handle.set_lba(vrec.lba);
            if vrec.template {
                self.templates.insert(vrec.id);
            }
            handle.set_access(vrec.access);
            if let Some(fs) = vrec.fs.clone() {
                self.fs_info.insert(vrec.id, fs);
            }
            if let Some(owner) = vrec.owner.clone() {
                self.owners.insert(vrec.id, owner);
            }
            self.volumes.insert(vrec.id, handle);
            if let Some(parent) = vrec.parent {
                self.parents.insert(vrec.id, parent);
            }
            report.volumes.push((vrec.id, vrec.name, vrec.virtual_size));
        }
        {
            let mut gem = self.gem.write().await;
            gem.absorb(rebuilt, &absorbed);
            let mut reg = self.registry.write().await;
            raise_shares(&mut reg, &view, &mut gem).await;
        }

        // An adopted slab keeps its own record from here on. Storage that
        // arrived as a file has no data directory of its own, and a store
        // whose contents live only in the process that imported them is a
        // store that does not survive a restart — which is what happened to
        // the first parts store built this way.
        for id in adopted_meta {
            if !self.metadata_slabs.contains(&id) {
                self.metadata_slabs.push(id);
            }
        }
        if !self.metadata_slabs.is_empty() {
            self.persist().await;
        }

        Ok(report)
    }

    /// Move a volume onto another tier, without dragging anything else with it.
    ///
    /// Extent by extent, so the volume keeps serving throughout, and shared
    /// extents are copied rather than moved — see
    /// [`ThinVolumeHandle::relocate_extent`]. What that yields is the demotion
    /// rule an appliance actually wants: last month's image ends up whole on
    /// the slow drive, whatever it still shares with this month's stays on the
    /// fast one, and whatever nothing else references gives its space back.
    ///
    /// The caller decides *when*. This decides *what moves*.
    pub async fn retier_volume(
        &mut self,
        id: VolumeId,
        tier: StorageTier,
    ) -> Result<RetierReport, VolumeError> {
        let handle = self
            .volumes
            .get(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?
            .clone();
        let role = handle.placement_role();

        let dest = {
            let reg = self.registry.read().await;
            reg.best_slab_for_tier_in_role(tier, role, handle.extent_size())
                .ok_or_else(|| VolumeError::InvalidSize(format!(
                    "no {tier} slab in the {role} role to move to"
                )))?
        };

        gem::ensure_resident(&self.gem, id).await.map_err(|e| VolumeError::InvalidSize(e.to_string()))?;
        let todo: Vec<u64> = {
            let gem = self.gem.read().await;
            gem.volume_extents(&id)
                .map(|it| {
                    it.filter(|(_, loc)| loc.legs().any(|l| l.slab_id != dest))
                        .map(|(vext, _)| vext)
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut report = RetierReport { destination: dest, ..Default::default() };
        for vext in todo {
            match handle.relocate_extent(vext, dest).await {
                Ok(crate::volume::thin::Relocated::Moved) => report.moved += 1,
                Ok(crate::volume::thin::Relocated::Copied) => report.copied += 1,
                Ok(_) => report.already += 1,
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(volume = %id, extent = vext, "retier: {e}");
                    // A handful of bad extents is worth reporting and pushing
                    // past; a wall of them means something is wrong with the
                    // destination and every further attempt makes it worse.
                    if report.failed > 16 {
                        tracing::error!(volume = %id, "retier: giving up after repeated failures");
                        break;
                    }
                }
            }
            // One extent per iteration, so I/O to the volume interleaves with
            // the move rather than queueing behind all of it.
            tokio::task::yield_now().await;
        }

        self.persist().await;
        tracing::info!(
            volume = %id, tier = %tier,
            moved = report.moved, copied = report.copied, failed = report.failed,
            "retier complete"
        );
        Ok(report)
    }

    /// Compose a volume from other volumes, sharing their extents.
    ///
    /// The components are usually sealed goldens, and the result is a disk
    /// made *of* them rather than a copy of them: nothing is read, nothing is
    /// written, and the slots they already occupy are simply referenced once
    /// more. Writing to the result copies on write, the same as a clone.
    ///
    /// Each component is placed at the byte offset given, and takes its span
    /// from the source's virtual size — a sparse golden still owns the whole
    /// span it was sized for.
    pub async fn compose_volume(
        &mut self,
        name: &str,
        declared_size: Option<u64>,
        placements: &[(VolumeId, u64)],
    ) -> Result<VolumeId, VolumeError> {
        let mut components = Vec::with_capacity(placements.len());
        let mut sizes: Vec<u64> = Vec::new();
        for (source, at) in placements {
            let handle = self.volumes.get(source)
                .ok_or(VolumeError::VolumeNotFound(*source))?;
            components.push(compose::Component {
                source: *source,
                at: *at,
                span: handle.capacity_bytes(),
            });
            sizes.push(handle.extent_size());
            gem::ensure_resident(&self.gem, *source).await.map_err(|e| VolumeError::InvalidSize(e.to_string()))?;
        }
        // A composition shares its members' extents: one size (#156).
        sizes.sort_unstable();
        sizes.dedup();
        let extent_size = match sizes.as_slice() {
            [] => self.slot_size,
            [one] => *one,
            many => {
                return Err(VolumeError::InvalidSize(format!(
                    "the components have different extent sizes ({many:?}): a composition shares their extents"
                )))
            }
        };

        let vol = {
            let mut gem = self.gem.write().await;
            let mut reg = self.registry.write().await;
            compose::compose_volume(
                name, declared_size, extent_size, &components, &mut gem, &mut reg,
            ).await?
        };

        let id = vol.id();
        // The composition inherits the first component's policy and role: the
        // result lives beside what it is made of, and a disk composed of
        // system goldens is a system volume.
        let handle = Arc::new(match placements.first() {
            Some((first, _)) => self.inherit_handle(vol, first),
            None => ThinVolumeHandle::with_redundancy(
                vol, self.gem.clone(), self.registry.clone(),
                PlacementPolicy::default(), RedundancyPolicy::none(),
            ),
        });
        self.volumes.insert(id, handle);
        if let Some((first, _)) = placements.first() {
            self.record_lineage(id, *first);
        }
        self.persist().await;
        Ok(id)
    }

    /// A volume's policy.
    pub fn redundancy(&self, id: &VolumeId) -> Option<RedundancyPolicy> {
        self.volumes.get(id).map(|h| h.redundancy())
    }

    /// Change a volume's policy; `none`/`mirror` to `mirror` only. Takes
    /// effect at the next `resync_volume`.
    pub async fn set_redundancy(&mut self, id: VolumeId, policy: RedundancyPolicy) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        handle.check_transition(&policy)?;
        let needed = policy.scheme.width();
        if needed > 1 {
            let available = self
                .registry
                .read()
                .await
                .distinct_domains_with_space_in_role(&policy.spread, handle.placement_role(), handle.extent_size());
            if available < needed {
                return Err(VolumeError::InsufficientDomains {
                    policy: policy.spelling(),
                    needed,
                    available,
                });
            }
        }
        handle.set_redundancy(policy)?;
        self.persist().await;
        Ok(())
    }

    /// Rebuild what a volume is missing and clear the slabs it stopped
    /// trusting. See [`ThinVolumeHandle::resync`].
    pub async fn resync_volume(&mut self, id: VolumeId, verify: bool) -> Result<ResyncReport, VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        let mut report = handle.resync_with(&ResyncOptions { verify, ..Default::default() }).await;
        // The replaced slots are freed only once the map that stopped naming
        // them is on disk.
        self.persist().await;
        let owed = std::mem::take(&mut report.owed);
        handle.release_slots(&owed).await;
        Ok(report)
    }

    /// Stop trusting a slab in every redundant volume that has a leg on it —
    /// what a drive-health report from stormdrive turns into (#70 item 4).
    /// An unreplicated volume's only copy is left alone: distrusting it
    /// would make the data unreadable rather than safer. Returns the ids of
    /// the volumes affected.
    pub async fn distrust_slab(&mut self, slab: SlabId) -> Vec<VolumeId> {
        let mut touched = Vec::new();
        let gem = self.gem.read().await;
        for (id, handle) in &self.volumes {
            if handle.redundancy().is_none() {
                continue;
            }
            let has_leg = gem
                .get_volume_map(id)
                .map(|m| m.all_legs().any(|l| l.slab_id == slab))
                .unwrap_or(false);
            if has_leg {
                let mut set: std::collections::HashSet<SlabId> = handle.failed_slabs().into_iter().collect();
                if set.insert(slab) {
                    handle.set_failed_slabs(set);
                    touched.push(*id);
                }
            }
        }
        drop(gem);
        if !touched.is_empty() {
            self.persist().await;
        }
        touched
    }

    /// Change a volume's policy to or from parity by rebuilding its
    /// placement: every extent is copied into a scratch volume with the new
    /// policy, the scratch map becomes the volume's, and the old slots are
    /// released. Holds the volume's mapping lock throughout, so it is an
    /// offline operation — the API refuses it while the volume is exported.
    pub async fn restripe(&mut self, id: VolumeId, policy: RedundancyPolicy) -> Result<RestripeReport, VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        let needed = policy.scheme.width();
        if needed > 1 {
            let available = self
                .registry
                .read()
                .await
                .distinct_domains_with_space_in_role(&policy.spread, handle.placement_role(), handle.extent_size());
            if available < needed {
                return Err(VolumeError::InsufficientDomains { policy: policy.spelling(), needed, available });
            }
        }
        let (name, size) = {
            let v = handle.lock().await;
            (v.name.clone(), v.virtual_size)
        };
        let scratch = ThinVolume::new(format!("{name}-restripe"), size, self.slot_size);
        let scratch_id = scratch.id();
        let dest = Arc::new(ThinVolumeHandle::with_redundancy(
            scratch,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy { role: handle.placement_role(), ..Default::default() },
            policy.clone(),
        ));

        let extents: Vec<u64> = {
            let gem = self.gem.read().await;
            gem.volume_extents(&id).map(|it| it.map(|(v, _)| v).collect()).unwrap_or_default()
        };
        let _hold = handle.lock().await;
        let mut buf = vec![0u8; self.slot_size as usize];
        let mut copied = 0usize;
        for vext in &extents {
            let off = vext * self.slot_size;
            if let Err(e) = handle.read(off, &mut buf).await {
                self.discard_scratch(scratch_id).await;
                return Err(VolumeError::Drive(e));
            }
            if let Err(e) = dest.write(off, &buf).await {
                self.discard_scratch(scratch_id).await;
                return Err(VolumeError::Drive(e));
            }
            copied += 1;
        }
        if let Err(e) = dest.flush().await {
            self.discard_scratch(scratch_id).await;
            return Err(VolumeError::Drive(e));
        }

        // Swap: the volume takes the scratch placement; the old one goes.
        let old = {
            let mut gem = self.gem.write().await;
            gem.rename_volume(scratch_id, id)
        };
        let mut released = 0usize;
        if let Some(old) = old {
            let mut reg = self.registry.write().await;
            let mut by_slab: HashMap<SlabId, Vec<u64>> = HashMap::new();
            for leg in old.all_legs() {
                by_slab.entry(leg.slab_id).or_default().push(leg.slot_idx);
            }
            for (slab_id, slots) in by_slab {
                if let Some(slab) = reg.get_mut(&slab_id) {
                    match slab.dec_ref_batch(&slots).await {
                        Ok(o) => released += o.freed,
                        Err(e) => tracing::warn!(volume = %id, slab = %slab_id, "restripe could not release old slots: {e}"),
                    }
                }
            }
        }
        handle.force_redundancy(policy.clone());
        handle.set_failed_slabs(Vec::new());
        drop(_hold);
        self.persist().await;
        Ok(RestripeReport { extents_copied: copied, slots_released: released, redundancy: policy.spelling() })
    }

    async fn discard_scratch(&self, scratch_id: VolumeId) {
        let mut gem = self.gem.write().await;
        let mut reg = self.registry.write().await;
        let _ = snapshot::delete_snapshot(scratch_id, &mut gem, &mut reg).await;
    }

    /// Whether a volume's data is all there and all protected.
    pub async fn health(&self, id: &VolumeId) -> Option<VolumeHealth> {
        match self.volumes.get(id) {
            Some(h) => Some(h.health().await),
            None => None,
        }
    }

    /// Snapshot several volumes at a single consistency point.
    ///
    /// Holds the GEM and slab-registry locks across every member clone, so
    /// no write can allocate or COW between the first and last snapshot —
    /// this is the single fence VolumeGroupSnapshot semantics require.
    pub async fn create_snapshots_atomic(
        &mut self,
        sources: &[(VolumeId, String)],
    ) -> Result<Vec<VolumeId>, VolumeError> {
        let mut params = Vec::with_capacity(sources.len());
        for (source_id, name) in sources {
            let handle = self.volumes.get(source_id)
                .ok_or(VolumeError::VolumeNotFound(*source_id))?
                .clone();
            let vol = handle.lock().await;
            params.push((*source_id, name.clone(), vol.virtual_size, vol.slot_size));
        }

        let mut snaps = Vec::with_capacity(sources.len());
        {
            let mut gem = self.gem.write().await;
            let mut reg = self.registry.write().await;
            for (source_id, name, virtual_size, slot_size) in &params {
                let snap = snapshot::create_snapshot(
                    *source_id, name, *virtual_size, *slot_size,
                    &mut gem, &mut reg,
                ).await?;
                snaps.push(snap);
            }
        }

        let mut ids = Vec::with_capacity(snaps.len());
        for (snap, (source_id, _)) in snaps.into_iter().zip(sources) {
            let snap_id = snap.id();
            let handle = Arc::new(self.inherit_handle(snap, source_id));
            self.volumes.insert(snap_id, handle);
            self.record_lineage(snap_id, *source_id);
            ids.push(snap_id);
        }
        self.persist().await;
        Ok(ids)
    }

    /// Delete a volume, freeing all slab slots.
    pub async fn delete_volume(&mut self, id: VolumeId) -> Result<(), VolumeError> {
        self.delete_volume_erasing(id, None).await
    }

    /// Delete a volume, overwriting the slots it frees with at least `erase`
    /// (#286) — more than the node's default, never less. Slots it still
    /// shares with another volume are not freed, so not erased.
    pub async fn delete_volume_erasing(
        &mut self,
        id: VolumeId,
        erase: Option<crate::drive::erase::EraseLevel>,
    ) -> Result<(), VolumeError> {
        // Never under something serving it (#267): a ublk device mounted
        // under running containers read zeros and other volumes' data once
        // its volume was deleted and its slots reused.
        let by = self.holds.held_by(id.0);
        if !by.is_empty() {
            return Err(VolumeError::InUse { id, by });
        }
        let _handle = self.volumes.remove(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?;
        self.parents.remove(&id);
        self.fs_info.remove(&id);
        self.owners.remove(&id);
        self.retentions.remove(&id);

        // Remove all extents from GEM and dec_ref on slabs: their table pages
        // read first, with no lock held (#155).
        self.prefetch_volume(id).await;
        let mut gem = self.gem.write().await;
        let mut reg = self.registry.write().await;
        reg.set_erase_override(erase);
        let res = snapshot::delete_snapshot(id, &mut gem, &mut reg).await;
        reg.set_erase_override(None);
        res?;
        drop(gem);
        drop(reg);

        self.persist().await;
        Ok(())
    }

    /// Grow a volume to `new_size` bytes.
    ///
    /// **Growth only.** A shrink comes back as
    /// [`VolumeError::ShrinkRefused`]: the extents past the new end are freed
    /// immediately, and xfs — which is what everything above this actually
    /// runs — cannot shrink at all, so a shrink of a mounted volume destroys
    /// live filesystem data with nothing to undo it (#19). A caller that means
    /// it uses [`VolumeManager::shrink_volume`]; a caller that wants a smaller
    /// volume with its data intact wants a move, which is a different
    /// operation (#20).
    pub async fn resize_volume(&mut self, id: VolumeId, new_size: u64) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        let current = handle.capacity_bytes();
        if new_size < current {
            return Err(VolumeError::ShrinkRefused { current, requested: new_size });
        }
        self.resize_volume_unchecked(id, new_size).await
    }

    /// Shrink a volume, freeing every extent past the new end.
    ///
    /// Separate from [`VolumeManager::resize_volume`] so that destroying data
    /// is something a caller has to name, rather than something it can reach by
    /// passing a smaller number to the same function (#19). Nothing checks what
    /// is on the volume — that is the caller's to know.
    pub async fn shrink_volume(&mut self, id: VolumeId, new_size: u64) -> Result<(), VolumeError> {
        self.resize_volume_unchecked(id, new_size).await
    }

    async fn resize_volume_unchecked(
        &mut self,
        id: VolumeId,
        new_size: u64,
    ) -> Result<(), VolumeError> {
        if new_size == 0 {
            return Err(VolumeError::InvalidSize("size must be > 0".to_string()));
        }
        let handle = self.volumes.get(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?
            .clone();
        handle.resize(new_size).await?;
        self.persist().await;
        Ok(())
    }

    /// Discard a clone's divergence, returning it to its source's contents.
    ///
    /// Cheaper than deleting and re-cloning: only the extents the clone wrote
    /// are touched, so a container restart costs what that container changed
    /// rather than the size of the golden image it started from.
    pub async fn reset_volume(
        &mut self,
        clone_id: VolumeId,
        source_id: VolumeId,
    ) -> Result<snapshot::ResetStats, VolumeError> {
        if !self.volumes.contains_key(&clone_id) {
            return Err(VolumeError::VolumeNotFound(clone_id));
        }
        if !self.volumes.contains_key(&source_id) {
            return Err(VolumeError::VolumeNotFound(source_id));
        }

        let stats = {
            let mut gem = self.gem.write().await;
            let mut reg = self.registry.write().await;
            snapshot::reset_to_source(clone_id, source_id, &mut gem, &mut reg).await?
        };
        self.persist().await;
        Ok(stats)
    }

    /// Get a volume handle as a `BlockDevice` for target protocols.
    pub fn get_volume(&self, id: &VolumeId) -> Option<Arc<dyn BlockDevice>> {
        self.volumes.get(id).map(|h| h.clone() as Arc<dyn BlockDevice>)
    }

    /// Get a volume handle for management operations.
    pub fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>> {
        self.volumes.get(id).cloned()
    }

    /// Create a snapshot of an existing volume.
    pub async fn create_snapshot(
        &mut self,
        source_id: VolumeId,
        name: &str,
    ) -> Result<VolumeId, VolumeError> {
        let id = self.create_snapshot_deferred(source_id, name).await?;
        self.persist().await;
        Ok(id)
    }

    /// [`create_snapshot`](Self::create_snapshot) without the persist — the
    /// caller persists once it has finished with the new volume (#137).
    pub async fn create_snapshot_deferred(
        &mut self,
        source_id: VolumeId,
        name: &str,
    ) -> Result<VolumeId, VolumeError> {
        let source_handle = self.volumes.get(&source_id)
            .ok_or(VolumeError::VolumeNotFound(source_id))?
            .clone();
        let source_vol = source_handle.lock().await;
        let virtual_size = source_vol.virtual_size;
        let slot_size = source_vol.slot_size;
        drop(source_vol);

        self.prefetch_volume(source_id).await;
        let snap = {
            let mut gem = self.gem.write().await;
            let mut reg = self.registry.write().await;
            snapshot::create_snapshot(
                source_id, name, virtual_size, slot_size,
                &mut gem, &mut reg,
            ).await?
        };
        let snap_id = snap.id();
        let snap_handle = Arc::new(self.inherit_handle(snap, &source_id));
        self.volumes.insert(snap_id, snap_handle);
        self.record_lineage(snap_id, source_id);
        Ok(snap_id)
    }

    /// Which half of the node's mutable storage a volume allocates from.
    pub fn volume_role(&self, id: &VolumeId) -> Option<SlabRole> {
        self.volumes.get(id).map(|h| h.placement_role())
    }

    /// Copy a volume into slabs of a different role — a clone that **shares
    /// nothing**.
    ///
    /// A copy-on-write clone shares its source's slots, so a clone is only as
    /// durable as the slab its source is in. That is the right trade inside
    /// one role and the wrong one across the boundary: a clone of a system
    /// golden is a *system* volume however it is named, and an install
    /// replaces the slab holding every extent it never wrote (#88).
    ///
    /// There is no way to make that sharing safe — a slot cannot be in two
    /// partitions — so the crossing costs a real copy: the source's allocated
    /// bytes, once, and the result depends on nothing in the slab it came
    /// from. Lineage and the filesystem record are inherited as with any
    /// clone, so the caller still stamps a fresh filesystem UUID.
    ///
    /// The source is not locked, for the same reason a snapshot does not lock
    /// it: a sealed volume cannot change, and an unsealed one is the caller's
    /// consistency question.
    pub async fn copy_volume(
        &mut self,
        source_id: VolumeId,
        name: &str,
        role: SlabRole,
    ) -> Result<VolumeId, VolumeError> {
        let source = self
            .volumes
            .get(&source_id)
            .ok_or(VolumeError::VolumeNotFound(source_id))?
            .clone();
        let virtual_size = source.lock().await.virtual_size;
        let opts = CreateOptions {
            redundancy: source.redundancy(),
            placement: PlacementPolicy::default(),
            role: Some(role),
            extent_size: Some(source.extent_size()),
            id: None,
        };
        let dest_id = self.create_volume_with(name, virtual_size, opts).await?;
        let dest = self
            .volumes
            .get(&dest_id)
            .ok_or(VolumeError::VolumeNotFound(dest_id))?
            .clone();

        // Only the mapped extents: an unmapped one reads as zeros on both
        // sides, and writing it would cost the destination a slot per hole —
        // the same thin provisioning the image builder is careful about.
        let extents: Vec<u64> = {
            let gem = self.gem.read().await;
            gem.volume_extents(&source_id)
                .map(|it| it.map(|(v, _)| v).collect())
                .unwrap_or_default()
        };
        let mut buf = vec![0u8; self.slot_size as usize];
        for vext in &extents {
            let off = vext * self.slot_size;
            let failed = match source.read(off, &mut buf).await {
                Err(e) => Some(e),
                Ok(_) => dest.write(off, &buf).await.err(),
            };
            if let Some(e) = failed {
                drop(dest);
                let _ = self.delete_volume(dest_id).await;
                return Err(VolumeError::Drive(e));
            }
        }
        if let Err(e) = dest.flush().await {
            drop(dest);
            let _ = self.delete_volume(dest_id).await;
            return Err(VolumeError::Drive(e));
        }
        drop(dest);

        self.record_lineage(dest_id, source_id);
        self.persist().await;
        tracing::info!(
            "volume {source_id} copied into a {role} slab as '{name}' ({dest_id}): \
             {} extent(s), sharing nothing with the source",
            extents.len()
        );
        Ok(dest_id)
    }

    /// A clone descends from its source and starts out carrying the same
    /// filesystem (same UUID, until something stamps it — which the
    /// filesystem-aware clone path does).
    fn record_lineage(&mut self, child: VolumeId, source: VolumeId) {
        self.parents.insert(child, source);
        if let Some(fs) = self.fs_info.get(&source).cloned() {
            self.fs_info.insert(child, fs);
        }
    }

    /// A clone is protected the way its source is: its shared extents
    /// already are, and every copy-on-write will be.
    ///
    /// The placement role is inherited for the same reason and is the
    /// stronger case: a clone shares the source's slots, so a clone of a
    /// data volume that copied-on-write into a *system* slab would put half
    /// of the node's identity in the half an install replaces (#88).
    fn inherit_handle(&self, vol: ThinVolume, source_id: &VolumeId) -> ThinVolumeHandle {
        let (policy, failed, role, pinned, lba) = match self.volumes.get(source_id) {
            Some(src) => (src.redundancy(), src.failed_slabs(), src.placement_role(), src.pinned_slab(), src.lba()),
            None => (RedundancyPolicy::none(), Vec::new(), SlabRole::System, None, thin::Lba::DEFAULT),
        };
        // A clone of a pinned volume shares its slots on that slab, and its
        // copy-on-writes stay there with them (#150).
        let handle = ThinVolumeHandle::with_redundancy(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy { role, pinned, ..Default::default() },
            policy,
        );
        handle.set_failed_slabs(failed);
        // A clone is read by whoever read its source: a boot disk's clone is
        // still what firmware reads, at the size its GPT and FAT were laid in
        // (#228).
        handle.set_lba(lba);
        handle
    }

    /// List all volumes: (id, name, virtual_size, allocated).
    pub async fn list_volumes(&self) -> Vec<(VolumeId, String, u64, u64)> {
        let mut list = Vec::with_capacity(self.volumes.len());
        for (id, handle) in &self.volumes {
            let name = handle.name().await;
            let allocated = handle.allocated().await;
            list.push((*id, name, handle.capacity_bytes(), allocated));
        }
        list
    }

    /// The slab slot size every volume in this manager is measured in.
    ///
    /// Fixed when the manager is built and shared by every slab it holds:
    /// `attach_slab` refuses one that disagrees, because an extent map means
    /// different things at different slot sizes.
    pub fn slot_size(&self) -> u64 {
        self.slot_size
    }

    /// What a clone has written since it was taken from its golden.
    ///
    /// The audit a copy-on-write clone makes possible: a clone that shares
    /// every extent with its golden has provably never been written, and the
    /// answer costs a map comparison rather than a scan of either volume.
    ///
    /// What it does *not* say is whether the content is what it should be —
    /// an extent can be rewritten with identical bytes. For that, compare the
    /// files; this is the cheap check that says whether it is worth doing.
    pub async fn divergence(
        &self,
        clone_id: VolumeId,
        golden_id: VolumeId,
    ) -> snapshot::Divergence {
        let gem = self.gem.read().await;
        snapshot::divergence(&gem, clone_id, golden_id, self.slot_size)
    }

    /// Say whether a volume is meant to be kept or thrown away.
    ///
    /// Persisted with the volume, so the answer survives a restart and does
    /// not depend on whoever happens to mount it next.
    pub async fn set_retention(&mut self, id: VolumeId, retention: Retention) {
        self.retentions.insert(id, retention);
        self.persist().await;
    }

    /// What a volume is for. [`Retention::Keep`] unless something said
    /// otherwise — silence must not throw data away.
    pub fn retention(&self, id: &VolumeId) -> Retention {
        self.retentions.get(id).copied().unwrap_or_default()
    }

    /// Every volume that is meant to be thrown away.
    ///
    /// What a node reads at boot to know which containers start from their
    /// golden again rather than from where they were left.
    pub fn ephemeral(&self) -> Vec<VolumeId> {
        self.retentions
            .iter()
            .filter(|(_, r)| **r == Retention::Ephemeral)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get the shared GEM.
    pub fn gem(&self) -> &Arc<tokio::sync::RwLock<GlobalExtentMap>> {
        &self.gem
    }

    /// Read the slot table pages of every slot a volume maps into their
    /// slabs' caches, holding no lock while the device is read (#155): what
    /// a delete or a clone changes next under the registry lock.
    pub async fn prefetch_volume(&self, id: VolumeId) {
        // What follows a prefetch (a delete, a clone) needs the map.
        if let Err(e) = gem::ensure_resident(&self.gem, id).await {
            tracing::error!("volume {}: loading its extent map: {e}", id.0);
        }
        let by_slab: HashMap<SlabId, Vec<u64>> = {
            let gem = self.gem.read().await;
            let mut m: HashMap<SlabId, Vec<u64>> = HashMap::new();
            if let Some(map) = gem.get_volume_map(&id) {
                for leg in map.all_legs() {
                    m.entry(leg.slab_id).or_default().push(leg.slot_idx);
                }
            }
            m
        };
        let tables: Vec<_> = {
            let reg = self.registry.read().await;
            by_slab.into_iter().filter_map(|(s, idx)| reg.get(&s).map(|slab| (slab.table(), idx))).collect()
        };
        for (t, idx) in tables {
            t.prefetch(idx).await;
        }
    }

    /// Get the shared SlabRegistry.
    pub fn registry(&self) -> &Arc<tokio::sync::RwLock<SlabRegistry>> {
        &self.registry
    }

    /// Persist all volume metadata to disk, including each volume's extent
    /// map. No-op if no data_dir configured.
    ///
    /// The extent maps are the piece slab slot tables cannot reconstruct: a
    /// COW snapshot's shared slots are recorded under the original writer,
    /// so without this file a snapshot reads as zeros after reattach (#13).
    /// See the `generation` field: monotonic, never reused within a run.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The RAID array a slab sits on, when it was created on one.
    pub fn array_of_slab(&self, slab: &SlabId) -> Option<RaidArrayId> {
        self.array_slabs.iter().find(|(_, s)| *s == slab).map(|(a, _)| *a)
    }

    pub async fn persist(&self) {
        // A persist made for the API or a consumer is foreground work: a
        // flow-over gives the disk back while there is any (#269). Its own
        // persists are `persist_detached` and do not count.
        crate::volume::thin::FOREGROUND_IO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let result = match self.records(generation).await {
            None => Ok(()),
            Some(records) => {
                Self::sync_then_write(&self.registry, &self.records_written, &self.records_on_slab, records).await
            }
        };
        Self::persisted(&self.durability, result);
        if let Some(mb) = Self::forced_cache() {
            self.evict_idle(mb << 20).await;
        }
    }

    /// [`persist`](Self::persist) holding the manager only to take the
    /// records (#269). The flushes and the writes run with no lock, so the
    /// API (which takes the manager for nearly everything) does not wait
    /// behind a caller that persists over and over: the flow-over persists
    /// after every extent it moves, and on server3's spinning disk each
    /// persist was several flushes of seconds each.
    pub async fn persist_detached(vm: &tokio::sync::Mutex<VolumeManager>) {
        let (registry, written, on_slab, durability, records) = {
            let g = vm.lock().await;
            let generation = g.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            (
                g.registry.clone(),
                g.records_written.clone(),
                g.records_on_slab.clone(),
                g.durability.clone(),
                g.records(generation).await,
            )
        };
        let result = match records {
            None => Ok(()),
            Some(r) => Self::sync_then_write(&registry, &written, &on_slab, r).await,
        };
        Self::persisted(&durability, result);
    }

    fn persisted(durability: &std::sync::Mutex<Option<String>>, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                if let Some(prev) = durability.lock().unwrap().take() {
                    tracing::info!("volume metadata is being written again (was: {prev})");
                }
            }
            Err(e) => {
                // Not a warning. Every write this node acknowledges from here
                // is one it cannot bring back, and the operation that caused
                // this has already returned success to its caller.
                tracing::error!(
                    "DURABILITY: volume metadata was not written: {e}. Volumes created or \
                     changed from now on will not survive a restart"
                );
                *durability.lock().unwrap() = Some(e.to_string());
            }
        }
    }

    /// Stop treating a volume's slabs as failed, and prove it by reading.
    ///
    /// A leg marked failed stays marked and the marking is written into the
    /// record, which is right when the media is gone and wrong when the
    /// marking came from something else — a request the device refused, a
    /// cable, a slab that was not attached yet. There was no way back short
    /// of a restripe, so a volume could be permanently unreadable with
    /// nothing actually wrong with it.
    ///
    /// Not a blind reset: the legs are cleared, the volume is read, and
    /// anything that fails for real marks itself again on the way through.
    /// The report says which slabs came back and which did not.
    pub async fn clear_failed_legs(&self, id: VolumeId) -> anyhow::Result<ClearFailedReport> {
        let handle = self
            .volumes
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("volume {} not found", id.0))?
            .clone();
        let was: Vec<SlabId> = handle.failed_slabs();
        if was.is_empty() {
            return Ok(ClearFailedReport { cleared: Vec::new(), still_failed: Vec::new() });
        }
        handle.set_failed_slabs(Vec::new());

        // One aligned block, so the read itself cannot be the thing that
        // fails. Errors are ignored here on purpose — what matters is which
        // slabs the read marked again, not what the caller sees.
        let bs = handle.block_size().max(1) as usize;
        let mut buf = crate::drive::dma::DmaBuf::zeroed(bs);
        let _ = handle.read(0, &mut buf).await;

        let still: Vec<SlabId> = handle.failed_slabs();
        let cleared: Vec<String> = was
            .iter()
            .filter(|s| !still.contains(s))
            .map(|s| s.0.to_string())
            .collect();
        self.persist().await;
        Ok(ClearFailedReport {
            cleared,
            still_failed: still.iter().map(|s| s.0.to_string()).collect(),
        })
    }

    /// Why the last persist failed, if it did. `None` while the record is
    /// being written.
    pub fn durability_fault(&self) -> Option<String> {
        self.durability.lock().unwrap().clone()
    }

    /// What each metadata slab's record currently encodes to, against what
    /// that slab reserved for it.
    ///
    /// The margin here is the thing to watch: a record grows with every
    /// volume and with every extent each volume maps, so a slab that fits
    /// today stops fitting silently as it fills.
    pub async fn metadata_pressure(&self) -> Vec<MetadataPressure> {
        let mut out = Vec::new();
        if !self.v1_sinks().await {
            // Every metadata slab in format v2 (#158): what each store holds,
            // with no map read.
            let reg = self.registry.read().await;
            let st = self.v2.lock().unwrap_or_else(|e| e.into_inner());
            for slab_id in &self.metadata_slabs {
                let sink = persist_v2::Sink::Slab(*slab_id);
                let (needed, capacity) = match st.usage(sink) {
                    Some(u) => (u.pages_used * metav2::PAGE, u.pages_total * metav2::PAGE),
                    None => (0, reg.get(slab_id).map(|s| s.metadata_capacity()).unwrap_or(0)),
                };
                out.push(MetadataPressure {
                    slab_id: slab_id.0.to_string(),
                    volumes: st.held_count(sink),
                    needed_bytes: needed,
                    capacity_bytes: capacity,
                    fits: capacity > 0 && needed * 10 <= capacity * 9,
                });
            }
            return out;
        }
        if let Err(e) = gem::ensure_all_resident(&self.gem).await {
            tracing::error!("metadata pressure: loading extent maps: {e}");
        }
        let reg = self.registry.read().await;
        for (slab_id, meta) in self.per_slab_metadata().await {
            if reg.get(&slab_id).is_some_and(|s| s.format_version() == crate::drive::slab::SLAB_VERSION_2) {
                // Format v2 (#158): pages in use of the region's pages. A
                // store not opened yet (nothing persisted since the start)
                // reports what the record would need as nothing.
                let usage = self.v2.lock().unwrap_or_else(|e| e.into_inner()).usage(persist_v2::Sink::Slab(slab_id));
                let (needed, capacity) = match usage {
                    Some(u) => (u.pages_used * metav2::PAGE, u.pages_total * metav2::PAGE),
                    None => (0, reg.get(&slab_id).map(|s| s.metadata_capacity()).unwrap_or(0)),
                };
                out.push(MetadataPressure {
                    slab_id: slab_id.0.to_string(),
                    volumes: meta.volumes.len(),
                    needed_bytes: needed,
                    capacity_bytes: capacity,
                    fits: capacity > 0 && needed * 10 <= capacity * 9,
                });
                continue;
            }
            let needed = MetadataStore::encode(&meta).map(|b| b.len() as u64).unwrap_or(0);
            let capacity = reg.get(&slab_id).map(|s| s.metadata_capacity()).unwrap_or(0);
            out.push(MetadataPressure {
                slab_id: slab_id.0.to_string(),
                volumes: meta.volumes.len(),
                needed_bytes: needed,
                capacity_bytes: capacity,
                fits: capacity > 0 && needed <= capacity,
            });
        }
        out
    }

    /// Each format v2 store this manager has open (#158): a metadata slab's
    /// id, or `metadata.v2`; pages in use, the log, what is not yet in the
    /// tree.
    pub fn metadata_v2_usage(&self) -> Vec<(String, metav2::Usage)> {
        let st = self.v2.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<(String, metav2::Usage)> = st
            .sinks
            .keys()
            .filter_map(|k| {
                let name = match k {
                    persist_v2::Sink::Slab(id) => id.0.to_string(),
                    persist_v2::Sink::Dir => persist_v2::DIR_FILE.to_string(),
                };
                st.usage(*k).map(|u| (name, u))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The records a persist writes, taken in memory. `None` when this
    /// manager keeps no records anywhere.
    async fn records(&self, generation: u64) -> Option<Records> {
        if self.metadata_store.is_none() && self.metadata_slabs.is_empty() {
            return None;
        }
        let dir_v2 = self.dir_v2();
        if crate::drive::slab::default_format() == crate::drive::slab::SLAB_VERSION_2 && self.v1_sinks().await {
            // Old slabs migrate at their first persist once format 2 is the
            // default (#158). One that cannot stays v1, and is written as v1.
            for (id, r) in self.upgrade_slabs(None).await {
                if let Err(e) = r {
                    tracing::warn!("slab {}: stays in metadata format 1: {e}", id.0);
                }
            }
        }
        if !self.gem.read().await.cold_ids().is_empty() && (self.v1_sinks().await || (self.metadata_store.is_some() && !dir_v2)) {
            // A v1 record is every map, whole.
            if let Err(e) = gem::ensure_all_resident(&self.gem).await {
                tracing::error!("loading extent maps for a v1 record: {e}");
            }
        }
        let mut store = None;
        if let Some(st) = self.metadata_store.as_ref().filter(|_| !dir_v2) {
            // Knowing about no volumes is not the same as there being none.
            // A manager whose slabs have not been attached yet holds nothing,
            // and writing that over a record describing real storage destroys
            // the only statement of what the slabs contain — the extents
            // survive in the slot tables, but nothing is left to say which
            // volume they belong to or what it was called.
            //
            // This happened: a restart came up with no slabs attached, and the
            // next persist replaced a two-volume record with an empty one.
            let had = if self.volumes.is_empty() && st.exists() {
                st.load().map(|m| m.volumes.len()).unwrap_or(0)
            } else {
                0
            };
            if had > 0 {
                tracing::warn!(
                    "not overwriting a record of {had} volume(s) with an empty one — \
                     this manager has no volumes, which usually means its slabs are \
                     not attached rather than that the storage is empty"
                );
                return None;
            }
            store = Some((st.clone(), self.snapshot_metadata().await));
        }
        let v1_slabs: Vec<SlabId> = {
            let reg = self.registry.read().await;
            self.metadata_slabs
                .iter()
                .filter(|id| reg.get(id).is_none_or(|s| s.format_version() != crate::drive::slab::SLAB_VERSION_2))
                .copied()
                .collect()
        };
        let slabs = if v1_slabs.is_empty() {
            Vec::new()
        } else {
            self.per_slab_metadata()
                .await
                .into_iter()
                .filter(|(id, _)| v1_slabs.contains(id))
                .map(|(id, meta)| (id, MetadataStore::encode(&meta).map_err(|e| format!("encode failed: {e}"))))
                .collect()
        };
        let v2 = self.records_v2(dir_v2).await;
        Some(Records { generation, store, slabs, v2 })
    }

    /// Migrate v1 metadata slabs to format 2 in place (#158): every one, or
    /// only `only`. Each from the record it carries now (what a persist would
    /// write to it), header last (see `Slab::upgrade_to_v2`). What came of
    /// each, by slab.
    pub async fn upgrade_slabs(&self, only: Option<SlabId>) -> Vec<(SlabId, Result<(), String>)> {
        let v1: Vec<SlabId> = {
            let reg = self.registry.read().await;
            self.metadata_slabs
                .iter()
                .filter(|id| only.is_none_or(|o| o == **id))
                .filter(|id| reg.get(id).is_some_and(|s| s.format_version() != crate::drive::slab::SLAB_VERSION_2))
                .copied()
                .collect()
        };
        if v1.is_empty() {
            return Vec::new();
        }
        if let Err(e) = gem::ensure_all_resident(&self.gem).await {
            return v1.into_iter().map(|id| (id, Err(format!("loading extent maps: {e}")))).collect();
        }
        let docs: HashMap<SlabId, metadata::VolumeMetadata> =
            self.per_slab_metadata().await.into_iter().filter(|(id, _)| v1.contains(id)).collect();
        let mut out = Vec::new();
        for id in v1 {
            let Some(doc) = docs.get(&id) else { continue };
            let record = match MetadataStore::encode(doc) {
                Ok(r) => r,
                Err(e) => {
                    out.push((id, Err(format!("encode: {e}"))));
                    continue;
                }
            };
            let entries = metav2::document_entries(doc);
            let r = {
                let mut reg = self.registry.write().await;
                match reg.get_mut(&id) {
                    Some(slab) => slab.upgrade_to_v2(&record, entries).await.map_err(|e| e.to_string()),
                    None => Err("not attached".to_string()),
                }
            };
            if r.is_ok() {
                // What this slab's v1 copy last held no longer matters.
                self.records_on_slab.lock().unwrap().remove(&id);
            }
            out.push((id, r));
        }
        out
    }

    /// Whether any metadata slab is in format v1.
    async fn v1_sinks(&self) -> bool {
        let reg = self.registry.read().await;
        self.metadata_slabs
            .iter()
            .any(|id| reg.get(id).is_none_or(|s| s.format_version() != crate::drive::slab::SLAB_VERSION_2))
    }

    /// Take least recently used maps out of memory until what stays is under
    /// `budget` bytes (#158 stage C). Only a map no one outside the manager
    /// holds a handle to (nothing attached, served, mid-I/O), nothing holds,
    /// that a format v2 store holds as it is now: every sink v2, every record
    /// taken written, no change since. Checked under the GEM's write lock,
    /// which a persist needs to take its records, so none is taken between.
    /// A map is loaded again by the first use of its volume.
    pub async fn evict_idle(&self, budget: u64) -> usize {
        if self.metadata_slabs.is_empty() && self.metadata_store.is_none() {
            return 0;
        }
        if (self.metadata_store.is_some() && !self.dir_v2()) || self.v1_sinks().await {
            return 0;
        }
        let mut idle: Vec<(u64, VolumeId)> = self
            .volumes
            .iter()
            .filter(|(id, h)| Arc::strong_count(h) == 1 && self.holds.held_by(id.0).is_empty())
            .map(|(id, h)| (h.last_use(), *id))
            .collect();
        idle.sort_by_key(|(t, _)| *t);
        let mut gem = self.gem.write().await;
        if gem.pager().is_none() {
            return 0;
        }
        let st = self.v2.lock().unwrap_or_else(|e| e.into_inner());
        if st.sink_count() == 0 || !st.quiet() {
            return 0;
        }
        const BYTES_PER_EXTENT: u64 = 32;
        let mut resident: u64 = gem
            .resident_ids()
            .iter()
            .filter_map(|id| gem.get_volume_map(id))
            .map(|m| (m.len() + m.parity.len()) as u64 * BYTES_PER_EXTENT)
            .sum();
        let mut evicted = 0;
        for (_, id) in idle {
            if resident <= budget {
                break;
            }
            if gem.is_cold(&id) {
                continue;
            }
            let Some(size) = gem.get_volume_map(&id).map(|m| (m.len() + m.parity.len()) as u64 * BYTES_PER_EXTENT)
            else {
                continue;
            };
            if st.held(&id) && gem.evict(id) {
                resident -= size;
                evicted += 1;
            }
        }
        evicted
    }

    /// `$STORMBLOCK_METADATA_CACHE_MB` (tests): evict at the end of every
    /// persist, so a path that touches a map without loading it panics.
    fn forced_cache() -> Option<u64> {
        static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        *V.get_or_init(|| std::env::var("STORMBLOCK_METADATA_CACHE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()))
    }

    /// Whether the data directory keeps `metadata.v2` rather than
    /// `volumes.dat`: the gate is on, or it already does (#158).
    fn dir_v2(&self) -> bool {
        self.metadata_store.as_ref().is_some_and(|s| {
            crate::drive::slab::default_format() == crate::drive::slab::SLAB_VERSION_2
                || s.dir().join(persist_v2::DIR_FILE).exists()
        })
    }

    /// What each format v2 store gets from this persist (#158).
    async fn records_v2(&self, dir_v2: bool) -> Option<persist_v2::V2Records> {
        use persist_v2::{Opener, Sink};
        let mut sinks = Vec::new();
        {
            let reg = self.registry.read().await;
            for id in &self.metadata_slabs {
                if let Some(s) = reg.get(id) {
                    if s.format_version() == crate::drive::slab::SLAB_VERSION_2 {
                        if let Some((d, o, z)) = s.metadata_region() {
                            sinks.push((Sink::Slab(*id), Opener::Region(d, o, z)));
                        }
                    }
                }
            }
        }
        if let (true, Some(st)) = (dir_v2, &self.metadata_store) {
            sinks.push((Sink::Dir, Opener::Dir(st.dir().join(persist_v2::DIR_FILE))));
        }
        if sinks.is_empty() {
            if self.gem.read().await.tracking() {
                // No store to load a map from from now on: every map back in
                // memory first.
                if let Err(e) = gem::ensure_all_resident(&self.gem).await {
                    tracing::error!("metadata v2: loading extent maps: {e}");
                }
                let mut g = self.gem.write().await;
                g.track_changes(false);
                g.set_pager(None);
            }
            return None;
        }
        if !self.gem.read().await.tracking() {
            // What changed before now was not recorded: every store whole.
            let mut g = self.gem.write().await;
            g.track_changes(true);
            g.set_pager(Some(Arc::new(persist_v2::StorePager { state: self.v2.clone() })));
            drop(g);
            self.v2.lock().unwrap_or_else(|e| e.into_inner()).forget();
        }
        let names: Vec<Sink> = sinks.iter().map(|(s, _)| *s).collect();
        if self.v2.lock().unwrap_or_else(|e| e.into_inner()).rewrites(&names) {
            // A store written whole reads every map it carries.
            if let Err(e) = gem::ensure_all_resident(&self.gem).await {
                tracing::error!("metadata v2: loading extent maps before a whole write: {e}");
            }
        }
        let headers = self.header_records().await;
        let pins: HashMap<VolumeId, SlabId> =
            self.volumes.iter().filter_map(|(id, h)| h.pinned_slab().map(|p| (*id, p))).collect();
        let roles: HashMap<VolumeId, SlabRole> =
            self.volumes.iter().map(|(id, h)| (*id, h.placement_role())).collect();
        let flowing = self.flowing_into.lock().unwrap().clone();

        let gem = self.gem.read().await;
        let reg = self.registry.read().await;
        let single = self.metadata_slabs.len() == 1 && !reg.is_dedicated(&self.metadata_slabs[0]);
        let sizes: HashMap<VolumeId, u64> = self.volumes.iter().map(|(id, h)| (*id, h.extent_size())).collect();
        let home = |vid: &VolumeId| -> Option<SlabId> {
            if let Some(p) = pins.get(vid) {
                return self.metadata_slabs.contains(p).then_some(*p);
            }
            let want = roles.get(vid).copied().unwrap_or_default();
            let ok = |s: &SlabId| reg.role_of(s) == want && !reg.is_dedicated(s);
            // Where its first write would land: a slab of its size (#156).
            let size = sizes.get(vid).copied().unwrap_or(0);
            self.metadata_slabs
                .iter()
                .copied()
                .find(|s| ok(s) && reg.size_ok(s, size))
                .or_else(|| self.metadata_slabs.iter().copied().find(|s| ok(s)))
        };
        let carries = |sink: Sink, vid: &VolumeId, on: &HashSet<SlabId>| -> bool {
            match sink {
                Sink::Dir => true,
                Sink::Slab(s) if single => s == self.metadata_slabs[0],
                Sink::Slab(s) => {
                    if on.is_empty() {
                        home(vid) == Some(s)
                    } else {
                        on.contains(&s)
                            || flowing.iter().any(|(dest, sources)| *dest == s && sources.iter().any(|x| on.contains(x)))
                    }
                }
            }
        };
        let all_arrays: Vec<(SlabId, metadata::ArrayRecord)> = self
            .array_slabs
            .iter()
            .map(|(array_id, slab_id)| {
                (
                    *slab_id,
                    metadata::ArrayRecord {
                        array_id: *array_id,
                        total_capacity: reg.get(slab_id).map(|s| s.total_slots() * s.slot_size()).unwrap_or(0),
                    },
                )
            })
            .collect();
        let arrays: HashMap<Sink, Vec<metadata::ArrayRecord>> = sinks
            .iter()
            .map(|(sink, _)| {
                let a = all_arrays
                    .iter()
                    .filter(|(slab, _)| matches!(sink, Sink::Dir) || *sink == Sink::Slab(*slab))
                    .map(|(_, a)| a.clone())
                    .collect();
                (*sink, a)
            })
            .collect();
        let changes = gem.take_changes();
        persist_v2::take(
            &self.v2,
            persist_v2::Inputs {
                sinks,
                headers,
                arrays,
                extent_size: self.slot_size,
                gem: &gem,
                changes,
                carries: &carries,
            },
        )
    }

    /// Make durable what the records name, then write them — holding no lock
    /// across a device flush (#269).
    ///
    /// Nothing durable may name a slot before its data is on the device
    /// (#171): every slab is flushed, publishing the entries of slots written
    /// since the last flush, after the records were taken and before they are
    /// written, so every slot they name that had been written is durable.
    async fn sync_then_write(
        registry: &Arc<tokio::sync::RwLock<SlabRegistry>>,
        written: &tokio::sync::Mutex<u64>,
        on_slab: &std::sync::Mutex<HashMap<SlabId, u64>>,
        mut records: Records,
    ) -> anyhow::Result<()> {
        // Every slab at once: slabs on one disk are partitions of one
        // device, and its flushes are shared by everyone who asked before
        // each began (#269) — two slabs' syncs cost two cache flushes, not
        // four, where one after the other cost each in full.
        let ids: Vec<SlabId> = registry.read().await.iter().map(|(id, _)| *id).collect();
        let synced = futures_util::future::join_all(
            ids.iter().map(|id| crate::drive::slab::sync_registered(registry, *id)),
        )
        .await;
        for (id, r) in ids.iter().zip(synced) {
            if let Err(e) = r {
                anyhow::bail!("slab {}: flush before writing volume records: {e}", id.0);
            }
        }

        // Format v2 stores: changes, applied in the order they were taken
        // (never skipped for a newer generation: a change list is not a
        // snapshot).
        let v2_failed = match records.v2.take() {
            Some(v2) => persist_v2::apply(v2).await,
            None => Vec::new(),
        };

        let mut last = written.lock().await;
        if *last > records.generation {
            // A newer snapshot is already on disk, and it holds all of this.
            if !v2_failed.is_empty() {
                anyhow::bail!("{}", v2_failed.join("; "));
            }
            return Ok(());
        }
        if let Some((store, meta)) = &records.store {
            store.save(meta)?;
        }
        // Report every copy that failed, not the first. A node with two data
        // slabs where one is short of room still has to say so about that one
        // while the other is written.
        let mut failed: Vec<String> = v2_failed;
        // The copies that changed, written at once (their flushes are shared,
        // as above).
        let mut writes = Vec::new();
        for (slab_id, bytes) in records.slabs {
            let bytes = match bytes {
                Ok(b) => b,
                Err(e) => {
                    failed.push(format!("slab {}: {e}", slab_id.0));
                    continue;
                }
            };
            let hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut h);
                h.finish()
            };
            if on_slab.lock().unwrap().get(&slab_id) == Some(&hash) {
                // This slab's copy already says exactly this.
                continue;
            }
            match registry.read().await.get(&slab_id).map(|s| s.metadata_writer()) {
                Some(w) => writes.push((slab_id, hash, w, bytes)),
                None => failed.push(format!("slab {} is not attached", slab_id.0)),
            }
        }
        let results = futures_util::future::join_all(writes.iter().map(|(_, _, w, b)| w.write(b))).await;
        for ((slab_id, hash, _, _), r) in writes.iter().zip(results) {
            match r {
                Ok(()) => {
                    on_slab.lock().unwrap().insert(*slab_id, *hash);
                }
                Err(e) => {
                    on_slab.lock().unwrap().remove(slab_id);
                    failed.push(format!("slab {}: {e}", slab_id.0));
                }
            }
        }
        *last = records.generation;
        if !failed.is_empty() {
            anyhow::bail!("{}", failed.join("; "));
        }
        Ok(())
    }

    /// One `volumes.dat` per metadata slab, each holding only what that slab
    /// actually carries.
    ///
    /// A volume goes into the copy of every slab it has a leg on. One with no
    /// extents yet — created and not written — has no slab to point at, so it
    /// goes to the first metadata slab of its own role: that is where its
    /// first write would land, and the role is the boundary an install
    /// respects (#88).
    async fn per_slab_metadata(&self) -> Vec<(SlabId, metadata::VolumeMetadata)> {
        if self.metadata_slabs.is_empty() {
            return Vec::new();
        }
        let full = self.snapshot_metadata().await;
        // One metadata slab carries everything — unless it is dedicated, which
        // carries only what is pinned to it (#150).
        if self.metadata_slabs.len() == 1 && !self.registry.read().await.is_dedicated(&self.metadata_slabs[0]) {
            let mut full = full;
            // A v1 record's one extent size is its slab's (#156).
            if let Some(sz) = self.registry.read().await.get(&self.metadata_slabs[0]).map(|s| s.slot_size()) {
                full.extent_size = sz;
            }
            return vec![(self.metadata_slabs[0], full)];
        }
        let pins: HashMap<VolumeId, SlabId> = self
            .volumes
            .iter()
            .filter_map(|(id, h)| h.pinned_slab().map(|p| (*id, p)))
            .collect();

        let mut roles: HashMap<VolumeId, SlabRole> = HashMap::new();
        for (id, handle) in &self.volumes {
            roles.insert(*id, handle.placement_role());
        }
        let touched: HashMap<VolumeId, HashSet<SlabId>> = {
            let gem = self.gem.read().await;
            self.volumes
                .keys()
                .map(|id| {
                    let slabs = gem
                        .get_volume_map(id)
                        .map(|m| m.all_legs().map(|l| l.slab_id).collect())
                        .unwrap_or_default();
                    (*id, slabs)
                })
                .collect()
        };
        let reg = self.registry.read().await;
        // Where a volume with no extents is recorded: the first metadata
        // slab whose role matches it.
        let sizes: HashMap<VolumeId, u64> = self.volumes.iter().map(|(id, h)| (*id, h.extent_size())).collect();
        let home = |vid: &VolumeId| -> Option<SlabId> {
            if let Some(p) = pins.get(vid) {
                return self.metadata_slabs.contains(p).then_some(*p);
            }
            let want = roles.get(vid).copied().unwrap_or_default();
            let ok = |s: &SlabId| reg.role_of(s) == want && !reg.is_dedicated(s);
            // Where its first write would land: a slab of its size (#156).
            let size = sizes.get(vid).copied().unwrap_or(0);
            self.metadata_slabs
                .iter()
                .copied()
                .find(|s| ok(s) && reg.size_ok(s, size))
                .or_else(|| self.metadata_slabs.iter().copied().find(|s| ok(s)))
        };
        let array_of: HashMap<SlabId, RaidArrayId> = self
            .array_slabs
            .iter()
            .map(|(array, slab)| (*slab, *array))
            .collect();

        let flowing = self.flowing_into.lock().unwrap().clone();
        let flows_into = |slab_id: &SlabId, on: &HashSet<SlabId>| {
            flowing.iter().any(|(dest, sources)| dest == slab_id && sources.iter().any(|s| on.contains(s)))
        };

        self.metadata_slabs
            .iter()
            .map(|slab_id| {
                let volumes: Vec<_> = full
                    .volumes
                    .iter()
                    .filter(|v| match touched.get(&v.id) {
                        Some(on) if !on.is_empty() => on.contains(slab_id) || flows_into(slab_id, on),
                        _ => home(&v.id) == Some(*slab_id),
                    })
                    .cloned()
                    .collect();
                let arrays: Vec<_> = match array_of.get(slab_id) {
                    Some(array_id) => full
                        .arrays
                        .iter()
                        .filter(|a| a.array_id == *array_id)
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                (
                    *slab_id,
                    metadata::VolumeMetadata {
                        // A v1 record's one extent size is its slab's (#156).
                        extent_size: reg.get(slab_id).map(|s| s.slot_size()).unwrap_or(full.extent_size),
                        arrays,
                        volumes,
                    },
                )
            })
            .collect()
    }

    /// The volume records the metadata slabs carry, each paired with the role
    /// of the slab it came from — which is what a volume with no extents of
    /// its own is placed by.
    async fn load_slab_records(
        &self,
    ) -> anyhow::Result<Option<Vec<(metadata::VolumeRecord, SlabRole)>>> {
        if self.metadata_slabs.is_empty() {
            return Ok(None);
        }
        let reg = self.registry.read().await;
        let mut out: Vec<(metadata::VolumeRecord, SlabRole)> = Vec::new();
        let mut seen: HashSet<VolumeId> = HashSet::new();
        let mut found = false;
        for slab_id in &self.metadata_slabs {
            let slab = reg
                .get(slab_id)
                .ok_or_else(|| anyhow::anyhow!("metadata slab {} is not attached", slab_id.0))?;
            let Some(doc) = metav2::read_slab(slab).await? else { continue };
            found = true;
            let role = slab.role();
            for v in doc.volumes {
                if seen.insert(v.id) {
                    out.push((v, role));
                }
            }
        }
        if !found {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Every volume's record without its extents and parity: what a format
    /// v2 store keeps as the volume's header (#158).
    async fn header_records(&self) -> Vec<metadata::VolumeRecord> {
        let mut out = Vec::with_capacity(self.volumes.len());
        for (id, handle) in &self.volumes {
            out.push(metadata::VolumeRecord {
                id: *id,
                name: handle.name().await,
                virtual_size: handle.capacity_bytes(),
                // A pin travels as the array the slab is (#150).
                array_id: handle.pinned_slab().and_then(|p| self.array_of_slab(&p)),
                retention: self.retentions.get(id).copied().unwrap_or_default(),
                parent: self.parents.get(id).copied(),
                sealed: handle.is_sealed(),
                template: self.templates.contains(id),
                access: handle.access(),
                fs: self.fs_info.get(id).cloned(),
                owner: self.owners.get(id).cloned(),
                lba: handle.lba(),
                extents: Default::default(),
                redundancy: handle.redundancy(),
                parity: Default::default(),
                failed_slabs: handle.failed_slabs(),
                extent_size: handle.extent_size(),
            });
        }
        out
    }

    /// The record every persist path writes: volumes, their sizes, and the
    /// extent maps the slot tables cannot reconstruct.
    async fn snapshot_metadata(&self) -> metadata::VolumeMetadata {
        // Gather per-volume info before taking gem/registry locks so we never
        // hold them across a volume-handle await (I/O paths lock the volume
        // first, then gem/registry).
        let mut vol_info = Vec::with_capacity(self.volumes.len());
        for (id, handle) in &self.volumes {
            vol_info.push((
                *id,
                handle.name().await,
                handle.capacity_bytes(),
                handle.redundancy(),
                handle.failed_slabs(),
                handle.is_sealed(),
                handle.access(),
                handle.pinned_slab(),
                handle.lba(),
                handle.extent_size(),
            ));
        }

        let gem = self.gem.read().await;
        let reg = self.registry.read().await;
        let arrays = self
            .array_slabs
            .iter()
            .map(|(array_id, slab_id)| metadata::ArrayRecord {
                array_id: *array_id,
                total_capacity: reg
                    .get(slab_id)
                    .map(|s| s.total_slots() * s.slot_size())
                    .unwrap_or(0),
            })
            .collect();
        let volumes = vol_info
            .into_iter()
            .map(|(id, name, virtual_size, redundancy, failed_slabs, sealed, access, pinned, lba, extent_size)| metadata::VolumeRecord {
                id,
                name,
                virtual_size,
                // A pin travels as the array the slab is (#150).
                array_id: pinned.and_then(|p| self.array_of_slab(&p)),
                retention: self.retentions.get(&id).copied().unwrap_or_default(),
                parent: self.parents.get(&id).copied(),
                sealed,
                template: self.templates.contains(&id),
                access,
                fs: self.fs_info.get(&id).cloned(),
                owner: self.owners.get(&id).cloned(),
                lba,
                extents: gem
                    .get_volume_map(&id)
                    .map(|m| m.extents.to_btree())
                    .unwrap_or_default(),
                redundancy,
                parity: gem
                    .get_volume_map(&id)
                    .map(|m| m.parity.clone())
                    .unwrap_or_default(),
                failed_slabs,
                extent_size,
            })
            .collect();
        metadata::VolumeMetadata {
            extent_size: self.slot_size,
            arrays,
            volumes,
        }
    }

    /// Persist, reporting what a background persist only logs.
    ///
    /// The image builder is not a running node: a metadata write that fails
    /// there produces an image that cannot boot, so it has to fail the build
    /// rather than warn into a log nobody reads.
    pub async fn persist_checked(&self) -> anyhow::Result<()> {
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        match self.records(generation).await {
            None => Ok(()),
            Some(records) => {
                Self::sync_then_write(&self.registry, &self.records_written, &self.records_on_slab, records).await
            }
        }
    }

    /// Restore volumes from persisted metadata. No-op if no data_dir or no metadata file.
    pub async fn restore(&mut self) -> anyhow::Result<()> {
        // A data directory wins where there is one: it is the record a running
        // node has been updating. The slab's own copy is the fallback for a
        // node that has no filesystem to keep one in.
        let from_dir = match &self.metadata_store {
            Some(s) if s.dir().join(persist_v2::DIR_FILE).exists() => persist_v2::load_dir(s.dir()).await?,
            Some(s) if s.exists() => Some(s.load()?),
            _ => None,
        };
        let records: Vec<(metadata::VolumeRecord, SlabRole)> = match from_dir {
            // The data directory's record does not say which half a volume
            // belongs to, and for a volume with no extents yet nothing else
            // does either. Taking `System` for all of them put an unwritten
            // data volume in the half an install replaces — and on a node
            // with only a data slab, made it unwritable: a 1 TiB class blank
            // resumed after a restart failed every write with "no system
            // slab" (#141). So: the half the slabs' own records keep it in,
            // else a half this node has, the way `create` chooses.
            Some(m) => {
                let from_slabs: HashMap<VolumeId, SlabRole> = self
                    .load_slab_records()
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.into_iter().map(|(v, role)| (v.id, role)).collect())
                    .unwrap_or_default();
                let fallback = {
                    let reg = self.registry.read().await;
                    let has = |r: SlabRole| reg.iter().any(|(_, s)| s.role() == r);
                    if has(SlabRole::System) || !has(SlabRole::Data) {
                        SlabRole::System
                    } else {
                        SlabRole::Data
                    }
                };
                m.volumes
                    .into_iter()
                    .map(|v| {
                        let role = from_slabs.get(&v.id).copied().unwrap_or(fallback);
                        (v, role)
                    })
                    .collect()
            }
            None => match self.load_slab_records().await? {
                Some(r) => r,
                None => {
                    if self.metadata_store.is_some() || !self.metadata_slabs.is_empty() {
                        tracing::info!("No persisted metadata found, starting fresh");
                    }
                    return Ok(());
                }
            },
        };

        // Rebuild GEM from slab slot tables — authoritative for owned and
        // COW'd slots (written at allocation time, so always at least as new
        // as the metadata file after a crash).
        // Every slab's table read once (#155): no per-slot record is kept in
        // memory, so this is where restore reads what the tables say.
        let view = {
            let sources = {
                let reg = self.registry.read().await;
                reg.iter().map(|(_, s)| s.view_source()).collect()
            };
            gem::SlotView::read(sources).await?
        };
        let mut rebuilt = GlobalExtentMap::rebuild_from_view(&view);

        // Which volumes a record may legitimately share a slot with: its
        // ancestors (#171). Every live volume, to tell a slot another volume
        // has since taken from one whose owner is gone.
        let parent_of: HashMap<VolumeId, VolumeId> =
            records.iter().filter_map(|(v, _)| v.parent.map(|p| (v.id, p))).collect();
        let live: HashSet<VolumeId> = records.iter().map(|(v, _)| v.id).collect();
        let lineage = |mut id: VolumeId| -> HashSet<VolumeId> {
            let mut set = HashSet::new();
            while set.insert(id) && set.len() < 256 {
                match parent_of.get(&id) {
                    Some(p) => id = *p,
                    None => break,
                }
            }
            set
        };

        let mut restored = 0u32;
        let mut dirty_to_verify: Vec<(VolumeId, Arc<ThinVolumeHandle>, Vec<u64>)> = Vec::new();
        for (vrec, home_role) in records {
            // Legacy V1 records bind volumes to arrays; skip if that array
            // isn't attached. V2 slab-placed records restore regardless.
            if let Some(array_id) = vrec.array_id {
                if !self.array_slabs.contains_key(&array_id) {
                    tracing::warn!(
                        "Skipping volume '{}' ({}): array {} not available",
                        vrec.name, vrec.id, array_id
                    );
                    continue;
                }
            }

            // Reconcile the record with the slot tables. The record is what
            // the running node knew — which slots are legs of one extent and
            // which are a clone's leftovers, which the slot tables cannot
            // say. The slot tables win only where they are provably newer: a
            // slot allocated at a higher generation for the same extent is a
            // copy-on-write the record never saw (a crash between the two
            // writes), and a recorded slot that is no longer allocated to
            // this extent has been freed and possibly reused. Persisted
            // mappings fill the gaps the slot tables cannot express — a
            // snapshot's shared slots (#13).
            {
                let reg = self.registry.read().await;
                reconcile_record(&reg, &view, &mut rebuilt, &vrec, &lineage(vrec.id), &live);
            }

            // Parity groups: the record is authoritative — it knows the
            // stripe width, which the slot tables do not.
            for (stripe, group) in &vrec.parity {
                rebuilt.insert_parity(vrec.id, *stripe, group.clone());
            }

            self.retentions.insert(vrec.id, vrec.retention);
            // Where a volume already lives is what it is. The role is not in
            // the record because it does not need to be: a volume whose
            // extents are in a data slab is a data volume, and one written
            // by an older build has no data slab to be in. Deriving it means
            // no metadata version to bump and no way for the record and the
            // placement to disagree (#88).
            let role = {
                let reg = self.registry.read().await;
                rebuilt
                    .get_volume_map(&vrec.id)
                    .and_then(|m| m.all_legs().next().map(|l| reg.role_of(&l.slab_id)))
                    .unwrap_or(home_role)
            };
            let extent_size = restored_extent_size(&*self.registry.read().await, &vrec, self.slot_size)?;
            let vol = ThinVolume::restore(vrec.id, vrec.name.clone(), vrec.virtual_size, extent_size);
            let handle = Arc::new(ThinVolumeHandle::with_redundancy(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy {
                    role,
                    pinned: vrec.array_id.and_then(|a| self.array_slabs.get(&a).copied()),
                    ..Default::default()
                },
                vrec.redundancy.clone(),
            ));
            handle.set_failed_slabs(vrec.failed_slabs.iter().copied());
            handle.set_sealed(vrec.sealed);
            handle.set_lba(vrec.lba);
            if vrec.template {
                self.templates.insert(vrec.id);
            }
            handle.set_access(vrec.access);
            if let Some(p) = vrec.parent {
                self.parents.insert(vrec.id, p);
            }
            if let Some(fs) = vrec.fs.clone() {
                self.fs_info.insert(vrec.id, fs);
            }
            if let (Some(store), true) = (&self.metadata_store, vrec.redundancy.scheme.is_parity()) {
                let dirty = handle.use_stripe_log(store.dir());
                if !dirty.is_empty() {
                    dirty_to_verify.push((vrec.id, handle.clone(), dirty));
                }
            }
            self.volumes.insert(vrec.id, handle);
            restored += 1;
            tracing::info!("Restored volume '{}' ({})", vrec.name, vrec.id);
        }

        // Share counts from the mappings restored (#171): see `raise_shares`.
        {
            let mut reg = self.registry.write().await;
            raise_shares(&mut reg, &view, &mut rebuilt).await;
        }

        *self.gem.write().await = rebuilt;

        // Stripes a previous run left mid-write: their parity may be stale.
        // Recompute those and only those.
        for (id, handle, dirty) in dirty_to_verify {
            let report = handle.verify_stripes(&dirty).await;
            tracing::info!(
                volume = %id, stripes = dirty.len(), verified = report.parity_verified,
                errors = report.errors.len(), "dirty stripes verified after restart"
            );
        }

        tracing::info!("Restored {restored} volume(s) from metadata");
        Ok(())
    }
}

/// Reconcile one volume's record with the slot tables into `rebuilt` (#171).
///
/// The record is what the running node knew — which slots are legs of one
/// extent and which are a clone's leftovers, which the slot tables cannot say.
/// The slot tables win only where they are provably newer: a slot allocated at
/// a higher generation for the same extent is a copy-on-write the record never
/// saw, and a recorded slot that is no longer allocated to this extent has
/// been freed and possibly reused. Persisted mappings fill the gaps the slot
/// tables cannot express — a snapshot's shared slots (#13). Shared by
/// `restore` and `adopt_slabs`, so a volume comes back the same way whichever
/// door it comes through.
fn reconcile_record(
    reg: &SlabRegistry,
    view: &gem::SlotView,
    rebuilt: &mut GlobalExtentMap,
    vrec: &metadata::VolumeRecord,
    lineage: &HashSet<VolumeId>,
    live: &HashSet<VolumeId>,
) {
    let slot_gen = |leg: gem::Leg| -> Option<u64> {
        view.get(leg).filter(|s| s.state.is_owned()).map(|s| s.generation)
    };
    for (vext, loc) in &vrec.extents {
        match rebuilt.lookup(vrec.id, *vext) {
            None => {
                if reg.get(&loc.slab_id).is_some() {
                    // No slot on disk names this extent of this
                    // volume, so the record's slot is someone
                    // else's: a share of an ancestor's (or of a
                    // golden a composed disk maps), or a slot
                    // this volume gave back since the record was
                    // written and that may have been taken again.
                    // Mapping the second is handing the consumer
                    // another volume's bytes (#171).
                    let in_range =
                        reg.get(&loc.slab_id).map(|s| (loc.slot_idx as u64) < s.total_slots()).unwrap_or(false);
                    let slot = view.get(loc.primary());
                    let why = match slot {
                        _ if !in_range => Some("is out of range".to_string()),
                        None => Some("has been freed".to_string()),
                        Some(s) if !s.state.is_owned() => {
                            Some("has been freed".to_string())
                        }
                        Some(s) if s.volume_id == vrec.id => {
                            Some(format!("was taken again for extent {}", s.virtual_extent_idx))
                        }
                        Some(s) if lineage.contains(&s.volume_id) => {
                            (s.virtual_extent_idx != *vext).then(|| {
                                format!("is the ancestor's extent {}", s.virtual_extent_idx)
                            })
                        }
                        // A share of a volume outside the lineage
                        // (a composed disk's golden) has a count
                        // above one; a slot another live volume
                        // took fresh has one.
                        Some(s) if live.contains(&s.volume_id) && s.ref_count <= 1 => {
                            Some(format!("now belongs to volume {}", s.volume_id))
                        }
                        Some(_) => None,
                    };
                    if let Some(why) = why {
                        tracing::warn!(
                            "Volume '{}' extent {vext}: the record's slot {}:{} {why}; \
                             the record is older than the slot table, mapping dropped",
                            vrec.name, loc.slab_id.0, loc.slot_idx
                        );
                        continue;
                    }
                    rebuilt.restore_mapping(vrec.id, *vext, loc.clone());
                } else {
                    tracing::warn!(
                        "Volume '{}' extent {vext}: slab {} not attached, mapping dropped",
                        vrec.name, loc.slab_id.0
                    );
                }
            }
            Some(rloc) => {
                let recorded_is_live = rloc.legs().any(|l| l == loc.primary());
                let newer_on_disk = rloc.primary() != loc.primary()
                    && slot_gen(rloc.primary()) > slot_gen(loc.primary());
                if recorded_is_live && !newer_on_disk {
                    // Legs the record names that are gone stay
                    // named: health reports them, resync rebuilds.
                    rebuilt.insert(vrec.id, *vext, loc.clone());
                } else {
                    tracing::info!(
                        "Volume '{}' extent {vext}: slot table is newer than the record, taking it",
                        vrec.name
                    );
                }
            }
        }
    }
}

/// The extent size a restored volume is addressed at (#156): its record's
/// (the document's, in format 1), which the slot size of every slab it has a
/// leg on must match. An engine that addressed 1 MiB extents in 4 MiB slots
/// once wrote every extent across its neighbours; a record and a slab that
/// disagree are refused, not guessed between.
fn restored_extent_size(
    reg: &SlabRegistry,
    vrec: &metadata::VolumeRecord,
    default: u64,
) -> Result<u64, VolumeError> {
    let size = if vrec.extent_size != 0 { vrec.extent_size } else { default };
    let mut on: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let legs = vrec.extents.values().flat_map(|l| l.legs().collect::<Vec<_>>()).chain(vrec.parity.values().flat_map(|g| g.legs.clone()));
    for leg in legs {
        if let Some(s) = reg.get(&leg.slab_id) {
            on.insert(s.slot_size());
        }
    }
    if on.iter().any(|s| *s != size) {
        return Err(VolumeError::InvalidSize(format!(
            "volume {} ({}) is recorded with {size}-byte extents and has legs on slabs of {on:?}-byte \
             slots: addressing it so would read and write across its slots' neighbours",
            vrec.name, vrec.id.0
        )));
    }
    Ok(size)
}

/// Raise share counts to the mappings in `gem` (#171). A count on disk can be
/// behind the map — a copy-on-write's decrement written, the entry of the slot
/// that replaced it not — and a count too low lets a write land in place in a
/// slot another volume still reads. Only ever raised: one too high costs a
/// needless copy, never data.
async fn raise_shares(reg: &mut SlabRegistry, view: &gem::SlotView, rebuilt: &mut GlobalExtentMap) {
    let mut maps: HashMap<gem::Leg, u32> = HashMap::new();
    for vol in rebuilt.volume_ids() {
        if let Some(it) = rebuilt.volume_extents(&vol) {
            for (_, loc) in it {
                for leg in loc.legs() {
                    *maps.entry(leg).or_default() += 1;
                }
            }
        }
    }
    let mut fix: Vec<(VolumeId, u64, u32)> = Vec::new();
    for vol in rebuilt.volume_ids() {
        if let Some(it) = rebuilt.volume_extents(&vol) {
            for (vext, loc) in it {
                let n = maps.get(&loc.primary()).copied().unwrap_or(1);
                if loc.ref_count < n {
                    fix.push((vol, vext, n));
                }
            }
        }
    }
    for (vol, vext, n) in &fix {
        rebuilt.set_extent_ref(*vol, *vext, *n);
    }
    let mut raised = 0usize;
    for (leg, n) in maps {
        // Only what the tables, as read, have below the maps: a slot the
        // view has not (free, or on a slab not read now) is left alone.
        match view.get(leg) {
            Some(s) if s.state.is_owned() && s.ref_count < n => {}
            _ => continue,
        }
        if let Some(slab) = reg.get_mut(&leg.slab_id) {
            if slab.raise_ref(leg.slot_idx, n).await {
                raised += 1;
            }
        }
    }
    if raised > 0 || !fix.is_empty() {
        tracing::info!(
            "restore: {raised} slot share count(s) and {} mapping(s) raised to the maps restored",
            fix.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::raid::{RaidArray, RaidLevel};

    /// A laid disk's slabs come first, and nothing is named twice (#118).
    #[test]
    fn local_metadata_slabs_go_first() {
        let (appliance_data, appliance_sys) = (SlabId::new(), SlabId::new());
        let (local_data, local_sys) = (SlabId::new(), SlabId::new());
        let mut mgr = VolumeManager::new(4096);
        mgr.persist_to_slabs(vec![appliance_data, appliance_sys]);
        mgr.keep_metadata_in_first(&[local_data, local_sys, appliance_sys]);
        assert_eq!(
            mgr.metadata_slabs(),
            &[local_data, local_sys, appliance_sys, appliance_data]
        );
    }

    async fn create_test_array() -> (RaidArrayId, Arc<dyn BlockDevice>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
        let mut paths = Vec::new();
        for i in 0..2 {
            let path = dir.join(format!("{test_id}-member-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }

        let array = RaidArray::create(RaidLevel::Raid1, devices, None)
            .await
            .unwrap();
        let array_id = array.array_id();
        let backing: Arc<dyn BlockDevice> = Arc::new(array);
        (array_id, backing, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn volume_manager_create_and_list() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let list = mgr.list_volumes().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, vol_id);
        assert_eq!(list[0].1, "data");
        assert_eq!(list[0].2, 100 * 1024 * 1024);
        assert_eq!(list[0].3, 0); // No data written yet

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_write_read_roundtrip() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        let data = vec![0xDE_u8; 4096];
        vol.write(0, &data).await.unwrap();

        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_snapshot_roundtrip() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

        let snap_id = mgr.create_snapshot(vol_id, "snap1").await.unwrap();

        // Write new data to source
        vol.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

        // Source has new data
        let mut src_buf = vec![0u8; 4096];
        vol.read(0, &mut src_buf).await.unwrap();
        assert!(src_buf.iter().all(|&b| b == 0xBB));

        // Snapshot has old data
        let snap = mgr.get_volume(&snap_id).unwrap();
        let mut snap_buf = vec![0u8; 4096];
        snap.read(0, &mut snap_buf).await.unwrap();
        assert!(snap_buf.iter().all(|&b| b == 0xAA));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_delete() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("to-delete", 50 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xFF_u8; 4096]).await.unwrap();
        drop(vol);

        mgr.delete_volume(vol_id).await.unwrap();
        assert!(mgr.get_volume(&vol_id).is_none());
        assert!(mgr.delete_volume(vol_id).await.is_err());

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_grow() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-grow", 50 * 1024 * 1024, array_id).await.unwrap();
        mgr.resize_volume(vol_id, 100 * 1024 * 1024).await.unwrap();

        let list = mgr.list_volumes().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].2, 100 * 1024 * 1024);

        let vol = mgr.get_volume(&vol_id).unwrap();
        let data = vec![0xCD_u8; 4096];
        vol.write(60 * 1024 * 1024, &data).await.unwrap();

        let mut buf = vec![0u8; 4096];
        vol.read(60 * 1024 * 1024, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_shrink() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-shrink", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        let data_low = vec![0xAA_u8; 4096];
        vol.write(0, &data_low).await.unwrap();
        vol.write(60 * 1024 * 1024, &vec![0xBB_u8; 4096]).await.unwrap();

        let handle = mgr.get_volume_handle(&vol_id).unwrap();
        let extents_before = handle.extent_count().await;
        assert_eq!(extents_before, 2);

        // Shrinking through the ordinary resize path is refused: it frees the
        // extents past the new end, and no filesystem above can follow (#19).
        let refused = mgr.resize_volume(vol_id, 50 * 1024 * 1024).await.unwrap_err();
        assert!(
            matches!(refused, VolumeError::ShrinkRefused { .. }),
            "{refused}"
        );
        assert_eq!(handle.extent_count().await, extents_before, "nothing was freed");

        // Naming it is what makes it happen.
        mgr.shrink_volume(vol_id, 50 * 1024 * 1024).await.unwrap();

        let extents_after = handle.extent_count().await;
        assert_eq!(extents_after, 1);

        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data_low);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_zero_rejected() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("no-zero", 50 * 1024 * 1024, array_id).await.unwrap();
        // Zero is a shrink first and an invalid size second, so that is what
        // comes back through the grow-only door.
        let result = mgr.resize_volume(vol_id, 0).await;
        assert!(matches!(result, Err(VolumeError::ShrinkRefused { .. })), "{result:?}");
        // Through the explicit door it is still rejected, as a size.
        let result = mgr.shrink_volume(vol_id, 0).await;
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("size must be > 0"));

        cleanup(&paths);
    }

    /// The boot-artifact / reboot path: build a slab + volume in one manager,
    /// reattach the same backing file in a fresh manager via
    /// open_backing_device (no reformat), restore metadata, read data back.
    #[tokio::test]
    async fn open_backing_device_restores_existing_volume() {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let backing_path = dir.join(format!("{test_id}-reopen.bin"));
        let backing_str = backing_path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&backing_path);
        let meta_dir = dir.join(format!("{test_id}-meta"));

        let array_id = RaidArrayId(uuid::Uuid::new_v4());
        let data: Vec<u8> = (0..2 * 1024 * 1024 + 331).map(|i| (i % 249) as u8).collect();

        // Phase 1: create, write, persist metadata.
        let vol_id = {
            let dev = FileDevice::open_with_capacity(&backing_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
            mgr.add_backing_device(array_id, Arc::new(dev)).await;
            let vol_id = mgr
                .create_volume("reopen-me", data.len() as u64, array_id)
                .await
                .unwrap();
            let vol = mgr.get_volume(&vol_id).unwrap();
            let mut off = 0usize;
            while off < data.len() {
                let n = vol.write(off as u64, &data[off..]).await.unwrap();
                assert!(n > 0);
                off += n;
            }
            vol.flush().await.unwrap();
            mgr.persist().await;
            vol_id
        };

        // Phase 2: fresh manager, attach WITHOUT reformatting, restore, read.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
        mgr.open_backing_device(array_id, Arc::new(dev))
            .await
            .unwrap();
        mgr.restore().await.unwrap();

        let vol = mgr
            .get_volume(&vol_id)
            .expect("volume restored from metadata");
        let mut got = vec![0u8; data.len()];
        let mut off = 0usize;
        while off < got.len() {
            let end = got.len();
            let n = vol.read(off as u64, &mut got[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(got, data, "restored volume content differs");

        // A manager of another extent size never misreads it: where its
        // records keep one size (format 1) the slab is refused; where they
        // keep each volume's (format 2, #156) the volume is addressed at its
        // own size, checked against the slab it is on.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut wrong = VolumeManager::with_data_dir(8192, meta_dir.clone()).unwrap();
        match wrong.open_backing_device(array_id, Arc::new(dev)).await {
            Err(_) => assert_eq!(crate::drive::slab::default_format(), crate::drive::slab::SLAB_VERSION),
            Ok(()) => {
                wrong.restore().await.unwrap();
                let v = wrong.get_volume_handle(&vol_id).expect("restored");
                assert_eq!(v.extent_size(), 4096);
                let mut again = vec![0u8; data.len()];
                let mut off = 0usize;
                while off < again.len() {
                    let end = again.len();
                    off += v.read(off as u64, &mut again[off..end]).await.unwrap();
                }
                assert_eq!(again, data, "read at its own extent size");
            }
        }

        let _ = std::fs::remove_file(&backing_path);
        let _ = std::fs::remove_dir_all(&meta_dir);
    }

    /// Issue #13: a COW snapshot must survive detach/reattach with its FULL
    /// content intact — including the shared (never-COW'd) extents that only
    /// exist in the persisted extent map, not in slab slot tables. The parent
    /// diverges after the snapshot, so any mapping confusion shows up as the
    /// snapshot reading the parent's new data (or zeros).
    #[tokio::test]
    async fn snapshot_full_content_survives_reattach() {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let backing_path = dir.join(format!("{test_id}-snap-reattach.bin"));
        let backing_str = backing_path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&backing_path);
        let meta_dir = dir.join(format!("{test_id}-snap-meta"));

        let array_id = RaidArrayId(uuid::Uuid::new_v4());
        // Multiple extents, deterministic per-byte pattern.
        let golden: Vec<u8> = (0..3 * 4096 + 777).map(|i| (i % 251) as u8).collect();

        // Phase 1: create parent, write golden, snapshot, diverge parent.
        let (parent_id, snap_id) = {
            let dev = FileDevice::open_with_capacity(&backing_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
            mgr.add_backing_device(array_id, Arc::new(dev)).await;
            let parent_id = mgr
                .create_volume("golden", golden.len() as u64, array_id)
                .await
                .unwrap();
            let vol = mgr.get_volume(&parent_id).unwrap();
            let mut off = 0;
            while off < golden.len() {
                off += vol.write(off as u64, &golden[off..]).await.unwrap();
            }
            let snap_id = mgr.create_snapshot(parent_id, "snap-cp-01").await.unwrap();

            // Diverge the parent AFTER the snapshot (COW moves the parent to
            // new slots; the snapshot keeps the originals).
            vol.write(0, &vec![0xEE_u8; 4096]).await.unwrap();
            vol.flush().await.unwrap();
            mgr.persist().await;
            (parent_id, snap_id)
        };

        // Phase 2: fresh manager — attach without reformat, restore, verify.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
        mgr.open_backing_device(array_id, Arc::new(dev)).await.unwrap();
        mgr.restore().await.unwrap();

        let snap = mgr.get_volume(&snap_id).expect("snapshot restored");
        let mut got = vec![0u8; golden.len()];
        let mut off = 0;
        while off < got.len() {
            let end = got.len();
            let n = snap.read(off as u64, &mut got[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(got, golden, "snapshot content diverged after reattach (#13)");

        // Parent kept its post-snapshot write.
        let parent = mgr.get_volume(&parent_id).expect("parent restored");
        let mut head = vec![0u8; 4096];
        parent.read(0, &mut head).await.unwrap();
        assert!(head.iter().all(|&b| b == 0xEE), "parent lost its divergent write");
        // And the rest of the parent still matches golden.
        let mut tail = vec![0u8; golden.len() - 4096];
        let mut off = 0;
        while off < tail.len() {
            let end = tail.len();
            let n = parent.read((4096 + off) as u64, &mut tail[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(tail, golden[4096..], "parent unshared content corrupted");

        let _ = std::fs::remove_file(&backing_path);
        let _ = std::fs::remove_dir_all(&meta_dir);
    }
}

#[cfg(test)]
mod redundancy_tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn file_slab(dir: &std::path::Path, tag: &str, slot: u64) -> (Slab, String) {
        let path = dir.join(format!("{tag}.bin"));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev = FileDevice::open_with_capacity(&path_str, 8 * 1024 * 1024).await.unwrap();
        (Slab::format(Arc::new(dev), slot, StorageTier::Hot).await.unwrap(), path_str)
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("stormblock-vm-redundancy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A general slab and an array's dedicated slab (#150): the pool never
    /// allocates on the array, a volume pinned to it never allocates
    /// anywhere else — nor does its clone — and the array cannot be removed
    /// out from under its volumes.
    #[tokio::test]
    async fn a_pinned_volume_lives_on_its_array_and_nothing_else_does() {
        let d = dir();
        let slot = 4096u64;
        let mut mgr = VolumeManager::new(slot);
        let (general, _) = file_slab(&d, "general", slot).await;
        let gid = general.slab_id();
        mgr.add_slab(general).await;
        let array = RaidArrayId(uuid::Uuid::new_v4());
        let adev = FileDevice::open_with_capacity(d.join("array.bin").to_str().unwrap(), 4 * 1024 * 1024).await.unwrap();
        let aslab = mgr.add_dedicated_array(array, Arc::new(adev)).await.unwrap();
        {
            let reg = mgr.registry().read().await;
            assert!(reg.is_dedicated(&aslab) && !reg.is_dedicated(&gid));
            assert_eq!(reg.role_of(&aslab), SlabRole::Data);
            assert!(reg.get(&aslab).unwrap().has_metadata_region());
        }

        // Unpinned volumes fill the general slab and never touch the array,
        // though the array is the emptier of the two once the general slab
        // is filling up.
        let plain = mgr.create_volume_any("plain", 1 << 20).await.unwrap();
        let data_plain = mgr.create_volume_with("data-plain", 1 << 20, CreateOptions::default().in_role(SlabRole::Data)).await.unwrap();
        let pv = mgr.get_volume(&plain).unwrap();
        for i in 0..64u64 {
            pv.write(i * slot, &[1u8; 4096]).await.unwrap();
        }
        // A data volume with no general data slab has nowhere to go: the
        // dedicated slab is not one.
        assert!(mgr.get_volume(&data_plain).unwrap().write(0, &[2u8; 4096]).await.is_err());

        let pinned = mgr.create_volume("mirror-vol", 1 << 20, array).await.unwrap();
        assert_eq!(mgr.get_volume_handle(&pinned).unwrap().pinned_slab(), Some(aslab));
        assert_eq!(mgr.volume_role(&pinned), Some(SlabRole::Data));
        let v = mgr.get_volume(&pinned).unwrap();
        for i in 0..16u64 {
            v.write(i * slot, &vec![0x40 + i as u8; 4096]).await.unwrap();
        }
        let clone = mgr.create_snapshot(pinned, "clone").await.unwrap();
        let cv = mgr.get_volume(&clone).unwrap();
        for i in 0..4u64 {
            cv.write(i * slot, &[0x99; 4096]).await.unwrap();
        }
        {
            let gem = mgr.gem().read().await;
            for id in [pinned, clone] {
                assert!(gem.get_volume_map(&id).unwrap().all_legs().all(|l| l.slab_id == aslab), "{id:?} left its array");
            }
            assert!(gem.get_volume_map(&plain).unwrap().all_legs().all(|l| l.slab_id == gid), "the pool used the array");
        }
        let mut buf = vec![0u8; 4096];
        v.read(5 * slot, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x45));

        // A pin carries the array's redundancy and nothing else.
        assert!(mgr
            .create_volume_with("m2", 1 << 20, CreateOptions { redundancy: RedundancyPolicy::mirror(2), ..CreateOptions::pinned_to(aslab) })
            .await
            .is_err());

        // Full is full: the pinned volume is refused, never spilled.
        let big = mgr.create_volume("big", 64 << 20, array).await.unwrap();
        let bv = mgr.get_volume(&big).unwrap();
        let mut refused = false;
        for i in 0..2048u64 {
            if bv.write(i * slot, &[3u8; 4096]).await.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "the array never filled");
        assert!(mgr.gem().read().await.get_volume_map(&big).unwrap().all_legs().all(|l| l.slab_id == aslab));

        // The array's own record carries its volumes and nobody else's.
        mgr.persist().await;
        {
            let reg = mgr.registry().read().await;
            let doc = metav2::read_slab(reg.get(&aslab).unwrap()).await.unwrap().unwrap();
            let mut names: Vec<String> = doc.volumes.iter().map(|v| v.name.clone()).collect();
            names.sort();
            assert_eq!(names, vec!["big", "clone", "mirror-vol"]);
            assert!(doc.volumes.iter().all(|v| v.array_id == Some(array)));
            assert_eq!(doc.arrays.len(), 1);
            assert_eq!(doc.arrays[0].array_id, array);
        }

        // Removal waits for every volume on it.
        assert!(mgr.remove_array(&array).await.is_err());
        let names: Vec<String> = mgr.volumes_on_slab(aslab).await.into_iter().map(|(_, n, _)| n).collect();
        assert_eq!(names, vec!["big", "clone", "mirror-vol"]);
        for id in [clone, pinned, big] {
            mgr.delete_volume(id).await.unwrap();
        }
        mgr.remove_array(&array).await.unwrap();
        assert!(mgr.registry().read().await.get(&aslab).is_none(), "the slab went with the array");
        assert!(!mgr.is_metadata_slab(&aslab));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A new head that reassembles the members adopts the array's slab: it is
    /// still dedicated (the flag is on disk), the volume comes back, and it is
    /// still pinned (the array is named in the slab's own record).
    #[tokio::test]
    async fn an_adopted_array_slab_keeps_its_pins_and_its_dedication() {
        let d = dir();
        let slot = 4096u64;
        let path = d.join("array.bin");
        let array = RaidArrayId(uuid::Uuid::new_v4());
        let (aslab, pinned) = {
            let mut mgr = VolumeManager::new(slot);
            let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), 4 * 1024 * 1024).await.unwrap();
            let aslab = mgr.add_dedicated_array(array, Arc::new(dev)).await.unwrap();
            let pinned = mgr.create_volume("pv", 1 << 20, array).await.unwrap();
            mgr.get_volume(&pinned).unwrap().write(0, &[7u8; 4096]).await.unwrap();
            mgr.persist().await;
            (aslab, pinned)
        };

        let mut head = VolumeManager::new(slot);
        let (general, _) = file_slab(&d, "general", slot).await;
        head.add_slab(general).await;
        let dev = FileDevice::open(path.to_str().unwrap()).await.unwrap();
        let slab = Slab::open(Arc::new(dev)).await.unwrap();
        assert!(slab.is_dedicated());
        let report = head
            .adopt_slabs(vec![crate::drive::discover::FoundSlab { label: "array".into(), slab }])
            .await
            .unwrap();
        assert_eq!(report.volumes.len(), 1);
        assert!(head.registry().read().await.is_dedicated(&aslab));
        assert_eq!(head.array_slab(&array), Some(aslab));
        let h = head.get_volume_handle(&pinned).unwrap();
        assert_eq!(h.pinned_slab(), Some(aslab), "still pinned after adoption");
        let mut buf = vec![0u8; 4096];
        h.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 7));
        // New writes stay on the array; the general slab is not a fallback.
        h.write(8 * slot, &[8u8; 4096]).await.unwrap();
        assert!(head.gem().read().await.get_volume_map(&pinned).unwrap().all_legs().all(|l| l.slab_id == aslab));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn create_refuses_a_policy_the_node_cannot_place() {
        let d = dir();
        let mut mgr = VolumeManager::new(4096);
        let (s, _) = file_slab(&d, "only", 4096).await;
        mgr.add_slab(s).await;
        let err = mgr
            .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
            .await
            .unwrap_err();
        assert!(matches!(err, VolumeError::InsufficientDomains { needed: 2, available: 1, .. }), "{err}");
        // Nothing was created.
        assert!(mgr.list_volumes().await.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An empty volume on a node with only a data slab comes back from the
    /// data directory as a data volume that takes writes (#141). It came back
    /// as `System`, with no system slab to write into.
    #[tokio::test]
    async fn an_empty_volume_on_a_data_only_node_is_still_writable_after_a_restart() {
        use crate::drive::slab::{SlabFormat, SlabRole};
        let d = dir();
        let meta = d.join("meta");
        let slot = 4096u64;
        let path = d.join("data.bin").to_str().unwrap().to_string();
        let id = {
            let dev = FileDevice::open_with_capacity(&path, 8 * 1024 * 1024).await.unwrap();
            let slab = Slab::format_with(
                Arc::new(dev),
                SlabFormat::new(slot, StorageTier::Hot).with_role(SlabRole::Data),
            )
            .await
            .unwrap();
            let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
            mgr.add_slab(slab).await;
            let id = mgr.create_volume_any("pvc-empty", 1 << 20).await.unwrap();
            assert_eq!(mgr.volume_role(&id), Some(SlabRole::Data));
            mgr.persist().await;
            id
        };
        let mut mgr = VolumeManager::with_data_dir(slot, meta).unwrap();
        let dev = FileDevice::open(&path).await.unwrap();
        mgr.add_slab(Slab::open(Arc::new(dev)).await.unwrap()).await;
        mgr.restore().await.unwrap();
        assert_eq!(mgr.volume_role(&id), Some(SlabRole::Data), "the half it can live in");
        let v = mgr.get_volume(&id).unwrap();
        v.write(0, &[7u8; 4096]).await.expect("an empty data volume takes writes after a restart");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_redundant_volume_survives_a_restart() {
        let d = dir();
        let meta = d.join("meta");
        let slot = 4096u64;
        let data: Vec<u8> = (0..2 * slot as usize + 100).map(|i| (i % 241) as u8).collect();

        let (vol_id, paths) = {
            let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
            let (a, pa) = file_slab(&d, "a", slot).await;
            let (b, pb) = file_slab(&d, "b", slot).await;
            mgr.add_slab(a).await;
            mgr.add_slab(b).await;
            let id = mgr
                .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
                .await
                .unwrap();
            let v = mgr.get_volume(&id).unwrap();
            let mut off = 0;
            while off < data.len() {
                off += v.write(off as u64, &data[off..]).await.unwrap();
            }
            v.flush().await.unwrap();
            // A clone inherits the policy.
            let snap = mgr.create_snapshot(id, "clone").await.unwrap();
            assert_eq!(mgr.redundancy(&snap).unwrap(), RedundancyPolicy::mirror(2));
            mgr.persist().await;
            (id, vec![pa, pb])
        };

        let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
        for p in &paths {
            let dev = FileDevice::open(p).await.unwrap();
            mgr.add_slab(Slab::open(Arc::new(dev)).await.unwrap()).await;
        }
        mgr.restore().await.unwrap();
        assert_eq!(mgr.redundancy(&vol_id).unwrap(), RedundancyPolicy::mirror(2));
        let h = mgr.health(&vol_id).await.unwrap();
        assert_eq!(h.state, HealthState::Healthy, "{h:?}");
        assert_eq!(h.legs_expected, 6, "three extents, two legs each");
        let v = mgr.get_volume(&vol_id).unwrap();
        let mut got = vec![0u8; data.len()];
        let mut off = 0;
        while off < got.len() {
            let end = got.len();
            off += v.read(off as u64, &mut got[off..end]).await.unwrap();
        }
        assert_eq!(got, data);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn setting_mirror_on_a_plain_volume_takes_effect_at_resync() {
        let d = dir();
        let slot = 4096u64;
        let mut mgr = VolumeManager::new(slot);
        let (a, _) = file_slab(&d, "a", slot).await;
        mgr.add_slab(a).await;
        let id = mgr.create_volume_any("plain", 1 << 20).await.unwrap();
        let v = mgr.get_volume(&id).unwrap();
        for i in 0..4u64 {
            v.write(i * slot, &vec![i as u8 + 1; slot as usize]).await.unwrap();
        }
        assert_eq!(mgr.get_volume_handle(&id).unwrap().physical().await, 4 * slot);

        // One drive: a mirror cannot be promised.
        assert!(mgr.set_redundancy(id, RedundancyPolicy::mirror(2)).await.is_err());
        let (b, _) = file_slab(&d, "b", slot).await;
        mgr.add_slab(b).await;
        mgr.set_redundancy(id, RedundancyPolicy::mirror(2)).await.unwrap();
        assert_eq!(mgr.health(&id).await.unwrap().state, HealthState::Degraded, "asked for more than exists");

        let report = mgr.resync_volume(id, false).await.unwrap();
        assert_eq!(report.legs_added, 4, "{report:?}");
        let h = mgr.health(&id).await.unwrap();
        assert_eq!(h.state, HealthState::Healthy, "{h:?}");
        assert_eq!(mgr.get_volume_handle(&id).unwrap().physical().await, 8 * slot);
        for i in 0..4u64 {
            let mut back = vec![0u8; slot as usize];
            v.read(i * slot, &mut back).await.unwrap();
            assert!(back.iter().all(|&b| b == i as u8 + 1));
        }

        // Parity is a restripe, refused as a setting.
        let err = mgr.set_redundancy(id, RedundancyPolicy::parity(2, 1)).await.unwrap_err();
        assert!(matches!(err, VolumeError::RestripeRequired { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A leg a golden shares with its snapshot is rebuilt once and every map
    /// that names it is pointed at the new slot (#146): the shared extents are
    /// published together at the end, while an extent the source has since
    /// written (its own now) is published under its lock.
    #[tokio::test]
    async fn a_shared_leg_is_rebuilt_once_for_every_map_that_names_it() {
        let d = dir();
        let slot = 4096u64;
        let mut mgr = VolumeManager::new(slot);
        let mut sids = Vec::new();
        for n in ["a", "b", "c"] {
            let (s, _) = file_slab(&d, n, slot).await;
            sids.push(s.slab_id());
            mgr.add_slab(s).await;
        }
        let src = mgr.create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2))).await.unwrap();
        let v = mgr.get_volume(&src).unwrap();
        for i in 0..6u64 {
            v.write(i * slot, &vec![0x30 + i as u8; slot as usize]).await.unwrap();
        }
        let snap = mgr.create_snapshot(src, "snap").await.unwrap();
        // The source moves on for one extent: that one is its own now.
        v.write(0, &vec![0x77; slot as usize]).await.unwrap();

        // The drive holding the most legs goes bad.
        let lost = {
            let gem = mgr.gem().read().await;
            *sids.iter().max_by_key(|s| gem.slab_extents(**s).len()).unwrap()
        };
        let touched = mgr.distrust_slab(lost).await;
        assert!(touched.contains(&src) && touched.contains(&snap), "{touched:?}");
        let free_before = mgr.registry().read().await.total_free_slots();

        let report = mgr.resync_volume(src, false).await.unwrap();
        assert_eq!(report.unrecoverable, 0, "{report:?}");
        assert_eq!(mgr.health(&src).await.unwrap().state, HealthState::Healthy);
        // The snapshot's shared legs moved with the source's. The one extent
        // the source rewrote is the snapshot's alone now, so that one is left
        // for the snapshot's own resync: at most one leg.
        {
            let gem = mgr.gem().read().await;
            assert!(gem.get_volume_map(&src).unwrap().all_legs().all(|l| l.slab_id != lost));
            let smap = gem.get_volume_map(&snap).unwrap();
            for (vext, loc) in &smap.extents {
                if vext != 0 {
                    assert!(loc.legs().all(|l| l.slab_id != lost), "snapshot extent {vext} still names the lost slab");
                }
            }
        }
        let again = mgr.resync_volume(snap, false).await.unwrap();
        assert!(again.legs_rebuilt <= 1, "{again:?}");
        assert!(mgr.gem().read().await.get_volume_map(&snap).unwrap().all_legs().all(|l| l.slab_id != lost));
        assert_eq!(mgr.health(&snap).await.unwrap().state, HealthState::Healthy);
        // Rebuilt once, not once per map: the pool gave up one slot per leg
        // that moved and got back the one it replaced.
        let free_after = mgr.registry().read().await.total_free_slots();
        assert_eq!(free_before, free_after, "every rebuilt slot freed the one it replaced");

        let sv = mgr.get_volume(&snap).unwrap();
        for i in 0..6u64 {
            let mut back = vec![0u8; slot as usize];
            sv.read(i * slot, &mut back).await.unwrap();
            assert!(back.iter().all(|&b| b == 0x30 + i as u8), "snapshot extent {i}");
            v.read(i * slot, &mut back).await.unwrap();
            let want = if i == 0 { 0x77 } else { 0x30 + i as u8 };
            assert!(back.iter().all(|&b| b == want), "source extent {i}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn distrust_touches_only_redundant_volumes_with_a_leg_there() {
        let d = dir();
        let slot = 4096u64;
        let mut mgr = VolumeManager::new(slot);
        let (a, _) = file_slab(&d, "a", slot).await;
        let (b, _) = file_slab(&d, "b", slot).await;
        let ida = a.slab_id();
        mgr.add_slab(a).await;
        mgr.add_slab(b).await;
        let plain = mgr.create_volume_any("plain", 1 << 20).await.unwrap();
        let m = mgr.create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2))).await.unwrap();
        let untouched = mgr.create_volume_with("u", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2))).await.unwrap();
        mgr.get_volume(&plain).unwrap().write(0, &[1u8; 4096]).await.unwrap();
        mgr.get_volume(&m).unwrap().write(0, &[2u8; 4096]).await.unwrap();

        let touched = mgr.distrust_slab(ida).await;
        assert_eq!(touched, vec![m], "only the mirror with a leg on a");
        assert_eq!(mgr.get_volume_handle(&m).unwrap().failed_slabs(), vec![ida]);
        assert!(mgr.get_volume_handle(&plain).unwrap().failed_slabs().is_empty(), "the only copy stays trusted");
        assert!(mgr.get_volume_handle(&untouched).unwrap().failed_slabs().is_empty());
        assert_eq!(mgr.health(&m).await.unwrap().state, HealthState::Degraded);
        let mut buf = vec![0u8; 4096];
        mgr.get_volume(&m).unwrap().read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&x| x == 2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn restripe_moves_a_volume_between_policies_with_its_data() {
        let d = dir();
        let slot = 4096u64;
        let mut mgr = VolumeManager::new(slot);
        for n in ["a", "b", "c"] {
            let (s, _) = file_slab(&d, n, slot).await;
            mgr.add_slab(s).await;
        }
        let free0 = mgr.registry().read().await.total_free_slots();
        let id = mgr.create_volume_any("v", 1 << 20).await.unwrap();
        let v = mgr.get_volume(&id).unwrap();
        let datas: Vec<Vec<u8>> = (0..5).map(|i| vec![0xA0 + i as u8; slot as usize]).collect();
        for (i, dd) in datas.iter().enumerate() {
            v.write(i as u64 * slot, dd).await.unwrap();
        }
        assert_eq!(mgr.registry().read().await.total_free_slots(), free0 - 5);

        // none → raid5:2+1: 5 data slots + 3 stripes of parity.
        let r = mgr.restripe(id, RedundancyPolicy::parity(2, 1)).await.unwrap();
        assert_eq!(r.extents_copied, 5);
        assert_eq!(r.slots_released, 5, "the old placement is gone");
        assert_eq!(mgr.redundancy(&id).unwrap(), RedundancyPolicy::parity(2, 1));
        assert_eq!(mgr.registry().read().await.total_free_slots(), free0 - 8);
        assert_eq!(mgr.health(&id).await.unwrap().state, HealthState::Healthy);
        let v = mgr.get_volume(&id).unwrap();
        for (i, dd) in datas.iter().enumerate() {
            let mut buf = vec![0u8; slot as usize];
            v.read(i as u64 * slot, &mut buf).await.unwrap();
            assert_eq!(&buf, dd, "extent {i} after restripe to parity");
        }
        assert_eq!(mgr.get_volume_handle(&id).unwrap().physical().await, 8 * slot);

        // raid5 → mirror:2: 10 slots.
        let r = mgr.restripe(id, RedundancyPolicy::mirror(2)).await.unwrap();
        assert_eq!(r.slots_released, 8);
        assert_eq!(mgr.registry().read().await.total_free_slots(), free0 - 10);
        for (i, dd) in datas.iter().enumerate() {
            let mut buf = vec![0u8; slot as usize];
            v.read(i as u64 * slot, &mut buf).await.unwrap();
            assert_eq!(&buf, dd, "extent {i} after restripe to mirror");
        }
        assert_eq!(mgr.health(&id).await.unwrap().state, HealthState::Healthy);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The dirty-stripe log: a parity write marks its stripe, a flush clears
    /// it, and a restart with marks left over verifies exactly those stripes
    /// and leaves nothing behind.
    #[tokio::test]
    async fn dirty_stripes_are_logged_cleared_on_flush_and_verified_on_restart() {
        let d = dir();
        let meta = d.join("meta");
        let slot = 4096u64;
        let (id, paths, parity_leg) = {
            let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
            let mut paths = Vec::new();
            for n in ["a", "b", "c"] {
                let (s, p) = file_slab(&d, n, slot).await;
                mgr.add_slab(s).await;
                paths.push(p);
            }
            let id = mgr.create_volume_with("p", 1 << 20, CreateOptions::redundant(RedundancyPolicy::parity(2, 1))).await.unwrap();
            let h = mgr.get_volume_handle(&id).unwrap();
            h.write(0, &[1u8; 4096]).await.unwrap();
            h.write(slot, &[2u8; 4096]).await.unwrap();
            h.write(2 * slot, &[3u8; 4096]).await.unwrap();
            assert_eq!(h.dirty_stripes(), vec![0, 1], "two stripes were written since the last flush");
            assert!(meta.join(format!("stripes-{}.log", id.0.simple())).exists());
            h.flush().await.unwrap();
            assert!(h.dirty_stripes().is_empty());
            assert!(!meta.join(format!("stripes-{}.log", id.0.simple())).exists());

            // A write after the flush, then a "crash": no flush, metadata persisted.
            h.write(0, &[9u8; 4096]).await.unwrap();
            assert_eq!(h.dirty_stripes(), vec![0]);
            let parity_leg = mgr.gem().read().await.lookup_parity(id, 0).unwrap().legs[0];
            // Corrupt stripe 0's parity to prove the restart recomputes it.
            {
                let reg = mgr.registry().read().await;
                reg.get(&parity_leg.slab_id).unwrap().write_slot(parity_leg.slot_idx, 0, &[0xFF; 4096]).await.unwrap();
            }
            mgr.persist().await;
            (id, paths, parity_leg)
        };

        let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
        for p in &paths {
            let dev = FileDevice::open(p).await.unwrap();
            mgr.add_slab(Slab::open(Arc::new(dev)).await.unwrap()).await;
        }
        mgr.restore().await.unwrap();
        assert!(!meta.join(format!("stripes-{}.log", id.0.simple())).exists(), "verified stripes are cleared");
        let mut p = vec![0u8; 4096];
        mgr.registry().read().await.get(&parity_leg.slab_id).unwrap().read_slot(parity_leg.slot_idx, 0, &mut p).await.unwrap();
        let want: Vec<u8> = (0..4096).map(|_| 9u8 ^ 2u8).collect();
        assert_eq!(p, want, "stripe 0 parity recomputed on restart");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// #228: a volume presented at 512 says so to every reader, takes a
    /// 512-byte write without disturbing its neighbours, hands the size to
    /// its clones, and keeps it across a restart.
    #[tokio::test]
    async fn a_512_byte_volume_is_512_to_readers_clones_and_restarts() {
        let d = dir();
        let meta = d.join("meta");
        let slot = 4096u64;
        let (boot, clone, plain, path) = {
            let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
            let (s, p) = file_slab(&d, "a", slot).await;
            mgr.add_slab(s).await;
            let boot = mgr.create_volume_any("boot", 1 << 20).await.unwrap();
            let plain = mgr.create_volume_any("plain", 1 << 20).await.unwrap();
            assert_eq!(mgr.lba(&boot), Some(Lba::DEFAULT), "4096 unless told");
            assert!(mgr.set_lba(boot, 1024).await.is_err(), "only 512 and 4096");
            mgr.set_lba(boot, Lba::BOOT).await.unwrap();

            let dev = mgr.get_volume(&boot).unwrap();
            assert_eq!(dev.block_size(), 512);
            assert_eq!(mgr.get_volume(&plain).unwrap().block_size(), 4096);
            // What an NVMe/TCP host is told: LBADS 9, and the capacity in
            // 512-byte blocks.
            #[cfg(feature = "nvmeof")]
            {
                let id = crate::target::nvmeof::admin::identify_namespace(&dev);
                assert_eq!(id[130], 9);
                assert_eq!(u64::from_le_bytes(id[0..8].try_into().unwrap()), (1 << 20) / 512);
            }

            // A one-sector write in the middle of a 4096-byte block.
            dev.write(0, &[0xAAu8; 4096]).await.unwrap();
            dev.write(1024, &[0x55u8; 512]).await.unwrap();
            let mut back = vec![0u8; 4096];
            dev.read(0, &mut back).await.unwrap();
            assert!(back[..1024].iter().all(|&b| b == 0xAA));
            assert!(back[1024..1536].iter().all(|&b| b == 0x55));
            assert!(back[1536..].iter().all(|&b| b == 0xAA));

            mgr.seal_volume(boot, None).await.unwrap();
            let clone = mgr.create_snapshot(boot, "boot-clone").await.unwrap();
            assert_eq!(mgr.lba(&clone), Some(Lba::BOOT), "a clone is read by whoever read its source");
            mgr.persist().await;
            (boot, clone, plain, p)
        };

        let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
        let dev = FileDevice::open(&path).await.unwrap();
        mgr.add_slab(Slab::open(Arc::new(dev)).await.unwrap()).await;
        mgr.restore().await.unwrap();
        assert_eq!(mgr.lba(&boot), Some(Lba::BOOT), "the size survives a restart");
        assert_eq!(mgr.lba(&clone), Some(Lba::BOOT));
        assert_eq!(mgr.lba(&plain), Some(Lba::DEFAULT));
        let mut back = vec![0u8; 512];
        mgr.get_volume(&clone).unwrap().read(1024, &mut back).await.unwrap();
        assert!(back.iter().all(|&b| b == 0x55));
    }

    /// #76: a sealed volume takes no writes, a clone records its parent and
    /// inherits the filesystem record, and all of it survives a restart.
    #[tokio::test]
    async fn sealing_and_lineage_are_volume_facts_that_survive_a_restart() {
        let d = dir();
        let meta = d.join("meta");
        let slot = 4096u64;
        let (golden, child, grandchild, path) = {
            let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
            let (s, p) = file_slab(&d, "a", slot).await;
            mgr.add_slab(s).await;
            let golden = mgr.create_volume_any("golden", 1 << 20).await.unwrap();
            mgr.get_volume(&golden).unwrap().write(0, &[7u8; 4096]).await.unwrap();
            let fs = FsInfo {
                kind: "ext4".into(), journal: true, features: None, sixty_four_bit: false,
                metadata_csum: true, csum_seed: true, label: "root".into(),
                uuid: Some(uuid::Uuid::from_u128(0xA0)),
            };
            mgr.seal_volume(golden, Some(fs.clone())).await.unwrap();
            assert!(mgr.is_sealed(&golden));
            let err = mgr.get_volume(&golden).unwrap().write(0, &[1u8; 4096]).await.unwrap_err();
            assert!(err.to_string().contains("sealed"), "{err}");
            assert!(mgr.get_volume(&golden).unwrap().discard(0, 4096).await.is_err());
            assert!(matches!(mgr.shrink_volume(golden, 4096).await, Err(VolumeError::Sealed(_))));

            let child = mgr.create_snapshot(golden, "child").await.unwrap();
            assert_eq!(mgr.parent(&child), Some(golden));
            assert_eq!(mgr.fs_info(&child).unwrap().uuid, fs.uuid, "inherited until stamped");
            assert!(!mgr.is_sealed(&child), "a clone is writable");
            mgr.set_fs_uuid(child, uuid::Uuid::from_u128(0xB0)).await.unwrap();
            let grandchild = mgr.create_snapshot(child, "grandchild").await.unwrap();
            assert_eq!(mgr.lineage(&grandchild), vec![grandchild, child, golden]);
            assert_eq!(mgr.children(&golden), vec![child]);
            mgr.persist().await;
            (golden, child, grandchild, p)
        };

        let mut mgr = VolumeManager::with_data_dir(slot, meta.clone()).unwrap();
        let dev = FileDevice::open(&path).await.unwrap();
        mgr.add_slab(Slab::open(Arc::new(dev)).await.unwrap()).await;
        mgr.restore().await.unwrap();
        assert!(mgr.is_sealed(&golden), "sealing survives");
        assert!(!mgr.is_sealed(&child));
        assert_eq!(mgr.lineage(&grandchild), vec![grandchild, child, golden]);
        assert_eq!(mgr.fs_info(&child).unwrap().uuid, Some(uuid::Uuid::from_u128(0xB0)));
        assert_eq!(mgr.fs_info(&grandchild).unwrap().uuid, Some(uuid::Uuid::from_u128(0xB0)), "inherited the stamped one");
        assert!(mgr.get_volume(&golden).unwrap().write(0, &[1u8; 4096]).await.is_err());
        assert_eq!(mgr.find_volume("child").await, Some(child));
        assert_eq!(mgr.find_volume(&golden.0.to_string()).await, Some(golden));

        // Deleting the golden leaves the child's link as a record.
        mgr.delete_volume(golden).await.unwrap();
        assert_eq!(mgr.parent(&child), Some(golden));
        assert_eq!(mgr.lineage(&child), vec![child, golden]);
        let mut buf = vec![0u8; 4096];
        mgr.get_volume(&child).unwrap().read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 7), "the clone keeps its refcounted extents");
        let _ = std::fs::remove_dir_all(&d);
    }
}
