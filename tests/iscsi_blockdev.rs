//! IscsiDevice + Slab + ThinVolume integration tests against real iSCSI hardware.
//!
//! Tests the production `IscsiDevice` (not the test initiator) against dedicated
//! iSCSI disks: `boot-iscsi-src` (5 GB) and `boot-iscsi-dst` (5 GB).
//!
//! Requires environment variables:
//!   ISCSI_PORTAL   — IP address (default "192.168.10.1")
//!   ISCSI_PORT     — TCP port (default "3260")
//!   ISCSI_IQN_SRC  — Source disk IQN
//!   ISCSI_IQN_DST  — Destination disk IQN (for migration tests)
//!
//! Run: cargo test --test iscsi_blockdev -- --ignored --nocapture

use std::sync::Arc;
use tokio::sync::Mutex;

use stormblock::drive::BlockDevice;
use stormblock::drive::iscsi_dev::IscsiDevice;
use stormblock::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
use stormblock::drive::slab_registry::SlabRegistry;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::extent::VolumeId;
use stormblock::volume::gem::{ExtentLocation, GlobalExtentMap};
use stormblock::volume::thin::{PlacementPolicy, ThinVolume, ThinVolumeHandle};

fn iscsi_src() -> Option<(String, u16, String)> {
    let portal = std::env::var("ISCSI_PORTAL").unwrap_or_else(|_| "192.168.10.1".to_string());
    let port: u16 = std::env::var("ISCSI_PORT")
        .unwrap_or_else(|_| "3260".to_string())
        .parse()
        .ok()?;
    let iqn = std::env::var("ISCSI_IQN_SRC").ok()?;
    Some((portal, port, iqn))
}

fn iscsi_dst() -> Option<(String, u16, String)> {
    let portal = std::env::var("ISCSI_PORTAL").unwrap_or_else(|_| "192.168.10.1".to_string());
    let port: u16 = std::env::var("ISCSI_PORT")
        .unwrap_or_else(|_| "3260".to_string())
        .parse()
        .ok()?;
    let iqn = std::env::var("ISCSI_IQN_DST").ok()?;
    Some((portal, port, iqn))
}

// ── IscsiDevice BlockDevice tests ────────────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_device_connect_and_capacity() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => {
            eprintln!("ISCSI_IQN_SRC not set, skipping");
            return;
        }
    };

    eprintln!("Connecting to {}:{} {}...", portal, port, iqn);
    let dev = IscsiDevice::connect(&portal, port, &iqn)
        .await
        .expect("IscsiDevice::connect failed");

    let cap = dev.capacity_bytes();
    let bs = dev.block_size();
    eprintln!("Capacity: {} bytes ({} MB), block_size: {}", cap, cap / 1024 / 1024, bs);

    assert!(cap > 0, "capacity must be > 0");
    assert!(bs == 512 || bs == 4096, "unexpected block size: {}", bs);
    assert_eq!(cap % bs as u64, 0, "capacity not aligned to block_size");

    eprintln!("Disconnecting...");
    dev.disconnect().await.expect("disconnect failed");
    eprintln!("PASS: connect + capacity");
}

#[tokio::test]
#[ignore]
async fn iscsi_device_write_read_verify() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev = IscsiDevice::connect(&portal, port, &iqn)
        .await
        .expect("connect failed");
    let bs = dev.block_size() as usize;

    // Write a deterministic pattern at offset 0
    let mut pattern = vec![0u8; bs * 4]; // 4 blocks
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i * 37 + 0xDE) & 0xFF) as u8;
    }

    eprintln!("Writing {} bytes at offset 0...", pattern.len());
    dev.write(0, &pattern).await.expect("write failed");

    // Read back
    let mut readback = vec![0u8; pattern.len()];
    eprintln!("Reading {} bytes at offset 0...", readback.len());
    dev.read(0, &mut readback).await.expect("read failed");

    assert_eq!(readback, pattern, "data mismatch after write/read");
    eprintln!("PASS: write/read/verify ({} bytes)", pattern.len());

    dev.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
