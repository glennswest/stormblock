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
    // A v1 slab stays v1 unless format 2 is the default (then it migrated).
    assert_eq!(v1.format_version(), stormblock::drive::slab::default_format());
    let doc1 = stormblock::volume::metav2::read_slab(&v1).await.unwrap().unwrap();
    assert_eq!(doc1.volumes.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), vec!["sys"]);
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

/// Stage C: an idle map leaves memory and comes back at its volume's first
/// use; listing it does not load it; GC reads it from the store and frees
/// none of its slots; a write to it after a restart lands where it should.
#[tokio::test]
async fn an_idle_map_leaves_memory_and_comes_back() {
    let dev = device("256M").await;
    let s = slab(&dev, SLAB_VERSION_2, SlabRole::Data).await;
    let sid = s.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s).await;
    vm.persist_to_slab(sid);

    let g = vm.create_volume_any("golden", VOL).await.unwrap();
    let gv = vm.get_volume(&g).unwrap();
    for e in 0..50u64 {
        gv.write(e * SLOT, &pattern(3, e, 0)).await.unwrap();
    }
    gv.flush().await.unwrap();
    drop(gv);
    vm.persist().await;
    let c = vm.create_snapshot(g, "clone").await.unwrap();
    let cv = vm.get_volume(&c).unwrap();
    for e in 0..5u64 {
        cv.write(e * SLOT, &pattern(4, e, 1)).await.unwrap();
    }
    cv.flush().await.unwrap();
    drop(cv);
    vm.persist().await;

    // (With `$STORMBLOCK_METADATA_CACHE_MB` the persist has done it already.)
    vm.evict_idle(0).await;
    assert!(vm.gem().read().await.is_cold(&g));
    assert!(vm.gem().read().await.is_cold(&c));

    // A listing's numbers come from what was kept.
    let h = vm.get_volume_handle(&g).unwrap();
    assert_eq!(h.mapped().await, 50 * SLOT);
    assert!(vm.gem().read().await.is_cold(&g), "a listing loaded the map");
    drop(h);

    // GC reads the cold maps from the store and frees nothing of theirs.
    let report = stormblock::volume::gc::run_once(
        vm.gem(),
        vm.registry(),
        stormblock::volume::gc::GcOptions { confirm_against: None, ..Default::default() },
    )
    .await;
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(vm.gem().read().await.is_cold(&g), "GC loaded the map");

    // First use loads it.
    assert_eq!(read_first(&vm, "golden", 7).await, pattern(3, 7, 0));
    assert_eq!(read_first(&vm, "clone", 2).await, pattern(4, 2, 1));
    assert_eq!(read_first(&vm, "clone", 30).await, pattern(3, 30, 0));
    assert!(!vm.gem().read().await.is_cold(&g));

    // Out again; a write to the clone (a copy-on-write of the golden's slot,
    // whose map is not in memory) and a restart.
    vm.evict_idle(0).await;
    assert!(vm.gem().read().await.is_cold(&g));
    let cv = vm.get_volume(&c).unwrap();
    cv.write(20 * SLOT, &pattern(4, 20, 2)).await.unwrap();
    cv.flush().await.unwrap();
    drop(cv);
    vm.persist().await;
    vm.evict_idle(0).await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    drop(vm);
    let vm = reopen(&[dev]).await;
    assert_eq!(read_first(&vm, "clone", 20).await, pattern(4, 20, 2));
    assert_eq!(read_first(&vm, "golden", 20).await, pattern(3, 20, 0));
    assert_eq!(read_first(&vm, "clone", 49).await, pattern(3, 49, 0));
}

