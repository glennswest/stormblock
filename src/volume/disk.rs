//! Composed pallets and composed disks — a per-node disk as a chain of goldens.
//!
//! [`compose`](super::compose) makes a volume out of other volumes by sharing
//! their slots. That is a *raw* composition: the result has no partition
//! table and nothing inside it knows it is on a disk, so it is not something
//! a node can boot. What a node boots is a GPT, an ESP, and pallets — and
//! `image build` lays every one of those down as bytes, per image, which is
//! why a fleet of a hundred nodes is a hundred copies of the same goldens.
//!
//! This module makes each of those things a golden too, so a disk is a chain
//! of them and costs its map:
//!
//! ```text
//! slot 0        1..            ..            ..              last
//! ┌──────────┬─────────────┬──────────────┬───────────────┬──────────┐
//! │ GPT head │ ESP         │ boot pallet  │ system pallet │ GPT tail │
//! │ (golden) │ (golden)    │ (pallet vol) │ (pallet vol)  │ (golden) │
//! └──────────┴─────────────┴──────────────┴───────────────┴──────────┘
//!   shared     shared        shared         shared          shared
//! ```
//!
//! - **A pallet is a sealed volume** ([`VolumeManager::compose_pallet`]).
//!   The header is written; every member that is already a golden is placed
//!   on a slot boundary and *shared in* by its extent map. The pallet verifies
//!   through the ordinary reader, exactly as one on a drive does.
//! - **The GPT is two goldens** — the protective MBR, primary header and
//!   entry array in the first slot; the backup array and header in the last.
//!   They are minted once per *layout* and named by a digest of it, so every
//!   disk with the same layout shares them. Partition GUIDs are derived from
//!   the layout too, which makes `root=PARTUUID=…` the same on every node.
//! - **A disk is `compose(head, partitions…, tail)`**
//!   ([`VolumeManager::compose_disk`]). Nothing is written unless the caller
//!   asks for a per-disk GUID, which costs the two GPT slots.
//!
//! Copy-on-write covers the rest, as it does for any composition: a node that
//! writes to its disk — the boot ladder decrementing `tries`, a filesystem
//! journal — gets its own slot for what it changed and keeps sharing
//! everything else.
//!
//! **The LBA size defaults to 4096.** A volume is presented at 4096-byte
//! blocks over NVMe/TCP and ublk, and firmware parses a GPT in the media's
//! own block size; a 512-LBA table on a 4Kn namespace is one the engine can
//! read and the node cannot boot (docs/pallets.md §2.4). A disk meant to be
//! copied onto a 512-byte drive says so.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::drive::slab::SlabRole;
use crate::drive::BlockDevice;
use crate::image::type_guid;
use crate::pallet::format::{
    Attributes, MemberKind, MemberSpec, Pallet, PalletBuilder, PalletKind, PALLET_TYPE_GUID,
};
use crate::pallet::gpt::{Gpt, GptEntry};
use crate::pallet::{BytesContent, DeviceContent, PartitionView};
use crate::volume::compose::{self, Component};
use crate::volume::extent::VolumeId;
use crate::volume::thin::VolumeError;
use crate::volume::{CreateOptions, FsInfo, VolumeManager};

/// The LBA size a composed disk is laid out in unless told otherwise — what
/// the engine presents a volume at.
pub const DEFAULT_LBA: u32 = 4096;

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

fn invalid(msg: impl Into<String>) -> VolumeError {
    VolumeError::InvalidSize(msg.into())
}

fn pallet_err(e: crate::pallet::PalletError) -> VolumeError {
    VolumeError::AllocatorError(format!("pallet: {e}"))
}

// ------------------------------------------------------------ pallet volume

/// Where one member's bytes come from.
pub enum MemberSource {
    /// A volume already in the slab — shared in by its map, never copied.
    /// `len` is what the manifest digests; it defaults to the volume's whole
    /// size, and may be less when the caller knows where the content ends
    /// (a kernel imported from a 15.3 MB file into a 16 MiB golden).
    Volume { id: VolumeId, len: Option<u64> },
    /// Inline bytes — a kernel command line, a small config. Written.
    Bytes(Vec<u8>),
}

/// One member of a composed pallet.
pub struct PalletMember {
    pub name: String,
    pub role: String,
    pub kind: MemberKind,
    pub source: MemberSource,
}

/// What to compose a pallet out of.
pub struct PalletVolumeSpec {
    /// The volume's name.
    pub name: String,
    /// The pallet's name — what the superblock and, later, the GPT entry
    /// carry. Defaults to the volume name.
    pub pallet: Option<String>,
    pub kind: PalletKind,
    pub version: u64,
    pub version_label: String,
    /// Block size the extent table counts in: the LBA size of the disk this
    /// pallet will sit in. Zero takes [`DEFAULT_LBA`].
    pub lba: u32,
    pub sealed: bool,
    pub read_only: bool,
    /// Virtual size. `None` fits the content, rounded up to a slot.
    pub size: Option<u64>,
    pub members: Vec<PalletMember>,
    /// Which half of the node to place in. `None` follows the first shared
    /// golden, else the node's default.
    pub role: Option<SlabRole>,
}