#[ignore]
async fn iscsi_device_large_io() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev = IscsiDevice::connect(&portal, port, &iqn)
        .await
        .expect("connect failed");
    let bs = dev.block_size() as usize;

    // Write 256 KB — tests chunking at FirstBurstLength (65536) boundaries
    let size = 256 * 1024;
    let blocks = size / bs;
    let mut data = vec![0u8; size];
    for (i, b) in data.iter_mut().enumerate() {
        *b = ((i * 113 + i / 256) & 0xFF) as u8;
    }

    // Write at a non-zero offset (block 1024)
    let offset = 1024 * bs as u64;
    eprintln!("Writing {} KB ({} blocks) at offset {}...", size / 1024, blocks, offset);
    dev.write(offset, &data).await.expect("large write failed");

    let mut readback = vec![0u8; size];
    eprintln!("Reading back...");
    dev.read(offset, &mut readback).await.expect("large read failed");

    assert_eq!(readback, data, "large I/O data mismatch");
    eprintln!("PASS: large I/O ({} KB)", size / 1024);

    dev.disconnect().await.expect("disconnect failed");
}

#[tokio::test]
#[ignore]
async fn iscsi_device_unaligned_data_length() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev = IscsiDevice::connect(&portal, port, &iqn)
        .await
        .expect("connect failed");
    let bs = dev.block_size() as usize;

    // Write data whose length is NOT a multiple of block_size.
    // This tests the padding logic added to fix CHECK CONDITION.
    let size = bs * 7 + 100; // 7.something blocks
    let mut data = vec![0u8; size];
    for (i, b) in data.iter_mut().enumerate() {
        *b = ((i * 53 + 0x7F) & 0xFF) as u8;
    }

    let offset = 2048 * bs as u64;
    eprintln!("Writing {} bytes (non-aligned to block_size={}) at offset {}...", size, bs, offset);
    dev.write(offset, &data).await.expect("unaligned write failed");

    // Read back — we need to read the full padded block count
    let read_blocks = size.div_ceil(bs);
    let read_size = read_blocks * bs;
    let mut readback = vec![0u8; read_size];
    dev.read(offset, &mut readback).await.expect("read failed");

    // The first `size` bytes must match; trailing pad bytes are zeros
    assert_eq!(&readback[..size], &data[..], "unaligned write data mismatch");
    // Verify trailing pad is zeros
    for (i, &b) in readback[size..].iter().enumerate() {
        assert_eq!(b, 0, "pad byte at offset {} is {:#x}, expected 0", size + i, b);
    }

    eprintln!("PASS: unaligned data length ({} bytes, padded to {})", size, read_size);
    dev.disconnect().await.expect("disconnect failed");
}

// ── Slab on iSCSI ────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_slab_format_allocate_readwrite() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let cap = dev.capacity_bytes();
    eprintln!("Formatting {} MB iSCSI device as slab...", cap / 1024 / 1024);

    let mut slab = Slab::format(dev.clone(), DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let total = slab.total_slots();
    let free = slab.free_slots();
    eprintln!("Slab: {} total slots, {} free ({} MB each)", total, free, DEFAULT_SLOT_SIZE / 1024 / 1024);
    assert!(total > 0);
    assert_eq!(total, free);

    // Allocate 3 slots
    let v1 = VolumeId::new();
    let v2 = VolumeId::new();
    let v3 = VolumeId::new();

    let s1 = slab.allocate(v1, 0).await.expect("allocate slot 1");
    let s2 = slab.allocate(v2, 0).await.expect("allocate slot 2");
    let s3 = slab.allocate(v3, 0).await.expect("allocate slot 3");

    assert_eq!(slab.allocated_slots(), 3);
    assert_eq!(slab.free_slots(), total - 3);

    // Write distinct patterns to each slot
    let data1: Vec<u8> = (0..4096).map(|i| ((i * 7 + 0xAA) & 0xFF) as u8).collect();
    let data2: Vec<u8> = (0..4096).map(|i| ((i * 13 + 0xBB) & 0xFF) as u8).collect();
    let data3: Vec<u8> = (0..4096).map(|i| ((i * 19 + 0xCC) & 0xFF) as u8).collect();

    eprintln!("Writing to 3 slots...");
    slab.write_slot(s1, 0, &data1).await.expect("write slot 1");
    slab.write_slot(s2, 0, &data2).await.expect("write slot 2");
    slab.write_slot(s3, 0, &data3).await.expect("write slot 3");

    // Read back and verify
    let mut buf = vec![0u8; 4096];

    slab.read_slot(s1, 0, &mut buf).await.expect("read slot 1");
    assert_eq!(buf, data1, "slot 1 data mismatch");

    slab.read_slot(s2, 0, &mut buf).await.expect("read slot 2");
    assert_eq!(buf, data2, "slot 2 data mismatch");

    slab.read_slot(s3, 0, &mut buf).await.expect("read slot 3");
    assert_eq!(buf, data3, "slot 3 data mismatch");

    eprintln!("PASS: slab format + allocate + read/write ({} slots)", total);
}

