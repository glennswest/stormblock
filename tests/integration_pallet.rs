//! Pallets end to end (#51, #52).
//!
//! Everything here runs against file-backed drives, which is enough because a
//! pallet is defined entirely in terms of a block device and byte offsets
//! relative to a partition. The cases that matter are the ones the design
//! claims: several pallets on one drive, several drives, a tampered upgrade
//! that must fall back rather than strand the node, and a move that preserves
//! identity.

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::pallet::format::PalletKind;
use stormblock::pallet::manager::{PublishSpec, RecomposeSpec};
use stormblock::pallet::{
    BytesContent, Gpt, MemberKind, MemberSpec, PalletBrowser, PalletManager, PalletStore,
};

use tempfile::TempDir;

const DRIVE_BYTES: u64 = 64 * 1024 * 1024;

async fn drive(dir: &TempDir, name: &str) -> Arc<dyn BlockDevice> {
    let path = dir.path().join(name);
    let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), DRIVE_BYTES)
        .await
        .expect("open file device");
    Arc::new(dev)
}

fn member(name: &str, role: &str, kind: MemberKind, bytes: &[u8]) -> MemberSpec {
    MemberSpec::new(name, role, kind, Arc::new(BytesContent(bytes.to_vec())))
}

async fn manager(dir: &TempDir, names: &[&str]) -> PalletManager {
    let mut store = PalletStore::default();
    for n in names {
        store.add_drive(*n, drive(dir, n).await);
    }
    let mgr = PalletManager::new(store);
    for i in 0..names.len() {
        mgr.init_gpt(i, true).await.expect("init gpt");
    }
    mgr
}

fn boot_spec(name: &str, kernel: &[u8], initramfs: &[u8]) -> PublishSpec {
    let mut spec = PublishSpec::new(name, PalletKind::Boot);
    spec.members = vec![
        member("kernel", "kernel", MemberKind::Kernel, kernel),
        member("initramfs", "initramfs", MemberKind::Initramfs, initramfs),
    ];
    spec
}

#[tokio::test]
async fn a_pallet_round_trips_through_a_partition() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let mut spec = boot_spec("stormcos-boot", b"vmlinuz-payload", b"initramfs-payload");
    spec.kind = PalletKind::Boot;
    spec.version_label = "6.12.0-200.fc41".into();
    let loc = mgr.publish(spec).await.expect("publish");

    assert_eq!(loc.name, "stormcos-boot");
    assert_eq!(loc.kind, PalletKind::Boot);
    assert_eq!(loc.version, 1);
    assert_eq!(loc.version_label, "6.12.0-200.fc41");
    assert_eq!(loc.member_count, 2);
    // Sealed and read-only are the defaults, because they are what a pallet
    // exists to provide.
    assert!(loc.attributes.sealed);
    assert!(loc.attributes.read_only);

    let report = mgr.verify(loc.id).await.expect("verify");
    assert!(report.ok, "{report:?}");
    assert_eq!(report.members.len(), 2);
    assert!(report.members.iter().all(|m| m.ok));

    // The content is readable back through the extent map, byte for byte.
    let pallet = mgr.store().open(&loc).await.unwrap();
    let view = mgr.store().view(&loc).unwrap();
    let kernel = pallet.find("kernel").unwrap();
    let mut buf = vec![0u8; kernel.byte_len as usize];
    pallet.read_member(&kernel, &view, 0, &mut buf).await.unwrap();
    assert_eq!(buf, b"vmlinuz-payload");
}