impl PalletVolumeSpec {
    pub fn new(name: impl Into<String>, kind: PalletKind) -> Self {
        PalletVolumeSpec {
            name: name.into(),
            pallet: None,
            kind,
            version: 1,
            version_label: String::new(),
            lba: 0,
            sealed: true,
            read_only: true,
            size: None,
            members: Vec::new(),
            role: None,
        }
    }
}

/// Where one member landed, and what it cost.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComposedMember {
    pub name: String,
    pub role: String,
    /// Offset inside the pallet, in bytes.
    pub offset: u64,
    /// Bytes the manifest digests.
    pub len: u64,
    /// Bytes the member occupies, slot-rounded.
    pub span: u64,
    /// Shared in by its map, or written.
    pub shared: bool,
    pub digest: String,
}

/// What composing a pallet produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PalletVolumeReport {
    pub id: Uuid,
    pub name: String,
    pub pallet: String,
    pub version: u64,
    pub kind: String,
    pub manifest_digest: String,
    pub lba: u32,
    pub virtual_size: u64,
    pub members: Vec<ComposedMember>,
    /// Bytes that are somebody else's slots.
    pub shared_bytes: u64,
    /// Bytes this pallet had to write: the header, and any inline member.
    pub written_bytes: u64,
}

// ------------------------------------------------------------------- disk

/// One partition of a composed disk.
pub struct DiskPartition {
    /// The volume that *is* the partition — a composed pallet, an ESP golden,
    /// any sealed volume.
    pub volume: VolumeId,
    /// GPT partition name. Defaults to the volume's name.
    pub name: Option<String>,
    /// Type GUID. `None` follows what the volume is: a pallet, a FAT
    /// filesystem (ESP), or plain Linux data.
    pub type_guid: Option<[u8; 16]>,
    /// For a pallet: selection order. Defaults to 1.
    pub priority: Option<u8>,
    /// For a pallet: boot attempts. Defaults to the publisher's default.
    pub tries: Option<u8>,
    /// Raw attribute bits, for a partition that is neither. Defaults to 0,
    /// or to *required* for an ESP.
    pub attributes: Option<u64>,
}

impl DiskPartition {
    pub fn new(volume: VolumeId) -> Self {
        DiskPartition {
            volume,
            name: None,
            type_guid: None,
            priority: None,
            tries: None,
            attributes: None,
        }
    }
}

/// What to compose a disk out of.
pub struct DiskSpec {
    pub name: String,
    /// Total size. `None` is the end of the last partition plus the tail slot.
    pub size: Option<u64>,
    /// LBA size of the table. Zero takes [`DEFAULT_LBA`].
    pub lba: u32,
    /// Stamp this disk its own disk GUID. Costs the two GPT slots; without it
    /// every disk of this layout carries the layout's GUID and the disk is
    /// entirely shared.
    pub fresh_guid: bool,
    pub partitions: Vec<DiskPartition>,
    /// Where the GPT goldens and the disk are placed. `None` follows the
    /// first partition.
    pub role: Option<SlabRole>,
}