#[tokio::test]
#[ignore]
async fn iscsi_slab_reopen() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    // Format a slab and write data
    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let mut slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("format failed");

    let slab_id = slab.slab_id();
    let vol = VolumeId::new();
    let slot = slab.allocate(vol, 0).await.expect("allocate");

    let pattern: Vec<u8> = (0..4096).map(|i| ((i * 31 + 0x42) & 0xFF) as u8).collect();
    slab.write_slot(slot, 0, &pattern).await.expect("write");

    drop(slab); // Close

    // Reconnect and reopen the slab
    eprintln!("Reconnecting to verify slab persistence...");
    let dev2: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("reconnect failed"),
    );

    let slab2 = Slab::open(dev2).await.expect("slab open failed");
    assert_eq!(slab2.slab_id(), slab_id, "slab ID mismatch after reopen");
    assert_eq!(slab2.allocated_slots(), 1);

    let mut buf = vec![0u8; 4096];
    slab2.read_slot(slot, 0, &mut buf).await.expect("read after reopen");
    assert_eq!(buf, pattern, "data mismatch after reopen");

    eprintln!("PASS: slab reopen + data persistence");
}

// ── ThinVolume on iSCSI slab ─────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_thin_volume_io() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Create a 5 MB volume
    let vol = ThinVolume::new("test-vol".to_string(), 5 * 1024 * 1024, DEFAULT_SLOT_SIZE);
    let handle = Arc::new(ThinVolumeHandle::new(
        vol,
        gem.clone(),
        registry.clone(),
        placement,
    ));

    // Write 4 KB at offset 0
    let write_data: Vec<u8> = (0..4096).map(|i| ((i * 41 + 0xFE) & 0xFF) as u8).collect();
    eprintln!("Writing 4 KB to ThinVolume on iSCSI slab...");
    handle.write(0, &write_data).await.expect("volume write");

    // Read back
    let mut read_buf = vec![0u8; 4096];
    handle.read(0, &mut read_buf).await.expect("volume read");
    assert_eq!(read_buf, write_data, "ThinVolume data mismatch");

    // Unallocated extent should return zeros
    let mut zero_buf = vec![0xFFu8; 4096];
    handle.read(DEFAULT_SLOT_SIZE, &mut zero_buf).await.expect("read unallocated");
    assert!(zero_buf.iter().all(|&b| b == 0), "unallocated extent should be zeros");

    eprintln!("PASS: ThinVolume I/O on iSCSI slab");
}

