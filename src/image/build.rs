//! Building a raw image: lay out the partitions, then fill them.
//!
//! The order on disk is the order of the spec's sections — ESP, then pallets in
//! the order written, then raw partitions, then the slab — because allocation
//! is first-fit out of measured free runs and the builder fills them in that
//! sequence. It is worth being predictable here: an image is often read by
//! something that was told where to look.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::drive::filedev::FileDevice;
use crate::drive::partition::PartitionDevice;
use crate::drive::slab::{auto_metadata_bytes, Slab, SlabFormat, DEFAULT_SLOT_SIZE};
use crate::drive::BlockDevice;
use crate::mgmt::config::parse_size;
use crate::pallet::format::{parse_member_kind, parse_pallet_kind, MemberKind, PalletKind};
use crate::pallet::manager::PublishSpec;
use crate::pallet::{
    BytesContent, Gpt, MemberSpec, PalletManager, PalletStore, PartitionView, PALLET_TYPE_GUID,
};
use crate::placement::topology::StorageTier;
use crate::raid::RaidArrayId;
use crate::volume::{VolumeId, VolumeManager};

use super::{type_guid, EspSpec, GoldenSpec, ImageError, ImageSpec, MemberEntry, PalletEntry, Result};

/// Partitions are 1 MiB aligned, as everywhere else.
const ALIGN: u64 = 1024 * 1024;
/// Room for the GPT at both ends plus the first alignment gap.
const GPT_OVERHEAD: u64 = 2 * ALIGN;
/// Default ESP when the spec does not say. Large enough for FAT32 with room.
const DEFAULT_ESP: u64 = 100 * ALIGN;
/// Slack added to an estimated pallet partition, so a manifest and its
/// alignment always fit inside the estimate.
const PALLET_SLACK: u64 = ALIGN;
const COPY_CHUNK: usize = 4 * 1024 * 1024;

/// What a build produced.
#[derive(Debug, Clone, Serialize)]
pub struct BuildReport {
    pub path: PathBuf,
    pub format: String,
    pub size_bytes: u64,
    pub block_size: u32,
    pub partitions: Vec<PartitionReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PartitionReport {
    pub index: usize,
    pub name: String,
    pub kind: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pallet_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pallet_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Volumes laid down in a slab partition: a golden and its first clone
    /// for every `[[slab.golden]]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeReport>,
}

/// A volume the build created in the slab.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeReport {
    pub id: Uuid,
    pub name: String,
    pub size_bytes: u64,
    /// Bytes mapped to slab slots. A fresh clone maps exactly what its golden
    /// maps and shares every slot with it, so this is what the volume can
    /// read, not what it costs the slab.
    pub allocated_bytes: u64,
    /// The golden this volume was cloned from, when it is a clone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_of: Option<Uuid>,
    /// Sealed: what clones are taken from (#77). Every golden is.
    pub sealed: bool,
    /// The filesystem UUID the volume carries, when the build could read one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_uuid: Option<Uuid>,
}

/// Builds an image from a spec.
pub struct ImageBuilder {
    spec: ImageSpec,
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

fn size_of(s: &Option<String>) -> Result<Option<u64>> {
    match s.as_deref() {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("rest") => Ok(None),
        Some(v) => parse_size(v).map(Some).map_err(ImageError::Spec),
    }
}

fn is_rest(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("rest"))
}

async fn file_len(p: &Path) -> Result<u64> {
    Ok(tokio::fs::metadata(p).await?.len())
}

impl ImageBuilder {
    pub fn new(spec: ImageSpec) -> Self {
        ImageBuilder { spec }
    }

    pub fn spec(&self) -> &ImageSpec {
        &self.spec
    }

    /// How big the image has to be, when the spec does not say.
    ///
    /// Every estimate here is an upper bound: a pallet partition is sized from
    /// its members' bytes plus slack, and the real layout is never larger. An
    /// image that turns out roomier than it needed costs nothing on a sparse
    /// file, whereas one that turns out too small costs the whole build.
    pub async fn estimated_size(&self) -> Result<u64> {
        let mut total = GPT_OVERHEAD;
        total += align_up(self.esp_size().await?.unwrap_or(0), ALIGN);
        for p in &self.spec.pallets {
            total += align_up(self.pallet_size(p).await?, ALIGN);
        }
        for r in &self.spec.partitions {
            let size = match size_of(&r.size)? {
                Some(s) => s,
                None => match &r.from_file {
                    Some(f) => file_len(f).await?,
                    None => {
                        return Err(ImageError::Spec(format!(
                            "partition '{}' has neither a size nor a file to take one from",
                            r.name
                        )))
                    }
                },
            };
            total += align_up(size, ALIGN);
        }
        if let Some(slab) = &self.spec.slab {
            total += align_up(size_of(&slab.size)?.unwrap_or(64 * ALIGN), ALIGN);
        }
        Ok(total)
    }

