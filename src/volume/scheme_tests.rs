//! #159, step 1: a volume whose header carries a scheme this engine does not
//! read is refused, never misread, and kept exactly as it was.

use std::sync::Arc;

use crate::drive::filedev::FileDevice;
use crate::drive::slab::{Slab, SlabFormat};
use crate::drive::BlockDevice;
use crate::placement::topology::StorageTier;
use crate::volume::metadata::SchemeExt;
use crate::volume::redundancy::RedundancyPolicy;
use crate::volume::{CreateOptions, VolumeId, VolumeManager};

const SLOT: u64 = 64 * 1024;

async fn drives(dir: &tempfile::TempDir, n: usize) -> Vec<Arc<dyn BlockDevice>> {
    let mut out = Vec::new();
    for i in 0..n {
        let path = dir.path().join(format!("d{i}.bin")).to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_with_capacity(&path, 32 << 20).await.unwrap());
        out.push(dev);
    }
    out
}

/// A node opening the drives as it would after a restart.
async fn reopen(devs: &[Arc<dyn BlockDevice>]) -> VolumeManager {
    let mut found = Vec::new();
    for d in devs {
        found.extend(crate::drive::discover::slabs_in_partitions(d).await);
    }
    let ids: Vec<_> = found.iter().map(|f| f.slab.slab_id()).collect();
    let mut vm = VolumeManager::new(SLOT);
    vm.adopt_slabs(found).await.unwrap();
    vm.persist_to_slabs(ids);
    vm
}

#[tokio::test]
async fn a_volume_of_a_scheme_this_engine_does_not_read_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let devs = drives(&dir, 3).await;
    // What a newer engine leaves: a parity volume whose header says
    // `ec:2+1`'s scheme (the record's own field says `raid5:2+1`).
    let (ec, plain) = {
        let mut vm = VolumeManager::new(SLOT);
        let mut ids = Vec::new();
        for d in &devs {
            let slab = Slab::format_with(d.clone(), SlabFormat::new(SLOT, StorageTier::Hot).with_metadata(4 << 20).with_version(2))
                .await
                .unwrap();
            ids.push(slab.slab_id());
            vm.add_slab(slab).await;
        }
        vm.persist_to_slabs(ids);
        let ec = vm
            .create_volume_with("ec", 8 * SLOT, CreateOptions::redundant(RedundancyPolicy::parse("raid5:2+1").unwrap()))
            .await
            .unwrap();
        let dev = vm.get_volume(&ec).unwrap();
        dev.write(0, &vec![0x11; SLOT as usize]).await.unwrap();
        dev.write(SLOT, &vec![0x22; SLOT as usize]).await.unwrap();
        dev.flush().await.unwrap();
        vm.set_scheme_ext(ec, Some(SchemeExt { code: SchemeExt::ERASURE, data: 2, parity: 1 }));
        let plain = vm.create_volume_any("plain", 4 * SLOT).await.unwrap();
        vm.get_volume(&plain).unwrap().write(0, &[7u8; 4096]).await.unwrap();
        vm.persist().await;
        (ec, plain)
    };
    let free_before: u64 = reopen(&devs).await.registry().read().await.total_free_slots();

    // This engine: the volume is there, and refused.
    let mut vm = reopen(&devs).await;
    let h = vm.get_volume_handle(&ec).expect("kept, listed");
    let why = h.refused().expect("refused").to_string();
    assert!(why.contains("ec:2+1") && why.contains("does not read"), "{why}");
    let mut buf = vec![0u8; 4096];
    assert!(h.read(0, &mut buf).await.is_err(), "never read as the raid5 its record says");
    assert!(h.write(0, &buf).await.is_err());
    assert!(h.flush().await.is_err());
    assert!(!h.resync(true).await.errors.is_empty(), "never resynced or verified");
    assert!(h.verify_stripes(&[0]).await.parity_verified == 0);
    assert!(vm.create_snapshot(ec, "clone-of-ec").await.is_err(), "never cloned");
    assert!(vm.restripe(ec, RedundancyPolicy::parse("mirror:2").unwrap()).await.is_err(), "never restriped");
    assert!(vm.compose_volume("comp", None, &[(ec, 0)]).await.is_err(), "never composed");
    // Every other volume is served as before.
    let p = vm.get_volume(&plain).unwrap();
    p.read(0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|x| *x == 7));

    // Written back as it was: another change persists the records, and the
    // scheme, the map and the slots are all still there for an engine that
    // reads it.
    let other = vm.create_volume_any("later", 4 * SLOT).await.unwrap();
    vm.get_volume(&other).unwrap().write(0, &[9u8; 4096]).await.unwrap();
    vm.persist().await;
    drop(vm);
    let again = reopen(&devs).await;
    assert_eq!(again.scheme_ext(&ec), Some(SchemeExt { code: SchemeExt::ERASURE, data: 2, parity: 1 }));
    assert!(again.get_volume_handle(&ec).unwrap().refused().is_some());
    assert!(again.gem().read().await.lookup(ec, 1).is_some(), "its map kept");
    assert_eq!(
        again.registry().read().await.total_free_slots() + 1,
        free_before,
        "the one slot `later` took; none of the refused volume's freed"
    );
    let _: VolumeId = ec;
}