#[tokio::test]
async fn many_pallets_live_on_one_drive() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    for i in 1..=3 {
        let payload = format!("kernel-v{i}");
        mgr.publish(boot_spec("stormcos-boot", payload.as_bytes(), b"initramfs"))
            .await
            .expect("publish");
    }
    // A different kind on the same drive: pallets are not one per disk, and
    // kinds do not collide.
    let mut app = PublishSpec::new("registry", PalletKind::App);
    app.members = vec![member("pause", "container", MemberKind::Container, b"pause-image")];
    mgr.publish(app).await.expect("publish app");

    let all = mgr.list().await;
    assert_eq!(all.len(), 4, "{all:#?}");
    assert_eq!(all.iter().filter(|p| p.kind == PalletKind::Boot).count(), 3);

    // Versions are assigned one past the highest of the same name.
    let mut versions: Vec<u64> =
        all.iter().filter(|p| p.name == "stormcos-boot").map(|p| p.version).collect();
    versions.sort_unstable();
    assert_eq!(versions, vec![1, 2, 3]);

    // Nothing overlaps: every partition holds its own LBA range.
    let gpt = mgr.store().gpt(0).await.unwrap();
    let mut ranges: Vec<(u64, u64)> =
        gpt.partitions().map(|(_, e)| (e.first_lba, e.last_lba)).collect();
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        assert!(w[0].1 < w[1].0, "partitions overlap: {ranges:?}");
    }
}

#[tokio::test]
async fn pallets_are_found_across_several_drives() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0", "disk1"]).await;

    let mut v1 = boot_spec("stormcos-boot", b"kernel-v1", b"initramfs");
    v1.drive = Some(0);
    let a = mgr.publish(v1).await.unwrap();
    let mut v2 = boot_spec("stormcos-boot", b"kernel-v2", b"initramfs");
    v2.drive = Some(1);
    let b = mgr.publish(v2).await.unwrap();

    assert_eq!(a.drive, "disk0");
    assert_eq!(b.drive, "disk1");

    let all = mgr.list().await;
    assert_eq!(all.len(), 2);
    // Discovery is per node, not per drive: one scan sees both.
    assert!(all.iter().any(|p| p.drive == "disk0"));
    assert!(all.iter().any(|p| p.drive == "disk1"));
}

#[tokio::test]
async fn activation_is_an_attribute_write_and_leaves_content_alone() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let old = mgr
        .publish(boot_spec("stormcos-boot", b"kernel-v1", b"initramfs"))
        .await
        .unwrap();
    mgr.activate(old.id).await.unwrap();
    mgr.mark_successful(old.id).await.unwrap();

    let new = mgr
        .publish(boot_spec("stormcos-boot", b"kernel-v2", b"initramfs"))
        .await
        .unwrap();

    // Snapshot the old pallet's bytes, then activate the new one over it.
    let view = mgr.store().view(&old).unwrap();
    let mut before = vec![0u8; 64 * 1024];
    view.read_at(0, &mut before).await.unwrap();

    mgr.activate(new.id).await.unwrap();

    let mut after = vec![0u8; 64 * 1024];
    view.read_at(0, &mut after).await.unwrap();
    assert_eq!(before, after, "activation must not write into another pallet");

    let status = mgr.status(Some(PalletKind::Boot)).await;
    assert_eq!(status.active.as_ref().unwrap().id, new.id);
    // The previous pallet is still there, still good, still selectable.
    assert!(status.available.iter().any(|p| p.id == old.id));
}

#[tokio::test]
async fn a_tampered_upgrade_is_refused_and_falls_back() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let good = mgr
        .publish(boot_spec("stormcos-boot", b"kernel-v1-good", b"initramfs"))
        .await
        .unwrap();
    mgr.activate(good.id).await.unwrap();
    mgr.mark_successful(good.id).await.unwrap();

    let bad = mgr
        .publish(boot_spec("stormcos-boot", b"kernel-v2-tampered", b"initramfs"))
        .await
        .unwrap();
    mgr.activate(bad.id).await.unwrap();

    // Rewrite a byte of the newest pallet's kernel, exactly as an attacker
    // with the disk would.
    {
        let loc = mgr.get(bad.id).await.unwrap();
        let pallet = mgr.store().open(&loc).await.unwrap();
        let view = mgr.store().view(&loc).unwrap();
        let m = pallet.find("kernel").unwrap();
        let map = pallet.map(&m, 0).unwrap();
        let at = map.partition_block * pallet.sb.block_size as u64;
        let mut b = vec![0u8; 1];
        view.read_at(at, &mut b).await.unwrap();
        b[0] ^= 0xFF;
        view.write_at(at, &b).await.unwrap();
        view.flush().await.unwrap();
    }

    let report = mgr.verify(bad.id).await.unwrap();
    assert!(!report.ok);
    assert!(report.members.iter().any(|m| m.name == "kernel" && !m.ok));

    // The read-only half of boot policy: walk the ladder and take the first
    // that verifies. The tampered upgrade is rejected; the previous pallet is
    // intact because publishing never rewrote it.
    let browser = PalletBrowser::new(mgr.store().clone());
    let (chosen, rejected) = browser.select_verified(Some(PalletKind::Boot)).await;
    assert_eq!(chosen.expect("a pallet must still be bootable").id, good.id);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0.id, bad.id);

    // And the running system can record that decision.
    let rolled = mgr.rollback(Some(PalletKind::Boot)).await.unwrap();
    assert_eq!(rolled.id, good.id);
    let status = mgr.status(Some(PalletKind::Boot)).await;
    assert_eq!(status.active.unwrap().id, good.id);
    assert!(status.failed.iter().any(|f| f.location.id == bad.id));
}

