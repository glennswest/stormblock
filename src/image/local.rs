//! Laying this node's own slabs on a drive it has taken over.
//!
//! A stormcos disk is two slabs, not one: a **data** slab holding identity and
//! state, and a **system** slab holding the goldens the node runs from. That
//! split is the whole reason an install is survivable — it replaces the system
//! end and leaves the data end alone, and the two are told apart from the
//! partition table rather than from a path someone typed (#88).
//!
//! `boot-local --local-disk` predates that split. It formats the whole device
//! as a single slab, which takes `SlabRole`'s default of `System`, and its
//! flow-over then drains only non-data slabs onto it. So the goldens can
//! already move to a local drive and **the writes cannot**: every log line,
//! every claim, every byte of `stormcos-state` still lands in a clone on the
//! appliance. One node can afford that. Twenty cannot, and the appliance is
//! the thing that pays.
//!
//! This lays the layout the image already uses, on a drive, so both halves
//! have somewhere local to go.

use std::sync::Arc;

use crate::drive::partition::PartitionDevice;
use crate::drive::slab::{auto_metadata_bytes, Slab, SlabFormat, SlabRole, DEFAULT_SLOT_SIZE};
use crate::drive::BlockDevice;
use crate::image::type_guid;
use crate::pallet::gpt::Gpt;
use crate::placement::topology::StorageTier;

use super::{ImageError, Result};

/// How the two slabs are sized on a drive.
pub struct LocalLayout {
    /// Bytes for the system slab, at the front. The data slab takes the rest
    /// of the drive, at the end.
    ///
    /// The system half is the one with a knowable size: the goldens of a
    /// release, which every install replaces wholesale. The data half is the
    /// one that grows — PVCs, VM disks, cloud images — so it gets the drive,
    /// and it is last so that it can keep growing when the drive does
    /// (stormcos#36, #48).
    pub system_bytes: u64,
    pub slot_size: u64,
    pub tier: StorageTier,
    /// GPT block size. `None` follows the device, which is what firmware and
    /// the kernel both read it in.
    pub lba: Option<u32>,
    /// Bytes left free at the **front** of the drive for what makes it boot
    /// on its own: an ESP holding stormuefi and the release's `boot` pallets
    /// (#123). Free GPT space rather than a partition, because pallets are
    /// partitions of their own, allocated first-fit — and the first free run
    /// on this drive is this one. Zero lays no boot area; the drive then
    /// only ever boots through the network claim.
    ///
    /// Sized for two generations of the boot pallet and the ESP: the one the
    /// node runs, and the one it falls back to.
    pub boot_bytes: u64,
    /// Lay the data half as a 1 MiB data slab and an 8 MiB **bulk** slab
    /// (#156, owner 2026-10-05): the bulk slab last, taking what is left of
    /// the drive, the data slab a quarter of the data half (at least 64 GiB).
    /// Only with metadata format 2 (bulk volumes are recorded only there) and
    /// a data half of [`BULK_MIN_DATA_HALF`] or more.
    pub bulk: bool,
}

/// The name of the bulk partition (#156): typed as a data slab, it is the
/// data half's second pool.
pub const BULK_PARTITION: &str = "stormblock-bulk";
/// A data half smaller than this is one 1 MiB data slab.
pub const BULK_MIN_DATA_HALF: u64 = 256 << 30;

impl LocalLayout {
    /// A sixteenth of the drive for the system half, floored at 32 GiB and
    /// capped at 128; the data half gets everything else.
    ///
    /// This was the other way round — a tenth for data, capped at 64 GiB, and
    /// 1.8 TB of a 2 TB drive for goldens that used 8 GB, while VM images and
    /// PVCs filled the data half. A release's goldens are ~11 GB today; the
    /// floor holds a few of them, which is room for the previous release to
    /// stay bootable beside the next, and the cap stops a 20 TB drive
    /// donating a terabyte to goldens.
    pub fn for_drive(capacity: u64) -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        LocalLayout {
            system_bytes: (capacity / 16).clamp(32 * GIB, 128 * GIB).min(capacity / 2),
            slot_size: DEFAULT_SLOT_SIZE,
            tier: StorageTier::Hot,
            lba: None,
            boot_bytes: boot_area_for(capacity),
            bulk: crate::drive::slab::default_format() == crate::drive::slab::SLAB_VERSION_2,
        }
    }
}

/// The boot area a drive of this size gets: 4 GiB on anything that is a real
/// system drive, 1 GiB on a small one, none on a drive too small to spare it.
///
/// A release's boot pallet — kernel, initramfs, modules, firmware — is under
/// a gigabyte, and the area holds two of them plus the ESP.
pub fn boot_area_for(capacity: u64) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    if capacity >= 64 * GIB {
        4 * GIB
    } else if capacity >= 16 * GIB {
        GIB
    } else {
        0
    }
}

/// Can firmware boot from this drive's layout, once the ESP and pallets are
/// on it? A table in the medium's own LBA size, and a boot area.
///
/// This is what the "already up to date" shortcut must also ask: a drive
/// laid before local boot existed holds every golden and still cannot boot,
/// and taking the shortcut would leave it that way forever.
pub async fn boot_ready(device: &Arc<dyn BlockDevice>, opts: &LocalLayout) -> bool {
    if opts.boot_bytes == 0 {
        return true;
    }
    let Ok(gpt) = Gpt::read(device).await else { return false };
    if opts.lba.is_some_and(|l| l != gpt.block_size) {
        return false;
    }
    has_boot_area(device, opts.boot_bytes).await
}

/// Re-express the table in `lba`-byte blocks, every partition at the same
/// bytes. Returns whether anything changed.
///
/// `lay_node_slabs` used to write the table in `FileDevice`'s block size,
/// which is 4096 whatever the drive is — and UEFI parses a GPT in the
/// medium's own block size. On a 512-byte drive that is a disk with no
/// partition table as far as firmware is concerned. Our reader probes both,
/// so nothing noticed until the disk was asked to boot (#123).
///
/// Nothing moves: a partition is a byte range, and only the unit it is
/// written in changes. Both directions are safe to interrupt, because the
/// new table's head and tail each cover the old header at their end — and
/// `Gpt::write` puts the tail down first, so at every point one complete
/// table describes the same byte ranges.
pub async fn retable(device: &Arc<dyn BlockDevice>, lba: u32) -> Result<bool> {
    let old = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    if old.block_size == lba {
        return Ok(false);
    }
    let obs = old.block_size as u64;
    let nbs = lba as u64;
    let mut new = Gpt::create_with_lba(device, lba);
    new.disk_guid = old.disk_guid;
    for (i, e) in old.partitions() {
        let start = e.first_lba * obs;
        let end = (e.last_lba + 1) * obs;
        if start % nbs != 0 || end % nbs != 0 {
            return Err(ImageError::Other(format!(
                "partition {} ({}) is not aligned to {lba}-byte blocks",
                i + 1,
                e.name
            )));
        }
        let mut n = e.clone();
        n.first_lba = start / nbs;
        n.last_lba = end / nbs - 1;
        if n.first_lba < new.first_usable_lba || n.last_lba > new.last_usable_lba {
            return Err(ImageError::Other(format!(
                "partition {} ({}) does not fit a {lba}-byte table",
                i + 1,
                e.name
            )));
        }
        new.entries[i] = n;
    }
    new.write(device)
        .await
        .map_err(|e| ImageError::Other(format!("writing the {lba}-byte table: {e}")))?;
    Ok(true)
}

/// Does this drive have somewhere to put what boots it?
///
/// An ESP already on it says yes. Otherwise the free space in front of the
/// system partition has to be at least `boot_bytes` — which it is not on a
/// drive laid before the boot area existed, where the system slab starts at
/// the first megabyte.
pub async fn has_boot_area(device: &Arc<dyn BlockDevice>, boot_bytes: u64) -> bool {
    if boot_bytes == 0 {
        return true;
    }
    let Ok(gpt) = Gpt::read(device).await else { return false };
    if gpt.partitions().any(|(_, e)| e.type_guid == type_guid::ESP) {
        return true;
    }
    let Some(s) = halves_on(device).await.1 else { return false };
    let bs = gpt.block_size as u64;
    let front = gpt.entries[s].first_lba.saturating_sub(gpt.first_usable_lba) * bs;
    // The first megabyte is alignment the table takes anyway.
    front + ALIGN >= boot_bytes
}

/// What was laid down.
pub struct LocalSlabs {
    pub data: Slab,
    pub system: Slab,
    /// The bulk slab (8 MiB slots, #156), when the layout has one.
    pub bulk: Option<Slab>,
    pub data_bytes: u64,
    pub system_bytes: u64,
    pub bulk_bytes: u64,
    pub lba: u32,
}

const ALIGN: u64 = 1024 * 1024;
/// How far the data slab can grow in place, as a multiple of its laid size.
const DATA_GROWTH: u64 = 4;
const GPT_OVERHEAD: u64 = 2 * ALIGN;

fn align_down(v: u64, a: u64) -> u64 {
    v / a * a
}

