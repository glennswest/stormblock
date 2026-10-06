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
    slabs_in_partitions_why(dev).await.0
}

/// [`slabs_in_partitions`], and why each place a slab could be did not
/// open (#301): the whole drive, a drive with no readable table, and every
/// partition whose slab failed. A caller that finds nothing says these
/// rather than the whole drive's "bad slab magic", which hid a partition's
/// real error.
pub async fn slabs_in_partitions_why(dev: &Arc<dyn BlockDevice>) -> (Vec<FoundSlab>, Vec<String>) {
    let mut why = Vec::new();
    // A drive that is itself a slab, with no partition table at all. This is
    // what a store built by `POST /api/v1/slabs` on a plain file looks like —
    // the shape an appliance's parts store has — and looking only inside
    // partitions found nothing in it, so a store survived exactly as long as
    // the process that made it.
    match Slab::open(dev.clone()).await {
        Ok(slab) => return (vec![FoundSlab { label: "the whole drive".to_string(), slab }], why),
        Err(e) => why.push(format!("the whole drive: {e}")),
    }

    let gpt = match crate::pallet::gpt::Gpt::read(dev).await {
        Ok(g) => g,
        Err(e) => {
            why.push(format!("partition table: {e}"));
            return (Vec::new(), why);
        }
    };
    let lba = gpt.block_size as u64;
    let mut found = Vec::new();
    for (i, e) in gpt.entries.iter().enumerate() {
        if e.first_lba == 0 || e.last_lba < e.first_lba {
            continue;
        }
        let label = if e.name.is_empty() { format!("partition {}", i + 1) } else { e.name.clone() };
        let start = e.first_lba * lba;
        let len = (e.last_lba + 1 - e.first_lba) * lba;
        let part = match PartitionDevice::new(dev.clone(), start, len) {
            Ok(p) => p,
            Err(err) => {
                why.push(format!("{label}: {err}"));
                continue;
            }
        };
        match Slab::open(Arc::new(part)).await {
            Ok(slab) => found.push(FoundSlab { label, slab }),
            Err(err) => why.push(format!("{label}: {err}")),
        }
    }
    (found, why)
}