#[tokio::test]
async fn read_only_and_sealed_are_settable_and_mirrored() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;
    let loc = mgr
        .publish(boot_spec("stormcos-boot", b"kernel", b"initramfs"))
        .await
        .unwrap();

    // Clearing read-only on a sealed pallet needs an explicit override: the
    // members may be referenced by something that expects them never to move.
    assert!(mgr.set_read_only(loc.id, false, false).await.is_err());

    let unsealed = mgr.set_sealed(loc.id, false).await.unwrap();
    assert!(!unsealed.attributes.sealed);
    let writable = mgr.set_read_only(loc.id, false, false).await.unwrap();
    assert!(!writable.attributes.read_only);

    // The superblock mirror agrees with the GPT, for a consumer that never
    // sees the GPT — and the pallet still verifies, because flags are not
    // covered by the manifest digest.
    let pallet = mgr.store().open(&writable).await.unwrap();
    assert!(!pallet.sb.read_only());
    assert!(!pallet.sb.sealed());
    assert!(mgr.verify(loc.id).await.unwrap().ok);
}

#[tokio::test]
async fn a_pallet_moves_between_drives_keeping_its_identity() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0", "disk1"]).await;

    let mut spec = boot_spec("stormcos-boot", b"kernel-payload", b"initramfs-payload");
    spec.drive = Some(0);
    let src = mgr.publish(spec).await.unwrap();

    let moved = mgr.move_pallet(src.id, 1).await.unwrap();
    assert_eq!(moved.id, src.id, "identity is stable across a move");
    assert_eq!(moved.drive, "disk1");
    assert_eq!(moved.version, src.version);

    // Exactly one copy exists, and it verifies where it landed — nothing
    // inside a pallet is absolute, so the move rewrote nothing.
    let all = mgr.list().await;
    assert_eq!(all.len(), 1);
    assert!(mgr.verify(moved.id).await.unwrap().ok);
}

#[tokio::test]
async fn a_member_moves_between_pallets_as_a_new_version_of_each() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let mut from = PublishSpec::new("platform", PalletKind::App);
    from.members = vec![
        member("etcd", "container", MemberKind::Container, b"etcd-image-bytes"),
        member("coredns", "container", MemberKind::Container, b"coredns-image-bytes"),
    ];
    let from = mgr.publish(from).await.unwrap();

    let mut into = PublishSpec::new("kube", PalletKind::Kube);
    into.members = vec![member("kubelet", "container", MemberKind::Container, b"kubelet-bytes")];
    let into = mgr.publish(into).await.unwrap();

    let (dest, source) = mgr.move_member(from.id, "etcd", into.id, false).await.unwrap();

    // Both are new versions. Neither original was touched — they could not be,
    // they are sealed.
    assert_eq!(dest.name, "kube");
    assert_eq!(dest.version, into.version + 1);
    assert_eq!(dest.member_count, 2);
    assert_eq!(source.name, "platform");
    assert_eq!(source.version, from.version + 1);
    assert_eq!(source.member_count, 1);

    let dest_pallet = mgr.store().open(&dest).await.unwrap();
    let view = mgr.store().view(&dest).unwrap();
    let m = dest_pallet.find("etcd").expect("moved member is in the destination");
    let mut buf = vec![0u8; m.byte_len as usize];
    dest_pallet.read_member(&m, &view, 0, &mut buf).await.unwrap();
    assert_eq!(buf, b"etcd-image-bytes", "content survived the move");
    assert!(mgr.verify(dest.id).await.unwrap().ok);

    let src_pallet = mgr.store().open(&source).await.unwrap();
    assert!(src_pallet.find("etcd").is_err(), "moved member left the source");
    assert!(src_pallet.find("coredns").is_ok());
    assert!(mgr.verify(source.id).await.unwrap().ok);

    // The originals are still on the drive, exactly as published.
    assert_eq!(mgr.list().await.len(), 4);
    assert!(mgr.verify(from.id).await.unwrap().ok);
    assert!(mgr.verify(into.id).await.unwrap().ok);
}

