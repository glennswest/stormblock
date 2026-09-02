//! Volume manager — thin provisioning, COW snapshots, slab-based allocation.
//!
//! The `VolumeManager` coordinates thin volumes on top of slab-backed storage.
//! Each `ThinVolume` implements `BlockDevice`, so target protocols
//! (NVMe-oF, iSCSI) see volumes as plain block devices.

#[cfg(feature = "stormfs-data")]
pub mod chunk;
pub mod extent;
pub mod gem;
pub mod metadata;
pub mod redundancy;
pub mod thin;
pub mod snapshot;
pub mod compose;
pub mod stripe;
pub mod stripelog;
#[cfg(feature = "stormfs-data")]
pub mod versioned;
pub mod gc;
pub mod pressure;
pub mod relocate;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::drive::slab::{Slab, SlabId, SlabRole};
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::topology::StorageTier;
use crate::raid::RaidArrayId;

pub use extent::{ExtentAllocator, VolumeId, DEFAULT_EXTENT_SIZE};
pub use metadata::{FsInfo, MetadataStore, Retention};
pub use thin::{ThinVolume, ThinVolumeHandle, VolumeError, PlacementPolicy, VolumeHealth, HealthState, ResyncReport};
pub use redundancy::{Redundancy, RedundancyPolicy};

/// What a restripe did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestripeReport {
    pub extents_copied: usize,
    pub slots_released: usize,
    pub redundancy: String,
}

/// Everything a volume is created with beyond a name and a size.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub redundancy: RedundancyPolicy,
    pub placement: PlacementPolicy,
}

impl CreateOptions {
    pub fn redundant(policy: RedundancyPolicy) -> Self {
        CreateOptions { redundancy: policy, ..Default::default() }
    }

    /// Place this volume in the node's data slabs — identity and state,
    /// which no install may reformat (#88).
    pub fn in_role(mut self, role: SlabRole) -> Self {
        self.placement.role = role;
        self
    }
}
pub use gem::GlobalExtentMap;

