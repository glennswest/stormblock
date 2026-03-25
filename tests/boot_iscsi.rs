//! Boot-from-iSCSI integration tests.
//!
//! Unit-level tests for layout parsing, size resolution, and provisioning
//! workflow (using FileDevice as a stand-in for iSCSI in offline tests).

use stormblock::boot_iscsi::BootDiskLayout;

// ── Layout parsing ──────────────────────────────────────────────

#[test]
fn parse_standard_layout() {
    let layout =
        BootDiskLayout::parse("esp:256M,boot:512M,root:6G,swap:1G,home:rest").unwrap();
    assert_eq!(layout.partitions.len(), 5);

    assert_eq!(layout.partitions[0].name, "esp");
    assert_eq!(layout.partitions[0].size, 256 * 1024 * 1024);
    assert_eq!(layout.partitions[0].fs_type, "vfat");
    assert_eq!(layout.partitions[0].mount_point, "/boot/efi");

    assert_eq!(layout.partitions[1].name, "boot");
    assert_eq!(layout.partitions[1].size, 512 * 1024 * 1024);
    assert_eq!(layout.partitions[1].fs_type, "ext4");
    assert_eq!(layout.partitions[1].mount_point, "/boot");

    assert_eq!(layout.partitions[2].name, "root");
    assert_eq!(layout.partitions[2].size, 6 * 1024 * 1024 * 1024);
    assert_eq!(layout.partitions[2].fs_type, "ext4");
    assert_eq!(layout.partitions[2].mount_point, "/");

    assert_eq!(layout.partitions[3].name, "swap");
    assert_eq!(layout.partitions[3].size, 1024 * 1024 * 1024);
    assert_eq!(layout.partitions[3].fs_type, "swap");
    assert_eq!(layout.partitions[3].mount_point, "swap");

    assert_eq!(layout.partitions[4].name, "home");
    assert_eq!(layout.partitions[4].size, 0); // rest
    assert_eq!(layout.partitions[4].fs_type, "ext4");
    assert_eq!(layout.partitions[4].mount_point, "/home");
}

#[test]
fn resolve_sizes_fills_remainder() {
    let mut layout =
        BootDiskLayout::parse("esp:256M,boot:512M,root:6G,swap:1G,home:rest").unwrap();
    let total = 10 * 1024 * 1024 * 1024u64; // 10 GB
    layout.resolve_sizes(total).unwrap();

    let fixed: u64 = 256 * 1024 * 1024
        + 512 * 1024 * 1024
        + 6 * 1024 * 1024 * 1024
        + 1024 * 1024 * 1024;
    let expected_home = total - fixed;
    assert_eq!(layout.partitions[4].size, expected_home);
    assert!(expected_home > 0);

    // Verify all sizes sum to total
    let actual_total: u64 = layout.partitions.iter().map(|p| p.size).sum();
    assert_eq!(actual_total, total);
}

#[test]
fn resolve_sizes_no_rest() {
    let mut layout = BootDiskLayout::parse("root:8G,swap:2G").unwrap();
    let total = 10 * 1024 * 1024 * 1024u64;
    layout.resolve_sizes(total).unwrap();

    assert_eq!(layout.partitions[0].size, 8 * 1024 * 1024 * 1024);
    assert_eq!(layout.partitions[1].size, 2 * 1024 * 1024 * 1024);
}

#[test]
fn resolve_sizes_rejects_overflow() {
    let mut layout = BootDiskLayout::parse("root:12G,swap:1G").unwrap();
    let total = 10 * 1024 * 1024 * 1024u64;
    let err = layout.resolve_sizes(total).unwrap_err();
    assert!(err.contains("exceed"));
}

#[test]
fn resolve_sizes_rejects_multiple_rest() {
    let mut layout = BootDiskLayout::parse("root:rest,home:rest").unwrap();
    let err = layout.resolve_sizes(10 * 1024 * 1024 * 1024).unwrap_err();
    assert!(err.contains("only one"));
}