#[tokio::test]
async fn a_refused_volume_can_still_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let devs = drives(&dir, 3).await;
    let ec = {
        let mut vm = VolumeManager::new(SLOT);
        let mut ids = Vec::new();
        for d in &devs {
            let slab = Slab::format_with(d.clone(), SlabFormat::new(SLOT, StorageTier::Hot).with_metadata(4 << 20).with_version(2))
                .await
                .unwrap();
            ids.push(slab.slab_id());
            vm.add_slab(slab).await;
        }
        vm.persist_to_slabs(ids);
        let ec = vm
            .create_volume_with("ec", 4 * SLOT, CreateOptions::redundant(RedundancyPolicy::parse("raid5:2+1").unwrap()))
            .await
            .unwrap();
        vm.get_volume(&ec).unwrap().write(0, &[1u8; 4096]).await.unwrap();
        vm.set_scheme_ext(ec, Some(SchemeExt { code: 9, data: 2, parity: 1 }));
        vm.persist().await;
        ec
    };
    let mut vm = reopen(&devs).await;
    assert!(vm.get_volume_handle(&ec).unwrap().refused().unwrap().contains("scheme 9"));
    vm.delete_volume(ec).await.expect("an operator may still remove it");
    assert!(vm.get_volume_handle(&ec).is_none());
}

/// #159 step 2 (decision A): an `ec:4+3` volume is refused while erasure
/// coding is off on the node, and once it is on, made, written and read back
/// after a restart. Its header carries the scheme (code 1), so an engine of
/// step 1, which does not read code 1, refuses it instead of reading it as
/// RAID-6; this engine reads it.
#[tokio::test]
async fn an_erasure_coded_volume_is_made_only_with_ec_on_and_reads_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let devs = drives(&dir, 7).await;
    let policy = RedundancyPolicy::parse("ec:4+3").unwrap();
    let datas: Vec<Vec<u8>> = (0..4u8).map(|i| vec![0x30 + i; SLOT as usize]).collect();
    let ec = {
        let mut vm = VolumeManager::new(SLOT);
        let mut ids = Vec::new();
        for d in &devs {
            let slab = Slab::format_with(d.clone(), SlabFormat::new(SLOT, StorageTier::Hot).with_metadata(4 << 20).with_version(2))
                .await
                .unwrap();
            ids.push(slab.slab_id());
            vm.add_slab(slab).await;
        }
        vm.persist_to_slabs(ids);
        let off = vm.create_volume_with("ec", 8 * SLOT, CreateOptions::redundant(policy.clone())).await;
        assert!(matches!(off, Err(crate::volume::VolumeError::ErasureCodingOff(_))), "{off:?}");
        assert!(off.unwrap_err().to_string().contains("off on this node"));
        assert!(
            vm.create_volume_with("r6", 8 * SLOT, CreateOptions::redundant(RedundancyPolicy::parse("raid6:4+2").unwrap())).await.is_ok(),
            "RAID-6 needs no switch"
        );
        vm.set_erasure_coding(true);
        let ec = vm.create_volume_with("ec", 8 * SLOT, CreateOptions::redundant(policy.clone())).await.unwrap();
        let dev = vm.get_volume(&ec).unwrap();
        for (i, d) in datas.iter().enumerate() {
            dev.write(i as u64 * SLOT, d).await.unwrap();
        }
        dev.flush().await.unwrap();
        vm.persist().await;
        ec
    };
    let vm = reopen(&devs).await;
    let want = SchemeExt { code: SchemeExt::ERASURE, data: 4, parity: 3 };
    assert_eq!(vm.scheme_ext(&ec), Some(want), "the header says ec:4+3");
    assert!(want.known(), "this engine reads it");
    let h = vm.get_volume_handle(&ec).unwrap();
    assert!(h.refused().is_none(), "not refused here");
    assert_eq!(h.redundancy().spelling(), "ec:4+3");
    assert_eq!(vm.gem().read().await.lookup_parity(ec, 0).unwrap().legs.len(), 3);
    let dev = vm.get_volume(&ec).unwrap();
    for (i, d) in datas.iter().enumerate() {
        let mut back = vec![0u8; SLOT as usize];
        dev.read(i as u64 * SLOT, &mut back).await.unwrap();
        assert_eq!(&back, d, "member {i} after the restart");
    }
}
