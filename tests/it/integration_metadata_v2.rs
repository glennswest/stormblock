//! Metadata format v2 through the volume manager (#158, #157): a node whose
//! metadata slabs are v2 keeps its volumes across a restart, a persist writes
//! what changed, persists running at once land in the order they were taken,
//! and v1 and v2 slabs sit side by side.

use std::collections::HashMap;
use std::sync::Arc;

use stormblock::drive::slab::{Slab, SlabFormat, SlabRole, SLAB_VERSION, SLAB_VERSION_2};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

const SLOT: u64 = 64 * 1024;
const VOL: u64 = 16 * 1024 * 1024; // 256 extents

async fn device(size: &str) -> Arc<dyn BlockDevice> {
    let name = format!("mv2-{}", uuid::Uuid::new_v4());
    stormblock::drive::open_path(&format!("emulated://{name}?size={size}"), false).await.unwrap()
}

async fn slab(dev: &Arc<dyn BlockDevice>, version: u32, role: SlabRole) -> Slab {
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(role)
        .with_version(version)
        .with_auto_metadata(dev.capacity_bytes());
    Slab::format_with(dev.clone(), fmt).await.unwrap()
}

fn pattern(vol: u8, extent: u64, gen: u8) -> Vec<u8> {
    let mut b = vec![vol ^ gen; 4096];
    b[..8].copy_from_slice(&extent.to_le_bytes());
    b
}

async fn reopen(devs: &[Arc<dyn BlockDevice>]) -> VolumeManager {
    let mut vm = VolumeManager::new(SLOT);
    let mut ids = Vec::new();
    for d in devs {
        let s = Slab::open(d.clone()).await.unwrap();
        ids.push(s.slab_id());
        vm.add_slab(s).await;
    }
    vm.persist_to_slabs(ids);
    vm.restore().await.unwrap();
    vm
}

async fn read_first(vm: &VolumeManager, name: &str, extent: u64) -> Vec<u8> {
    let id = vm.find_volume(name).await.unwrap_or_else(|| panic!("{name} did not come back"));
    let v = vm.get_volume(&id).unwrap();
    let mut b = vec![0u8; 4096];
    v.read(extent * SLOT, &mut b).await.unwrap();
    b
}

#[tokio::test]
async fn a_v2_node_keeps_its_volumes_across_restarts() {
    let dev = device("256M").await;
    let s = slab(&dev, SLAB_VERSION_2, SlabRole::Data).await;
    let sid = s.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s).await;
    vm.persist_to_slab(sid);

    let a = vm.create_volume_any("a", VOL).await.unwrap();
    let av = vm.get_volume(&a).unwrap();
    for e in 0..100u64 {
        av.write(e * SLOT, &pattern(1, e, 0)).await.unwrap();
    }
    av.flush().await.unwrap();
    vm.persist().await;
    let snap = vm.create_snapshot(a, "a-snap").await.unwrap();
    for e in 0..10u64 {
        av.write(e * SLOT, &pattern(1, e, 1)).await.unwrap();
    }
    let gone = vm.create_volume_any("gone", VOL).await.unwrap();
    vm.get_volume(&gone).unwrap().write(0, &pattern(9, 0, 0)).await.unwrap();
    av.flush().await.unwrap();
    vm.persist().await;
    vm.delete_volume(gone).await.unwrap();
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    let used = vm.metadata_v2_usage();
    assert_eq!(used.len(), 1, "one v2 store: {used:?}");
    drop(vm);

    // Twice: the first restart's persist writes the store whole, the second
    // reads that.
    for round in 0..2 {
        let mut vm = reopen(std::slice::from_ref(&dev)).await;
        assert!(vm.find_volume("gone").await.is_none(), "round {round}: a deleted volume came back");
        assert_eq!(read_first(&vm, "a", 3).await, pattern(1, 3, 1), "round {round}");
        assert_eq!(read_first(&vm, "a", 50).await, pattern(1, 50, 0), "round {round}");
        assert_eq!(read_first(&vm, "a-snap", 3).await, pattern(1, 3, 0), "round {round}");
        let sid2 = vm.find_volume("a-snap").await.unwrap();
        assert_eq!(sid2, snap);
        // Something new each round.
        let n = vm.create_volume_any(&format!("new{round}"), VOL).await.unwrap();
        let nv = vm.get_volume(&n).unwrap();
        nv.write(0, &pattern(7, 0, round)).await.unwrap();
        nv.flush().await.unwrap();
        vm.persist().await;
        assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    }
    let vm = reopen(std::slice::from_ref(&dev)).await;
    assert_eq!(read_first(&vm, "new0", 0).await, pattern(7, 0, 0));
    assert_eq!(read_first(&vm, "new1", 0).await, pattern(7, 0, 1));
}