#[tokio::test]
async fn recompose_drops_a_member_and_refuses_one_that_is_not_there() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let mut spec = PublishSpec::new("platform", PalletKind::System);
    spec.members = vec![
        member("a", "container", MemberKind::Container, b"aaaa"),
        member("b", "container", MemberKind::Container, b"bbbb"),
    ];
    let base = mgr.publish(spec).await.unwrap();

    let err = mgr
        .recompose(
            base.id,
            RecomposeSpec { remove: vec!["nope".into()], ..Default::default() },
        )
        .await;
    assert!(err.is_err(), "removing a member that is not there is an error");

    let next = mgr
        .recompose(base.id, RecomposeSpec { remove: vec!["a".into()], ..Default::default() })
        .await
        .unwrap();
    assert_eq!(next.member_count, 1);
    assert!(mgr.verify(next.id).await.unwrap().ok);
}

#[tokio::test]
async fn retention_keeps_the_fallback() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;

    let mut ids = Vec::new();
    for i in 1..=4 {
        let payload = format!("kernel-v{i}");
        let loc = mgr
            .publish(boot_spec("stormcos-boot", payload.as_bytes(), b"initramfs"))
            .await
            .unwrap();
        mgr.activate(loc.id).await.unwrap();
        ids.push(loc.id);
    }

    // keep is floored at 2 whatever the caller asks: the active pallet, and
    // the one fallback depends on.
    let removed = mgr.prune("stormcos-boot", 0).await.unwrap();
    assert_eq!(removed.len(), 2);
    let left = mgr.list().await;
    assert_eq!(left.len(), 2);
    assert!(left.iter().any(|p| p.id == ids[3]), "newest survives");
    assert!(left.iter().any(|p| p.id == ids[2]), "the fallback survives");

    // And the active pallet cannot simply be deleted.
    assert!(mgr.delete(ids[3], false).await.is_err());
}

#[tokio::test]
async fn a_torn_publish_is_refused_rather_than_believed() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0"]).await;
    let loc = mgr
        .publish(boot_spec("stormcos-boot", b"kernel", b"initramfs"))
        .await
        .unwrap();

    // Content lands before the superblock, so the shape an interrupted publish
    // leaves behind is a partition with no valid superblock. Simulate it.
    let view = mgr.store().view(&loc).unwrap();
    view.write_at(0, &vec![0u8; 4096]).await.unwrap();
    view.flush().await.unwrap();

    let scanned = mgr.list().await;
    assert_eq!(scanned.len(), 1);
    assert!(!scanned[0].is_readable(), "an unreadable pallet is reported, not hidden");

    // It is not a candidate for anything, and activating it is refused.
    let status = mgr.status(Some(PalletKind::Boot)).await;
    assert!(status.active.is_none());
    assert_eq!(status.failed.len(), 1);
    assert!(mgr.activate(loc.id).await.is_err());
}

#[tokio::test]
async fn gpt_survives_a_lost_primary_header() {
    let dir = TempDir::new().unwrap();
    let dev = drive(&dir, "disk0").await;
    let mut store = PalletStore::default();
    store.add_drive("disk0", dev.clone());
    let mgr = PalletManager::new(store);
    mgr.init_gpt(0, true).await.unwrap();
    mgr.publish(boot_spec("stormcos-boot", b"kernel", b"initramfs"))
        .await
        .unwrap();

    // Wipe the primary header — LBA 1, and the table is in 512-byte LBAs
    // because an image has to be readable by something that assumes that.
    // Both copies are written on every mutation exactly so this is
    // recoverable.
    let whole = stormblock::pallet::PartitionView::whole(dev.clone());
    whole.write_at(512, &[0u8; 512]).await.unwrap();
    whole.flush().await.unwrap();

    let gpt = Gpt::read(&dev).await.expect("backup GPT");
    assert!(gpt.recovered_from_backup);
    assert_eq!(gpt.pallets().count(), 1);
    assert_eq!(mgr.list().await.len(), 1);
}