/// Migration (#158 E): a v1 slab's record moves into a v2 store in place.
/// A cut before the header is written leaves the v1 slab with its record;
/// done, the slab is v2 and its volumes come back.
#[tokio::test]
async fn a_v1_slab_migrates_in_place_and_a_cut_leaves_it_v1() {
    // Format 1 until the migration (this test's process only, under nextest).
    stormblock::drive::slab::set_default_format(SLAB_VERSION);
    let dev = device("256M").await;
    let s = slab(&dev, SLAB_VERSION, SlabRole::Data).await;
    let sid = s.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(s).await;
    vm.persist_to_slab(sid);
    let a = vm.create_volume_any("a", VOL).await.unwrap();
    let av = vm.get_volume(&a).unwrap();
    for e in 0..30u64 {
        av.write(e * SLOT, &pattern(6, e, 0)).await.unwrap();
    }
    av.flush().await.unwrap();
    drop(av);
    let _snap = vm.create_snapshot(a, "a-snap").await.unwrap();
    vm.persist().await;
    drop(vm);

    // Cut: everything but the header.
    let s = Slab::open(dev.clone()).await.unwrap();
    let record = s.read_metadata().await.unwrap().unwrap();
    let doc = stormblock::volume::MetadataStore::decode(&record).unwrap();
    s.prepare_v2(&record, stormblock::volume::metav2::document_entries(&doc)).await.unwrap();
    drop(s);
    let s = Slab::open(dev.clone()).await.unwrap();
    assert_eq!(s.format_version(), SLAB_VERSION, "a cut before the header leaves v1");
    let again = stormblock::volume::MetadataStore::decode(&s.read_metadata().await.unwrap().unwrap()).unwrap();
    assert_eq!(again.volumes.len(), 2, "and its record readable");
    drop(s);

    // A serving engine with format 2 the default migrates it at its first
    // persist.
    stormblock::drive::slab::set_default_format(SLAB_VERSION_2);
    let vm = reopen(std::slice::from_ref(&dev)).await;
    vm.persist().await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    let fmt = vm.registry().read().await.get(&sid).unwrap().format_version();
    assert_eq!(fmt, SLAB_VERSION_2);
    drop(vm);
    let s = Slab::open(dev.clone()).await.unwrap();
    assert_eq!(s.format_version(), SLAB_VERSION_2);
    let doc = stormblock::volume::metav2::read_slab(&s).await.unwrap().unwrap();
    assert_eq!(doc.volumes.len(), 2);
    drop(s);
    let vm = reopen(&[dev]).await;
    assert_eq!(read_first(&vm, "a", 29).await, pattern(6, 29, 0));
    assert_eq!(read_first(&vm, "a-snap", 3).await, pattern(6, 3, 0));
}