/// #157: after the first persist, one more extent written costs one log
/// record, not the volume's whole record.
#[tokio::test]
async fn a_persist_writes_what_changed() {
    let dev = device("1G").await;
    let s = slab(&dev, SLAB_VERSION_2, SlabRole::Data).await;
    let sid = s.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s).await;
    vm.persist_to_slab(sid);
    let a = vm.create_volume_any("big", 64 * VOL).await.unwrap();
    let av = vm.get_volume(&a).unwrap();
    for e in 0..4000u64 {
        av.write(e * SLOT, &pattern(2, e, 0)).await.unwrap();
    }
    av.flush().await.unwrap();
    vm.persist().await;
    let before = vm.metadata_v2_usage()[0].1;
    assert!(before.pages_used > 20, "4000 extents in the tree: {before:?}");
    av.write(4000 * SLOT, &pattern(2, 4000, 0)).await.unwrap();
    av.flush().await.unwrap();
    vm.persist().await;
    let after = vm.metadata_v2_usage()[0].1;
    assert_eq!(after.pages_used, before.pages_used, "no page rewritten: {before:?} → {after:?}");
    assert!(
        after.log_used - before.log_used <= 4096,
        "one record of one page: {before:?} → {after:?}"
    );
    assert!(vm.durability_fault().is_none());
    drop(vm);
    let vm = reopen(&[dev]).await;
    assert_eq!(read_first(&vm, "big", 4000).await, pattern(2, 4000, 0));
    assert_eq!(read_first(&vm, "big", 1234).await, pattern(2, 1234, 0));
}

/// Persists running at once: each takes its changes in turn and they are
/// written in that order, so the newest value of every extent is what a
/// restart finds.
#[tokio::test]
async fn persists_at_once_land_in_the_order_they_were_taken() {
    let dev = device("512M").await;
    let s = slab(&dev, SLAB_VERSION_2, SlabRole::Data).await;
    let sid = s.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s).await;
    vm.persist_to_slab(sid);
    let mut vols = Vec::new();
    for i in 0..4u8 {
        let id = vm.create_volume_any(&format!("v{i}"), VOL).await.unwrap();
        vols.push((i, id));
    }
    let vm = Arc::new(tokio::sync::Mutex::new(vm));
    let mut want: HashMap<(u8, u64), u8> = HashMap::new();
    for gen in 1..=6u8 {
        let mut tasks = Vec::new();
        for (i, id) in &vols {
            let v = vm.lock().await.get_volume(id).unwrap();
            for e in 0..40u64 {
                if (e + gen as u64 + *i as u64) % 3 == 0 {
                    v.write(e * SLOT, &pattern(*i, e, gen)).await.unwrap();
                    want.insert((*i, e), gen);
                }
            }
            v.flush().await.unwrap();
            let vm = vm.clone();
            tasks.push(tokio::spawn(async move { VolumeManager::persist_detached(&vm).await }));
        }
        // A snapshot and its deletion between, so whole-volume changes are
        // in the mix.
        {
            let mut g = vm.lock().await;
            let s = g.create_snapshot(vols[0].1, &format!("s{gen}")).await.unwrap();
            if gen % 2 == 0 {
                g.delete_volume(s).await.unwrap();
            }
        }
        for t in tasks {
            t.await.unwrap();
        }
    }
    vm.lock().await.persist().await;
    assert!(vm.lock().await.durability_fault().is_none());
    drop(vm);
    let vm = reopen(&[dev]).await;
    for ((i, e), gen) in &want {
        assert_eq!(read_first(&vm, &format!("v{i}"), *e).await, pattern(*i, *e, *gen), "v{i} extent {e}");
    }
    assert!(vm.find_volume("s1").await.is_some());
    assert!(vm.find_volume("s2").await.is_none());
}