    async fn esp_size(&self) -> Result<Option<u64>> {
        let Some(esp) = &self.spec.esp else { return Ok(None) };
        if let Some(s) = size_of(&esp.size)? {
            return Ok(Some(s));
        }
        if let Some(img) = &esp.from_image {
            return Ok(Some(file_len(img).await?));
        }
        Ok(Some(DEFAULT_ESP))
    }

    async fn pallet_size(&self, p: &PalletEntry) -> Result<u64> {
        if let Some(s) = size_of(&p.size)? {
            return Ok(s);
        }
        if let Some(src) = &p.from_image {
            // Whatever is in there, plus the table it needs at the front.
            return Ok(align_up(file_len(src).await?, ALIGN).min(file_len(src).await?) + PALLET_SLACK);
        }
        let mut bytes = 0u64;
        for m in &p.members {
            bytes += align_up(member_len(m).await?, 4096);
        }
        Ok(bytes + PALLET_SLACK)
    }

    /// Build the raw image at `out`.
    pub async fn build(&self, out: &Path) -> Result<BuildReport> {
        let lba = self.spec.block_size.unwrap_or(512);
        let declared = size_of(&self.spec.size)?;
        let rest_count = self.count_rest();
        if rest_count > 1 {
            return Err(ImageError::Spec(
                "only one partition can take the `rest` of an image".into(),
            ));
        }
        if rest_count == 1 && declared.is_none() {
            return Err(ImageError::Spec(
                "a partition asks for the `rest` of the image, but the image has no `size` — \
                 there is no rest to take"
                    .into(),
            ));
        }
        let total = match declared {
            Some(s) => s,
            None => self.estimated_size().await?,
        };
        let need = self.estimated_size().await?;
        if rest_count == 0 && total < need {
            return Err(ImageError::TooSmall { need, have: total });
        }

        // A sparse file: an image is mostly holes until something fills them,
        // and a 32 GiB image should not cost 32 GiB to build.
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        if tokio::fs::try_exists(out).await.unwrap_or(false) {
            tokio::fs::remove_file(out).await?;
        }
        let total = align_up(total, ALIGN);
        let device: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(
                out.to_str()
                    .ok_or_else(|| ImageError::Spec("output path is not UTF-8".into()))?,
                total,
            )
            .await?,
        );

        let mut gpt = Gpt::create_with_lba(&device, lba);
        gpt.write(&device).await?;

        let mut report = BuildReport {
            path: out.to_path_buf(),
            format: "raw".into(),
            size_bytes: total,
            block_size: lba,
            partitions: Vec::new(),
        };

        // 1. The ESP first: firmware looks for it, and it is the floor.
        if let Some(esp) = self.spec.esp.clone() {
            let size = align_up(self.esp_size().await?.unwrap_or(DEFAULT_ESP), ALIGN);
            let slot = gpt.allocate(
                esp.label.as_deref().unwrap_or("EFI System"),
                type_guid::ESP,
                size,
                1, // required-partition
            )?;
            gpt.write(&device).await?;
            let start = gpt.entries[slot].start_bytes(lba);
            let len = gpt.entries[slot].size_bytes(lba);
            self.fill_esp(&device, start, len, &esp).await?;
            report.partitions.push(PartitionReport {
                index: slot,
                name: gpt.entries[slot].name.clone(),
                kind: "esp".into(),
                start_bytes: start,
                size_bytes: len,
                pallet_id: None,
                pallet_version: None,
                verified: None,
                volumes: Vec::new(),
            });
        }