#[tokio::test]
#[ignore]
async fn iscsi_multi_volume_isolation() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Create 3 volumes (simulating boot partitions)
    let names = ["root", "swap", "home"];
    let patterns = [0xAAu8, 0xBBu8, 0xCCu8];
    let mut handles = Vec::new();

    for (i, name) in names.iter().enumerate() {
        let vol = ThinVolume::new(name.to_string(), 2 * 1024 * 1024, DEFAULT_SLOT_SIZE);
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem.clone(),
            registry.clone(),
            placement.clone(),
        ));

        // Write a unique pattern
        let data = vec![patterns[i]; 4096];
        handle.write(0, &data).await.unwrap_or_else(|e| panic!("write to '{}' failed: {}", name, e));
        handles.push(handle);
    }

    // Verify each volume has its own pattern — no cross-contamination
    for (i, handle) in handles.iter().enumerate() {
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap_or_else(|e| panic!("read from '{}' failed: {}", names[i], e));
        assert!(
            buf.iter().all(|&b| b == patterns[i]),
            "volume '{}' has wrong data (expected {:#x})",
            names[i],
            patterns[i]
        );
    }

    eprintln!("PASS: multi-volume isolation ({} volumes on iSCSI slab)", names.len());
}

// ── Migration between iSCSI disks ───────────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_migrate_between_disks() {
    let (src_portal, src_port, src_iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };
    let (dst_portal, dst_port, dst_iqn) = match iscsi_dst() {
        Some(v) => v,
        None => {
            eprintln!("ISCSI_IQN_DST not set, skipping migration test");
            return;
        }
    };

    // Connect source disk
    let src_dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&src_portal, src_port, &src_iqn)
            .await
            .expect("connect src failed"),
    );
    eprintln!("Source: {} ({} MB)", src_iqn, src_dev.capacity_bytes() / 1024 / 1024);

    // Format source slab and write data
    let mut src_slab = Slab::format(src_dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("format src failed");
    let src_id = src_slab.slab_id();

    let vol_a = VolumeId::new();
    let vol_b = VolumeId::new();
    let vol_c = VolumeId::new();

    let s_a = src_slab.allocate(vol_a, 0).await.expect("alloc a");
    let s_b = src_slab.allocate(vol_b, 0).await.expect("alloc b");
    let s_c = src_slab.allocate(vol_c, 0).await.expect("alloc c");

    // Write random-ish patterns
    let data_a: Vec<u8> = (0..4096).map(|i| ((i * 7 + 0xAA) & 0xFF) as u8).collect();
    let data_b: Vec<u8> = (0..4096).map(|i| ((i * 13 + 0xBB) & 0xFF) as u8).collect();
    let data_c: Vec<u8> = (0..4096).map(|i| ((i * 19 + 0xCC) & 0xFF) as u8).collect();

    src_slab.write_slot(s_a, 0, &data_a).await.expect("write a");
    src_slab.write_slot(s_b, 0, &data_b).await.expect("write b");
    src_slab.write_slot(s_c, 0, &data_c).await.expect("write c");

    eprintln!("Wrote 3 extents to source slab");

    // Build GEM
    let mut gem = GlobalExtentMap::new();
    gem.insert(vol_a, 0, ExtentLocation { slab_id: src_id, slot_idx: s_a, ref_count: 1, generation: 1 });
    gem.insert(vol_b, 0, ExtentLocation { slab_id: src_id, slot_idx: s_b, ref_count: 1, generation: 1 });
    gem.insert(vol_c, 0, ExtentLocation { slab_id: src_id, slot_idx: s_c, ref_count: 1, generation: 1 });

    let mut registry = SlabRegistry::new();
    registry.add(src_slab);

    // Connect destination disk
    let dst_dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&dst_portal, dst_port, &dst_iqn)
            .await
            .expect("connect dst failed"),
    );
    eprintln!("Destination: {} ({} MB)", dst_iqn, dst_dev.capacity_bytes() / 1024 / 1024);

    // Migrate
    let engine = stormblock::placement::PlacementEngine::new();
    let (_tx, rx) = tokio::sync::watch::channel(false);

    eprintln!("Migrating extents from src to dst...");
    let result = stormblock::migrate::migrate_to_slab(
        &mut gem,
        &mut registry,
        &engine,
        src_id,
        dst_dev,
        StorageTier::Hot,
        DEFAULT_SLOT_SIZE,
        &rx,
    )
    .await
    .expect("migration failed");

    assert_eq!(result.migrated, 3, "expected 3 migrated extents");
    assert_eq!(result.failed, 0, "expected 0 failed extents");

    // Verify all extents now on destination
    let loc_a = gem.lookup(vol_a, 0).expect("vol_a not in GEM");
    let loc_b = gem.lookup(vol_b, 0).expect("vol_b not in GEM");
    let loc_c = gem.lookup(vol_c, 0).expect("vol_c not in GEM");

    assert_eq!(loc_a.slab_id, result.dest_slab, "vol_a on wrong slab");
    assert_eq!(loc_b.slab_id, result.dest_slab, "vol_b on wrong slab");
    assert_eq!(loc_c.slab_id, result.dest_slab, "vol_c on wrong slab");

    // Verify data integrity on destination
    let dst_slab = registry.get(&result.dest_slab).expect("dest slab not in registry");
    let mut buf = vec![0u8; 4096];

    dst_slab.read_slot(loc_a.slot_idx, 0, &mut buf).await.expect("read a from dst");
    assert_eq!(buf, data_a, "vol_a data mismatch after migration");

    dst_slab.read_slot(loc_b.slot_idx, 0, &mut buf).await.expect("read b from dst");
    assert_eq!(buf, data_b, "vol_b data mismatch after migration");

    dst_slab.read_slot(loc_c.slot_idx, 0, &mut buf).await.expect("read c from dst");
    assert_eq!(buf, data_c, "vol_c data mismatch after migration");

    // Source slab should be empty
    assert!(gem.slab_extents(src_id).is_empty(), "source slab should be empty after migration");

    eprintln!("PASS: migration between iSCSI disks (3 extents, data verified)");
}

