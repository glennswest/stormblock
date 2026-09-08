//! `stormblock slab volumes` — what a slab says it holds, offline (#108).
//!
//! The point of the command is that it answers without a daemon, a reactor,
//! ublk or root, so these drive the real binary against a slab file and read
//! its stdout the way a script would: positive evidence, in the shape
//! `slab list` uses.
//!
//! The distinction the tests exist for is the one a boot decision turns on.
//! "Holds no volumes" and "keeps no volume metadata" are different answers —
//! one is a slab that is empty, the other a slab that cannot say — and a
//! caller that treats them alike boots a node off a disk that was formatted
//! and never filled.

mod common;

use std::process::Command;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, SlabFormat};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::raid::RaidArrayId;
use stormblock::volume::VolumeManager;

use tempfile::TempDir;

const SLOT: u64 = 1024 * 1024;
const CAP: u64 = 64 * 1024 * 1024;

fn slab_volumes(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .args(["slab", "volumes"])
        .args(args)
        .output()
        .expect("spawn stormblock slab volumes");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A slab file with a metadata region, as a data slab has.
async fn formatted(dir: &TempDir, name: &str) -> String {
    let path = dir.path().join(name).to_string_lossy().to_string();
    let dev: Arc<dyn BlockDevice> =
        Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
    Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_auto_metadata(CAP))
        .await
        .unwrap();
    path
}

#[tokio::test]
async fn a_slab_says_what_it_holds_with_nothing_attached() {
    let dir = TempDir::new().unwrap();
    let path = formatted(&dir, "root.slab").await;

    let (boot_id, store_id) = {
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open(&path).await.unwrap());
        let slab = Slab::open(dev).await.unwrap();
        let slab_id = slab.slab_id();
        let mut mgr = VolumeManager::new(SLOT);
        mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), slab).await.unwrap();
        // The records go *into* the slab, which is the whole point: this is
        // storage that arrived as a file and has no data directory beside it.
        mgr.persist_to_slab(slab_id);

        let boot = mgr.create_volume_any("boot-cp-01", 8 * 1024 * 1024).await.unwrap();
        let store = mgr
            .create_volume_any("image-store-stormcos-0.1.0", 4 * 1024 * 1024)
            .await
            .unwrap();
        // Write into one of them, so its slot count is not zero: an allocation
        // is what makes a thin volume occupy anything at all.
        let v = mgr.get_volume(&boot).unwrap();
        v.write(0, &vec![0x5A_u8; SLOT as usize]).await.unwrap();
        v.flush().await.unwrap();
        mgr.persist().await;
        (boot.0, store.0)
    };

    let out = slab_volumes(&[&path]);

    // Positive evidence, greppable, in `slab list`'s shape.
    assert!(
        out.contains(&format!("{path}: volume boot-cp-01 ")),
        "the boot volume must be named: {out}"
    );
    assert!(out.contains(&boot_id.to_string()), "with its uuid: {out}");
    assert!(
        out.contains(&format!("{path}: volume image-store-stormcos-0.1.0 ")),
        "and so must the other one: {out}"
    );
    assert!(out.contains(&store_id.to_string()), "{out}");
    // Sorted by name, so two runs of the same disk read the same.
    let boot_at = out.find("boot-cp-01").unwrap();
    let store_at = out.find("image-store").unwrap();
    assert!(boot_at < store_at, "volumes come out sorted: {out}");
    // The one that was written to occupies a slot; the sizes are the volume's.
    assert!(out.contains("8.0 MB, 1 slots"), "allocation is reported in slots: {out}");
    assert!(out.contains("4.0 MB, 0 slots"), "a thin volume maps nothing until written: {out}");
}

/// The failure #108 was filed for: formatted and never filled. It passes
/// `slab list` — "2047 slots, 2047 free" — and boots nothing.
#[tokio::test]
async fn an_empty_slab_says_it_holds_no_volumes() {
    let dir = TempDir::new().unwrap();
    let path = formatted(&dir, "empty.slab").await;
    let out = slab_volumes(&[&path]);
    assert!(out.contains("holds no volumes"), "{out}");
    assert!(!out.contains("keeps no volume metadata"), "an empty slab can answer: {out}");
}

/// A slab with nowhere to keep its records cannot say, which is not the same
/// as saying no — its volumes may be in the directory `rd.stormblock.meta=`
/// names.
#[tokio::test]
async fn a_slab_with_no_metadata_region_says_it_cannot_say() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("noregion.slab").to_string_lossy().to_string();
    let dev: Arc<dyn BlockDevice> =
        Arc::new(FileDevice::open_with_capacity(&path, CAP).await.unwrap());
    Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot)).await.unwrap();

    let out = slab_volumes(&[&path]);
    assert!(out.contains("keeps no volume metadata"), "{out}");
}

/// Every role gets a region, because a slab that cannot say what is on it can
/// only be read by attaching it — and `image build` has always given both
/// roles one, so a disk formatted by hand and a disk the builder laid down
/// were not the same kind of thing.
#[tokio::test]
async fn a_system_slab_formatted_by_the_cli_can_say_what_it_holds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("system.slab").to_string_lossy().to_string();
    std::fs::write(&path, vec![0u8; CAP as usize]).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .args(["slab", "format", &path, "--role", "system"])
        .output()
        .expect("spawn stormblock slab format");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("role: system"), "{text}");
    assert!(text.contains("own record: "), "the reserved region is reported: {text}");
    assert!(!text.contains("own record: none"), "a system slab reserves one: {text}");

    // "Holds no volumes" — it can answer, and the answer is that it is empty.
    // That is the answer a boot decision needs: the other one, "keeps no
    // volume metadata", would mean the question cannot be settled here.
    let out = slab_volumes(&[&path]);
    assert!(out.contains("holds no volumes"), "{out}");
}

/// And the door out, for a slab that deliberately keeps no record of itself.
#[tokio::test]
async fn metadata_bytes_zero_formats_a_slab_that_keeps_no_record() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bare.slab").to_string_lossy().to_string();
    std::fs::write(&path, vec![0u8; CAP as usize]).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .args(["slab", "format", &path, "--metadata-bytes", "0"])
        .output()
        .expect("spawn stormblock slab format");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("own record: none"), "{text}");
    assert!(slab_volumes(&[&path]).contains("keeps no volume metadata"));
}

/// Read-only, and never creates what it was asked to look at: this is the
/// command something runs on a machine it knows nothing about.
#[tokio::test]
async fn it_reads_and_never_writes() {
    let dir = TempDir::new().unwrap();
    let path = formatted(&dir, "untouched.slab").await;
    let before = std::fs::read(&path).unwrap();

    let missing = dir.path().join("not-here.img").to_string_lossy().to_string();
    let plain = dir.path().join("plain.img").to_string_lossy().to_string();
    std::fs::write(&plain, vec![0u8; 4096]).unwrap();

    let out = slab_volumes(&[&path, &plain, &missing]);
    assert!(out.contains(&format!("{plain}: not a slab")), "{out}");
    assert!(out.contains(&format!("{missing}: cannot open")), "{out}");
    assert!(
        !std::path::Path::new(&missing).exists(),
        "looking at a device must not create one"
    );
    assert_eq!(before, std::fs::read(&path).unwrap(), "the slab must come back byte-identical");
}