        // 2. The pallets, in the order the spec lists them. Publishing into an
        //    image is the same operation as publishing onto a disk — same
        //    allocator, same verify-where-it-landed — so nothing here is a
        //    second implementation of it.
        if !self.spec.pallets.is_empty() {
            let mut store = PalletStore::default();
            store.add_drive(out.display().to_string(), device.clone());
            let mut sources: Vec<PathBuf> = Vec::new();
            for p in &self.spec.pallets {
                if let Some(src) = &p.from_image {
                    if !sources.contains(src) {
                        sources.push(src.clone());
                    }
                }
            }
            for src in &sources {
                let dev = FileDevice::open(
                    src.to_str()
                        .ok_or_else(|| ImageError::Spec("source path is not UTF-8".into()))?,
                )
                .await?;
                store.add_drive(src.display().to_string(), Arc::new(dev));
            }
            let mgr = PalletManager::new(store);

            for entry in &self.spec.pallets {
                let landed = match &entry.from_image {
                    Some(src) => self.copy_in(&mgr, entry, src).await?,
                    None => vec![self.publish_one(&mgr, entry).await?],
                };
                for id in landed {
                    let loc = mgr.get(id).await?;
                    let verdict = mgr.verify(id).await?;
                    report.partitions.push(PartitionReport {
                        index: loc.entry_index,
                        name: loc.name.clone(),
                        kind: format!("pallet/{}", loc.kind),
                        start_bytes: loc.start_bytes,
                        size_bytes: loc.size_bytes,
                        pallet_id: Some(loc.id),
                        pallet_version: Some(loc.version),
                        verified: Some(verdict.ok),
                        volumes: Vec::new(),
                    });
                    if !verdict.ok {
                        return Err(ImageError::Other(format!(
                            "pallet '{}' did not verify in the image: {}",
                            loc.name,
                            verdict.reason.unwrap_or_else(|| "unknown".into())
                        )));
                    }
                }
            }
            // The pallet code owns the table while it is publishing; re-read it
            // before allocating anything else, or the entries it wrote would be
            // overwritten by a stale copy.
            gpt = Gpt::read(&device).await?;
        }