#[test]
fn parse_empty_rejected() {
    assert!(BootDiskLayout::parse("").is_err());
}

#[test]
fn parse_invalid_spec_rejected() {
    assert!(BootDiskLayout::parse("badspec").is_err());
}

#[test]
fn parse_custom_partition_name() {
    let layout = BootDiskLayout::parse("data:5G").unwrap();
    assert_eq!(layout.partitions[0].name, "data");
    assert_eq!(layout.partitions[0].mount_point, "/data");
    assert_eq!(layout.partitions[0].fs_type, "ext4");
}

#[test]
fn parse_lowercase_sizes() {
    let layout = BootDiskLayout::parse("root:6g,swap:1g").unwrap();
    assert_eq!(layout.partitions[0].size, 6 * 1024 * 1024 * 1024);
    assert_eq!(layout.partitions[1].size, 1024 * 1024 * 1024);
}

// ── Provisioning workflow (offline, no real iSCSI) ──────────────

use std::sync::Arc;
use tokio::sync::Mutex;

use stormblock::drive::BlockDevice;
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
use stormblock::drive::slab_registry::SlabRegistry;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::gem::GlobalExtentMap;
use stormblock::volume::thin::{ThinVolume, ThinVolumeHandle, PlacementPolicy};

/// Test that ThinVolumes on a slab work correctly for the boot partition use case.
#[tokio::test]
async fn provision_volumes_on_file_slab() {
    let dir = std::env::temp_dir().join("stormblock-boot-iscsi-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("boot-test-{}.bin", uuid::Uuid::new_v4().simple()));
    let path_str = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    // Create a 50 MB backing device (simulating iSCSI)
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(&path_str, 50 * 1024 * 1024)
            .await
            .unwrap(),
    );

    // Format as slab
    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .unwrap();
    let slab_id = slab.slab_id();
    let total_slots = slab.total_slots();
    assert!(total_slots > 0);

    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    // Parse and resolve layout for 50 MB
    let mut layout = BootDiskLayout::parse("root:30M,swap:10M,home:rest").unwrap();
    layout.resolve_sizes(50 * 1024 * 1024).unwrap();
    assert_eq!(layout.partitions[2].size, 10 * 1024 * 1024); // 50 - 30 - 10 = 10 MB

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Create volumes
    let mut handles = Vec::new();
    for part in &layout.partitions {
        let vol = ThinVolume::new(part.name.clone(), part.size, DEFAULT_SLOT_SIZE);
        let vol_id = vol.id();
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem.clone(),
            registry.clone(),
            placement.clone(),
        ));
        handles.push((part.name.clone(), vol_id, handle));
    }

    assert_eq!(handles.len(), 3);

    // Write test patterns to each volume
    for (name, _vol_id, handle) in &handles {
        let pattern = match name.as_str() {
            "root" => 0xAAu8,
            "swap" => 0xBBu8,
            "home" => 0xCCu8,
            _ => 0x00u8,
        };
        let data = vec![pattern; 4096];
        handle.write(0, &data).await.unwrap();
    }

    // Read back and verify isolation
    for (name, _vol_id, handle) in &handles {
        let expected = match name.as_str() {
            "root" => 0xAAu8,
            "swap" => 0xBBu8,
            "home" => 0xCCu8,
            _ => 0x00u8,
        };
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        assert!(
            buf.iter().all(|&b| b == expected),
            "volume '{}' data mismatch",
            name
        );
    }

    // Verify volumes are independent — unallocated reads return zeros
    for (_name, _vol_id, handle) in &handles {
        let mut buf = vec![0xFFu8; 4096];
        handle.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    // Verify slab has allocated slots
    {
        let reg = registry.lock().await;
        let slab = reg.get(&slab_id).unwrap();
        assert_eq!(slab.allocated_slots(), 3); // one slot per volume (one write each)
    }

    // Cleanup
    let _ = std::fs::remove_file(&path);
}