/// Default slot size for slabs created via add_backing_device.
pub const DEFAULT_SLOT_SIZE: u64 = crate::drive::slab::DEFAULT_SLOT_SIZE;

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
    /// What is known about the filesystem on each volume.
    fs_info: HashMap<VolumeId, FsInfo>,
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
            fs_info: HashMap::new(),
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
            fs_info: HashMap::new(),
        })
    }

    // ── Lineage, sealing, filesystem identity (#76) ────────────────────

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

    /// Reopen a sealed volume for writes. Exists for an operator undoing a
    /// mistake; clones already taken keep their own extents either way.
    pub async fn unseal_volume(&mut self, id: VolumeId) -> Result<(), VolumeError> {
        let handle = self.volumes.get(&id).ok_or(VolumeError::VolumeNotFound(id))?.clone();
        handle.set_sealed(false);
        self.persist().await;
        Ok(())
    }

    pub fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo> {
        self.fs_info.get(id)
    }

    pub async fn set_fs_info(&mut self, id: VolumeId, fs: Option<FsInfo>) -> Result<(), VolumeError> {
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
        self.persist().await;
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
        if slab.slot_size() != self.slot_size {
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
        if !self.array_slabs.contains_key(&array_id) {
            return Err(VolumeError::AllocatorError(
                format!("no backing device for array {array_id}")
            ));
        }
        self.create_volume_with(name, virtual_size, CreateOptions::default()).await
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
        let needed = opts.redundancy.scheme.width();
        if needed > 1 {
            let available = self
                .registry
                .read()
                .await
                .distinct_domains_with_space_in_role(&opts.redundancy.spread, opts.placement.role);
            if available < needed {
                return Err(VolumeError::InsufficientDomains {
                    policy: opts.redundancy.spelling(),
                    needed,
                    available,
                });
            }
        }
        let vol = ThinVolume::new(name.to_string(), virtual_size, self.slot_size);
        let id = vol.id();
        let parity = opts.redundancy.scheme.is_parity();
        let handle = Arc::new(ThinVolumeHandle::with_redundancy(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            opts.placement,
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
        {
            let mut reg = self.registry.write().await;
            for f in found {
                let id = f.slab.slab_id();
                if f.slab.slot_size() != self.slot_size {
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
        let mut records: Vec<metadata::VolumeRecord> = Vec::new();
        {
            let reg = self.registry.read().await;
            let mut seen: HashSet<VolumeId> = HashSet::new();
            for slab_id in &metadata_slabs {
                let Some(slab) = reg.get(slab_id) else { continue };
                let bytes = match slab.read_metadata().await {
                    Ok(Some(b)) => b,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(slab = %slab_id, "adopt: metadata unreadable: {e}");
                        continue;
                    }
                };
                let doc = match MetadataStore::decode(&bytes) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(slab = %slab_id, "adopt: metadata undecodable: {e}");
                        continue;
                    }
                };
                for v in doc.volumes {
                    if seen.insert(v.id) {
                        records.push(v);
                    }
                }
            }
        }

        for vrec in records {
            if self.volumes.contains_key(&vrec.id) {
                report.already_known += 1;
                continue;
            }

            // Restore only what this drive can actually serve. `restore_mapping`
            // never displaces an existing claim, so a golden and the clone that
            // shares its slots both land without either stealing the other's.
            {
                let reg = self.registry.read().await;
                let mut gem = self.gem.write().await;
                for (vext, loc) in &vrec.extents {
                    if reg.get(&loc.slab_id).is_some() {
                        gem.restore_mapping(vrec.id, *vext, loc.clone());
                    }
                }
                for (stripe, g) in &vrec.parity {
                    gem.restore_parity(vrec.id, *stripe, g.clone());
                }
            }

            let vol = ThinVolume::restore(
                vrec.id, vrec.name.clone(), vrec.virtual_size, self.slot_size,
            );
            let handle = Arc::new(ThinVolumeHandle::with_redundancy(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy::default(),
                vrec.redundancy.clone(),
            ));
            handle.set_failed_slabs(vrec.failed_slabs.iter().copied());
            handle.set_sealed(vrec.sealed);
            if let Some(fs) = vrec.fs.clone() {
                self.fs_info.insert(vrec.id, fs);
            }
            self.volumes.insert(vrec.id, handle);
            if let Some(parent) = vrec.parent {
                self.parents.insert(vrec.id, parent);
            }
            report.volumes.push((vrec.id, vrec.name, vrec.virtual_size));
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
        for (source, at) in placements {
            let handle = self.volumes.get(source)
                .ok_or(VolumeError::VolumeNotFound(*source))?;
            components.push(compose::Component {
                source: *source,
                at: *at,
                span: handle.capacity_bytes(),
            });
        }

        let vol = {
            let mut gem = self.gem.write().await;
            let mut reg = self.registry.write().await;
            compose::compose_volume(
                name, declared_size, self.slot_size, &components, &mut gem, &mut reg,
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
                .distinct_domains_with_space_in_role(&policy.spread, handle.placement_role());
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
        let report = handle.resync(verify).await;
        self.persist().await;
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
                .distinct_domains_with_space_in_role(&policy.spread, handle.placement_role());
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
            gem.volume_extents(&id).map(|it| it.map(|(v, _)| *v).collect()).unwrap_or_default()
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
            let mut by_slab: HashMap<SlabId, Vec<u32>> = HashMap::new();
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
        let _handle = self.volumes.remove(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?;
        self.parents.remove(&id);
        self.fs_info.remove(&id);
        self.retentions.remove(&id);

        // Remove all extents from GEM and dec_ref on slabs
        let mut gem = self.gem.write().await;
        let mut reg = self.registry.write().await;
        snapshot::delete_snapshot(id, &mut gem, &mut reg).await?;
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
        let source_handle = self.volumes.get(&source_id)
            .ok_or(VolumeError::VolumeNotFound(source_id))?
            .clone();
        let source_vol = source_handle.lock().await;
        let virtual_size = source_vol.virtual_size;
        let slot_size = source_vol.slot_size;
        drop(source_vol);

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
        self.persist().await;
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
            placement: PlacementPolicy { role, ..Default::default() },
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
                .map(|it| it.map(|(v, _)| *v).collect())
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
        let (policy, failed, role) = match self.volumes.get(source_id) {
            Some(src) => (src.redundancy(), src.failed_slabs(), src.placement_role()),
            None => (RedundancyPolicy::none(), Vec::new(), SlabRole::System),
        };
        let handle = ThinVolumeHandle::with_redundancy(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy { role, ..Default::default() },
            policy,
        );
        handle.set_failed_slabs(failed);
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
    pub async fn persist(&self) {
        if self.metadata_store.is_none() && self.metadata_slabs.is_empty() {
            return;
        }

        if let Some(store) = &self.metadata_store {
            let meta = self.snapshot_metadata().await;
            if let Err(e) = store.save(&meta) {
                tracing::warn!("Volume metadata persist failed: {e}");
            }
        }

        for (slab_id, meta) in self.per_slab_metadata().await {
            match MetadataStore::encode(&meta) {
                Ok(bytes) => {
                    let reg = self.registry.read().await;
                    match reg.get(&slab_id) {
                        Some(slab) => {
                            if let Err(e) = slab.write_metadata(&bytes).await {
                                tracing::warn!("Slab metadata persist failed: {e}");
                            }
                        }
                        None => tracing::warn!(
                            "Slab metadata persist failed: slab {} is not attached",
                            slab_id.0
                        ),
                    }
                }
                Err(e) => tracing::warn!("Slab metadata encode failed: {e}"),
            }
        }
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
        if self.metadata_slabs.len() == 1 {
            return vec![(self.metadata_slabs[0], full)];
        }

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
        let home = |vid: &VolumeId| -> Option<SlabId> {
            let want = roles.get(vid).copied().unwrap_or_default();
            self.metadata_slabs.iter().copied().find(|s| reg.role_of(s) == want)
        };
        let array_of: HashMap<SlabId, RaidArrayId> = self
            .array_slabs
            .iter()
            .map(|(array, slab)| (*slab, *array))
            .collect();

        self.metadata_slabs
            .iter()
            .map(|slab_id| {
                let volumes: Vec<_> = full
                    .volumes
                    .iter()
                    .filter(|v| match touched.get(&v.id) {
                        Some(on) if !on.is_empty() => on.contains(slab_id),
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
                    metadata::VolumeMetadata { extent_size: full.extent_size, arrays, volumes },
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
            let Some(bytes) = slab.read_metadata().await? else { continue };
            found = true;
            let doc = MetadataStore::decode(&bytes)?;
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
            .map(|(id, name, virtual_size, redundancy, failed_slabs, sealed)| metadata::VolumeRecord {
                id,
                name,
                virtual_size,
                array_id: None,
                retention: self.retentions.get(&id).copied().unwrap_or_default(),
                parent: self.parents.get(&id).copied(),
                sealed,
                fs: self.fs_info.get(&id).cloned(),
                extents: gem
                    .get_volume_map(&id)
                    .map(|m| m.extents.clone())
                    .unwrap_or_default(),
                redundancy,
                parity: gem
                    .get_volume_map(&id)
                    .map(|m| m.parity.clone())
                    .unwrap_or_default(),
                failed_slabs,
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
        if let Some(store) = &self.metadata_store {
            store.save(&self.snapshot_metadata().await)?;
        }
        for (slab_id, meta) in self.per_slab_metadata().await {
            let bytes = MetadataStore::encode(&meta)?;
            let reg = self.registry.read().await;
            let slab = reg
                .get(&slab_id)
                .ok_or_else(|| anyhow::anyhow!("slab {} is not attached", slab_id.0))?;
            slab.write_metadata(&bytes).await?;
        }
        Ok(())
    }

    /// Restore volumes from persisted metadata. No-op if no data_dir or no metadata file.
    pub async fn restore(&mut self) -> anyhow::Result<()> {
        // A data directory wins where there is one: it is the record a running
        // node has been updating. The slab's own copy is the fallback for a
        // node that has no filesystem to keep one in.
        let from_dir = match &self.metadata_store {
            Some(s) if s.exists() => Some(s.load()?),
            _ => None,
        };
        let records: Vec<(metadata::VolumeRecord, SlabRole)> = match from_dir {
            Some(m) => m.volumes.into_iter().map(|v| (v, SlabRole::System)).collect(),
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
        let mut rebuilt = {
            let reg = self.registry.read().await;
            GlobalExtentMap::rebuild_from_slabs(reg.iter())
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
                let slot_gen = |leg: gem::Leg| -> Option<u64> {
                    reg.get(&leg.slab_id)
                        .and_then(|s| s.get_slot(leg.slot_idx))
                        .filter(|s| s.state != crate::drive::slab::SlotState::Free)
                        .map(|s| s.generation)
                };
                for (vext, loc) in &vrec.extents {
                    match rebuilt.lookup(vrec.id, *vext).cloned() {
                        None => {
                            if reg.get(&loc.slab_id).is_some() {
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
            let vol = ThinVolume::restore(
                vrec.id,
                vrec.name.clone(),
                vrec.virtual_size,
                self.slot_size,
            );
            let handle = Arc::new(ThinVolumeHandle::with_redundancy(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy { role, ..Default::default() },
                vrec.redundancy.clone(),
            ));
            handle.set_failed_slabs(vrec.failed_slabs.iter().copied());
            handle.set_sealed(vrec.sealed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::raid::{RaidArray, RaidLevel};

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

        // Slot-size mismatch must be rejected, not silently misread.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut wrong = VolumeManager::new(8192);
        assert!(wrong
            .open_backing_device(array_id, Arc::new(dev))
            .await
            .is_err());

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