/// #156 with #158: extent size per volume, pools by size. A node with a
/// 1 MiB and an 8 MiB data slab (format 2) places a small volume in 1 MiB
/// extents and a 64 GiB one in 8 MiB extents; a clone keeps its golden's;
/// an explicit size is honoured or refused; a restart brings every volume
/// back at its size; a move never takes an extent into a slot of another size.
#[tokio::test]
async fn volumes_of_each_extent_size_live_in_their_own_pool() {
    use stormblock::volume::{CreateOptions, BULK_EXTENT};
    const MIB: u64 = 1 << 20;
    let (d1, d8) = (device("512M").await, device("1G").await);
    let mk = |dev: Arc<dyn BlockDevice>, slot: u64| async move {
        let fmt = SlabFormat::new(slot, StorageTier::Hot)
            .with_role(SlabRole::Data)
            .with_version(SLAB_VERSION_2)
            .with_auto_metadata(dev.capacity_bytes());
        Slab::format_with(dev, fmt).await.unwrap()
    };
    let (s1, s8) = (mk(d1.clone(), MIB).await, mk(d8.clone(), BULK_EXTENT).await);
    let (id1, id8) = (s1.slab_id(), s8.slab_id());
    let mut vm = VolumeManager::new(MIB);
    vm.add_slab(s1).await;
    vm.add_slab(s8).await;
    vm.persist_to_slabs(vec![id1, id8]);

    let small = vm.create_volume_any("small", 256 * MIB).await.unwrap();
    let big = vm.create_volume_any("big", 64 << 30).await.unwrap();
    let asked = vm
        .create_volume_with("asked", 256 * MIB, CreateOptions::default().with_extent_size(Some(BULK_EXTENT)))
        .await
        .unwrap();
    assert!(vm
        .create_volume_with("nowhere", MIB, CreateOptions::default().with_extent_size(Some(4 * MIB)))
        .await
        .is_err());
    assert_eq!(esize(&vm, small), MIB);
    assert_eq!(esize(&vm, big), BULK_EXTENT);
    assert_eq!(esize(&vm, asked), BULK_EXTENT);

    for (id, tag) in [(small, 1u8), (big, 2), (asked, 3)] {
        let v = vm.get_volume(&id).unwrap();
        for e in 0..4u64 {
            let off = e * esize(&vm, id) + 4096;
            v.write(off, &pattern(tag, e, 0)).await.unwrap();
        }
        v.flush().await.unwrap();
    }
    assert_eq!(legs_on(&vm, small).await, [id1].into_iter().collect());
    assert_eq!(legs_on(&vm, big).await, [id8].into_iter().collect());

    let clone = vm.create_snapshot(big, "big-clone").await.unwrap();
    assert_eq!(esize(&vm, clone), BULK_EXTENT);
    let cv = vm.get_volume(&clone).unwrap();
    cv.write(BULK_EXTENT + 8192, &pattern(9, 1, 1)).await.unwrap();
    cv.flush().await.unwrap();
    drop(cv);
    assert_eq!(legs_on(&vm, clone).await, [id8].into_iter().collect(), "the copy-on-write stayed in its pool");

    // A move of a 1 MiB extent onto the 8 MiB slab is refused.
    let r = vm.retier_volume(small, StorageTier::Hot).await;
    assert!(legs_on(&vm, small).await.iter().all(|s| *s == id1), "{r:?}");
    vm.persist().await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    drop(vm);

    let vm = reopen(&[d1, d8]).await;
    for (name, want) in [("small", MIB), ("big", BULK_EXTENT), ("big-clone", BULK_EXTENT), ("asked", BULK_EXTENT)] {
        let id = vm.find_volume(name).await.unwrap();
        assert_eq!(esize(&vm, id), want, "{name}");
    }
    assert_eq!(read_at(&vm, "small", 2 * MIB + 4096).await, pattern(1, 2, 0));
    assert_eq!(read_at(&vm, "big", 3 * BULK_EXTENT + 4096).await, pattern(2, 3, 0));
    assert_eq!(read_at(&vm, "big-clone", BULK_EXTENT + 8192).await, pattern(9, 1, 1));
    assert_eq!(read_at(&vm, "big-clone", 2 * BULK_EXTENT + 4096).await, pattern(2, 2, 0));
    assert_eq!(read_at(&vm, "asked", BULK_EXTENT + 4096).await, pattern(3, 1, 0));
}

fn esize(vm: &VolumeManager, id: stormblock::volume::VolumeId) -> u64 {
    vm.get_volume_handle(&id).unwrap().extent_size()
}

async fn legs_on(vm: &VolumeManager, id: stormblock::volume::VolumeId) -> std::collections::HashSet<stormblock::drive::slab::SlabId> {
    stormblock::volume::gem::ensure_resident(vm.gem(), id).await.unwrap();
    let g = vm.gem().read().await;
    g.get_volume_map(&id).unwrap().all_legs().map(|l| l.slab_id).collect()
}

async fn read_at(vm: &VolumeManager, name: &str, off: u64) -> Vec<u8> {
    let id = vm.find_volume(name).await.unwrap();
    let mut b = vec![0u8; 4096];
    vm.get_volume(&id).unwrap().read(off, &mut b).await.unwrap();
    b
}