/// Is this drive one this node already installed onto?
///
/// Both partitions, by type, in the layout `lay_node_slabs` writes. `None`
/// means the drive is something else — a foreign table, a bare slab, an empty
/// disk — and taking it means laying a new table over it.
///
/// Deliberately a question about the *table*, not about a path: a path proves
/// nothing about what is on a device (#88), and the two halves are told apart
/// by their partition types precisely so that an install can replace one and
/// leave the other.
pub async fn node_layout(device: &Arc<dyn BlockDevice>) -> Result<Option<(usize, usize)>> {
    let Ok(gpt) = Gpt::read(device).await else { return Ok(None) };
    let mut data = None;
    let mut system = None;
    for (i, e) in gpt.partitions() {
        // The data slab, not the bulk slab beside it (#156).
        if e.type_guid == type_guid::SLAB_DATA && data.is_none() && e.name != BULK_PARTITION {
            data = Some(i);
        } else if e.type_guid == type_guid::SLAB && system.is_none() {
            system = Some(i);
        }
    }
    Ok(match (data, system) {
        (Some(d), Some(s)) => Some((d, s)),
        _ => None,
    })
}

/// Whether a slab's magic is at `offset` (#346): `Ok(false)` when the bytes
/// read and hold none (a zeroed disk, a table laid and never filled),
/// `Err` when they cannot be read (a fault to name, never a disk to lay over).
pub async fn slab_magic_at(device: &Arc<dyn BlockDevice>, offset: u64) -> Result<bool> {
    let mut buf = vec![0u8; (device.block_size() as usize).max(4096)];
    device
        .read(offset, &mut buf)
        .await
        .map_err(|e| ImageError::Other(format!("reading {} bytes at {offset}: {e}", buf.len())))?;
    Ok(buf[..8] == crate::drive::slab::SLAB_MAGIC)
}

/// Whether a node layout's data half holds a slab at all (#346): its data
/// partition, or its bulk one, carries slab magic. `None` when the drive
/// carries no node layout. A table whose data partitions hold no magic has
/// nothing in them to keep: the drive is laid fresh, both halves. Only a
/// slab that is there (magic present) and does not open is refused.
pub async fn node_data_half_present(device: &Arc<dyn BlockDevice>) -> Result<Option<bool>> {
    let Some((data_i, _)) = node_layout(device).await? else { return Ok(None) };
    let gpt = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let start = |i: usize| gpt.entries[i].start_bytes(gpt.block_size);
    let mut present = slab_magic_at(device, start(data_i)).await?;
    if let Some(b) = bulk_partition(device).await {
        present |= slab_magic_at(device, start(b)).await?;
    }
    Ok(Some(present))
}

/// Whether a drive's data partition (with or without a system half beside
/// it, #402) holds a slab: `None` when it has no data partition.
pub async fn data_half_present(device: &Arc<dyn BlockDevice>) -> Result<Option<bool>> {
    let Some(data_i) = halves_on(device).await.0 else { return Ok(None) };
    let gpt = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let start = |i: usize| gpt.entries[i].start_bytes(gpt.block_size);
    let mut present = slab_magic_at(device, start(data_i)).await?;
    if let Some(b) = bulk_partition(device).await {
        present |= slab_magic_at(device, start(b)).await?;
    }
    Ok(Some(present))
}

/// The bulk partition of a node layout (#156), if it has one.
pub async fn bulk_partition(device: &Arc<dyn BlockDevice>) -> Option<usize> {
    let gpt = Gpt::read(device).await.ok()?;
    let found = gpt
        .partitions()
        .find(|(_, e)| e.type_guid == type_guid::SLAB_DATA && e.name == BULK_PARTITION)
        .map(|(i, _)| i);
    found
}

/// What the system half already holds, by volume id, according to the slab
/// itself.
///
/// Read offline, from the slab's own metadata region: no attach, no daemon,
/// nothing made live. `None` means the question cannot be answered here — the
/// drive is not a node layout, or the slab keeps no record of itself — and a
/// caller must then assume it holds nothing.
///
/// This is what makes "no need to do anything" a decision rather than a
/// guess. A node that netboots regularly would otherwise reformat its own
/// system half and re-copy every golden on every boot, destroying a working
/// local half to rebuild the same bytes — and running from the appliance for
/// the minutes that takes, each time.
pub async fn system_slab_volumes(
    device: &Arc<dyn BlockDevice>,
) -> Result<Option<std::collections::HashSet<uuid::Uuid>>> {
    // The system partition, on a drive with both halves or with the system
    // half alone (#402).
    let Some(system_i) = halves_on(device).await.1 else { return Ok(None) };
    let gpt = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let e = &gpt.entries[system_i];
    let part = Arc::new(
        PartitionDevice::new(device.clone(), e.start_bytes(gpt.block_size), e.size_bytes(gpt.block_size))
            .map_err(|err| ImageError::Other(format!("system partition: {err}")))?,
    );
    let Ok(slab) = Slab::open(part).await else { return Ok(None) };
    // "Holds nothing" and "cannot say" are different answers, and the
    // difference is the region rather than its contents (#108, one layer up).
    // A freshly laid system half has a region and no record in it: it can
    // answer, and the answer is that it holds nothing, which is exactly the
    // case that must go on to copy.
    if !slab.has_metadata_region() {
        return Ok(None);
    }
    let meta = match crate::volume::metav2::read_slab(&slab).await {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(Some(std::collections::HashSet::new())),
        Err(_) => return Ok(None),
    };
    Ok(Some(meta.volumes.into_iter().map(|v| v.id.0).collect()))
}

/// Whether a local drive already holds a release (#236).
///
/// Without a boot intent (forge on an engine older than v20, #235) nothing
/// says whether a netboot is an install: stormbootx claims on every boot, so
/// "it came over the network" is true of a reboot too. What does tell them
/// apart is the release. A reboot boots the release the local disk already
/// holds; an install boots one it does not. So this compares the image's
/// goldens — its sealed volumes, by id, which a flow-over keeps (it moves a
/// volume's extents, it does not make a new volume) — with every volume the
/// local drive records. The same test as the flow-over's "already up to
/// date", asked before anything is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseHeld {
    /// Every golden of the image is on the local drive.
    Held { goldens: usize },
    /// `missing` of the image's `goldens` are not on the local drive.
    NotHeld { goldens: usize, missing: usize },
    /// Every golden is recorded, but the install never finished: the local
    /// records still place extents on `slabs` slab(s) that are not on the
    /// drive — the appliance clone a flow-over was moving from (#239). The
    /// same release, cut short (a power cut during the flow-over): the drive
    /// boots and the engine finishes the flow-over from a fresh clone (#171).
    /// Since b189b3e nothing the node writes stays on the clone (owned
    /// extents relocate while it is a flow source), so the fresh clone's
    /// pristine bytes are exactly what the records still point at; installing
    /// over the drive instead destroyed its data half (#258).
    Unfinished { goldens: usize, slabs: usize },
    /// One side cannot answer: no slab, no metadata, or no sealed volume in
    /// the image. A caller must not read this as either answer.
    CannotSay(String),
}

/// Every volume record the slabs on `device` keep, read offline. `device` is
/// a slab, or a disk whose partitions are slabs. `None` when no slab there
/// keeps a record of itself.
pub async fn recorded_volumes(
    device: &Arc<dyn BlockDevice>,
) -> Option<Vec<crate::volume::metadata::VolumeRecord>> {
    recorded_volumes_and_slabs(device).await.map(|(v, _)| v)
}

/// `recorded_volumes`, and the ids of the slabs on `device`.
async fn recorded_volumes_and_slabs(
    device: &Arc<dyn BlockDevice>,
) -> Option<(Vec<crate::volume::metadata::VolumeRecord>, std::collections::HashSet<crate::drive::slab::SlabId>)> {
    let slabs: Vec<Slab> = match Slab::open(device.clone()).await {
        Ok(s) => vec![s],
        Err(_) => crate::drive::discover::slabs_in_partitions(device)
            .await
            .into_iter()
            .map(|f| f.slab)
            .collect(),
    };
    let ids: std::collections::HashSet<_> = slabs.iter().map(|s| s.slab_id()).collect();
    let mut answered = false;
    let mut out = Vec::new();
    for slab in slabs {
        if !slab.has_metadata_region() {
            continue;
        }
        match crate::volume::metav2::read_slab(&slab).await {
            Ok(Some(meta)) => {
                answered = true;
                out.extend(meta.volumes);
            }
            Ok(None) => answered = true,
            Err(_) => {}
        }
    }
    answered.then_some((out, ids))
}