#[tokio::test]
#[ignore]
async fn iscsi_device_flush_nop_out() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev = IscsiDevice::connect(&portal, port, &iqn)
        .await
        .expect("connect failed");

    // Flush sends a NOP-Out and expects a NOP-In response
    eprintln!("Sending flush (NOP-Out keepalive)...");
    dev.flush().await.expect("flush/NOP-Out failed");
    eprintln!("Flush 1 OK");

    // Do it again to verify session is still alive
    dev.flush().await.expect("second flush failed");
    eprintln!("Flush 2 OK");

    // Write + flush + read should work
    let bs = dev.block_size() as usize;
    let data = vec![0x55u8; bs];
    dev.write(3072 * bs as u64, &data).await.expect("write failed");
    dev.flush().await.expect("flush after write failed");

    let mut buf = vec![0u8; bs];
    dev.read(3072 * bs as u64, &mut buf).await.expect("read after flush failed");
    assert_eq!(buf, data, "data mismatch after write+flush+read");

    eprintln!("PASS: flush (NOP-Out keepalive)");
    dev.disconnect().await.expect("disconnect failed");
}

// ── Full-slot and multi-extent tests ─────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_slab_full_slot_write() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let mut slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let vol = VolumeId::new();
    let slot = slab.allocate(vol, 0).await.expect("allocate");

    // Write a full 1 MB slot with a deterministic pattern
    let slot_size = DEFAULT_SLOT_SIZE as usize; // 1 MB
    let data: Vec<u8> = (0..slot_size)
        .map(|i| ((i * 67 + i / 1024 + 0x3D) & 0xFF) as u8)
        .collect();

    eprintln!("Writing full 1 MB slot to iSCSI slab...");
    slab.write_slot(slot, 0, &data).await.expect("full slot write");

    // Read back and verify
    let mut readback = vec![0u8; slot_size];
    slab.read_slot(slot, 0, &mut readback).await.expect("full slot read");

    assert_eq!(readback.len(), data.len());
    let mismatches: Vec<usize> = readback
        .iter()
        .zip(data.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    assert!(
        mismatches.is_empty(),
        "full slot data mismatch at {} offsets (first: {})",
        mismatches.len(),
        mismatches.first().unwrap_or(&0)
    );

    eprintln!("PASS: full 1 MB slot write/read/verify");
}