        // 3. Anything else the spec asks for, verbatim.
        for raw in &self.spec.partitions {
            let size = match size_of(&raw.size)? {
                Some(s) => s,
                None if is_rest(&raw.size) => gpt.largest_free_bytes(),
                None => match &raw.from_file {
                    Some(f) => file_len(f).await?,
                    None => {
                        return Err(ImageError::Spec(format!(
                            "partition '{}' has neither a size nor a file",
                            raw.name
                        )))
                    }
                },
            };
            let ty = match raw.r#type.as_deref() {
                Some(t) => type_guid::parse(t)
                    .ok_or_else(|| ImageError::Spec(format!("unknown partition type '{t}'")))?,
                None => type_guid::LINUX,
            };
            let slot = gpt.allocate(&raw.name, ty, align_up(size, ALIGN), 0)?;
            gpt.write(&device).await?;
            let start = gpt.entries[slot].start_bytes(lba);
            let len = gpt.entries[slot].size_bytes(lba);
            if let Some(f) = &raw.from_file {
                copy_file_into(&device, start, len, f).await?;
            }
            report.partitions.push(PartitionReport {
                index: slot,
                name: raw.name.clone(),
                kind: "raw".into(),
                start_bytes: start,
                size_bytes: len,
                pallet_id: None,
                pallet_version: None,
                verified: None,
                volumes: Vec::new(),
            });
        }

        // 4. The mutable end.
        if let Some(slab) = &self.spec.slab {
            let size = match size_of(&slab.size)? {
                Some(s) => align_up(s, ALIGN),
                None => gpt.largest_free_bytes() / ALIGN * ALIGN,
            };
            if size == 0 {
                return Err(ImageError::Spec(
                    "no room left for the slab; give the image a larger `size`".into(),
                ));
            }
            let name = slab.name.clone().unwrap_or_else(|| "stormblock".to_string());
            let slot = gpt.allocate(&name, type_guid::SLAB, size, 0)?;
            gpt.write(&device).await?;
            let start = gpt.entries[slot].start_bytes(lba);
            let len = gpt.entries[slot].size_bytes(lba);
            let part = Arc::new(PartitionDevice::new(device.clone(), start, len)?);
            let tier = slab
                .tier
                .as_deref()
                .map(parse_tier)
                .transpose()?
                .unwrap_or(StorageTier::Hot);
            let slot_size = slab.slot_size.unwrap_or(DEFAULT_SLOT_SIZE);
            // The slab keeps its own volumes.dat: an image has no filesystem
            // to keep one in, and a slab whose contents are only described
            // somewhere else is a slab that boots to "no volume metadata".
            let meta_bytes = match size_of(&slab.meta_size)? {
                Some(b) => b,
                None => auto_metadata_bytes(len, slot_size),
            };
            let formatted = Slab::format_with(
                part,
                SlabFormat::new(slot_size, tier).with_metadata(meta_bytes),
            )
            .await?;
            let volumes = self.fill_slab(&device, formatted, &slab.goldens).await?;
            report.partitions.push(PartitionReport {
                index: slot,
                name,
                kind: "slab".into(),
                start_bytes: start,
                size_bytes: len,
                pallet_id: None,
                pallet_version: None,
                verified: None,
                volumes,
            });
        }

        device.flush().await?;
        Ok(report)
    }

    /// Lay the goldens into the freshly formatted slab, take the first clone
    /// of each, and leave the slab describing itself.
    ///
    /// The clones are what the node runs from; a golden is never used
    /// directly. That is what makes the upgrade story work — publishing a new
    /// system pallet beside the old one and taking a first clone is the whole
    /// of it, and the previous clone is still there because nothing was
    /// overwritten (#62).
    async fn fill_slab(
        &self,
        device: &Arc<dyn BlockDevice>,
        slab: Slab,
        goldens: &[GoldenSpec],
    ) -> Result<Vec<VolumeReport>> {
        let slab_id = slab.slab_id();
        let slot_size = slab.slot_size();
        let free_bytes = slab.free_slots() * slot_size;

        let mut mgr = VolumeManager::new(slot_size);
        mgr.attach_slab(RaidArrayId(Uuid::new_v4()), slab)
            .await
            .map_err(|e| ImageError::Other(format!("attach slab: {e}")))?;
        mgr.persist_to_slab(slab_id);

        let mut pallets: Option<PalletStore> = None;
        let mut taken: HashSet<String> = HashSet::new();
        let mut pairs: Vec<(VolumeId, VolumeId)> = Vec::new();
        let mut need = 0u64;

        for g in goldens {
            let golden_name = g
                .golden_name
                .clone()
                .unwrap_or_else(|| format!("{}.golden", g.name));
            let clone_name = g.clone.clone().unwrap_or_else(|| g.name.clone());
            for n in [&golden_name, &clone_name] {
                if !taken.insert(n.clone()) {
                    return Err(ImageError::Spec(format!(
                        "two volumes in the slab would both be called '{n}'; a boot resolves a \
                         volume by name, so the names have to be distinct"
                    )));
                }
            }

            let mut source = self.golden_source(device, g, &mut pallets).await?;
            let content = source.byte_len();
            let size = match size_of(&g.size)? {
                Some(s) => s,
                None => align_up(content, slot_size),
            };
            if size < content {
                return Err(ImageError::Spec(format!(
                    "golden '{}' is {content} bytes and its `size` is {size}",
                    g.name
                )));
            }
            need += align_up(content, slot_size);
            if need > free_bytes {
                return Err(ImageError::TooSmall { need, have: free_bytes });
            }

            let golden_id = mgr
                .create_volume_any(&golden_name, size)
                .await
                .map_err(|e| ImageError::Other(format!("create golden '{golden_name}': {e}")))?;
            let vol = mgr
                .get_volume(&golden_id)
                .ok_or_else(|| ImageError::Other("golden vanished after create".into()))?;

            // Refuse a filesystem whose blocks are smaller than the sectors it
            // will be read through. This is caught here because it cannot be
            // caught anywhere useful later: the image builds, the pallets
            // verify, the volume attaches, and the kernel refuses the mount at
            // boot with "bad block size" (#40).
            if let Some(fs_block) = source.ext4_block_size().await? {
                let sector = vol.block_size();
                if fs_block < sector {
                    return Err(ImageError::Spec(format!(
                        "golden '{}' is an ext4 filesystem with {fs_block}-byte blocks, and the \
                         volume it goes in has {sector}-byte sectors — the kernel will refuse to \
                         mount it. Rebuild the golden with `mkfs.ext4 -b {sector}`",
                        g.name
                    )));
                }
            }

            source.write_into(&vol, slot_size as usize).await?;
            vol.flush().await?;

            // A golden arrives sealed (#77): what the build lays down is what
            // clones are taken from, and the first pod that wants one must
            // not be the thing that makes it cloneable. The filesystem on it
            // is read into the record here, so a clone can be stamped.
            let fs = crate::fs::disk::probe(&vol).await;
            drop(vol);
            mgr.seal_volume(golden_id, fs.clone())
                .await
                .map_err(|e| ImageError::Other(format!("seal golden '{golden_name}': {e}")))?;

            let clone_id = mgr
                .create_snapshot(golden_id, &clone_name)
                .await
                .map_err(|e| ImageError::Other(format!("clone '{clone_name}': {e}")))?;
            // A clone is a filesystem — or a disk — of its own: two live ones
            // must never claim one identity, and the golden's is the one
            // every clone would otherwise carry (#76).
            if let Some(f) = &fs {
                let dev = mgr
                    .get_volume(&clone_id)
                    .ok_or_else(|| ImageError::Other("clone vanished after create".into()))?;
                let fresh = if f.kind == "ext4" {
                    let u = Uuid::new_v4();
                    crate::fs::ext4::stamp_uuid(&dev, u, false)
                        .await
                        .map_err(|e| ImageError::Other(format!("stamp clone '{clone_name}': {e}")))?;
                    Some(u)
                } else {
                    crate::fs::disk::stamp(&dev, f)
                        .await
                        .map_err(|e| ImageError::Other(format!("stamp clone '{clone_name}': {e}")))?
                };
                drop(dev);
                if let Some(u) = fresh {
                    mgr.set_fs_uuid(clone_id, u)
                        .await
                        .map_err(|e| ImageError::Other(format!("record clone '{clone_name}': {e}")))?;
                }
            }
            pairs.push((golden_id, clone_id));
        }

        // Write volumes.dat into the slab. A failure here is the build's, not
        // a warning in a log: an image whose slab cannot say what is in it is
        // an image that drops to a shell.
        mgr.persist_checked()
            .await
            .map_err(|e| ImageError::Other(format!("slab metadata: {e}")))?;

        let clone_of: std::collections::HashMap<VolumeId, VolumeId> =
            pairs.iter().map(|(g, c)| (*c, *g)).collect();
        let order: Vec<VolumeId> = pairs.iter().flat_map(|(g, c)| [*g, *c]).collect();
        let listed = mgr.list_volumes().await;
        let mut out = Vec::with_capacity(listed.len());
        for id in order {
            if let Some((_, name, size, allocated)) = listed.iter().find(|(i, ..)| *i == id) {
                out.push(VolumeReport {
                    id: id.0,
                    name: name.clone(),
                    size_bytes: *size,
                    allocated_bytes: *allocated,
                    clone_of: clone_of.get(&id).map(|g| g.0),
                    sealed: mgr.is_sealed(&id),
                    fs_uuid: mgr.fs_info(&id).and_then(|f| f.uuid),
                });
            }
        }
        Ok(out)
    }

    /// Where a golden's content comes from: a file, or a member of a pallet
    /// this image already carries.
    async fn golden_source(
        &self,
        device: &Arc<dyn BlockDevice>,
        g: &GoldenSpec,
        pallets: &mut Option<PalletStore>,
    ) -> Result<GoldenSource> {
        let from = match (&g.file, &g.from) {
            (Some(f), _) => return GoldenSource::open_file(f).await,
            (None, Some(from)) => from.clone(),
            (None, None) => {
                return Err(ImageError::Spec(format!(
                    "golden '{}' has neither `from` nor `file`",
                    g.name
                )))
            }
        };

        let Some(rest) = from.strip_prefix("pallet:") else {
            return GoldenSource::open_file(Path::new(&from)).await;
        };
        let (pallet_ref, member_name) = rest.split_once('/').ok_or_else(|| {
            ImageError::Spec(format!(
                "golden '{}': `{from}` should be pallet:<pallet>/<member>",
                g.name
            ))
        })?;

        if pallets.is_none() {
            let mut store = PalletStore::default();
            store.add_drive("image".to_string(), device.clone());
            *pallets = Some(store);
        }
        let store = pallets.as_ref().expect("just filled");
        let found = store.scan().await;
        let by_id = Uuid::parse_str(pallet_ref).ok();
        let mut matching: Vec<_> = found
            .into_iter()
            .filter(|p| match by_id {
                Some(id) => p.id == id,
                None => p.name == pallet_ref,
            })
            .collect();
        // Several versions of a name: the newest is the one a fresh image
        // means, and it is also the one selection would boot.
        matching.sort_by_key(|p| std::cmp::Reverse(p.version));
        let loc = matching.into_iter().next().ok_or_else(|| {
            ImageError::Spec(format!(
                "golden '{}': no pallet '{pallet_ref}' in this image",
                g.name
            ))
        })?;
        let view = store.view(&loc)?;
        let pallet = store.open(&loc).await?;
        let member = pallet.find(member_name).map_err(|_| {
            ImageError::Spec(format!(
                "golden '{}': pallet '{}' has no member '{member_name}'",
                g.name, loc.name
            ))
        })?;
        Ok(GoldenSource::Member { pallet: Box::new(pallet), view, member })
    }

    fn count_rest(&self) -> usize {
        let mut n = 0;
        if self.spec.esp.as_ref().is_some_and(|e| is_rest(&e.size)) {
            n += 1;
        }
        n += self.spec.pallets.iter().filter(|p| is_rest(&p.size)).count();
        n += self.spec.partitions.iter().filter(|p| is_rest(&p.size)).count();
        if self.spec.slab.as_ref().is_some_and(|s| is_rest(&s.size)) {
            n += 1;
        }
        n
    }

    async fn fill_esp(
        &self,
        device: &Arc<dyn BlockDevice>,
        start: u64,
        len: u64,
        esp: &EspSpec,
    ) -> Result<()> {
        match (&esp.from_image, &esp.from_dir) {
            (Some(img), _) => copy_file_into(device, start, len, img).await,
            (None, Some(dir)) => {
                let part = Arc::new(PartitionDevice::new(device.clone(), start, len)?);
                super::fat::format_from_dir(part, dir, esp.label.as_deref().unwrap_or("EFI")).await
            }
            // An empty, formatted ESP is still useful: something else fills it.
            (None, None) => {
                let part = Arc::new(PartitionDevice::new(device.clone(), start, len)?);
                super::fat::format(part, esp.label.as_deref().unwrap_or("EFI")).await
            }
        }
    }

    async fn publish_one(&self, mgr: &PalletManager, entry: &PalletEntry) -> Result<Uuid> {
        let name = entry
            .name
            .clone()
            .ok_or_else(|| ImageError::Spec("a composed pallet needs a `name`".into()))?;
        let mut spec = PublishSpec::new(
            name,
            entry.kind.as_deref().map(parse_pallet_kind).unwrap_or(PalletKind::Unspecified),
        );
        spec.version = entry.version;
        spec.version_label = entry.version_label.clone().unwrap_or_default();
        spec.drive = Some(0);
        spec.size_bytes = size_of(&entry.size)?;
        spec.priority = entry.priority;
        spec.read_only = entry.read_only.unwrap_or(true);
        spec.sealed = entry.sealed.unwrap_or(true);
        if let Some(t) = entry.tries {
            spec.tries = t;
        }
        for m in &entry.members {
            spec.members.push(member_spec(m).await?);
        }
        if spec.members.is_empty() {
            return Err(ImageError::Spec(format!(
                "pallet '{}' has no members and no `from_image`",
                spec.name
            )));
        }
        Ok(mgr.publish(spec).await?.id)
    }

    async fn copy_in(
        &self,
        mgr: &PalletManager,
        entry: &PalletEntry,
        src: &Path,
    ) -> Result<Vec<Uuid>> {
        let src_index = mgr.store().drive_index_of(&src.display().to_string())?;
        let found = mgr.store().scan_drive(src_index).await?;
        let wanted: Vec<_> = match &entry.id {
            Some(id) => {
                let id = Uuid::parse_str(id)
                    .map_err(|_| ImageError::Spec(format!("'{id}' is not a pallet UUID")))?;
                found.into_iter().filter(|p| p.id == id).collect()
            }
            None => found,
        };
        if wanted.is_empty() {
            return Err(ImageError::Spec(format!(
                "no matching pallet in {}",
                src.display()
            )));
        }
        let mut out = Vec::new();
        for loc in wanted {
            if !loc.is_readable() {
                return Err(ImageError::Other(format!(
                    "pallet {} in {} does not parse; it will not be copied into an image",
                    loc.id,
                    src.display()
                )));
            }
            out.push(mgr.copy_pallet(loc.id, 0).await?.id);
        }
        Ok(out)
    }
}