/// Does `local` hold the release on `image`? See [`ReleaseHeld`].
pub async fn release_held(local: &Arc<dyn BlockDevice>, image: &Arc<dyn BlockDevice>) -> ReleaseHeld {
    let Some(image_vols) = recorded_volumes(image).await else {
        return ReleaseHeld::CannotSay("the image keeps no volume records".into());
    };
    let goldens: std::collections::HashSet<uuid::Uuid> =
        image_vols.iter().filter(|v| v.sealed).map(|v| v.id.0).collect();
    if goldens.is_empty() {
        return ReleaseHeld::CannotSay("the image holds no sealed volume".into());
    }
    let Some((local_vols, here)) = recorded_volumes_and_slabs(local).await else {
        return ReleaseHeld::CannotSay("the local drive keeps no volume records".into());
    };
    // Sealed here too: a golden a stage is still copying in (#122) has the
    // release's id and is sealed only once it is whole, so a stage cut short
    // never reads as holding the release.
    let have: std::collections::HashSet<uuid::Uuid> =
        local_vols.iter().filter(|v| v.sealed).map(|v| v.id.0).collect();
    let missing = goldens.iter().filter(|id| !have.contains(id)).count();
    if missing > 0 {
        return ReleaseHeld::NotHeld { goldens: goldens.len(), missing };
    }
    // Complete, not only recorded: nothing still placed on a slab that is
    // not on this drive. Asked of the release's goldens and of whatever has a
    // leg here: a data volume wholly on another drive of this node (the data
    // half's own drive, #402) is that drive's, not unfinished here.
    let elsewhere: std::collections::HashSet<_> = local_vols
        .iter()
        .filter(|v| {
            goldens.contains(&v.id.0)
                || v.extents.values().any(|l| l.legs().any(|leg| here.contains(&leg.slab_id)))
        })
        .flat_map(|v| {
            v.extents
                .values()
                .flat_map(|l| l.legs().map(|leg| leg.slab_id))
                .chain(v.parity.values().flat_map(|g| g.legs.iter().map(|l| l.slab_id)))
                .collect::<Vec<_>>()
        })
        .filter(|id| !here.contains(id))
        .collect();
    if !elsewhere.is_empty() {
        return ReleaseHeld::Unfinished { goldens: goldens.len(), slabs: elsewhere.len() };
    }
    ReleaseHeld::Held { goldens: goldens.len() }
}

/// What the *data* half of a drive already holds, read offline.
///
/// The same question as `system_slab_volumes` and a far more consequential
/// answer: this partition is where a node's CA key and its ServiceAccount
/// signing key live, and nothing can mint them again. So the only safe thing
/// to do with a data slab that holds volumes is leave it alone, and the only
/// way to know is to ask it before anything is attached.
///
/// `Some(empty)` is the case that matters — a data half that was laid and
/// never filled, which is every drive this node has taken so far, because the
/// flow-over deliberately migrates only the system half. `None` means the
/// question cannot be answered, and an unanswerable question about identity is
/// answered by doing nothing.
pub async fn data_slab_volumes(
    device: &Arc<dyn BlockDevice>,
) -> Result<Option<std::collections::HashSet<uuid::Uuid>>> {
    let Some((data_i, _)) = node_layout(device).await? else { return Ok(None) };
    let gpt = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let e = &gpt.entries[data_i];
    let part = Arc::new(
        PartitionDevice::new(device.clone(), e.start_bytes(gpt.block_size), e.size_bytes(gpt.block_size))
            .map_err(|err| ImageError::Other(format!("data partition: {err}")))?,
    );
    let Ok(slab) = Slab::open(part).await else { return Ok(None) };
    if !slab.has_metadata_region() {
        return Ok(None);
    }
    let meta = match crate::volume::metav2::read_slab(&slab).await {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(Some(std::collections::HashSet::new())),
        Err(_) => return Ok(None),
    };
    Ok(Some(meta.volumes.into_iter().map(|v| v.id.0).collect()))
}