#[tokio::test]
#[ignore]
async fn iscsi_multi_extent_volume() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Create a volume that spans 4 extents (4 MB, slot_size=1MB)
    let vol = ThinVolume::new("multi-extent".to_string(), 4 * 1024 * 1024, DEFAULT_SLOT_SIZE);
    let handle = Arc::new(ThinVolumeHandle::new(
        vol,
        gem.clone(),
        registry.clone(),
        placement,
    ));

    // Write unique patterns to each extent
    let slot_size = DEFAULT_SLOT_SIZE as usize;
    for ext_idx in 0u64..4 {
        let pattern = (ext_idx as u8).wrapping_mul(0x37).wrapping_add(0x11);
        let data = vec![pattern; 4096];
        let offset = ext_idx * slot_size as u64;
        eprintln!("Writing 4 KB at extent {} (offset {}, pattern {:#x})...", ext_idx, offset, pattern);
        handle.write(offset, &data).await.unwrap_or_else(|e| panic!("write extent {} failed: {}", ext_idx, e));
    }

    // Verify each extent has its unique pattern
    for ext_idx in 0u64..4 {
        let expected = (ext_idx as u8).wrapping_mul(0x37).wrapping_add(0x11);
        let offset = ext_idx * slot_size as u64;
        let mut buf = vec![0u8; 4096];
        handle.read(offset, &mut buf).await.unwrap_or_else(|e| panic!("read extent {} failed: {}", ext_idx, e));
        assert!(
            buf.iter().all(|&b| b == expected),
            "extent {} data mismatch (expected {:#x}, got {:#x})",
            ext_idx,
            expected,
            buf[0]
        );
    }

    // Verify GEM has 4 allocated extents
    {
        let g = gem.lock().await;
        for ext_idx in 0u64..4 {
            assert!(
                g.lookup(handle.volume_id(), ext_idx).is_some(),
                "extent {} not in GEM",
                ext_idx
            );
        }
    }

    eprintln!("PASS: multi-extent volume (4 extents on iSCSI slab)");
}

// ── Snapshot COW on iSCSI ────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_snapshot_cow() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Create original volume and write data
    let orig_vol = ThinVolume::new("original".to_string(), 2 * 1024 * 1024, DEFAULT_SLOT_SIZE);
    let orig_id = orig_vol.id();
    let orig_handle = Arc::new(ThinVolumeHandle::new(
        orig_vol,
        gem.clone(),
        registry.clone(),
        placement.clone(),
    ));

    let orig_data = vec![0xAA; 4096];
    orig_handle.write(0, &orig_data).await.expect("write original");
    eprintln!("Wrote 0xAA to original volume");

    // Snapshot: clone the volume map in GEM and bump ref counts in slab
    let snap_vol = ThinVolume::new("snapshot".to_string(), 2 * 1024 * 1024, DEFAULT_SLOT_SIZE);
    let snap_id = snap_vol.id();
    {
        let mut g = gem.lock().await;
        let cloned = g.clone_volume_map(orig_id, snap_id);
        assert!(cloned.is_some(), "clone_volume_map returned None");

        // Bump ref_count in the slab
        let loc = g.lookup(orig_id, 0).expect("original extent not in GEM");
        let slab_id = loc.slab_id;
        let slot_idx = loc.slot_idx;
        let mut reg = registry.lock().await;
        let slab = reg.get_mut(&slab_id).expect("slab not found");
        slab.inc_ref(slot_idx).await.expect("inc_ref failed");
    }

    let snap_handle = Arc::new(ThinVolumeHandle::new(
        snap_vol,
        gem.clone(),
        registry.clone(),
        placement,
    ));

    // Verify snapshot reads the same data
    let mut snap_buf = vec![0u8; 4096];
    snap_handle.read(0, &mut snap_buf).await.expect("read snapshot");
    assert_eq!(snap_buf, orig_data, "snapshot data should match original");
    eprintln!("Snapshot reads 0xAA (matches original)");

    // Write different data to original (should trigger COW)
    let new_data = vec![0xBB; 4096];
    orig_handle.write(0, &new_data).await.expect("COW write to original");
    eprintln!("Wrote 0xBB to original (COW)");

    // Verify original now has new data
    let mut orig_buf = vec![0u8; 4096];
    orig_handle.read(0, &mut orig_buf).await.expect("read original after COW");
    assert_eq!(orig_buf, new_data, "original should have new data after COW");

    // Verify snapshot STILL has old data (isolation)
    let mut snap_buf2 = vec![0u8; 4096];
    snap_handle.read(0, &mut snap_buf2).await.expect("read snapshot after COW");
    assert_eq!(snap_buf2, orig_data, "snapshot should still have old data after COW");

    // Verify GEM now has different slots for original and snapshot
    {
        let g = gem.lock().await;
        let orig_loc = g.lookup(orig_id, 0).expect("original not in GEM");
        let snap_loc = g.lookup(snap_id, 0).expect("snapshot not in GEM");
        assert_ne!(
            orig_loc.slot_idx, snap_loc.slot_idx,
            "after COW, original and snapshot should be on different slots"
        );
        eprintln!(
            "Original on slot {}, snapshot on slot {} (isolated)",
            orig_loc.slot_idx, snap_loc.slot_idx
        );
    }

    eprintln!("PASS: snapshot COW on iSCSI slab");
}

