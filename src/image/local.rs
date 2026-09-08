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
    /// Bytes for the data slab. The system slab takes what is left.
    ///
    /// Sized here rather than in an image, which is the point: 5G and 10G PVC
    /// classes never fit an 8 GiB partition chosen by a build that had not
    /// seen the drive (stormcos#36).
    pub data_bytes: u64,
    pub slot_size: u64,
    pub tier: StorageTier,
    /// GPT block size. `None` follows the device, which is what firmware and
    /// the kernel both read it in.
    pub lba: Option<u32>,
}

impl LocalLayout {
    /// A tenth of the drive for data, floored at 8 GiB and capped at 64.
    ///
    /// A guess, and a deliberately dull one: the data slab holds identity,
    /// logs and claims, which grow with what the node is asked to run rather
    /// than with how big its disk is. The floor is what the image ships today
    /// and is known to hold the ladder; the cap stops a 20 TB drive donating
    /// 2 TB to log files.
    pub fn for_drive(capacity: u64) -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        LocalLayout {
            data_bytes: (capacity / 10).clamp(8 * GIB, 64 * GIB).min(capacity / 2),
            slot_size: DEFAULT_SLOT_SIZE,
            tier: StorageTier::Hot,
            lba: None,
        }
    }
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
    let Ok(Some(bytes)) = slab.read_metadata().await else { return Ok(None) };
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
    let gpt = Gpt::read(&device)
        .await
        .map_err(|e| ImageError::Other(format!("reading the table: {e}")))?;
    let (data_i, system_i) = node_layout(&device)
        .await?
        .ok_or_else(|| ImageError::Spec("this drive does not carry a node layout".into()))?;
    let lba = gpt.block_size;

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
/// The data slab is allocated **first**, deliberately. It is the half an
/// install keeps, and putting it before the half that grows means a system
/// slab that gets larger across a release cannot move the partition holding
/// the node's identity.
pub async fn lay_node_slabs(
    device: Arc<dyn BlockDevice>,
    opts: &LocalLayout,
) -> Result<LocalSlabs> {
    let capacity = device.capacity_bytes();
    let lba = opts.lba.unwrap_or_else(|| device.block_size());

    let data_bytes = align_down(opts.data_bytes, ALIGN);
    if data_bytes == 0 {
        return Err(ImageError::Spec("the data slab would be empty".into()));
    }
    // What is left after the GPT at both ends and the data slab, rounded down
    // so the last partition never runs past the tail the table needs.
    let system_bytes = capacity
        .saturating_sub(GPT_OVERHEAD)
        .saturating_sub(data_bytes);
    let system_bytes = align_down(system_bytes, ALIGN);
    if system_bytes < 64 * ALIGN {
        return Err(ImageError::Spec(format!(
            "a {} byte drive leaves {} for the system slab after a {} data slab, which is not \
             enough to hold the goldens",
            capacity, system_bytes, data_bytes
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

    let mut out = Vec::new();
    for (name, guid, size, role) in [
        ("stormblock-data", type_guid::SLAB_DATA, data_bytes, SlabRole::Data),
        ("stormblock", type_guid::SLAB, system_bytes, SlabRole::System),
    ] {
        let slot = gpt
            .allocate(name, guid, size, 0)
            .map_err(|e| ImageError::Other(format!("allocating {name}: {e}")))?;
        gpt.write(&device).await.map_err(|e| ImageError::Other(format!("gpt: {e}")))?;
        let start = gpt.entries[slot].start_bytes(lba);
        let len = gpt.entries[slot].size_bytes(lba);
        let part = Arc::new(
            PartitionDevice::new(device.clone(), start, len)
                .map_err(|e| ImageError::Other(format!("partition {name}: {e}")))?,
        );
        // The slab keeps its own record of what is in it. A node reads that
        // at boot; there is no filesystem underneath it to keep one in, and a
        // slab that cannot say what it holds boots to "no volume metadata".
        let meta = auto_metadata_bytes(len, opts.slot_size);
        let slab = Slab::format_with(
            part,
            SlabFormat::new(opts.slot_size, opts.tier)
                .with_metadata(meta)
                .with_role(role),
        )
        .await
        .map_err(|e| ImageError::Other(format!("format {name}: {e}")))?;
        out.push(slab);
    }

    let system = out.pop().expect("two slabs");
    let data = out.pop().expect("two slabs");
    Ok(LocalSlabs { data, system, data_bytes, system_bytes, lba })
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
}