/// `metadata.v2` in a data directory starts small and doubles when it fills
/// (stormcos copies the data directory a whole file at a time): no write is
/// lost to a full file, and a restart reads what the grown file holds.
#[tokio::test]
async fn the_data_directory_file_grows_as_it_fills() {
    std::env::set_var("STORMBLOCK_METADATA_DIR_INITIAL", (256 * 1024).to_string());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("metadata.v2"), b"").unwrap();
    let dev = device("1G").await;
    const SMALL: u64 = 4096;
    let fmt = SlabFormat::new(SMALL, StorageTier::Hot).with_role(SlabRole::Data).with_version(SLAB_VERSION);
    let s = Slab::format_with(dev.clone(), fmt).await.unwrap();
    let mut vm = VolumeManager::with_data_dir(SMALL, dir.path().to_path_buf()).unwrap();
    vm.add_slab(s).await;
    let id = vm.create_volume_any("many", 64 << 20).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    for round in 0..4u64 {
        for e in 0..5000u64 {
            let x = round * 5000 + e;
            v.write(x * SMALL, &pattern(8, x, 0)).await.unwrap();
        }
        v.flush().await.unwrap();
        vm.persist().await;
        assert!(vm.durability_fault().is_none(), "round {round}: {:?}", vm.durability_fault());
    }
    let len = std::fs::metadata(dir.path().join("metadata.v2")).unwrap().len();
    assert!(len > 256 * 1024, "the file grew: {len}");
    assert!(len < 64 << 20, "and stayed in proportion: {len}");
    drop(v);
    drop(vm);
    let mut vm = VolumeManager::with_data_dir(SMALL, dir.path().to_path_buf()).unwrap();
    vm.add_slab(Slab::open(dev).await.unwrap()).await;
    vm.restore().await.unwrap();
    let id = vm.find_volume("many").await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    for x in [0u64, 4999, 12345, 19999] {
        let mut b = vec![0u8; 4096];
        v.read(x * SMALL, &mut b).await.unwrap();
        assert_eq!(b, pattern(8, x, 0), "extent {x}");
    }
}

/// A data directory moves from `volumes.dat` to `metadata.v2` once format 2
/// is the default: read from the old, written to the new, the old set aside
/// so an older engine finds no record rather than a stale one.
#[tokio::test]
async fn a_data_directory_moves_to_metadata_v2() {
    stormblock::drive::slab::set_default_format(SLAB_VERSION);
    let dir = tempfile::tempdir().unwrap();
    let dev = device("128M").await;
    let s = slab(&dev, SLAB_VERSION, SlabRole::Data).await;
    let mut vm = VolumeManager::with_data_dir(SLOT, dir.path().to_path_buf()).unwrap();
    vm.add_slab(s).await;
    let id = vm.create_volume_any("kept", VOL).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    v.write(3 * SLOT, &pattern(5, 3, 0)).await.unwrap();
    v.flush().await.unwrap();
    drop(v);
    vm.persist().await;
    drop(vm);
    assert!(dir.path().join("volumes.dat").exists());

    stormblock::drive::slab::set_default_format(SLAB_VERSION_2);
    let mut vm = VolumeManager::with_data_dir(SLOT, dir.path().to_path_buf()).unwrap();
    vm.add_slab(Slab::open(dev.clone()).await.unwrap()).await;
    vm.restore().await.unwrap();
    vm.persist().await;
    assert!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    drop(vm);
    assert!(dir.path().join("metadata.v2").exists());
    assert!(!dir.path().join("volumes.dat").exists(), "the v1 record is set aside");
    assert!(dir.path().join("volumes.dat.pre-v2").exists());

    let mut vm = VolumeManager::with_data_dir(SLOT, dir.path().to_path_buf()).unwrap();
    vm.add_slab(Slab::open(dev).await.unwrap()).await;
    vm.restore().await.unwrap();
    assert_eq!(read_first(&vm, "kept", 3).await, pattern(5, 3, 0));
}