/// Test migration path: create data on one slab, evacuate to another.
#[tokio::test]
async fn migrate_boot_volumes_between_slabs() {
    use stormblock::volume::gem::ExtentLocation;

    let dir = std::env::temp_dir().join("stormblock-boot-migrate-test");
    std::fs::create_dir_all(&dir).unwrap();
    let test_id = uuid::Uuid::new_v4().simple().to_string();

    // Source slab (simulating iSCSI)
    let src_path = dir.join(format!("{test_id}-src.bin"));
    let _ = std::fs::remove_file(&src_path);
    let src_dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(src_path.to_str().unwrap(), 20 * 1024 * 1024)
            .await
            .unwrap(),
    );
    let mut src_slab = Slab::format(src_dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .unwrap();
    let src_id = src_slab.slab_id();

    // Allocate 3 "partition" extents and write data
    let vol_root = stormblock::volume::extent::VolumeId::new();
    let vol_swap = stormblock::volume::extent::VolumeId::new();
    let vol_home = stormblock::volume::extent::VolumeId::new();

    let slot_r = src_slab.allocate(vol_root, 0).await.unwrap();
    let slot_s = src_slab.allocate(vol_swap, 0).await.unwrap();
    let slot_h = src_slab.allocate(vol_home, 0).await.unwrap();

    src_slab.write_slot(slot_r, 0, &vec![0xAA; 4096]).await.unwrap();
    src_slab.write_slot(slot_s, 0, &vec![0xBB; 4096]).await.unwrap();
    src_slab.write_slot(slot_h, 0, &vec![0xCC; 4096]).await.unwrap();

    // Build GEM
    let mut gem = GlobalExtentMap::new();
    gem.insert(vol_root, 0, ExtentLocation { slab_id: src_id, slot_idx: slot_r, ref_count: 1, generation: 1 });
    gem.insert(vol_swap, 0, ExtentLocation { slab_id: src_id, slot_idx: slot_s, ref_count: 1, generation: 1 });
    gem.insert(vol_home, 0, ExtentLocation { slab_id: src_id, slot_idx: slot_h, ref_count: 1, generation: 1 });

    let mut registry = SlabRegistry::new();
    registry.add(src_slab);

    // Destination slab (simulating local NVMe)
    let dst_path = dir.join(format!("{test_id}-dst.bin"));
    let _ = std::fs::remove_file(&dst_path);
    let dst_dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(dst_path.to_str().unwrap(), 20 * 1024 * 1024)
            .await
            .unwrap(),
    );

    // Migrate
    let engine = stormblock::placement::PlacementEngine::new();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let result = stormblock::migrate::migrate_to_slab(
        &mut gem, &mut registry, &engine,
        src_id, dst_dev, StorageTier::Hot, DEFAULT_SLOT_SIZE,
        &rx,
    ).await.unwrap();

    assert_eq!(result.migrated, 3);
    assert_eq!(result.failed, 0);

    // Verify all extents now on destination slab
    let loc_r = gem.lookup(vol_root, 0).unwrap();
    assert_eq!(loc_r.slab_id, result.dest_slab);
    let loc_s = gem.lookup(vol_swap, 0).unwrap();
    assert_eq!(loc_s.slab_id, result.dest_slab);
    let loc_h = gem.lookup(vol_home, 0).unwrap();
    assert_eq!(loc_h.slab_id, result.dest_slab);

    // Verify data integrity
    let dst_slab = registry.get(&result.dest_slab).unwrap();
    let mut buf = vec![0u8; 4096];

    dst_slab.read_slot(loc_r.slot_idx, 0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA));

    dst_slab.read_slot(loc_s.slot_idx, 0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB));

    dst_slab.read_slot(loc_h.slot_idx, 0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0xCC));

    // Source slab should be empty
    assert!(gem.slab_extents(src_id).is_empty());

    // Cleanup
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}
