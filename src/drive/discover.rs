//! Finding the slabs that are already on a drive.
//!
//! A stormcos disk is a GPT with pallet partitions and one or two slab
//! partitions, so "what is on this drive" is a question with an answer that
//! can be read rather than configured. Every path that attaches to an existing
//! node's storage asks it — `boot-local` at boot, `adopt-ublk` at handover,
//! and the management API when an appliance is handed an image and asked what
//! is inside it.

use std::sync::Arc;

use crate::drive::partition::PartitionDevice;
use crate::drive::slab::Slab;
use crate::drive::BlockDevice;

/// A slab found on a drive, with the partition label it was found in.
pub struct FoundSlab {
    /// The GPT partition name, or `partition N` where the entry has none.
    pub label: String,
    pub slab: Slab,
}

/// Find every slab inside a partitioned disk.
///
/// Returns an empty vector when there is no partition table, or none of its
/// partitions holds a slab — in which case the caller's original error is the
/// honest one to report, since "this is not a slab" beats "and it has no GPT
/// either".
///
/// **Every** slab, not the first that opens. A node's mutable storage is a
/// system slab *and* a data slab (#88), and the data slab is allocated first,
/// so it is the earlier GPT entry. Returning the first match meant a
/// whole-disk path like `rd.stormblock.slab=/dev/sda` attached identity
/// storage, looked for `stormblock.volume=stormpump` inside it, and found no
/// root device — a boot failure that reads as a missing volume rather than as
/// the wrong partition (stormpump#12).
pub async fn slabs_in_partitions(dev: &Arc<dyn BlockDevice>) -> Vec<FoundSlab> {
    // A drive that is itself a slab, with no partition table at all. This is
    // what a store built by `POST /api/v1/slabs` on a plain file looks like —
    // the shape an appliance's parts store has — and looking only inside
    // partitions found nothing in it, so a store survived exactly as long as
    // the process that made it.
    if let Ok(slab) = Slab::open(dev.clone()).await {
        return vec![FoundSlab { label: "the whole drive".to_string(), slab }];
    }

    let Ok(gpt) = crate::pallet::gpt::Gpt::read(dev).await else {
        return Vec::new();
    };
    let lba = gpt.block_size as u64;
    let mut found = Vec::new();
    for (i, e) in gpt.entries.iter().enumerate() {
        if e.first_lba == 0 || e.last_lba < e.first_lba {
            continue;
        }
        let start = e.first_lba * lba;
        let len = (e.last_lba + 1 - e.first_lba) * lba;
        let Ok(part) = PartitionDevice::new(dev.clone(), start, len) else { continue };
        if let Ok(slab) = Slab::open(Arc::new(part)).await {
            let label = if e.name.is_empty() {
                format!("partition {}", i + 1)
            } else {
                e.name.clone()
            };
            found.push(FoundSlab { label, slab });
        }
    }
    found
}