// ── Stress: sequential writes across many extents ───────────────

#[tokio::test]
#[ignore]
async fn iscsi_sequential_write_stress() {
    let (portal, port, iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };

    let dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&portal, port, &iqn)
            .await
            .expect("connect failed"),
    );

    let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("slab format failed");

    let total_slots = slab.total_slots();
    let mut registry = SlabRegistry::new();
    registry.add(slab);
    let registry = Arc::new(Mutex::new(registry));
    let gem = Arc::new(Mutex::new(GlobalExtentMap::new()));

    let placement = PlacementPolicy {
        preferred_tier: StorageTier::Cool,
        tier_fallback: vec![StorageTier::Hot, StorageTier::Warm, StorageTier::Cold],
    };

    // Allocate 20 extents across a volume (20 MB)
    let num_extents = 20u64.min(total_slots);
    let vol = ThinVolume::new(
        "stress".to_string(),
        num_extents * DEFAULT_SLOT_SIZE,
        DEFAULT_SLOT_SIZE,
    );
    let handle = Arc::new(ThinVolumeHandle::new(
        vol,
        gem.clone(),
        registry.clone(),
        placement,
    ));

    eprintln!("Writing {} extents (4 KB each) to iSCSI slab...", num_extents);
    let start = std::time::Instant::now();

    for i in 0..num_extents {
        let pattern = ((i * 17 + 5) & 0xFF) as u8;
        let data = vec![pattern; 4096];
        let offset = i * DEFAULT_SLOT_SIZE;
        handle
            .write(offset, &data)
            .await
            .unwrap_or_else(|e| panic!("write extent {} failed: {}", i, e));
    }

    let write_elapsed = start.elapsed();
    eprintln!(
        "Wrote {} extents in {:?} ({:.1} extents/sec)",
        num_extents,
        write_elapsed,
        num_extents as f64 / write_elapsed.as_secs_f64()
    );

    // Verify all extents
    let start = std::time::Instant::now();
    for i in 0..num_extents {
        let expected = ((i * 17 + 5) & 0xFF) as u8;
        let offset = i * DEFAULT_SLOT_SIZE;
        let mut buf = vec![0u8; 4096];
        handle
            .read(offset, &mut buf)
            .await
            .unwrap_or_else(|e| panic!("read extent {} failed: {}", i, e));
        assert!(
            buf.iter().all(|&b| b == expected),
            "extent {} mismatch (expected {:#x})",
            i,
            expected
        );
    }

    let read_elapsed = start.elapsed();
    eprintln!(
        "Verified {} extents in {:?} ({:.1} extents/sec)",
        num_extents,
        read_elapsed,
        num_extents as f64 / read_elapsed.as_secs_f64()
    );

    eprintln!("PASS: sequential write stress ({} extents)", num_extents);
}