impl DiskSpec {
    pub fn new(name: impl Into<String>) -> Self {
        DiskSpec { name: name.into(), size: None, lba: 0, fresh_guid: false, partitions: Vec::new(), role: None }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiskPartitionReport {
    pub index: usize,
    pub name: String,
    pub type_guid: Uuid,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub volume: Uuid,
    /// The partition's own GUID — derived from the layout, so the same on
    /// every disk composed from it.
    pub partuuid: Uuid,
    pub attributes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiskReport {
    pub id: Uuid,
    pub name: String,
    pub lba: u32,
    pub size_bytes: u64,
    pub disk_guid: Uuid,
    /// The layout's digest — what names the GPT goldens.
    pub layout: String,
    pub head_golden: Uuid,
    pub tail_golden: Uuid,
    /// Whether this call had to mint the GPT goldens, or found them.
    pub gpt_minted: bool,
    pub partitions: Vec<DiskPartitionReport>,
    pub shared_bytes: u64,
    /// Bytes written for this disk alone: zero unless `fresh_guid`.
    pub written_bytes: u64,
}

/// Namespace for every identity derived from a layout.
const LAYOUT_NS: Uuid = Uuid::from_u128(0x5f0c_a4c2_7c1e_4d3a_9b6b_2e8f_1a7d_3c55);

impl VolumeManager {
    /// Compose a pallet as a sealed volume, sharing its members' slots.
    ///
    /// The header is written into the first slot; each member is placed on a
    /// slot boundary, and a member that is a volume is shared in by its map
    /// rather than copied. The result is read back and verified through the
    /// ordinary pallet reader before it is sealed — a pallet that does not
    /// verify is deleted and the error returned.
    pub async fn compose_pallet(
        &mut self,
        spec: PalletVolumeSpec,
    ) -> Result<PalletVolumeReport, VolumeError> {
        if spec.members.is_empty() {
            return Err(invalid("a composed pallet needs at least one member"));
        }
        let slot = self.slot_size;
        let lba = if spec.lba == 0 { DEFAULT_LBA } else { spec.lba };
        if !lba.is_power_of_two() || lba < 512 || lba as u64 > slot {
            return Err(invalid(format!("lba {lba}: must be a power of two from 512 up to the {slot}-byte slot")));
        }
        let pallet_name = spec.pallet.clone().unwrap_or_else(|| spec.name.clone());

        // Resolve every volume member first, so a bad one is refused before
        // anything exists.
        let mut role = spec.role;
        let mut specs: Vec<(MemberSpec, bool, u64)> = Vec::with_capacity(spec.members.len());
        for m in &spec.members {
            match &m.source {
                MemberSource::Volume { id, len } => {
                    let dev = self.get_volume(id).ok_or(VolumeError::VolumeNotFound(*id))?;
                    let cap = dev.capacity_bytes();
                    let len = len.unwrap_or(cap);
                    if len == 0 || len > cap {
                        return Err(invalid(format!(
                            "member '{}': len {len} is not within the volume's {cap} bytes",
                            m.name
                        )));
                    }
                    if role.is_none() {
                        role = self.volume_role(id);
                    }
                    let span = align_up(cap, slot);
                    let content = Arc::new(DeviceContent::new(dev, len));
                    specs.push((
                        MemberSpec::new(&m.name, &m.role, m.kind, content).with_reserve(cap),
                        true,
                        span,
                    ));
                }
                MemberSource::Bytes(b) => {
                    let span = align_up(b.len() as u64, slot);
                    let content = Arc::new(BytesContent(b.clone()));
                    specs.push((MemberSpec::new(&m.name, &m.role, m.kind, content), false, span));
                }
            }
        }

        let attrs = Attributes {
            priority: 1,
            tries_left: crate::pallet::manager::DEFAULT_TRIES,
            successful: false,
            sealed: spec.sealed,
            read_only: spec.read_only,
            required: true,
        };
        let mut builder = PalletBuilder::new(pallet_name.clone(), spec.version)
            .kind(spec.kind)
            .version_label(spec.version_label.clone())
            .attributes(attrs)
            .block_size(lba)
            .content_align(slot);
        for (m, _, _) in &specs {
            builder = builder.member(MemberSpec {
                name: m.name.clone(),
                role: m.role.clone(),
                kind: m.kind,
                flags: m.flags,
                content: m.content.clone(),
                reserve: m.reserve,
            });
        }
        // Digests every member — reading a golden through the engine, once.
        let built = builder.build().await.map_err(pallet_err)?;

        let virtual_size = align_up(spec.size.unwrap_or(0).max(built.total_bytes), slot);
        let id = self
            .create_volume_with(&spec.name, virtual_size, CreateOptions::default().in_role_opt(role))
            .await?;

        let result: Result<PalletVolumeReport, VolumeError> = async {
            let handle = self.get_volume_handle(&id).ok_or(VolumeError::VolumeNotFound(id))?;
            let dev: Arc<dyn BlockDevice> = handle.clone();

            // The header, without its zero padding: the volume reads
            // unallocated space as zeros already.
            let used = built.header.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
            let header_len = align_up(used as u64, lba as u64) as usize;
            handle.write(0, &built.header[..header_len]).await?;
            let mut written = header_len as u64;
            let mut shared_bytes = 0u64;

            let mut members = Vec::with_capacity(specs.len());
            for p in &built.placements {
                let (m, is_volume, span) = &specs[p.member];
                if *is_volume {
                    let source = match &spec.members[p.member].source {
                        MemberSource::Volume { id, .. } => *id,
                        MemberSource::Bytes(_) => unreachable!(),
                    };
                    let comp = Component { source, at: p.offset, span: *span };
                    {
                        let mut gem = self.gem.write().await;
                        let mut reg = self.registry.write().await;
                        compose::share_into(id, slot, &[comp], &mut gem, &mut reg).await?;
                    }
                    shared_bytes += span;
                } else if p.len > 0 {
                    let bytes = match &spec.members[p.member].source {
                        MemberSource::Bytes(b) => b,
                        MemberSource::Volume { .. } => unreachable!(),
                    };
                    handle.write(p.offset, bytes).await?;
                    written += align_up(bytes.len() as u64, lba as u64);
                }
                members.push(ComposedMember {
                    name: m.name.clone(),
                    role: m.role.clone(),
                    offset: p.offset,
                    len: p.len,
                    span: *span,
                    shared: *is_volume,
                    digest: hex::encode(p.digest),
                });
            }

            // Check it where it landed, through the reader every consumer
            // uses. A map that put a member one slot off would digest wrong
            // here and nowhere sooner.
            let view = PartitionView::whole(dev.clone());
            let pallet = Pallet::read(&view).await.map_err(pallet_err)?;
            pallet.verify_all(&view).await.map_err(|e| {
                VolumeError::AllocatorError(format!("composed pallet did not verify: {e}"))
            })?;

            Ok(PalletVolumeReport {
                id: id.0,
                name: spec.name.clone(),
                pallet: pallet_name.clone(),
                version: spec.version,
                kind: spec.kind.to_string(),
                manifest_digest: hex::encode(built.manifest_digest),
                lba,
                virtual_size,
                members,
                shared_bytes,
                written_bytes: written,
            })
        }
        .await;

        match result {
            Ok(report) => {
                let fs = FsInfo {
                    kind: "pallet".into(),
                    journal: false,
                    features: Some(format!("lba={lba}")),
                    sixty_four_bit: false,
                    metadata_csum: false,
                    csum_seed: false,
                    label: pallet_name,
                    uuid: None,
                };
                self.seal_volume(id, Some(fs)).await?;
                Ok(report)
            }
            Err(e) => {
                // Half a pallet is worse than none: it parses as nothing and
                // holds references.
                if let Err(d) = self.delete_volume(id).await {
                    tracing::warn!(volume = %id, "composed pallet failed ({e}) and its volume could not be removed: {d}");
                }
                Err(e)
            }
        }
    }

    /// Compose a bootable disk out of volumes: a GPT whose partitions are the
    /// volumes named, each shared in by its map.
    ///
    /// The partition table itself is two goldens minted once per layout and
    /// reused, so a disk costs nothing but its map unless `fresh_guid` asks
    /// for a per-disk identity. The result is sealed as nothing — it is a
    /// node's disk, and the node writes to it.
    pub async fn compose_disk(&mut self, spec: DiskSpec) -> Result<DiskReport, VolumeError> {
        if spec.partitions.is_empty() {
            return Err(invalid("a composed disk needs at least one partition"));
        }
        let slot = self.slot_size;
        let lba = if spec.lba == 0 { DEFAULT_LBA } else { spec.lba };
        if !lba.is_power_of_two() || lba < 512 || lba as u64 > slot {
            return Err(invalid(format!("lba {lba}: must be a power of two from 512 up to the {slot}-byte slot")));
        }

        // Lay the partitions out: each on a slot boundary, in order, after the
        // head slot. A partition's span is its volume's whole size — that is
        // what its map brings.
        struct Laid {
            volume: VolumeId,
            name: String,
            type_guid: [u8; 16],
            attributes: u64,
            start: u64,
            span: u64,
        }
        let mut role = spec.role;
        let mut laid: Vec<Laid> = Vec::with_capacity(spec.partitions.len());
        let mut cursor = slot;
        for p in &spec.partitions {
            let handle = self.get_volume_handle(&p.volume).ok_or(VolumeError::VolumeNotFound(p.volume))?;
            if role.is_none() {
                role = self.volume_role(&p.volume);
            }
            let fs_kind = self.fs_info(&p.volume).map(|f| f.kind.clone()).unwrap_or_default();
            let type_guid = p.type_guid.unwrap_or(match fs_kind.as_str() {
                "pallet" => PALLET_TYPE_GUID,
                "vfat" | "fat" | "fat16" | "fat32" => type_guid::ESP,
                _ => type_guid::LINUX,
            });
            let attributes = match p.attributes {
                Some(a) => a,
                None if type_guid == PALLET_TYPE_GUID => Attributes {
                    priority: p.priority.unwrap_or(1),
                    tries_left: p.tries.unwrap_or(crate::pallet::manager::DEFAULT_TRIES),
                    successful: false,
                    sealed: true,
                    read_only: true,
                    required: true,
                }
                .to_u64(),
                None if type_guid == type_guid::ESP => 1,
                None => 0,
            };
            let name = match &p.name {
                Some(n) => n.clone(),
                None => handle.name().await,
            };
            let span = align_up(handle.capacity_bytes(), slot);
            laid.push(Laid { volume: p.volume, name, type_guid, attributes, start: cursor, span });
            cursor += span;
        }
        let end = cursor + slot;
        let total = match spec.size {
            Some(s) if s < end => {
                return Err(invalid(format!(
                    "size {s} is smaller than the partitions and the GPT, which need {end}"
                )))
            }
            Some(s) => align_up(s, slot),
            None => end,
        };

        // The table. Head in slot 0, tail in the last slot; both have to fit.
        let mut gpt = Gpt::create_for(lba, total);
        if gpt.head_bytes() > slot || gpt.tail_bytes() > slot {
            return Err(invalid(format!(
                "a {slot}-byte slot cannot hold a GPT at {lba}-byte LBAs ({} + {} bytes)",
                gpt.head_bytes(),
                gpt.tail_bytes()
            )));
        }
        let lba64 = lba as u64;
        gpt.first_usable_lba = slot / lba64;
        gpt.last_usable_lba = (total - slot) / lba64 - 1;

        // Identity is a function of the layout, so two disks of one layout
        // are one pair of GPT goldens — and one set of PARTUUIDs, which is
        // what lets a kernel command line be the same on every node.
        let mut h = Sha256::new();
        h.update(lba.to_le_bytes());
        h.update(total.to_le_bytes());
        for l in &laid {
            h.update(l.type_guid);
            h.update(l.start.to_le_bytes());
            h.update(l.span.to_le_bytes());
            h.update(l.attributes.to_le_bytes());
            h.update(l.name.as_bytes());
            h.update([0u8]);
        }
        let layout: [u8; 32] = h.finalize().into();
        let layout_hex = hex::encode(&layout[..8]);
        let disk_guid = Uuid::new_v5(&LAYOUT_NS, &[b"disk:", &layout[..]].concat());
        gpt.disk_guid = disk_guid.to_bytes_le();

        let mut parts = Vec::with_capacity(laid.len());
        for (i, l) in laid.iter().enumerate() {
            let partuuid = Uuid::new_v5(&LAYOUT_NS, &[format!("part:{i}:").as_bytes(), &layout[..]].concat());
            let index = gpt
                .insert(GptEntry {
                    type_guid: l.type_guid,
                    unique_guid: partuuid.to_bytes_le(),
                    first_lba: l.start / lba64,
                    last_lba: (l.start + l.span) / lba64 - 1,
                    attributes: l.attributes,
                    name: l.name.clone(),
                })
                .map_err(pallet_err)?;
            parts.push(DiskPartitionReport {
                index,
                name: l.name.clone(),
                type_guid: Uuid::from_bytes_le(l.type_guid),
                start_bytes: l.start,
                size_bytes: l.span,
                volume: l.volume.0,
                partuuid,
                attributes: l.attributes,
            });
        }

        // The GPT goldens: found by name, or minted.
        let head_name = format!("gpt-{layout_hex}.head.golden");
        let tail_name = format!("gpt-{layout_hex}.tail.golden");
        let (head, tail) = gpt.render();
        let mut minted = false;
        let head_id = match self.find_volume(&head_name).await {
            Some(id) if self.is_sealed(&id) => id,
            Some(id) => return Err(invalid(format!("{head_name} exists ({id}) and is not sealed"))),
            None => {
                minted = true;
                self.mint_golden(&head_name, slot, &head, 0, role, FsInfo {
                    kind: "gpt".into(),
                    journal: false,
                    features: Some(format!("lba={lba}")),
                    sixty_four_bit: false,
                    metadata_csum: false,
                    csum_seed: false,
                    label: String::new(),
                    uuid: Some(disk_guid),
                })
                .await?
            }
        };
        let tail_id = match self.find_volume(&tail_name).await {
            Some(id) if self.is_sealed(&id) => id,
            Some(id) => return Err(invalid(format!("{tail_name} exists ({id}) and is not sealed"))),
            None => {
                minted = true;
                // The backup half sits at the end of its slot, so that the
                // slot's last LBA is the disk's last LBA.
                let at = slot - gpt.tail_bytes();
                self.mint_golden(&tail_name, slot, &tail, at, role, FsInfo {
                    kind: "gpt-backup".into(),
                    journal: false,
                    features: Some(format!("lba={lba}")),
                    sixty_four_bit: false,
                    metadata_csum: false,
                    csum_seed: false,
                    label: String::new(),
                    uuid: Some(disk_guid),
                })
                .await?
            }
        };

        // The chain.
        let mut placements: Vec<(VolumeId, u64)> = Vec::with_capacity(laid.len() + 2);
        placements.push((head_id, 0));
        for l in &laid {
            placements.push((l.volume, l.start));
        }
        placements.push((tail_id, total - slot));
        let id = self.compose_volume(&spec.name, Some(total), &placements).await?;
        let shared_bytes: u64 = laid.iter().map(|l| l.span).sum::<u64>() + 2 * slot;

        let result: Result<(Uuid, u64), VolumeError> = async {
            let dev = self.get_volume(&id).ok_or(VolumeError::VolumeNotFound(id))?;
            let mut written = 0u64;
            let mut guid = disk_guid;
            if spec.fresh_guid {
                guid = Uuid::new_v4();
                crate::fs::disk::stamp_gpt(&dev, lba64, guid).await.map_err(VolumeError::Drive)?;
                written = 2 * slot;
            }
            // Read the table back through the map, and each pallet's manifest
            // through its entry: the proof that the chain lines up.
            let back = Gpt::read(&dev).await.map_err(pallet_err)?;
            if back.block_size != lba || back.partitions().count() != laid.len() {
                return Err(VolumeError::AllocatorError(format!(
                    "composed disk reads back as {} partitions at {}-byte LBAs; expected {} at {lba}",
                    back.partitions().count(),
                    back.block_size,
                    laid.len()
                )));
            }
            for (i, l) in laid.iter().enumerate() {
                if l.type_guid == PALLET_TYPE_GUID {
                    let view = back.view(&dev, parts[i].index).map_err(pallet_err)?;
                    let p = Pallet::read(&view).await.map_err(|e| {
                        VolumeError::AllocatorError(format!("partition '{}' does not read as a pallet: {e}", l.name))
                    })?;
                    p.verify_manifest().map_err(pallet_err)?;
                }
            }
            Ok((guid, written))
        }
        .await;

        match result {
            Ok((guid, written)) => {
                self.set_fs_info(id, Some(FsInfo {
                    kind: "gpt".into(),
                    journal: false,
                    features: Some(format!("lba={lba}")),
                    sixty_four_bit: false,
                    metadata_csum: false,
                    csum_seed: false,
                    label: String::new(),
                    uuid: Some(guid),
                }))
                .await?;
                Ok(DiskReport {
                    id: id.0,
                    name: spec.name,
                    lba,
                    size_bytes: total,
                    disk_guid: guid,
                    layout: layout_hex,
                    head_golden: head_id.0,
                    tail_golden: tail_id.0,
                    gpt_minted: minted,
                    partitions: parts,
                    shared_bytes,
                    written_bytes: written,
                })
            }
            Err(e) => {
                if let Err(d) = self.delete_volume(id).await {
                    tracing::warn!(volume = %id, "composed disk failed ({e}) and could not be removed: {d}");
                }
                Err(e)
            }
        }
    }

    /// A one-off golden: `size` bytes, `bytes` written at `at`, sealed.
    async fn mint_golden(
        &mut self,
        name: &str,
        size: u64,
        bytes: &[u8],
        at: u64,
        role: Option<SlabRole>,
        fs: FsInfo,
    ) -> Result<VolumeId, VolumeError> {
        let id = self
            .create_volume_with(name, size, CreateOptions::default().in_role_opt(role))
            .await?;
        let handle = self.get_volume_handle(&id).ok_or(VolumeError::VolumeNotFound(id))?;
        if let Err(e) = handle.write(at, bytes).await {
            let _ = self.delete_volume(id).await;
            return Err(VolumeError::Drive(e));
        }
        self.seal_volume(id, Some(fs)).await?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::pallet::store::PalletStore;
    use crate::placement::topology::StorageTier;

    const SLOT: u64 = 64 * 1024;

    async fn manager() -> (VolumeManager, String) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-disk-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.bin"));
        let path_str = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024).await.unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(dev);
        let slab = Slab::format(dev, SLOT, StorageTier::Hot).await.unwrap();
        let mut vm = VolumeManager::new(SLOT);
        vm.registry().write().await.add(slab);
        (vm, path_str)
    }

    async fn golden(vm: &mut VolumeManager, name: &str, size: u64, fill: u8) -> VolumeId {
        let id = vm.create_volume_with(name, size, Default::default()).await.unwrap();
        let h = vm.get_volume_handle(&id).unwrap();
        h.write(0, &vec![fill; size as usize]).await.unwrap();
        vm.seal_volume(id, None).await.unwrap();
        id
    }

    async fn free_slots(vm: &VolumeManager) -> u64 {
        vm.registry().read().await.total_free_slots()
    }

    fn boot_pallet(kernel: VolumeId, initrd: VolumeId) -> PalletVolumeSpec {
        let mut spec = PalletVolumeSpec::new("boot1", PalletKind::Boot);
        spec.version_label = "6.12.0".into();
        spec.members.push(PalletMember {
            name: "kernel".into(),
            role: "kernel".into(),
            kind: MemberKind::Kernel,
            source: MemberSource::Volume { id: kernel, len: None },
        });
        spec.members.push(PalletMember {
            name: "initramfs".into(),
            role: "initramfs".into(),
            kind: MemberKind::Initramfs,
            source: MemberSource::Volume { id: initrd, len: None },
        });
        spec.members.push(PalletMember {
            name: "cmdline".into(),
            role: "cmdline".into(),
            kind: MemberKind::BootConfig,
            source: MemberSource::Bytes(b"root=PARTUUID=x ro".to_vec()),
        });
        spec
    }

    /// A composed pallet shares its goldens' slots and writes only its header
    /// and inline members — and it verifies through the reader every
    /// consumer uses, which is the property that makes it a pallet.
    #[tokio::test]
    async fn a_composed_pallet_verifies_and_costs_its_header() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", 3 * SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", 2 * SLOT, 0x49).await;
        let before = free_slots(&vm).await;

        let report = vm.compose_pallet(boot_pallet(kernel, initrd)).await.unwrap();

        // One slot for the header, one for the cmdline; the goldens shared.
        assert_eq!(before - free_slots(&vm).await, 2, "only the header and the inline member cost a slot");
        assert_eq!(report.shared_bytes, 5 * SLOT);
        assert!(report.members[0].shared && report.members[1].shared && !report.members[2].shared);
        assert_eq!(report.members[0].offset, SLOT, "first member sits on the slot after the header");
        assert_eq!(report.members[1].offset, 4 * SLOT, "second member starts after the whole first golden");
        assert!(vm.is_sealed(&VolumeId(report.id)));
        assert_eq!(vm.fs_info(&VolumeId(report.id)).unwrap().kind, "pallet");

        // Read back as a pallet, verify every member, and the content is the
        // goldens'.
        let dev = vm.get_volume(&VolumeId(report.id)).unwrap();
        let view = PartitionView::whole(dev.clone());
        let pallet = Pallet::read(&view).await.unwrap();
        assert_eq!(pallet.name(), "boot1");
        assert_eq!(pallet.kind(), PalletKind::Boot);
        pallet.verify_all(&view).await.unwrap();
        let k = pallet.find("kernel").unwrap();
        let mut buf = vec![0u8; 4096];
        pallet.read_member(&k, &view, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x4B));
        let c = pallet.find("cmdline").unwrap();
        let mut buf = vec![0u8; c.byte_len as usize];
        pallet.read_member(&c, &view, 0, &mut buf).await.unwrap();
        assert_eq!(&buf, b"root=PARTUUID=x ro");

