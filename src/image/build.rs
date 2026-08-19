//! Building a raw image: lay out the partitions, then fill them.
//!
//! The order on disk is the order of the spec's sections — ESP, then pallets in
//! the order written, then raw partitions, then the slab — because allocation
//! is first-fit out of measured free runs and the builder fills them in that
//! sequence. It is worth being predictable here: an image is often read by
//! something that was told where to look.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::drive::filedev::FileDevice;
use crate::drive::partition::PartitionDevice;
use crate::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
use crate::drive::BlockDevice;
use crate::mgmt::config::parse_size;
use crate::pallet::format::{parse_member_kind, parse_pallet_kind, MemberKind, PalletKind};
use crate::pallet::manager::PublishSpec;
use crate::pallet::{
    BytesContent, Gpt, MemberSpec, PalletManager, PalletStore, PartitionView, PALLET_TYPE_GUID,
};
use crate::placement::topology::StorageTier;

use super::{type_guid, EspSpec, ImageError, ImageSpec, MemberEntry, PalletEntry, Result};

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

#[derive(Debug, Clone, Serialize)]
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
            Slab::format(part, slab.slot_size.unwrap_or(DEFAULT_SLOT_SIZE), tier).await?;
            report.partitions.push(PartitionReport {
                index: slot,
                name,
                kind: "slab".into(),
                start_bytes: start,
                size_bytes: len,
                pallet_id: None,
                pallet_version: None,
                verified: None,
            });
        }

        device.flush().await?;
        Ok(report)
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