// ── Large migration (many extents) ──────────────────────────────

#[tokio::test]
#[ignore]
async fn iscsi_large_migration() {
    let (src_portal, src_port, src_iqn) = match iscsi_src() {
        Some(v) => v,
        None => return,
    };
    let (dst_portal, dst_port, dst_iqn) = match iscsi_dst() {
        Some(v) => v,
        None => {
            eprintln!("ISCSI_IQN_DST not set, skipping");
            return;
        }
    };

    // Connect source, format, allocate 10 extents
    let src_dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&src_portal, src_port, &src_iqn)
            .await
            .expect("connect src"),
    );

    let mut src_slab = Slab::format(src_dev, DEFAULT_SLOT_SIZE, StorageTier::Cool)
        .await
        .expect("format src");
    let src_id = src_slab.slab_id();

    let num_extents = 10u32;
    let mut volumes = Vec::new();
    let mut expected_data = Vec::new();

    for i in 0..num_extents {
        let vol = VolumeId::new();
        let slot = src_slab.allocate(vol, 0).await.expect("allocate");
        // Write 4KB with a unique pattern per extent
        let data: Vec<u8> = (0..4096)
            .map(|j| ((j * (i as usize + 3) + i as usize * 97) & 0xFF) as u8)
            .collect();
        src_slab.write_slot(slot, 0, &data).await.expect("write slot");
        volumes.push((vol, slot));
        expected_data.push(data);
    }

    eprintln!("Wrote {} extents to source slab", num_extents);

    // Build GEM
    let mut gem = GlobalExtentMap::new();
    for (i, (vol, slot)) in volumes.iter().enumerate() {
        gem.insert(
            *vol,
            0,
            ExtentLocation {
                slab_id: src_id,
                slot_idx: *slot,
                ref_count: 1,
                generation: 1,
            },
        );
        let _ = i;
    }

    let mut registry = SlabRegistry::new();
    registry.add(src_slab);

    // Connect destination
    let dst_dev: Arc<dyn BlockDevice> = Arc::new(
        IscsiDevice::connect(&dst_portal, dst_port, &dst_iqn)
            .await
            .expect("connect dst"),
    );

    // Migrate
    let engine = stormblock::placement::PlacementEngine::new();
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let start = std::time::Instant::now();
    eprintln!("Migrating {} extents...", num_extents);

    let result = stormblock::migrate::migrate_to_slab(
        &mut gem,
        &mut registry,
        &engine,
        src_id,
        dst_dev,
        StorageTier::Hot,
        DEFAULT_SLOT_SIZE,
        &rx,
    )
    .await
    .expect("migration failed");

    let elapsed = start.elapsed();
    eprintln!("Migration took {:?}", elapsed);

    assert_eq!(result.migrated, num_extents as u64);
    assert_eq!(result.failed, 0);

    // Verify all data on destination
    let dst_slab = registry.get(&result.dest_slab).expect("dst slab");
    let mut buf = vec![0u8; 4096];

    for (i, (vol, _)) in volumes.iter().enumerate() {
        let loc = gem.lookup(*vol, 0).expect("vol not in GEM");
        assert_eq!(loc.slab_id, result.dest_slab, "extent {} on wrong slab", i);

        dst_slab
            .read_slot(loc.slot_idx, 0, &mut buf)
            .await
            .expect("read migrated slot");
        assert_eq!(
            buf, expected_data[i],
            "extent {} data mismatch after migration",
            i
        );
    }

    assert!(gem.slab_extents(src_id).is_empty());

    eprintln!(
        "PASS: large migration ({} extents, {:?})",
        num_extents, elapsed
    );
}