/// Content for a golden volume, wherever it comes from.
enum GoldenSource {
    File { file: tokio::fs::File, len: u64 },
    /// A qcow2, a VMDK or the VMDK inside an OVA: a cloud image or a VM
    /// export, laid in as the raw disk it describes.
    Decoded(crate::image::import::Source),
    Member {
        pallet: Box<crate::pallet::Pallet>,
        view: PartitionView,
        member: crate::pallet::format::Member,
    },
}

impl GoldenSource {
    async fn open_file(path: &Path) -> Result<GoldenSource> {
        match crate::image::decode::detect_file(path).await {
            Ok(crate::image::decode::SourceFormat::Raw) | Err(_) => {}
            Ok(fmt) => {
                let (src, _) = crate::image::import::open_source(path, Some(&fmt.to_string()))
                    .await
                    .map_err(|e| ImageError::Spec(format!("golden content {}: {e}", path.display())))?;
                return Ok(GoldenSource::Decoded(src));
            }
        }
        let len = file_len(path).await?;
        let file = tokio::fs::File::open(path).await.map_err(|e| {
            ImageError::Spec(format!("golden content {}: {e}", path.display()))
        })?;
        Ok(GoldenSource::File { file, len })
    }

    fn byte_len(&self) -> u64 {
        match self {
            GoldenSource::File { len, .. } => *len,
            GoldenSource::Decoded(src) => src.virtual_size(),
            GoldenSource::Member { member, .. } => member.byte_len,
        }
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            GoldenSource::File { file, .. } => {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                // Seek rather than assume the caller reads in order: the block
                // size is probed from the front before the copy walks the
                // whole thing, and a source that is only correct when read
                // sequentially is a trap for the next caller.
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                file.read_exact(buf).await?;
                Ok(())
            }
            GoldenSource::Decoded(src) => src
                .read_at(offset, buf)
                .await
                .map_err(|e| ImageError::Other(format!("decoding golden content: {e}"))),
            GoldenSource::Member { pallet, view, member } => {
                pallet.read_member(member, view, offset, buf).await?;
                Ok(())
            }
        }
    }

    /// The ext4 block size of this content, if it is an ext4 filesystem.
    ///
    /// A golden is usually a filesystem image built somewhere else, by a
    /// `mke2fs` that sized its blocks for the *file* it was writing into: a
    /// 64 MB file reads as 512-byte sectors, so the size class picks 1024-byte
    /// blocks. Land that in a volume whose logical sector is 4096 and the
    /// kernel refuses the mount outright — `EXT4-fs: bad block size 1024` —
    /// after the image has been built, shipped and booted (#40).
    async fn ext4_block_size(&mut self) -> Result<Option<u32>> {
        // Superblock at byte 1024: magic 0xEF53 at +0x38, s_log_block_size at
        // +0x18, and the block size is 1024 << that.
        const SB_OFF: u64 = 1024;
        if self.byte_len() < SB_OFF + 0x40 {
            return Ok(None);
        }
        let mut sb = [0u8; 64];
        self.read_at(SB_OFF, &mut sb).await?;
        if u16::from_le_bytes([sb[0x38], sb[0x39]]) != 0xEF53 {
            return Ok(None);
        }
        let log = u32::from_le_bytes(sb[0x18..0x1C].try_into().unwrap());
        if log > 16 {
            return Ok(None);
        }
        Ok(Some(1024u32 << log))
    }

    /// Copy the content into a volume, skipping runs of zeros.
    ///
    /// A golden is a filesystem image and mostly holes; writing them would
    /// allocate a slab slot per hole and cost the image the thin provisioning
    /// it exists to have. Zero runs are measured a slot at a time, because a
    /// slot is the unit that would be allocated.
    async fn write_into(&mut self, vol: &Arc<dyn BlockDevice>, chunk: usize) -> Result<u64> {
        let len = self.byte_len();
        let chunk = chunk.clamp(4096, COPY_CHUNK);
        let mut buf = vec![0u8; chunk];
        let mut off = 0u64;
        let mut written = 0u64;
        while off < len {
            let take = ((len - off) as usize).min(chunk);
            // A decoded image knows which clusters it never wrote.
            if let GoldenSource::Decoded(src) = self {
                let may = src
                    .may_have_data(off, take as u64)
                    .await
                    .map_err(|e| ImageError::Other(format!("decoding golden content: {e}")))?;
                if !may {
                    off += take as u64;
                    continue;
                }
            }
            self.read_at(off, &mut buf[..take]).await?;
            if buf[..take].iter().any(|&b| b != 0) {
                vol.write(off, &buf[..take]).await?;
                written += take as u64;
            }
            off += take as u64;
        }
        Ok(written)
    }
}