        let _ = std::fs::remove_file(&path);
    }

    /// A member may digest less than its golden holds; the next member still
    /// lands after the whole golden, or its slots would be aliased.
    #[tokio::test]
    async fn a_shorter_member_still_reserves_its_golden() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", 3 * SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let mut spec = boot_pallet(kernel, initrd);
        if let MemberSource::Volume { len, .. } = &mut spec.members[0].source {
            *len = Some(SLOT + 100);
        }
        let report = vm.compose_pallet(spec).await.unwrap();
        assert_eq!(report.members[0].len, SLOT + 100);
        assert_eq!(report.members[0].span, 3 * SLOT);
        assert_eq!(report.members[1].offset, 4 * SLOT);
        let dev = vm.get_volume(&VolumeId(report.id)).unwrap();
        let view = PartitionView::whole(dev);
        Pallet::read(&view).await.unwrap().verify_all(&view).await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// The whole point: a disk is a chain of goldens. It reads back as a GPT
    /// whose pallet partition verifies, and it allocated nothing of its own.
    #[tokio::test]
    async fn a_composed_disk_is_a_gpt_over_shared_goldens() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", 2 * SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let pallet = vm.compose_pallet(boot_pallet(kernel, initrd)).await.unwrap();
        let esp = golden(&mut vm, "esp.golden", 2 * SLOT, 0xE5).await;

        let before = free_slots(&vm).await;
        let mut spec = DiskSpec::new("node1.disk");
        spec.partitions.push(DiskPartition { name: Some("EFI".into()), type_guid: Some(type_guid::ESP), ..DiskPartition::new(esp) });
        spec.partitions.push(DiskPartition { priority: Some(7), ..DiskPartition::new(VolumeId(pallet.id)) });
        let report = vm.compose_disk(spec).await.unwrap();

        // Two slots minted for the GPT goldens, and nothing for the disk.
        assert!(report.gpt_minted);
        assert_eq!(before - free_slots(&vm).await, 2, "the GPT goldens are the only new slots");
        assert_eq!(report.written_bytes, 0);
        assert_eq!(report.lba, 4096);
        assert_eq!(report.partitions.len(), 2);
        assert_eq!(report.partitions[0].start_bytes, SLOT);
        assert_eq!(report.partitions[1].start_bytes, 3 * SLOT);
        assert_eq!(report.size_bytes, 3 * SLOT + pallet.virtual_size + SLOT);

        let dev = vm.get_volume(&VolumeId(report.id)).unwrap();
        let gpt = Gpt::read(&dev).await.unwrap();
        assert!(!gpt.recovered_from_backup, "the primary header is where it should be");
        assert_eq!(gpt.block_size, 4096);
        assert_eq!(Uuid::from_bytes_le(gpt.disk_guid), report.disk_guid);
        let parts: Vec<_> = gpt.partitions().collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].1.name, "EFI");
        assert_eq!(parts[0].1.type_guid, type_guid::ESP);
        assert_eq!(parts[1].1.type_guid, PALLET_TYPE_GUID);
        assert_eq!(parts[1].1.start_bytes(4096), 3 * SLOT);
        assert!(parts[1].1.attributes & (1 << 0) != 0, "required");

        // The pallet inside verifies through its GPT entry, and the ESP reads
        // as its golden.
        let view = gpt.view(&dev, parts[1].0).unwrap();
        let p = Pallet::read(&view).await.unwrap();
        p.verify_all(&view).await.unwrap();
        let mut buf = vec![0u8; 4096];
        gpt.view(&dev, parts[0].0).unwrap().read_at(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xE5));

        // The backup header is at the last LBA, which is what a tool reads
        // when the primary is gone.
        let last = dev.capacity_bytes() - 4096;
        let mut hdr = vec![0u8; 4096];
        dev.read(last, &mut hdr).await.unwrap();
        assert_eq!(&hdr[..8], b"EFI PART");

        // The disk records what it is.
        let fs = vm.fs_info(&VolumeId(report.id)).unwrap();
        assert_eq!(fs.kind, "gpt");
        assert_eq!(fs.uuid, Some(report.disk_guid));

        // And a pallet store over it selects the pallet, as firmware would.
        let mut store = PalletStore::default();
        store.add_drive("node1.disk".into(), dev.clone());
        let found = store.scan().await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "boot1");
        assert_eq!(found[0].attributes.priority, 7);

        let _ = std::fs::remove_file(&path);
    }

    /// A second disk of the same layout finds the GPT goldens rather than
    /// minting them, carries the same PARTUUIDs, and allocates nothing.
    #[tokio::test]
    async fn two_disks_of_one_layout_share_the_gpt() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let pallet = vm.compose_pallet(boot_pallet(kernel, initrd)).await.unwrap();

        let mut spec = DiskSpec::new("node1.disk");
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let one = vm.compose_disk(spec).await.unwrap();
        let before = free_slots(&vm).await;

        let mut spec = DiskSpec::new("node2.disk");
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let two = vm.compose_disk(spec).await.unwrap();

        assert!(!two.gpt_minted);
        assert_eq!(two.head_golden, one.head_golden);
        assert_eq!(two.tail_golden, one.tail_golden);
        assert_eq!(two.disk_guid, one.disk_guid);
        assert_eq!(two.partitions[0].partuuid, one.partitions[0].partuuid);
        assert_eq!(free_slots(&vm).await, before, "the second disk is only a map");

        // Writing to one disk does not reach the other: the boot ladder on
        // node 2 decrementing tries is node 2's slot.
        let d2 = vm.get_volume(&VolumeId(two.id)).unwrap();
        let mut gpt = Gpt::read(&d2).await.unwrap();
        gpt.entries[0].attributes |= 1 << 56; // successful
        gpt.write(&d2).await.unwrap();
        let d1 = vm.get_volume(&VolumeId(one.id)).unwrap();
        let g1 = Gpt::read(&d1).await.unwrap();
        assert_eq!(g1.entries[0].attributes & (1 << 56), 0, "node 1's table is untouched");
        assert_eq!(before - free_slots(&vm).await, 2, "and node 2 paid two slots for its two headers");

        let _ = std::fs::remove_file(&path);
    }

    /// A per-disk GUID costs exactly the two GPT slots, and the goldens keep
    /// the layout's.
    #[tokio::test]
    async fn a_fresh_guid_costs_the_two_gpt_slots() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let pallet = vm.compose_pallet(boot_pallet(kernel, initrd)).await.unwrap();

        let mut spec = DiskSpec::new("node1.disk");
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let one = vm.compose_disk(spec).await.unwrap();
        let before = free_slots(&vm).await;

        let mut spec = DiskSpec::new("node2.disk");
        spec.fresh_guid = true;
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let two = vm.compose_disk(spec).await.unwrap();

        assert_ne!(two.disk_guid, one.disk_guid);
        assert_eq!(two.written_bytes, 2 * SLOT);
        assert_eq!(before - free_slots(&vm).await, 2);
        let d2 = vm.get_volume(&VolumeId(two.id)).unwrap();
        let gpt = Gpt::read(&d2).await.unwrap();
        assert_eq!(Uuid::from_bytes_le(gpt.disk_guid), two.disk_guid);
        // The head golden still carries the layout's GUID: the stamp landed
        // on node 2's copy-on-write slot, not on the shared one.
        let head = vm.get_volume(&VolumeId(two.head_golden)).unwrap();
        let mut hdr = vec![0u8; 4096];
        head.read(4096, &mut hdr).await.unwrap();
        assert_eq!(Uuid::from_bytes_le(hdr[56..72].try_into().unwrap()), one.disk_guid);

        let _ = std::fs::remove_file(&path);
    }

    /// A 512-byte table for a disk that will be copied onto a 512-byte drive.
    #[tokio::test]
    async fn a_512_lba_disk_reads_back_at_512() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let mut ps = boot_pallet(kernel, initrd);
        ps.lba = 512;
        let pallet = vm.compose_pallet(ps).await.unwrap();
        let mut spec = DiskSpec::new("node1.disk");
        spec.lba = 512;
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let report = vm.compose_disk(spec).await.unwrap();
        let dev = vm.get_volume(&VolumeId(report.id)).unwrap();
        let gpt = Gpt::read(&dev).await.unwrap();
        assert_eq!(gpt.block_size, 512);
        let view = gpt.view(&dev, report.partitions[0].index).unwrap();
        Pallet::read(&view).await.unwrap().verify_all(&view).await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// A declared size smaller than the chain is refused, and a larger one
    /// puts the tail at the end.
    #[tokio::test]
    async fn declared_size_bounds_the_chain() {
        let (mut vm, path) = manager().await;
        let kernel = golden(&mut vm, "kernel.golden", SLOT, 0x4B).await;
        let initrd = golden(&mut vm, "initrd.golden", SLOT, 0x49).await;
        let pallet = vm.compose_pallet(boot_pallet(kernel, initrd)).await.unwrap();

        let mut spec = DiskSpec::new("small.disk");
        spec.size = Some(2 * SLOT);
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        assert!(vm.compose_disk(spec).await.is_err());

        let mut spec = DiskSpec::new("big.disk");
        spec.size = Some(32 * SLOT);
        spec.partitions.push(DiskPartition::new(VolumeId(pallet.id)));
        let report = vm.compose_disk(spec).await.unwrap();
        assert_eq!(report.size_bytes, 32 * SLOT);
        let dev = vm.get_volume(&VolumeId(report.id)).unwrap();
        let gpt = Gpt::read(&dev).await.unwrap();
        assert_eq!(gpt.alternate_lba, 32 * SLOT / 4096 - 1);
        assert_eq!(gpt.last_usable_lba, 31 * SLOT / 4096 - 1);
        let _ = std::fs::remove_file(&path);
    }
}
