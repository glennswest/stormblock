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
}

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
    let Ok(Some((_, s))) = node_layout(device).await else { return false };
    let bs = gpt.block_size as u64;
    let front = gpt.entries[s].first_lba.saturating_sub(gpt.first_usable_lba) * bs;
    // The first megabyte is alignment the table takes anyway.
    front + ALIGN >= boot_bytes
}

/// What was laid down.
pub struct LocalSlabs {
    pub data: Slab,
    pub system: Slab,
    pub data_bytes: u64,
    pub system_bytes: u64,
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
        if e.type_guid == type_guid::SLAB_DATA && data.is_none() {
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
    let Some((_, system_i)) = node_layout(device).await? else { return Ok(None) };
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
    let bytes = match slab.read_metadata().await {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(Some(std::collections::HashSet::new())),
        Err(_) => return Ok(None),
    };
    let Ok(meta) = crate::volume::MetadataStore::decode(&bytes) else { return Ok(None) };
    Ok(Some(meta.volumes.into_iter().map(|v| v.id.0).collect()))
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
    let bytes = match slab.read_metadata().await {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(Some(std::collections::HashSet::new())),
        Err(_) => return Ok(None),
    };
    let Ok(meta) = crate::volume::MetadataStore::decode(&bytes) else { return Ok(None) };
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
    let meta = auto_metadata_bytes(system_bytes, opts.slot_size);
    let system = Slab::format_with(
        system_part,
        SlabFormat::new(opts.slot_size, opts.tier)
            .with_metadata(meta)
            .with_role(SlabRole::System),
    )
    .await
    .map_err(|e| ImageError::Other(format!("formatting the system slab: {e}")))?;

    Ok(LocalSlabs { data, system, data_bytes, system_bytes, lba })
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

    // **Destroy what was there before writing what is.**
    //
    // A fresh table is not a clean drive. Writing a GPT leaves everything the
    // old one described exactly where it was, and every one of those things
    // is read by something that scans rather than asks: an ext4 backup
    // superblock, an LVM label, an mdraid superblock at the end of the device,
    // a stale *backup* GPT that survives because the old table used a
    // different LBA size and its backup header sits at a different offset than
    // ours. Then udev names a drive that is ours after a filesystem that is
    // not, mdadm assembles an array out of a slab, and a rescue tool offers to
    // "repair" the partition table by restoring the one we replaced.
    //
    // A node boots this image to be installed, so the drive is the node's and
    // what it carried is garbage. Garbage that is still readable is garbage
    // something will read.
    //
    // The two ends, because that is where signatures live: the front holds the
    // MBR, the primary GPT and the start of the first filesystem, and the tail
    // holds the backup GPT and the superblocks that are written from the end.
    // Bounded by the device, so a small test image is not asked for more than
    // it has.
    // Block-aligned in length and in offset: a write that starts or ends
    // off-block is EINVAL on anything opened O_DIRECT, and the tail of a drive
    // is not a round number of megabytes.
    let bs = device.block_size().max(1) as u64;
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

    let mut gpt = Gpt::create_with_lba(&device, lba);
    gpt.write(&device).await.map_err(|e| ImageError::Other(format!("gpt: {e}")))?;

    // The boot area is the free space in front of the system partition, held
    // by a placeholder while the two slabs are allocated first-fit behind it
    // and dropped again before the table is final. Nothing is written into
    // it here: the ESP and the boot pallets arrive later, from the engine
    // that outlives this boot (see `image::local_boot`).
    let hold = if boot_bytes > 0 {
        Some(
            gpt.allocate("boot-area", type_guid::BASIC, boot_bytes, 0)
                .map_err(|e| ImageError::Other(format!("reserving the boot area: {e}")))?,
        )
    } else {
        None
    };

    // Both allocated before the table is written, so the placeholder never
    // reaches the disk: a table that still carried it would read as a drive
    // whose boot area is taken.
    let mut slots = Vec::new();
    for (name, guid, size, role) in [
        ("stormblock", type_guid::SLAB, system_bytes, SlabRole::System),
        ("stormblock-data", type_guid::SLAB_DATA, data_bytes, SlabRole::Data),
    ] {
        let slot = gpt
            .allocate(name, guid, size, 0)
            .map_err(|e| ImageError::Other(format!("allocating {name}: {e}")))?;
        slots.push((name, slot, role));
    }
    if let Some(i) = hold {
        gpt.remove(i).map_err(|e| ImageError::Other(format!("gpt: {e}")))?;
    }
    gpt.write(&device).await.map_err(|e| ImageError::Other(format!("gpt: {e}")))?;

    let mut out = Vec::new();
    for (name, slot, role) in slots {
        let start = gpt.entries[slot].start_bytes(lba);
        let len = gpt.entries[slot].size_bytes(lba);
        let part = Arc::new(
            PartitionDevice::new(device.clone(), start, len)
                .map_err(|e| ImageError::Other(format!("partition {name}: {e}")))?,
        );
        // The slab keeps its own record of what is in it. A node reads that
        // at boot; there is no filesystem underneath it to keep one in, and a
        // slab that cannot say what it holds boots to "no volume metadata".
        // The data slab reserves room to grow in place to four times its
        // size — its table and its record both — which costs 0.024% of it.
        let grow_to = if role == SlabRole::Data { len.saturating_mul(DATA_GROWTH) } else { 0 };
        let meta = auto_metadata_bytes(len.max(grow_to), opts.slot_size);
        let slab = Slab::format_with(
            part,
            SlabFormat::new(opts.slot_size, opts.tier)
                .with_metadata(meta)
                .with_role(role)
                .with_growth(grow_to),
        )
        .await
        .map_err(|e| ImageError::Other(format!("format {name}: {e}")))?;
        out.push(slab);
    }

    let data = out.pop().expect("two slabs");
    let system = out.pop().expect("two slabs");
    Ok(LocalSlabs { data, system, data_bytes, system_bytes, lba })
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
    let Some((data_i, _)) = node_layout(&device).await? else { return Ok(None) };
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
        assert_eq!(data.find_slot(vol, 7), Some(slot), "ownership survived the grow");
        let mut buf = vec![0u8; 4096];
        data.read_slot(slot, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x5A), "the data survived the grow");
        assert_eq!(data.free_slots(), now - 1);

        // And a second call finds nothing more to take.
        assert_eq!(grow_data_half(dev).await.unwrap(), None);
    }
}