fn parse_tier(s: &str) -> Result<StorageTier> {
    match s.to_ascii_lowercase().as_str() {
        "hot" => Ok(StorageTier::Hot),
        "warm" => Ok(StorageTier::Warm),
        "cool" => Ok(StorageTier::Cool),
        "cold" => Ok(StorageTier::Cold),
        other => Err(ImageError::Spec(format!("unknown tier '{other}'"))),
    }
}

async fn member_len(m: &MemberEntry) -> Result<u64> {
    match (&m.file, &m.text) {
        (Some(f), _) => file_len(f).await,
        (None, Some(t)) => Ok(t.len() as u64),
        (None, None) => Err(ImageError::Spec(format!(
            "member '{}' has neither `file` nor `text`",
            m.name
        ))),
    }
}

async fn member_spec(m: &MemberEntry) -> Result<MemberSpec> {
    let kind = m.kind.as_deref().map(parse_member_kind).unwrap_or(MemberKind::Raw);
    match (&m.file, &m.text) {
        (Some(f), _) => Ok(crate::pallet::manager::file_member(
            m.name.to_string(),
            m.role.to_string(),
            kind,
            f.clone(),
        )
        .await?),
        (None, Some(t)) => Ok(MemberSpec::new(
            m.name.to_string(),
            m.role.to_string(),
            kind,
            Arc::new(BytesContent(t.clone().into_bytes())),
        )),
        (None, None) => Err(ImageError::Spec(format!(
            "member '{}' has neither `file` nor `text`",
            m.name
        ))),
    }
}