/// A v1 slab and a v2 slab on one node: each keeps the volumes on it, in
/// its own format.
#[tokio::test]
async fn a_v1_slab_and_a_v2_slab_side_by_side() {
    let (d1, d2) = (device("128M").await, device("128M").await);
    let s1 = slab(&d1, SLAB_VERSION, SlabRole::System).await;
    let s2 = slab(&d2, SLAB_VERSION_2, SlabRole::Data).await;
    let ids = vec![s1.slab_id(), s2.slab_id()];
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s1).await;
    vm.add_slab(s2).await;
    vm.persist_to_slabs(ids);
    let sys = vm
        .create_volume_with("sys", VOL, stormblock::volume::CreateOptions { role: Some(SlabRole::System), ..Default::default() })
        .await
        .unwrap();
    let dat = vm
        .create_volume_with("dat", VOL, stormblock::volume::CreateOptions { role: Some(SlabRole::Data), ..Default::default() })
        .await
        .unwrap();
    for (id, tag) in [(sys, 1u8), (dat, 2u8)] {
        let v = vm.get_volume(&id).unwrap();
        v.write(0, &pattern(tag, 0, 0)).await.unwrap();
        v.flush().await.unwrap();
    }
    vm.persist().await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    let v1 = Slab::open(d1.clone()).await.unwrap();
    assert!(v1.read_metadata().await.unwrap().is_some(), "the v1 slab has its v1 record");
    let v2 = Slab::open(d2.clone()).await.unwrap();
    let doc = stormblock::volume::metav2::read_slab(&v2).await.unwrap().unwrap();
    assert_eq!(doc.volumes.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), vec!["dat"]);
    drop(vm);
    let vm = reopen(&[d1, d2]).await;
    assert_eq!(read_first(&vm, "sys", 0).await, pattern(1, 0, 0));
    assert_eq!(read_first(&vm, "dat", 0).await, pattern(2, 0, 0));
}

/// The data directory in format v2: `metadata.v2` instead of `volumes.dat`.
#[tokio::test]
async fn the_data_directory_keeps_metadata_v2() {
    let dir = tempfile::tempdir().unwrap();
    // Already v2: the gate is not needed for a directory that has the file.
    std::fs::write(dir.path().join("metadata.v2"), b"").unwrap();
    let dev = device("128M").await;
    let s = slab(&dev, SLAB_VERSION, SlabRole::Data).await;
    let mut vm = VolumeManager::with_data_dir(SLOT, dir.path().to_path_buf()).unwrap();
    vm.add_slab(s).await;
    let a = vm.create_volume_any("in-dir", VOL).await.unwrap();
    let av = vm.get_volume(&a).unwrap();
    av.write(SLOT, &pattern(5, 1, 0)).await.unwrap();
    av.flush().await.unwrap();
    vm.persist().await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    assert!(!dir.path().join("volumes.dat").exists(), "no v1 record beside it");
    drop(vm);
    let mut vm = VolumeManager::with_data_dir(SLOT, dir.path().to_path_buf()).unwrap();
    vm.add_slab(Slab::open(dev).await.unwrap()).await;
    vm.restore().await.unwrap();
    assert_eq!(read_first(&vm, "in-dir", 1).await, pattern(5, 1, 0));
}