#[tokio::test]
async fn a_whole_drive_pallet_is_still_found_and_can_be_adopted() {
    let dir = TempDir::new().unwrap();
    let old = drive(&dir, "legacy").await;
    let new = drive(&dir, "disk0").await;

    // The arrangement that predates partitioned drives: one pallet, superblock
    // at byte zero, no table anywhere.
    let view = stormblock::pallet::PartitionView::whole(old.clone());
    stormblock::pallet::PalletBuilder::new("stormcos-boot", 4)
        .kind(PalletKind::Boot)
        .block_size(4096)
        .member(member("kernel", "kernel", MemberKind::Kernel, b"legacy-kernel"))
        .publish(&view)
        .await
        .expect("publish whole-drive pallet");

    let mut store = PalletStore::default();
    store.add_drive("legacy", old.clone());
    store.add_drive("disk0", new.clone());
    let mgr = PalletManager::new(store);
    mgr.init_gpt(1, true).await.unwrap();

    // It is discovered, not lost, even though there is no partition table.
    let all = mgr.list().await;
    assert_eq!(all.len(), 1);
    assert!(all[0].is_whole_drive());
    assert_eq!(all[0].name, "stormcos-boot");
    assert!(mgr.verify(all[0].id).await.unwrap().ok);

    // It cannot join the priority ladder — there is no GPT entry to record it.
    assert!(mgr.activate(all[0].id).await.is_err());
    // And subdividing its own drive in place is refused rather than attempted.
    assert!(mgr.init_gpt(0, false).await.is_err());

    let adopted = mgr.adopt_whole_drive(0, 1).await.expect("adopt");
    assert_eq!(adopted.drive, "disk0");
    assert!(!adopted.is_whole_drive());
    assert_eq!(adopted.version, 4);
    assert!(mgr.verify(adopted.id).await.unwrap().ok);
    // Now it can be selected like any other pallet.
    mgr.activate(adopted.id).await.unwrap();
    assert_eq!(
        mgr.status(Some(PalletKind::Boot)).await.active.unwrap().id,
        adopted.id
    );

    // The source drive can then be subdivided and carry several pallets.
    mgr.init_gpt(0, true).await.unwrap();
    for i in 1..=2 {
        let mut s = PublishSpec::new(format!("data-{i}"), PalletKind::Data);
        s.drive = Some(0);
        s.members = vec![member("blob", "data", MemberKind::Raw, b"payload")];
        mgr.publish(s).await.unwrap();
    }
    assert_eq!(mgr.list().await.iter().filter(|p| p.drive == "legacy").count(), 2);
}

#[tokio::test]
async fn publishing_spills_onto_the_next_drive_with_room() {
    let dir = TempDir::new().unwrap();
    let mgr = manager(&dir, &["disk0", "disk1"]).await;

    // Fill disk0 with pallets sized to leave nothing useful behind.
    let payload = vec![7u8; 20 * 1024 * 1024];
    for i in 0..3 {
        let mut s = PublishSpec::new(format!("filler-{i}"), PalletKind::Data);
        s.members = vec![member("blob", "data", MemberKind::Raw, &payload)];
        s.drive = Some(0);
        if mgr.publish(s).await.is_err() {
            break;
        }
    }
    // With no drive named, the publish looks for one with room rather than
    // failing on the first.
    let mut s = PublishSpec::new("stormcos-boot", PalletKind::Boot);
    s.members = vec![member("kernel", "kernel", MemberKind::Kernel, &payload)];
    let loc = mgr.publish(s).await.expect("publish should find room");
    assert_eq!(loc.drive, "disk1");
    assert!(mgr.verify(loc.id).await.unwrap().ok);
}
