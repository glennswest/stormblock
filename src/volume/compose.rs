//! Composed volumes — a disk that is a *list of* goldens, not a copy of them.
//!
//! A snapshot maps one source volume onto one destination, extent for extent.
//! A composition does the same for several sources at once, each placed at its
//! own offset, so the result is a volume whose contents are the goldens it
//! names — sharing their slab slots rather than holding a second copy of them.
//!
//! That is the whole of the "virtual disk" idea. A stormcos image today lands
//! every golden twice: once into a pallet partition and once into the slab. A
//! hundred nodes is a hundred of those. Composed, a hundred nodes is one set of
//! goldens and a hundred extent maps, and cutting a new version writes maps
//! rather than gigabytes.
//!
//! Two properties make it safe to hand to a consumer:
//!
//! * **Copy-on-write is already the rule.** Shared slots carry a `ref_count`,
//!   and `ThinVolumeHandle::cow_write` copies before it changes anything. A
//!   node that writes to a composed disk gets its own slot for what it wrote
//!   and keeps sharing the rest, exactly as a clone does.
//! * **Placement is checked, not trusted.** An offset that is not slot-aligned
//!   cannot be expressed by an extent map — it would silently land at the slot
//!   below — and two components that overlap would each believe they own the
//!   slot. Both are refused with the offending pair named.

use std::collections::HashMap;

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::volume::extent::VolumeId;
use crate::volume::gem::GlobalExtentMap;
use crate::volume::thin::{ThinVolume, VolumeError, VolumePurpose};
use crate::drive::DeviceId;

/// One component of a composed volume.
#[derive(Debug, Clone)]
pub struct Component {
    /// The volume whose extents are shared in. Usually a sealed golden.
    pub source: VolumeId,
    /// Where it starts in the composed volume, in bytes. Slot-aligned.
    pub at: u64,
    /// How much of the composed volume it claims, in bytes — the source's
    /// virtual size, not what it happens to have written. A sparse golden
    /// still owns the whole span it was sized for, and the next component
    /// must start after it.
    pub span: u64,
}

impl Component {
    fn end(&self) -> u64 {
        self.at + self.span
    }
}

