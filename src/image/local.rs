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
/// **This destroys whatever is on the device.** The decision that it may be
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