/// Copy a file into a partition, refusing one that does not fit rather than
/// truncating it — a half-copied filesystem is worse than a failed build.
async fn copy_file_into(
    device: &Arc<dyn BlockDevice>,
    start: u64,
    len: u64,
    file: &Path,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let size = file_len(file).await?;
    if size > len {
        return Err(ImageError::TooSmall { need: size, have: len });
    }
    let view = PartitionView::new(device.clone(), start, len);
    let mut f = tokio::fs::File::open(file).await?;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut off = 0u64;
    while off < size {
        let take = ((size - off) as usize).min(COPY_CHUNK);
        f.read_exact(&mut buf[..take]).await?;
        view.write_at(off, &buf[..take]).await?;
        off += take as u64;
    }
    view.flush().await?;
    Ok(())
}

/// Every pallet in an existing image, for tools that want to look before they
/// copy.
pub async fn pallets_in(image: &Path) -> Result<Vec<crate::pallet::PalletLocation>> {
    let dev = FileDevice::open(
        image
            .to_str()
            .ok_or_else(|| ImageError::Spec("path is not UTF-8".into()))?,
    )
    .await?;
    let mut store = PalletStore::default();
    store.add_drive(image.display().to_string(), Arc::new(dev));
    Ok(store.scan().await)
}