/// Compose a volume from components, sharing their extents.
///
/// `declared_size` fixes the composed volume's size; without one it is the end
/// of the last component. Nothing is read or written — this is a map.
pub async fn compose_volume(
    name: &str,
    declared_size: Option<u64>,
    slot_size: u64,
    components: &[Component],
    gem: &mut GlobalExtentMap,
    registry: &mut SlabRegistry,
) -> Result<ThinVolume, VolumeError> {
    if components.is_empty() {
        return Err(VolumeError::InvalidSize(
            "a composed volume needs at least one component".into(),
        ));
    }

    // Alignment first: an unaligned offset is not a smaller mistake than an
    // overlap, it is the same one arriving later.
    for c in components {
        if c.at % slot_size != 0 {
            return Err(VolumeError::InvalidSize(format!(
                "component at {} is not a multiple of the {slot_size}-byte slot: \
                 an extent map cannot express it",
                c.at
            )));
        }
        if c.span == 0 {
            return Err(VolumeError::InvalidSize(format!(
                "component at {} claims no space", c.at
            )));
        }
    }

    // Overlap, against every earlier component rather than only the previous
    // one — the list is not required to be in order.
    for (i, a) in components.iter().enumerate() {
        for b in &components[i + 1..] {
            if a.at < b.end() && b.at < a.end() {
                return Err(VolumeError::InvalidSize(format!(
                    "components overlap: {}..{} and {}..{} would share slots \
                     and each believe it owned them",
                    a.at, a.end(), b.at, b.end()
                )));
            }
        }
    }

    let end = components.iter().map(|c| c.end()).max().unwrap_or(0);
    let virtual_size = match declared_size {
        Some(s) if s < end => {
            return Err(VolumeError::InvalidSize(format!(
                "declared size {s} is smaller than the components, which reach {end}"
            )))
        }
        Some(s) => s,
        None => end,
    };

    let dest_id = VolumeId::new();
    let mut shared: HashMap<SlabId, Vec<u32>> = HashMap::new();

    for c in components {
        let base_vext = c.at / slot_size;
        for leg in gem.gather_into(c.source, dest_id, base_vext) {
            shared.entry(leg.slab_id).or_default().push(leg.slot_idx);
        }
    }

    // The slots are now referenced by one more volume than before. Grouped per
    // slab so the slot table is written by sector: composing a disk out of
    // fifty goldens costs sectors touched, not one round trip per extent.
    for (slab_id, slots) in shared {
        if let Some(slab) = registry.get_mut(&slab_id) {
            slab.inc_ref_batch(&slots).await.map_err(VolumeError::Drive)?;
        } else {
            // Refusing here would leave the map half-built; naming it is what
            // lets someone find out why a composed disk reads short.
            tracing::warn!(
                volume = %dest_id, slab = %slab_id, extents = slots.len(),
                "slab not in registry while composing — its extents are unprotected"
            );
        }
    }

    Ok(ThinVolume {
        id: dest_id,
        name: name.to_string(),
        virtual_size,
        slot_size,
        purpose: VolumePurpose::Partition,
        device_id: DeviceId {
            uuid: dest_id.0,
            serial: format!("comp-{}", &dest_id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{dest_id}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::placement::topology::StorageTier;
    use crate::drive::BlockDevice;
    use crate::volume::VolumeManager;

    const SLOT: u64 = 64 * 1024;

    /// A manager over one slab, and a golden written with `fill` in it.
    async fn manager() -> (VolumeManager, String) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-compose-test");
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
        let id = vm
            .create_volume_with(name, size, Default::default())
            .await
            .unwrap();
        let h = vm.get_volume_handle(&id).unwrap();
        h.write(0, &vec![fill; size as usize]).await.unwrap();
        id
    }

    /// The point of the whole thing: composing costs the map, not the bytes.
    /// Two goldens laid end to end read back as themselves, and the slab has
    /// not allocated a slot more than the goldens already took.
    #[tokio::test]
    async fn a_composition_reads_as_its_components_and_allocates_nothing() {
        let (mut vm, path) = manager().await;
        let a = golden(&mut vm, "a.golden", 2 * SLOT, 0xAA).await;
        let b = golden(&mut vm, "b.golden", 2 * SLOT, 0xBB).await;

        let free_before = vm.registry().read().await.total_free_slots();

        let composed = vm
            .compose_volume("disk", None, &[(a, 0), (b, 2 * SLOT)])
            .await
            .unwrap();

        let free_after = vm.registry().read().await.total_free_slots();
        assert_eq!(
            free_before, free_after,
            "composing allocated slots — it is supposed to share them"
        );

        let h = vm.get_volume_handle(&composed).unwrap();
        assert_eq!(h.capacity_bytes(), 4 * SLOT);

        let mut buf = vec![0u8; SLOT as usize];
        h.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&x| x == 0xAA), "first component");
        h.read(2 * SLOT, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&x| x == 0xBB), "second component, at its offset");

        let _ = std::fs::remove_file(&path);
    }

    /// Writing to a composition must not reach through into the golden it
    /// shares with — that is what ref-counted extents and cow_write are for,
    /// and it is the property that makes one golden safe to hand to a fleet.
    #[tokio::test]
    async fn writing_a_composition_leaves_the_golden_alone() {
        let (mut vm, path) = manager().await;
        let a = golden(&mut vm, "a.golden", 2 * SLOT, 0xAA).await;

        let composed = vm.compose_volume("disk", None, &[(a, 0)]).await.unwrap();
        let ch = vm.get_volume_handle(&composed).unwrap();
        ch.write(0, &vec![0xCC; SLOT as usize]).await.unwrap();

        let mut buf = vec![0u8; SLOT as usize];
        ch.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&x| x == 0xCC), "the composition took the write");

        let gh = vm.get_volume_handle(&a).unwrap();
        gh.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&x| x == 0xAA), "the golden is untouched");

        let _ = std::fs::remove_file(&path);
    }

    /// A slab whose slots are a different size from the manager's extents is
    /// refused outright. This is the shape of the defect that corrupted the
    /// serving path: divide by one size, address by another, and every extent
    /// is written across its neighbours. Adoption is a door into the engine
    /// from a disk someone else formatted, so it is the obvious way back in.
    #[tokio::test]
    async fn adopting_a_slab_with_a_different_slot_size_is_refused() {
        use crate::drive::discover::FoundSlab;

        let (mut vm, path) = manager().await;

        let dir = std::env::temp_dir().join("stormblock-compose-test");
        let other = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
        let other_str = other.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&other_str, 16 * 1024 * 1024).await.unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(dev);
        // Four times this manager's extent size — the exact 4 MiB against
        // 1 MiB that went wrong in production, scaled down.
        let slab = Slab::format(dev, SLOT * 4, StorageTier::Hot).await.unwrap();

        let err = vm
            .adopt_slabs(vec![FoundSlab { label: "test".into(), slab }])
            .await
            .expect_err("a slot-size mismatch must be refused");
        let msg = err.to_string();
        assert!(msg.contains("slots"), "the error names the sizes: {msg}");

        let _ = std::fs::remove_file(&other_str);
        let _ = std::fs::remove_file(&path);
    }

    /// An offset an extent map cannot express would quietly land at the slot
    /// below it, which is a component silently in the wrong place.
    #[tokio::test]
    async fn an_unaligned_component_is_refused() {
        let (mut vm, path) = manager().await;
        let a = golden(&mut vm, "a.golden", SLOT, 0xAA).await;
        let err = vm.compose_volume("disk", None, &[(a, SLOT + 4096)]).await;
        assert!(err.is_err(), "an unaligned offset must be refused");
        let _ = std::fs::remove_file(&path);
    }

    /// Overlapping components would each map the same destination extent, and
    /// the last one written would win with nothing said.
    #[tokio::test]
    async fn overlapping_components_are_refused() {
        let (mut vm, path) = manager().await;
        let a = golden(&mut vm, "a.golden", 2 * SLOT, 0xAA).await;
        let b = golden(&mut vm, "b.golden", 2 * SLOT, 0xBB).await;
        let err = vm.compose_volume("disk", None, &[(a, 0), (b, SLOT)]).await;
        assert!(err.is_err(), "overlapping components must be refused");
        let _ = std::fs::remove_file(&path);
    }
}
