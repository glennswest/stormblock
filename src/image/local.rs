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
        {
            let dev = FileDevice::open_with_capacity(&path, CAP).await.unwrap();
            dev.write(0, &vec![0xAB_u8; 1024 * 1024]).await.unwrap();
            dev.write(CAP - 1024 * 1024, &vec![0xCD_u8; 1024 * 1024]).await.unwrap();
            dev.flush().await.unwrap();
        }

        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&path).await.unwrap());
        let mut layout = LocalLayout::for_drive(CAP);
        layout.slot_size = 1024 * 1024;
        let laid = lay_node_slabs(dev, &layout).await.unwrap();
        assert_eq!(laid.data.role(), SlabRole::Data);
        assert_eq!(laid.system.role(), SlabRole::System);

        let front = window(&path, 0, 4 * 1024 * 1024);
        assert!(!front.contains(&0xAB), "the front still carries what was there");
        // The tail is the half a fresh GPT alone would not have touched: our
        // backup header lands there only if the LBA size happens to match the
        // old table's.
        let tail = window(&path, CAP - 1024 * 1024, 1024 * 1024);
        assert!(!tail.contains(&0xCD), "the tail still carries what was there");
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