/// Reinstall onto a drive that is already this node's: **replace the system
/// half, keep the data half, and let the node boot normally.**
///
/// This is what an install *is*. A node that netboots this image to be
/// installed, onto the drive a previous install used, has one thing on that
/// drive that cannot be made again — the data slab, holding its CA key and
/// its ServiceAccount signing key — and one thing that is meant to be
/// replaced, the goldens in the system slab. Laying a fresh table over both
/// destroys the first to refresh the second; refusing the drive leaves the
/// node running from the appliance forever. Neither is an install.
///
/// So: no wipe, no new table, the data slab opened and left exactly as it is,
/// and only the system partition formatted afresh for the goldens the
/// flow-over is about to copy into it.
///
/// The data slab is *opened*, not assumed. A drive whose data partition will
/// not open as a data slab is the abandoned-install case, and that is the one
/// `--local-disk-force` exists for: this returns an error rather than
/// guessing, because guessing here costs a node its identity.
pub async fn update_system_slab(
    device: Arc<dyn BlockDevice>,
    opts: &LocalLayout,
) -> Result<LocalSlabs> {
    if let Some(lba) = opts.lba {
        retable(&device, lba).await?;
    }
    let gpt = Gpt::read(&device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let (data_i, system_i) = node_layout(&device)
        .await?
        .ok_or_else(|| ImageError::Spec("this drive does not carry a node layout".into()))?;
    let lba = gpt.block_size;

    // **Make room for the boot area** on a drive laid before it existed. The
    // system half is about to be formatted afresh, so moving where it starts
    // costs nothing that this call was not already going to destroy — and it
    // is the only half that may move: the data half holds the node's identity
    // and stays exactly where it is.
    let mut gpt = gpt;
    if !has_boot_area(&device, opts.boot_bytes).await {
        let bs = lba as u64;
        let e = &gpt.entries[system_i];
        let want_first = (gpt.first_usable_lba * bs).div_ceil(ALIGN) * ALIGN + opts.boot_bytes;
        let new_first = want_first / bs;
        let size_after = (e.last_lba + 1).saturating_sub(new_first) * bs;
        if size_after < 64 * ALIGN {
            return Err(ImageError::Spec(format!(
                "the system partition is too small to give up {} bytes for a boot area",
                opts.boot_bytes
            )));
        }
        gpt.entries[system_i].first_lba = new_first;
        gpt.write(&device)
            .await
            .map_err(|e| ImageError::Other(format!("moving the system partition: {e}")))?;
    }

    let part = |i: usize| -> Result<Arc<PartitionDevice>> {
        let e = &gpt.entries[i];
        PartitionDevice::new(device.clone(), e.start_bytes(lba), e.size_bytes(lba))
            .map(Arc::new)
            .map_err(|err| ImageError::Other(format!("partition {}: {err}", i + 1)))
    };

    let data_part = part(data_i)?;
    let data_bytes = data_part.capacity_bytes();
    let data = Slab::open(data_part).await.map_err(|e| {
        ImageError::Other(format!(
            "the data partition on this drive will not open as a slab ({e}) — it holds this \
             node's identity and this will not guess at it"
        ))
    })?;
    if !data.is_data() {
        return Err(ImageError::Other(
            "the partition typed as a data slab says it is a system slab; not touching this drive"
                .into(),
        ));
    }

    // The system half, and only it. Formatted rather than adopted: what is on
    // it is the last install's goldens, and this boot is the next install.
    let system_part = part(system_i)?;
    let system_bytes = system_part.capacity_bytes();
    // Discarded before it is formatted again (#401), as a fresh lay discards
    // the drive: the last install's goldens are garbage to the disk too.
    let dev: Arc<dyn BlockDevice> = system_part.clone();
    discard_quietly(&dev, 0, system_bytes / lba as u64 * lba as u64, "the system partition").await;
    let meta = auto_metadata_bytes(system_bytes, opts.slot_size);
    let system = Slab::format_with(
        system_part,
        SlabFormat::new(opts.slot_size, opts.tier)
            .with_metadata(meta)
            .with_role(SlabRole::System),
    )
    .await
    .map_err(|e| ImageError::Other(format!("formatting the system slab: {e}")))?;

    // The bulk slab beside it, kept with the data half (#156).
    let (bulk, bulk_bytes) = match bulk_partition(&device).await {
        Some(i) => {
            let p = part(i)?;
            let n = p.capacity_bytes();
            let slab = Slab::open(p)
                .await
                .map_err(|e| ImageError::Other(format!("the bulk partition will not open as a slab ({e})")))?;
            (Some(slab), n)
        }
        None => (None, 0),
    };
    Ok(LocalSlabs { data, system, bulk, data_bytes, system_bytes, bulk_bytes, lba })
}

/// Discard a range before it is laid (#401), saying how long it took. A
/// discard is a hint, and a disk that refuses one is laid all the same.
async fn discard_quietly(device: &Arc<dyn BlockDevice>, offset: u64, len: u64, what: &str) {
    if len == 0 {
        return;
    }
    let t = std::time::Instant::now();
    match device.discard(offset, len).await {
        Ok(()) => tracing::info!("discarded {what} ({len} bytes) before laying it in {} ms", t.elapsed().as_millis()),
        Err(e) => tracing::info!("{what} not discarded before laying it ({e}): laid all the same"),
    }
}

/// One partition [`lay_table`] lays, and the slab formatted on it.
struct Part {
    name: &'static str,
    guid: [u8; 16],
    size: u64,
    role: SlabRole,
    slot: u64,
}

/// **Destroy what was there before writing what is.**
///
/// A fresh table is not a clean drive. Writing a GPT leaves everything the
/// old one described exactly where it was, and every one of those things is
/// read by something that scans rather than asks: an ext4 backup superblock,
/// an LVM label, an mdraid superblock at the end of the device, a stale
/// *backup* GPT that survives because the old table used a different LBA
/// size and its backup header sits at a different offset than ours. Then udev
/// names a drive that is ours after a filesystem that is not, mdadm assembles
/// an array out of a slab, and a rescue tool offers to "repair" the
/// partition table by restoring the one we replaced.
///
/// The two ends, because that is where signatures live: the front holds the
/// MBR, the primary GPT and the start of the first filesystem, and the tail
/// holds the backup GPT and the superblocks that are written from the end.
/// Bounded by the device, so a small test image is not asked for more than it
/// has, and block-aligned: a write that starts or ends off-block is EINVAL on
/// anything opened O_DIRECT. The whole drive is discarded first (#401): a
/// drive-managed SMR disk then knows every zone is empty.
async fn wipe_drive(device: &Arc<dyn BlockDevice>, capacity: u64) -> Result<()> {
    let bs = device.block_size().max(1) as u64;
    discard_quietly(device, 0, capacity / bs * bs, "the drive").await;
    let wipe = (capacity / 16).min(8 * ALIGN) / bs * bs;
    if wipe > 0 {
        let zeros = vec![0u8; wipe as usize];
        device
            .write(0, &zeros)
            .await
            .map_err(|e| ImageError::Other(format!("clearing the front of the drive: {e}")))?;
        let tail = (capacity - wipe) / bs * bs;
        device
            .write(tail, &zeros)
            .await
            .map_err(|e| ImageError::Other(format!("clearing the tail of the drive: {e}")))?;
        device
            .flush()
            .await
            .map_err(|e| ImageError::Other(format!("flushing the wipe: {e}")))?;
    }
    Ok(())
}

/// A fresh GPT in `lba`-byte blocks with `boot_bytes` left free at the front
/// (the boot area, #123) and `parts` after it, first-fit in order, each
/// formatted as a slab with its own metadata region. The last partition, when
/// it is a data slab, reserves room to grow in place to four times its size
/// (its table and record both; 0.024 % of it): only the last partition can
/// take space the drive gains.
async fn lay_table(
    device: &Arc<dyn BlockDevice>,
    lba: u32,
    boot_bytes: u64,
    parts: Vec<Part>,
    tier: StorageTier,
) -> Result<Vec<Slab>> {
    let mut gpt = Gpt::create_with_lba(device, lba);
    gpt.write(device).await.map_err(|e| ImageError::Other(format!("gpt: {e}")))?;
    // The boot area is the free space in front of the slabs, held by a
    // placeholder while they are allocated first-fit behind it and dropped
    // again before the table is final. Nothing is written into it here: the
    // ESP and the boot pallets arrive later, from the engine that outlives
    // this boot (see `image::local_boot`).
    let hold = if boot_bytes > 0 {
        Some(
            gpt.allocate("boot-area", type_guid::BASIC, boot_bytes, 0)
                .map_err(|e| ImageError::Other(format!("reserving the boot area: {e}")))?,
        )
    } else {
        None
    };
    let last = parts.len().saturating_sub(1);
    let mut slots = Vec::new();
    for (n, p) in parts.into_iter().enumerate() {
        let slot = gpt
            .allocate(p.name, p.guid, p.size, 0)
            .map_err(|e| ImageError::Other(format!("allocating {}: {e}", p.name)))?;
        slots.push((p, slot, n == last));
    }
    if let Some(i) = hold {
        gpt.remove(i).map_err(|e| ImageError::Other(format!("gpt: {e}")))?;
    }
    gpt.write(device).await.map_err(|e| ImageError::Other(format!("gpt: {e}")))?;

    let mut out = Vec::new();
    for (p, slot, is_last) in slots {
        let start = gpt.entries[slot].start_bytes(lba);
        let len = gpt.entries[slot].size_bytes(lba);
        let part = Arc::new(
            PartitionDevice::new(device.clone(), start, len)
                .map_err(|e| ImageError::Other(format!("partition {}: {e}", p.name)))?,
        );
        // The slab keeps its own record of what is in it. A node reads that
        // at boot; there is no filesystem underneath it to keep one in, and a
        // slab that cannot say what it holds boots to "no volume metadata".
        let grow_to = if p.role == SlabRole::Data && is_last { len.saturating_mul(DATA_GROWTH) } else { 0 };
        let meta = auto_metadata_bytes(len.max(grow_to), p.slot);
        let slab = Slab::format_with(
            part,
            SlabFormat::new(p.slot, tier).with_metadata(meta).with_role(p.role).with_growth(grow_to),
        )
        .await
        .map_err(|e| ImageError::Other(format!("format {}: {e}", p.name)))?;
        out.push(slab);
    }
    Ok(out)
}

/// What [`lay_system_drive`] laid: a drive of its own for the system half
/// (#402).
pub struct SystemDrive {
    pub system: Slab,
    pub system_bytes: u64,
    pub lba: u32,
}

/// What [`lay_data_drive`] laid: the data half on a drive of its own (#402).
pub struct DataDrive {
    pub data: Slab,
    pub bulk: Option<Slab>,
    pub data_bytes: u64,
    pub bulk_bytes: u64,
    pub lba: u32,
}

/// What the system slab on a drive of its own leaves unused at the end: a
/// percent of the drive, at least 64 MiB (#402). Room for the table's
/// backup, and slack for a replacement drive that is a little smaller.
pub fn system_drive_margin(capacity: u64) -> u64 {
    (capacity / 100).max(64 * ALIGN) / ALIGN * ALIGN
}

/// The system slab a drive of its own takes (#402): the whole drive, less
/// the GPT at both ends, the boot area and [`system_drive_margin`].
pub fn system_drive_slab_bytes(capacity: u64, boot_bytes: u64) -> u64 {
    align_down(
        capacity
            .saturating_sub(GPT_OVERHEAD)
            .saturating_sub(align_down(boot_bytes, ALIGN))
            .saturating_sub(system_drive_margin(capacity)),
        ALIGN,
    )
}

/// Lay the system half on a drive of its own (#402, owner's A: the drive
/// stormdrive designates `system`, an SSD beside the HDD that keeps the
/// data): a boot area at the front for the ESP and the boot pallets, then
/// one system slab sized to the drive. **Destroys whatever is on it**; the
/// caller has decided the drive is the system drive.
pub async fn lay_system_drive(device: Arc<dyn BlockDevice>, opts: &LocalLayout) -> Result<SystemDrive> {
    let capacity = device.capacity_bytes();
    let lba = opts.lba.unwrap_or_else(|| device.block_size());
    let boot_bytes = align_down(opts.boot_bytes, ALIGN);
    let system_bytes = system_drive_slab_bytes(capacity, boot_bytes);
    if system_bytes < 64 * ALIGN {
        return Err(ImageError::Spec(format!(
            "a {capacity} byte drive leaves {system_bytes} bytes for a system slab"
        )));
    }
    wipe_drive(&device, capacity).await?;
    let mut out = lay_table(
        &device,
        lba,
        boot_bytes,
        vec![Part { name: "stormblock", guid: type_guid::SLAB, size: system_bytes, role: SlabRole::System, slot: opts.slot_size }],
        opts.tier,
    )
    .await?;
    Ok(SystemDrive { system: out.pop().expect("one slab"), system_bytes, lba })
}

/// Lay the data half on a drive of its own (#402): a data slab across the
/// whole drive (and the bulk slab beside it on a large one, #156), the last
/// one with room to grow. **Destroys whatever is on it**: only for a drive
/// that carries no data half (a data slab there is kept, never laid over).
pub async fn lay_data_drive(device: Arc<dyn BlockDevice>, opts: &LocalLayout) -> Result<DataDrive> {
    let capacity = device.capacity_bytes();
    let lba = opts.lba.unwrap_or_else(|| device.block_size());
    let half = align_down(capacity.saturating_sub(GPT_OVERHEAD), ALIGN);
    if half < 64 * ALIGN {
        return Err(ImageError::Spec(format!("a {capacity} byte drive is too small for a data slab")));
    }
    let (data_bytes, bulk_bytes) = if opts.bulk && half >= BULK_MIN_DATA_HALF {
        let data = align_down((half / 4).max(64 << 30), ALIGN);
        (data, half - data)
    } else {
        (half, 0)
    };
    wipe_drive(&device, capacity).await?;
    let mut parts =
        vec![Part { name: "stormblock-data", guid: type_guid::SLAB_DATA, size: data_bytes, role: SlabRole::Data, slot: opts.slot_size }];
    if bulk_bytes > 0 {
        parts.push(Part { name: BULK_PARTITION, guid: type_guid::SLAB_DATA, size: bulk_bytes, role: SlabRole::Data, slot: crate::volume::BULK_EXTENT });
    }
    let mut out = lay_table(&device, lba, 0, parts, opts.tier).await?;
    let bulk = if bulk_bytes > 0 { out.pop() } else { None };
    Ok(DataDrive { data: out.pop().expect("a data slab"), bulk, data_bytes, bulk_bytes, lba })
}

/// Format the system slab of a drive that already carries the system half
/// alone again (#402: an install over a split node): the boot area, its ESP
/// and boot pallets, are kept (the fallback a node boots when the new
/// release does not come up); the slab holds the last release's goldens,
/// which this install replaces.
pub async fn reformat_system_drive(device: Arc<dyn BlockDevice>, opts: &LocalLayout) -> Result<SystemDrive> {
    let gpt = Gpt::read(&device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let (data_i, system_i) = halves_on(&device).await;
    if data_i.is_some() {
        return Err(ImageError::Spec("this drive carries a data half: not a system drive of its own".into()));
    }
    let system_i = system_i.ok_or_else(|| ImageError::Spec("this drive carries no system half".into()))?;
    let lba = gpt.block_size;
    let e = &gpt.entries[system_i];
    let part = Arc::new(
        PartitionDevice::new(device.clone(), e.start_bytes(lba), e.size_bytes(lba))
            .map_err(|err| ImageError::Other(format!("the system partition: {err}")))?,
    );
    let system_bytes = part.capacity_bytes();
    let dev: Arc<dyn BlockDevice> = part.clone();
    discard_quietly(&dev, 0, system_bytes / lba as u64 * lba as u64, "the system partition").await;
    let meta = auto_metadata_bytes(system_bytes, opts.slot_size);
    let system = Slab::format_with(
        part,
        SlabFormat::new(opts.slot_size, opts.tier).with_metadata(meta).with_role(SlabRole::System),
    )
    .await
    .map_err(|e| ImageError::Other(format!("formatting the system slab: {e}")))?;
    Ok(SystemDrive { system, system_bytes, lba })
}

/// The bytes a system slab must hold so the next release can be staged
/// beside this one (#402, #122): a stage copies N+1's goldens under their own
/// ids, and a new build's are all new, so an update holds two system halves
/// at once until N+1 is activated. Twice the release's mapped bytes, and 5 %.
pub fn update_room_needed(release_bytes: u64) -> u64 {
    release_bytes.saturating_mul(2).saturating_add(release_bytes / 20)
}

/// What [`retire_system_half`] took off a drive.
#[derive(Debug, Default)]
pub struct Retired {
    /// The partitions removed, by name.
    pub removed: Vec<String>,
}

/// Retire the system half of a drive whose system half moves to a drive of
/// its own (#402, owner's A): the system slab (its header zeroed, so nothing
/// that scans finds a slab there) and everything in front of it, the boot
/// area's ESP and boot pallets. stormuefi scans every drive: a boot pallet
/// left here would still boot the old release. The data half, after it, is
/// not touched: same partitions, same offsets, the node's identity and data
/// where they were. The freed space stays unused.
pub async fn retire_system_half(device: &Arc<dyn BlockDevice>) -> Result<Retired> {
    let mut gpt = Gpt::read(device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let Some(sys) = gpt.partitions().find(|(_, e)| e.type_guid == type_guid::SLAB).map(|(i, _)| i) else {
        return Ok(Retired::default());
    };
    let bs = gpt.block_size;
    let sys_first = gpt.entries[sys].first_lba;
    let first_data = gpt
        .partitions()
        .filter(|(_, e)| e.type_guid == type_guid::SLAB_DATA)
        .map(|(_, e)| e.first_lba)
        .min()
        .unwrap_or(u64::MAX);
    if first_data < sys_first {
        return Err(ImageError::Spec(
            "the data half is in front of the system half on this drive: not a layout this retires".into(),
        ));
    }
    let doomed: Vec<usize> = gpt
        .partitions()
        .filter(|(i, e)| {
            *i == sys
                || (e.last_lba < sys_first
                    && (e.type_guid == type_guid::ESP || e.type_guid == crate::pallet::PALLET_TYPE_GUID))
        })
        .map(|(i, _)| i)
        .collect();
    let mut out = Retired::default();
    for i in &doomed {
        let e = &gpt.entries[*i];
        let (start, len) = (e.start_bytes(bs), e.size_bytes(bs));
        // The first megabytes zeroed: a slab's magic and header, a pallet's
        // superblock, a FAT's boot sector. A scan finds nothing there.
        let z = len.min(8 * ALIGN) / bs as u64 * bs as u64;
        if z > 0 {
            device
                .write(start, &vec![0u8; z as usize])
                .await
                .map_err(|err| ImageError::Other(format!("clearing {}: {err}", e.name)))?;
        }
        out.removed.push(e.name.clone());
    }
    device
        .flush()
        .await
        .map_err(|e| ImageError::Other(format!("flushing the cleared system half: {e}")))?;
    // Removed from the table last: a cut before this leaves partitions that
    // hold nothing, which the next install retires again.
    let mut idx = doomed.clone();
    idx.sort_unstable_by(|a, b| b.cmp(a));
    for i in idx {
        gpt.remove(i).map_err(|e| ImageError::Other(format!("gpt: {e}")))?;
    }
    gpt.write(device)
        .await
        .map_err(|e| ImageError::Other(format!("writing the table without the system half: {e}")))?;
    Ok(out)
}

/// The drive's data partition and its system partition, each when it has
/// one (#402: a drive may carry one half).
pub async fn halves_on(device: &Arc<dyn BlockDevice>) -> (Option<usize>, Option<usize>) {
    let Ok(gpt) = Gpt::read(device).await else { return (None, None) };
    let mut data = None;
    let mut system = None;
    for (i, e) in gpt.partitions() {
        if e.type_guid == type_guid::SLAB_DATA && data.is_none() && e.name != BULK_PARTITION {
            data = Some(i);
        } else if e.type_guid == type_guid::SLAB && system.is_none() {
            system = Some(i);
        }
    }
    (data, system)
}

/// Write a GPT with a data slab and a system slab, and format both.
///
/// **This destroys whatever is on the device**, and deliberately more than the
/// table: the first and last few megabytes are zeroed first, so the drive
/// stops being what it was rather than merely stopping being described that
/// way. The decision that it may be
/// destroyed is the caller's and is the hard part; see `data_slab_on` in the
/// boot path, which refuses a drive already carrying a data slab because that
/// partition holds the node's CA key and its ServiceAccount signing key, and
/// nothing can mint those again.
///
/// **The system slab is first and the data slab is last.** The data half is
/// the one that grows — it holds PVCs, VM disks and images — and the last
/// partition on a drive is the only one that can grow into space the drive
/// gains (a bigger virtual disk, a grown array), because that space always
/// appears at the end. So the data slab is laid last, takes the rest of the
/// drive, and is formatted with room in its slot table to grow in place; see
/// [`grow_data_half`].
///
/// This was the other way round, data first "so a system slab that gets
/// larger cannot move the partition holding the node's identity". That
/// protected the half with a fixed size from the half that never moves, and
/// left the one that fills up boxed in at the front. The system half does not
/// grow in place at all: every install formats it afresh at its fixed size.
pub async fn lay_node_slabs(
    device: Arc<dyn BlockDevice>,
    opts: &LocalLayout,
) -> Result<LocalSlabs> {
    let capacity = device.capacity_bytes();
    let lba = opts.lba.unwrap_or_else(|| device.block_size());

    let system_bytes = align_down(opts.system_bytes, ALIGN);
    if system_bytes < 64 * ALIGN {
        return Err(ImageError::Spec(format!(
            "a {system_bytes} byte system slab is not enough to hold the goldens"
        )));
    }
    // What is left after the GPT at both ends and the system slab, rounded
    // down so the last partition never runs past the tail the table needs.
    let boot_bytes = align_down(opts.boot_bytes, ALIGN);
    let data_bytes = align_down(
        capacity
            .saturating_sub(GPT_OVERHEAD)
            .saturating_sub(boot_bytes)
            .saturating_sub(system_bytes),
        ALIGN,
    );
    if data_bytes < 64 * ALIGN {
        return Err(ImageError::Spec(format!(
            "a {capacity} byte drive leaves {data_bytes} for the data slab after a \
             {system_bytes} system slab"
        )));
    }
    // The data half as a 1 MiB data slab and an 8 MiB bulk slab (#156).
    let (data_bytes, bulk_bytes) = if opts.bulk && data_bytes >= BULK_MIN_DATA_HALF {
        let data = align_down((data_bytes / 4).max(64 << 30), ALIGN);
        (data, data_bytes - data)
    } else {
        (data_bytes, 0)
    };

    wipe_drive(&device, capacity).await?;
    let mut parts = vec![
        Part { name: "stormblock", guid: type_guid::SLAB, size: system_bytes, role: SlabRole::System, slot: opts.slot_size },
        Part { name: "stormblock-data", guid: type_guid::SLAB_DATA, size: data_bytes, role: SlabRole::Data, slot: opts.slot_size },
    ];
    if bulk_bytes > 0 {
        parts.push(Part { name: BULK_PARTITION, guid: type_guid::SLAB_DATA, size: bulk_bytes, role: SlabRole::Data, slot: crate::volume::BULK_EXTENT });
    }
    let mut out = lay_table(&device, lba, boot_bytes, parts, opts.tier).await?;

    let bulk = if bulk_bytes > 0 { out.pop() } else { None };
    let data = out.pop().expect("two slabs");
    let system = out.pop().expect("two slabs");
    Ok(LocalSlabs { data, system, bulk, data_bytes, system_bytes, bulk_bytes, lba })
}

/// Grow the data half into whatever the drive has after it.
///
/// The data slab is the last partition (see [`lay_node_slabs`]), so space the
/// drive gains — a bigger virtual disk, a grown array — lands right behind it.
/// This extends the partition to the end of the drive, rewrites both copies of
/// the table, and grows the slab into the new length. Nothing moves: the slab
/// was formatted with slot-table room to grow, and a grow is a header write.
///
/// The order is what makes an interruption harmless. The table is written
/// first, so the worst a crash leaves is a partition longer than its slab,
/// which the next call grows into. The other order would leave a slab claiming
/// slots past the end of its partition.
///
/// Returns the slot count before and after, or `None` when there is nothing to
/// do: no node layout, a data partition that is not last, or no space after it.
pub async fn grow_data_half(device: Arc<dyn BlockDevice>) -> Result<Option<(u64, u64)>> {
    // The data partition, on a drive with both halves or the data half
    // alone (#402).
    let Some(data_i) = halves_on(&device).await.0 else { return Ok(None) };
    // The bulk slab, when there is one, is the last (#156).
    let data_i = bulk_partition(&device).await.unwrap_or(data_i);
    let mut gpt = Gpt::read(&device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let bs = gpt.block_size;
    let data_last = gpt.entries[data_i].last_lba;
    // Only the last partition can take space at the end of the drive.
    if gpt.partitions().any(|(i, e)| i != data_i && e.last_lba > data_last) {
        return Ok(None);
    }

    gpt.extend_to_device();
    // Whole megabytes, like everything `lay_node_slabs` lays.
    let per = (ALIGN / bs as u64).max(1);
    let new_last = ((gpt.last_usable_lba + 1) / per * per).saturating_sub(1);
    if new_last <= data_last {
        return Ok(None);
    }
    gpt.entries[data_i].last_lba = new_last;
    gpt.write(&device)
        .await
        .map_err(|e| ImageError::Other(format!("writing the extended table: {e}")))?;

    let e = &gpt.entries[data_i];
    let part = PartitionDevice::new(device.clone(), e.start_bytes(bs), e.size_bytes(bs))
        .map_err(|err| ImageError::Other(format!("the extended data partition: {err}")))?;
    let mut slab = Slab::open(Arc::new(part))
        .await
        .map_err(|err| ImageError::Other(format!("opening the data slab to grow it: {err}")))?;
    if !slab.is_data() {
        return Err(ImageError::Other(
            "the partition typed as a data slab says it is a system slab; not growing it".into(),
        ));
    }
    let before = slab.total_slots();
    let added = slab
        .grow()
        .await
        .map_err(|err| ImageError::Other(format!("growing the data slab: {err}")))?;
    Ok(Some((before, before + added)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use std::io::{Read, Seek, SeekFrom};

    /// Enough drive for both halves: `for_drive` gives the data slab half of a
    /// small disk, and the system slab has a floor of its own.
    const CAP: u64 = 256 * 1024 * 1024;

    /// #402: a system drive of its own is a boot area and one system slab,
    /// the drive less the GPT and the margin; a data drive of its own is the
    /// data half alone. Each is recognised as the half it carries.
    #[tokio::test]
    async fn a_system_drive_and_a_data_drive_each_carry_one_half() {
        let ssd = crate::drive::open_path(&format!("emulated://ssd-{}?size=512G", uuid::Uuid::new_v4().simple()), false)
            .await
            .unwrap();
        let mut layout = LocalLayout::for_drive(ssd.capacity_bytes());
        layout.slot_size = 1 << 20;
        let laid = lay_system_drive(ssd.clone(), &layout).await.unwrap();
        let cap = ssd.capacity_bytes();
        assert_eq!(laid.system_bytes, system_drive_slab_bytes(cap, layout.boot_bytes));
        assert!(laid.system_bytes > cap - cap / 50, "the drive, less a few percent: {} of {cap}", laid.system_bytes);
        assert!(!laid.system.is_data());
        let (d0, s0) = halves_on(&ssd).await;
        assert!(d0.is_none() && s0.is_some(), "the system half alone");
        assert!(has_boot_area(&ssd, layout.boot_bytes).await, "the boot area in front of the slab");
        assert!(node_layout(&ssd).await.unwrap().is_none(), "one half is not a node layout");
        assert_eq!(system_slab_volumes(&ssd).await.unwrap().map(|v| v.len()), Some(0), "it can say it holds nothing");

        let hdd = crate::drive::open_path(&format!("emulated://hdd-{}?size=2T", uuid::Uuid::new_v4().simple()), false)
            .await
            .unwrap();
        let mut dl = LocalLayout::for_drive(hdd.capacity_bytes());
        dl.slot_size = 1 << 20;
        dl.bulk = true;
        let d = lay_data_drive(hdd.clone(), &dl).await.unwrap();
        assert!(d.data.is_data());
        assert!(d.bulk.is_some(), "a 2 TB data drive gets its bulk slab");
        assert_eq!(halves_on(&hdd).await.1, None);
        assert!(halves_on(&hdd).await.0.is_some());
        assert_eq!(data_half_present(&hdd).await.unwrap(), Some(true));
        assert_eq!(data_half_present(&ssd).await.unwrap(), None);
    }

    /// #402: retiring the system half of a node's drive takes the system
    /// slab and the boot area's ESP and pallets off it, and leaves the data
    /// half exactly where it was, with the same slab.
    #[tokio::test]
    async fn retiring_the_system_half_keeps_the_data_half_where_it_was() {
        let dev = crate::drive::open_path(&format!("emulated://both-{}?size=64G", uuid::Uuid::new_v4().simple()), false)
            .await
            .unwrap();
        let mut layout = LocalLayout::for_drive(dev.capacity_bytes());
        layout.slot_size = 1 << 20;
        layout.bulk = false;
        let laid = lay_node_slabs(dev.clone(), &layout).await.unwrap();
        let data_id = laid.data.slab_id();
        // What local boot puts in the boot area: an ESP and a boot pallet.
        let mut gpt = Gpt::read(&dev).await.unwrap();
        gpt.allocate("esp", type_guid::ESP, 64 << 20, 0).unwrap();
        gpt.allocate("boot-1", crate::pallet::PALLET_TYPE_GUID, 256 << 20, 0).unwrap();
        gpt.write(&dev).await.unwrap();
        let (data_i, sys_i) = node_layout(&dev).await.unwrap().unwrap();
        let before = Gpt::read(&dev).await.unwrap();
        let data_at = (before.entries[data_i].first_lba, before.entries[data_i].last_lba);
        let sys_at = before.entries[sys_i].start_bytes(before.block_size);

        let r = retire_system_half(&dev).await.unwrap();
        let mut removed = r.removed.clone();
        removed.sort();
        assert_eq!(removed, vec!["boot-1".to_string(), "esp".to_string(), "stormblock".to_string()]);
        let after = Gpt::read(&dev).await.unwrap();
        assert!(after.partitions().all(|(_, e)| e.type_guid != type_guid::SLAB && e.type_guid != type_guid::ESP
            && e.type_guid != crate::pallet::PALLET_TYPE_GUID));
        let d = halves_on(&dev).await.0.expect("the data partition is still there");
        assert_eq!((after.entries[d].first_lba, after.entries[d].last_lba), data_at, "same place");
        assert!(!slab_magic_at(&dev, sys_at).await.unwrap(), "no slab left where the system slab was");
        let e = &after.entries[d];
        let part = PartitionDevice::new(dev.clone(), e.start_bytes(after.block_size), e.size_bytes(after.block_size)).unwrap();
        let slab = Slab::open(Arc::new(part)).await.unwrap();
        assert_eq!(slab.slab_id(), data_id, "the same data slab");
        assert!(retire_system_half(&dev).await.unwrap().removed.is_empty(), "nothing left to retire");
    }

    /// #156: a data half of 256 GiB or more is a 1 MiB data slab and an 8 MiB
    /// bulk slab, the bulk one last; a smaller one is one data slab; a
    /// reinstall of the system half keeps both.
    #[tokio::test]
    async fn a_large_drive_gets_a_bulk_slab_last() {
        let uri = format!("emulated://bulk-{}?size=600G", uuid::Uuid::new_v4().simple());
        let dev = crate::drive::open_path(&uri, false).await.unwrap();
        let mut layout = LocalLayout::for_drive(dev.capacity_bytes());
        layout.bulk = true;
        layout.slot_size = 1 << 20;
        let laid = lay_node_slabs(dev.clone(), &layout).await.unwrap();
        let bulk = laid.bulk.as_ref().expect("a bulk slab");
        assert_eq!(bulk.slot_size(), crate::volume::BULK_EXTENT);
        assert!(bulk.is_data());
        assert_eq!(laid.data.slot_size(), 1 << 20);
        assert!(laid.data_bytes >= 64 << 30, "{}", laid.data_bytes);
        assert!(laid.bulk_bytes > 2 * laid.data_bytes, "{} {}", laid.bulk_bytes, laid.data_bytes);
        let bulk_id = bulk.slab_id();
        let data_id = laid.data.slab_id();

        let (d, _) = node_layout(&dev).await.unwrap().expect("a node layout");
        let b = bulk_partition(&dev).await.expect("a bulk partition");
        let gpt = Gpt::read(&dev).await.unwrap();
        assert_eq!(gpt.entries[d].name, "stormblock-data");
        assert!(gpt.partitions().all(|(i, e)| i == b || e.last_lba < gpt.entries[b].first_lba), "bulk is last");
        drop(laid);

        // The system half replaced; both data pools kept.
        let kept = update_system_slab(dev.clone(), &layout).await.unwrap();
        assert_eq!(kept.data.slab_id(), data_id);
        assert_eq!(kept.bulk.as_ref().map(|b| b.slab_id()), Some(bulk_id));

        // A smaller data half: one data slab.
        let uri = format!("emulated://nobulk-{}?size=200G", uuid::Uuid::new_v4().simple());
        let small = crate::drive::open_path(&uri, false).await.unwrap();
        let mut layout = LocalLayout::for_drive(small.capacity_bytes());
        layout.bulk = true;
        layout.slot_size = 1 << 20;
        let laid = lay_node_slabs(small.clone(), &layout).await.unwrap();
        assert!(laid.bulk.is_none());
        assert!(bulk_partition(&small).await.is_none());
    }

    fn window(path: &str, at: u64, len: usize) -> Vec<u8> {
        let mut f = std::fs::File::open(path).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).unwrap();
        buf
    }

    /// A drive that is taken over stops being what it was.
    ///
    /// Writing a fresh GPT over an old one leaves everything the old table
    /// described exactly where it was — an ext4 backup superblock, an LVM
    /// label, an mdraid superblock at the tail, a stale backup GPT whose
    /// header sits at a different offset because the old table used a
    /// different LBA size. Each of those is read by something that scans
    /// rather than asks, and a node that boots this image is being installed:
    /// what its drive carried is garbage, and garbage that is still readable
    /// is garbage something will read.
    #[tokio::test]
    async fn taking_a_drive_destroys_what_it_carried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.disk").to_string_lossy().to_string();

        // Somebody else's life on it, at both ends — which is where every
        // scanner looks.
        //
        // A recognisable *string*, not a byte value: what replaces this is a
        // GPT, whose entries are GUIDs, and a single byte value turns up in
        // random data often enough to fail a test that is looking at the wrong
        // thing.
        const OLD_FRONT: &[u8] = b"OLD-FILESYSTEM-SUPERBLOCK";
        const OLD_TAIL: &[u8] = b"OLD-MDRAID-SUPERBLOCK-AT-THE-END";
        {
            let dev = FileDevice::open_with_capacity(&path, CAP).await.unwrap();
            let mut front = vec![0u8; 1024 * 1024];
            for chunk in front.chunks_mut(4096) {
                chunk[..OLD_FRONT.len()].copy_from_slice(OLD_FRONT);
            }
            let mut tail = vec![0u8; 1024 * 1024];
            for chunk in tail.chunks_mut(4096) {
                chunk[..OLD_TAIL.len()].copy_from_slice(OLD_TAIL);
            }
            dev.write(0, &front).await.unwrap();
            dev.write(CAP - 1024 * 1024, &tail).await.unwrap();
            dev.flush().await.unwrap();
        }

        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;
        let laid = lay_node_slabs(dev, &layout).await.unwrap();
        assert_eq!(laid.data.role(), SlabRole::Data);
        assert_eq!(laid.system.role(), SlabRole::System);

        let found = |hay: &[u8], needle: &[u8]| {
            hay.windows(needle.len()).any(|w| w == needle)
        };
        let front = window(&path, 0, 4 * 1024 * 1024);
        assert!(!found(&front, OLD_FRONT), "the front still carries what was there");
        // The tail is the half a fresh GPT alone would not have touched: our
        // backup header lands there only if the LBA size happens to match the
        // old table's.
        let tail = window(&path, CAP - 1024 * 1024, 1024 * 1024);
        assert!(!found(&tail, OLD_TAIL), "the tail still carries what was there");
    }

    /// A reinstall keeps what cannot be made again and replaces what this boot
    /// exists to replace.
    ///
    /// The node's identity lives in the data slab — its CA key, its
    /// ServiceAccount signing key — and the goldens live in the system slab.
    /// Laying a fresh table over a drive that is already ours destroys the
    /// first to refresh the second; refusing the drive leaves the node running
    /// from the appliance forever. Neither is an install.
    #[tokio::test]
    async fn a_reinstall_keeps_the_data_half_and_replaces_the_system_half() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.disk").to_string_lossy().to_string();

        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;

        // The install that came before.
        let (data_id, system_id, data_start) = {
            let dev: Arc<dyn BlockDevice> =
                Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
            let laid = lay_node_slabs(dev.clone(), &layout).await.unwrap();
            let gpt = Gpt::read(&dev).await.unwrap();
            let (d, _s) = node_layout(&dev).await.unwrap().unwrap();
            let start = gpt.entries[d].start_bytes(gpt.block_size);
            (laid.data.slab_id(), laid.system.slab_id(), start)
        };

        // This node's identity, written into the data half the way anything
        // else would: through the slab, at a slot it owns.
        const IDENTITY: &[u8] = b"THIS-NODE-CA-KEY";
        {
            let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
            let gpt = Gpt::read(&dev).await.unwrap();
            let (d, _) = node_layout(&dev).await.unwrap().unwrap();
            let e = &gpt.entries[d];
            let part: Arc<dyn BlockDevice> = Arc::new(
                PartitionDevice::new(dev, e.start_bytes(gpt.block_size), e.size_bytes(gpt.block_size))
                    .unwrap(),
            );
            let slab = Slab::open(part.clone()).await.unwrap();
            let at = slab.data_offset();
            let mut buf = vec![0u8; 4096];
            buf[..IDENTITY.len()].copy_from_slice(IDENTITY);
            part.write(at, &buf).await.unwrap();
            part.flush().await.unwrap();
        }

        // The reinstall.
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        assert!(node_layout(&dev).await.unwrap().is_some(), "the drive is ours");
        let updated = update_system_slab(dev.clone(), &layout).await.unwrap();

        // The data half is the same slab, in the same place, with what was
        // written to it still there.
        assert_eq!(updated.data.slab_id(), data_id, "the data slab was replaced");
        assert!(updated.data.is_data());
        let gpt = Gpt::read(&dev).await.unwrap();
        let (d, _) = node_layout(&dev).await.unwrap().unwrap();
        assert_eq!(
            gpt.entries[d].start_bytes(gpt.block_size),
            data_start,
            "the data partition moved"
        );
        let keep = window(&path, data_start + updated.data.data_offset(), 4096);
        assert!(
            keep.windows(IDENTITY.len()).any(|w| w == IDENTITY),
            "the node's identity did not survive its own reinstall"
        );

        // The system half is a new slab: this boot is the next install.
        assert_ne!(updated.system.slab_id(), system_id, "the system slab was kept");
        assert_eq!(updated.system.role(), SlabRole::System);
        assert_eq!(updated.system.free_slots(), updated.system.total_slots());
    }

    /// "Already up to date" has to be answerable from the drive alone, before
    /// anything is attached — otherwise the only way to find out whether a
    /// copy is needed is to do it.
    #[tokio::test]
    async fn the_system_half_says_which_volumes_it_holds() {
        use crate::raid::RaidArrayId;
        use crate::volume::VolumeManager;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.disk").to_string_lossy().to_string();
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;

        // A drive that is nobody's cannot answer.
        {
            let dev: Arc<dyn BlockDevice> =
                Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
            assert!(system_slab_volumes(&dev).await.unwrap().is_none());
        }

        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        let laid = lay_node_slabs(dev.clone(), &layout).await.unwrap();
        // Freshly laid and empty: it can answer, and the answer is nothing.
        let held = system_slab_volumes(&dev).await.unwrap();
        assert_eq!(held, Some(std::collections::HashSet::new()));

        // The goldens arrive.
        let system_id = laid.system.slab_id();
        let wanted = {
            let mut mgr = VolumeManager::new(layout.slot_size);
            mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), laid.system).await.unwrap();
            mgr.persist_to_slab(system_id);
            let a = mgr.create_volume_any("stormpump", 4 * 1024 * 1024).await.unwrap();
            let b = mgr.create_volume_any("stormcos-0.1.0", 4 * 1024 * 1024).await.unwrap();
            mgr.persist().await;
            std::collections::HashSet::from([a.0, b.0])
        };

        let held = system_slab_volumes(&dev).await.unwrap().unwrap();
        assert_eq!(held, wanted, "the system half must name what it holds");
    }

    /// An install is a release the local drive does not hold; a reboot is one
    /// it does (#236). Asked of two drives, before anything is attached.
    #[tokio::test]
    async fn a_release_is_held_only_when_every_golden_is() {
        use crate::raid::RaidArrayId;
        use crate::volume::VolumeManager;

        let dir = tempfile::tempdir().unwrap();
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;

        // A drive laid out and given some volumes; the flag says which are goldens.
        async fn drive(
            path: &str,
            layout: &LocalLayout,
            vols: &[(&str, bool)],
        ) -> (Arc<dyn BlockDevice>, Vec<uuid::Uuid>) {
            let dev: Arc<dyn BlockDevice> =
                Arc::new(FileDevice::open_with_capacity(path, CAP).await.unwrap());
            let laid = lay_node_slabs(dev.clone(), layout).await.unwrap();
            let system_id = laid.system.slab_id();
            let mut mgr = VolumeManager::new(layout.slot_size);
            mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), laid.system).await.unwrap();
            mgr.persist_to_slab(system_id);
            let mut ids = Vec::new();
            for (name, seal) in vols {
                let id = mgr.create_volume_any(name, 4 * 1024 * 1024).await.unwrap();
                if *seal {
                    mgr.seal_volume(id, None).await.unwrap();
                }
                ids.push(id.0);
            }
            mgr.persist().await;
            (dev, ids)
        }

        let p = |n: &str| dir.path().join(n).to_string_lossy().to_string();
        let (image, image_ids) =
            drive(&p("image"), &layout, &[("stormpump", true), ("etc", true), ("scratch", false)])
                .await;

        // A drive that is nobody's cannot say.
        let blank: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&p("blank"), CAP).await.unwrap());
        assert!(matches!(release_held(&blank, &image).await, ReleaseHeld::CannotSay(_)));

        // Another release's goldens: not held, and every golden is missing.
        let (other, _) = drive(&p("other"), &layout, &[("stormpump", true)]).await;
        assert_eq!(release_held(&other, &image).await, ReleaseHeld::NotHeld { goldens: 2, missing: 2 });

        // The same release: the image read against itself holds everything,
        // and its unsealed volume is not a golden, so it is not asked for.
        assert_eq!(release_held(&image, &image).await, ReleaseHeld::Held { goldens: 2 });

        // Every golden recorded, but an extent still on a slab that is not
        // on the drive (a flow-over that never finished, #239): not held.
        let (half, _) = drive(&p("half"), &layout, &[("stormpump", true), ("etc", true)]).await;
        {
            let away = Arc::new(FileDevice::open_with_capacity(&p("away"), 64 * 1024 * 1024).await.unwrap())
                as Arc<dyn BlockDevice>;
            let away = crate::drive::slab::Slab::format(away, layout.slot_size, StorageTier::Hot).await.unwrap();
            let found = crate::drive::discover::slabs_in_partitions(&half).await;
            let mut mgr = VolumeManager::new(layout.slot_size);
            let sys = found.iter().find(|f| !f.slab.is_data()).map(|f| f.slab.slab_id()).unwrap();
            mgr.adopt_slabs(found).await.unwrap();
            mgr.persist_to_slab(sys);
            mgr.add_slab(away).await;
            // A volume written while the drive's own slab is quarantined
            // lands on the other one, as an extent not yet moved would be.
            let scratch = mgr.create_volume_any("scratch", 4 * 1024 * 1024).await.unwrap();
            {
                let mut reg = mgr.registry().write().await;
                reg.set_quarantined(sys, true);
            }
            mgr.get_volume(&scratch).unwrap().write(0, &[7u8; 4096]).await.unwrap();
            mgr.persist().await;
        }
        assert!(
            matches!(release_held(&half, &half).await, ReleaseHeld::Unfinished { slabs: 1, .. }),
            "{:?}",
            release_held(&half, &half).await
        );

        // An image with nothing sealed cannot name a release.
        let (unsealed, _) = drive(&p("unsealed"), &layout, &[("scratch", false)]).await;
        assert!(matches!(release_held(&image, &unsealed).await, ReleaseHeld::CannotSay(_)));
        assert_eq!(image_ids.len(), 3);
    }

    /// The wipe is bounded by the drive, so a small one is not asked for more
    /// than it has — and still comes out with both halves.
    #[tokio::test]
    async fn a_small_drive_is_laid_out_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.disk").to_string_lossy().to_string();
        let small = 160 * 1024 * 1024;
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, small).await.unwrap());
        let mut layout = LocalLayout::for_drive(small);
        layout.slot_size = 1024 * 1024;
        let laid = lay_node_slabs(dev, &layout).await.unwrap();
        assert!(laid.data_bytes > 0 && laid.system_bytes > 0);
    }

    /// A 2 TB drive gets a fixed system half and the rest is data.
    #[test]
    fn a_big_drive_is_mostly_data() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let two_tb = 2_000_398_934_016u64;
        let l = LocalLayout::for_drive(two_tb);
        assert_eq!(l.system_bytes, two_tb / 16);
        assert!(l.system_bytes <= 128 * GIB);
        assert_eq!(LocalLayout::for_drive(100 * GIB).system_bytes, 32 * GIB);
        assert_eq!(LocalLayout::for_drive(40 * 1024 * GIB).system_bytes, 128 * GIB);
    }

    /// The data partition is the last one on the drive, so it is the one that
    /// can take space the drive gains.
    #[tokio::test]
    async fn the_data_half_is_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.disk").to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;
        lay_node_slabs(dev.clone(), &layout).await.unwrap();

        let gpt = Gpt::read(&dev).await.unwrap();
        let (d, s) = node_layout(&dev).await.unwrap().expect("a node layout");
        assert!(
            gpt.entries[d].first_lba > gpt.entries[s].last_lba,
            "the data partition must come after the system partition"
        );
        assert!(
            gpt.partitions().all(|(_, e)| e.last_lba <= gpt.entries[d].last_lba),
            "nothing may sit after the data partition"
        );
    }

    /// A drive that grows gives the data half its new space, in place: the
    /// partition is extended, the slab takes the new slots, and what was
    /// written stays where it was, owned by what owned it.
    #[tokio::test]
    async fn the_data_half_grows_into_a_grown_drive() {
        use crate::volume::extent::VolumeId;
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow.disk").to_string_lossy().to_string();

        let (vol, slot, before_slots) = {
            let dev: Arc<dyn BlockDevice> =
                Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
            let mut layout = LocalLayout::for_drive(CAP);
            layout.slot_size = MIB;
            let mut laid = lay_node_slabs(dev.clone(), &layout).await.unwrap();
            assert!(laid.data.table_capacity() >= 4 * laid.data.total_slots() - 4);
            let vol = VolumeId(uuid::Uuid::new_v4());
            let slot = laid.data.allocate(vol, 7).await.unwrap();
            laid.data.write_slot(slot, 0, &[0x5A; 4096]).await.unwrap();
            dev.flush().await.unwrap();
            (vol, slot, laid.data.total_slots())
        };

        // Nothing to do on a drive that has not grown.
        {
            let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
            assert_eq!(grow_data_half(dev).await.unwrap(), None);
        }

        // The drive doubles, the way a virtual disk is grown: at the end.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(2 * CAP).unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        let (was, now) = grow_data_half(dev.clone()).await.unwrap().expect("it grew");
        assert_eq!(was, before_slots);
        assert!(now >= was + (CAP / MIB) - 2, "grew by about the added space: {was} -> {now}");

        // Reopened from scratch, it is the grown slab with the old data.
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        let gpt = Gpt::read(&dev).await.unwrap();
        assert!(!gpt.recovered_from_backup, "both copies of the table were rewritten");
        let (d, _) = node_layout(&dev).await.unwrap().expect("still a node layout");
        let e = &gpt.entries[d];
        let part = PartitionDevice::new(dev.clone(), e.start_bytes(gpt.block_size), e.size_bytes(gpt.block_size)).unwrap();
        let data = Slab::open(Arc::new(part)).await.unwrap();
        assert_eq!(data.total_slots(), now);
        assert!(data.is_data());
        assert_eq!(data.find_slot(vol, 7).await, Some(slot), "ownership survived the grow");
        let mut buf = vec![0u8; 4096];
        data.read_slot(slot, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x5A), "the data survived the grow");
        assert_eq!(data.free_slots(), now - 1);

        // And a second call finds nothing more to take.
        assert_eq!(grow_data_half(dev).await.unwrap(), None);
    }
    /// A table re-expressed in another LBA size describes the same bytes, and
    /// is found at the new size — which is the size firmware looks at.
    #[tokio::test]
    async fn a_table_is_re_expressed_without_moving_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retable.disk").to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;
        layout.lba = Some(4096);
        lay_node_slabs(dev.clone(), &layout).await.unwrap();
        let ranges = |g: &Gpt| -> Vec<(u64, u64, String)> {
            g.partitions()
                .map(|(_, e)| (e.start_bytes(g.block_size), e.size_bytes(g.block_size), e.name.clone()))
                .collect()
        };
        let before = Gpt::read(&dev).await.unwrap();
        assert_eq!(before.block_size, 4096);

        assert!(retable(&dev, 512).await.unwrap());
        let after = Gpt::read(&dev).await.unwrap();
        assert_eq!(after.block_size, 512);
        assert!(!after.recovered_from_backup, "both copies were written");
        assert_eq!(ranges(&after), ranges(&before));
        assert_eq!(after.disk_guid, before.disk_guid);
        // Nothing of the old table is left for a scanner to find: the 4096
        // header was at byte 4096, inside the new entry array.
        let old_hdr = window(&path, 4096, 8);
        assert_ne!(&old_hdr[..], b"EFI PART");

        assert!(!retable(&dev, 512).await.unwrap(), "already 512: nothing to do");
        assert!(retable(&dev, 4096).await.unwrap());
        assert_eq!(ranges(&Gpt::read(&dev).await.unwrap()), ranges(&before));
    }

    /// A disk laid before local boot existed — a 4096-byte table on a drive
    /// firmware reads at 512, and the system slab at the first megabyte — gets
    /// both put right by the next install, and the data half does not move.
    #[tokio::test]
    async fn an_old_layout_gains_a_boot_area_and_a_native_table() {
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.disk").to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
        let mut old = LocalLayout::for_drive(CAP);
        old.slot_size = MIB;
        old.boot_bytes = 0;
        let laid = lay_node_slabs(dev.clone(), &old).await.unwrap();
        let data_id = laid.data.slab_id();
        let (d, _) = node_layout(&dev).await.unwrap().unwrap();
        let g = Gpt::read(&dev).await.unwrap();
        let data_range = (g.entries[d].start_bytes(g.block_size), g.entries[d].size_bytes(g.block_size));

        let mut new = old;
        new.lba = Some(512);
        new.boot_bytes = 32 * MIB;
        assert!(!boot_ready(&dev, &new).await, "a 4096 table and no boot area");

        let updated = update_system_slab(dev.clone(), &new).await.unwrap();
        assert_eq!(updated.data.slab_id(), data_id, "the data slab is the same slab");
        assert!(boot_ready(&dev, &new).await);
        let g = Gpt::read(&dev).await.unwrap();
        assert_eq!(g.block_size, 512);
        let (d, s) = node_layout(&dev).await.unwrap().unwrap();
        assert_eq!(
            (g.entries[d].start_bytes(512), g.entries[d].size_bytes(512)),
            data_range,
            "the data partition moved"
        );
        assert!(g.entries[s].start_bytes(512) >= new.boot_bytes);
    }
}