/// What a slab partition in an existing image holds, according to the slab
/// itself.
#[derive(Debug, Clone, Serialize)]
pub struct SlabContents {
    pub index: usize,
    pub name: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub slot_size: u64,
    pub total_slots: u64,
    pub free_slots: u64,
    /// False for a slab with nowhere to keep its own volumes.dat — the volume
    /// list is then empty because there is none to read, not because the slab
    /// is empty.
    pub self_describing: bool,
    pub volumes: Vec<VolumeReport>,
}

/// Every slab in an image, and the volumes each one says it holds.
pub async fn slabs_in(image: &Path) -> Result<Vec<SlabContents>> {
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open(
            image
                .to_str()
                .ok_or_else(|| ImageError::Spec("path is not UTF-8".into()))?,
        )
        .await?,
    );
    let gpt = Gpt::read(&dev).await?;
    let mut out = Vec::new();
    for (index, entry) in gpt.partitions() {
        if entry.type_guid != type_guid::SLAB {
            continue;
        }
        let start = entry.start_bytes(gpt.block_size);
        let size = entry.size_bytes(gpt.block_size);
        let part = Arc::new(PartitionDevice::new(dev.clone(), start, size)?);
        let slab = Slab::open(part).await?;
        let mut volumes = Vec::new();
        if let Some(bytes) = slab.read_metadata().await? {
            let meta = crate::volume::MetadataStore::decode(&bytes)
                .map_err(|e| ImageError::Other(format!("slab volume metadata: {e}")))?;
            for v in meta.volumes {
                volumes.push(VolumeReport {
                    id: v.id.0,
                    name: v.name,
                    size_bytes: v.virtual_size,
                    allocated_bytes: v.extents.len() as u64 * meta.extent_size,
                    // Lineage is in the record since V5; a CoW is a volume
                    // like any other once it exists.
                    clone_of: v.parent.map(|p| p.0),
                    sealed: v.sealed,
                    fs_uuid: v.fs.as_ref().and_then(|f| f.uuid),
                });
            }
            volumes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        out.push(SlabContents {
            index,
            name: entry.name.clone(),
            start_bytes: start,
            size_bytes: size,
            slot_size: slab.slot_size(),
            total_slots: slab.total_slots(),
            free_slots: slab.free_slots(),
            self_describing: slab.has_metadata_region(),
            volumes,
        });
    }
    Ok(out)
}

/// The GPT of an existing image, for the same reason.
pub async fn table_of(image: &Path) -> Result<Gpt> {
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open(
            image
                .to_str()
                .ok_or_else(|| ImageError::Spec("path is not UTF-8".into()))?,
        )
        .await?,
    );
    Ok(Gpt::read(&dev).await?)
}

/// The pallet type GUID, re-exported for callers laying out their own tables.
pub const PALLET_GUID: [u8; 16] = PALLET_TYPE_GUID;
