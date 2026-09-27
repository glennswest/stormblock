//! A flow-over cut short resumes from a fresh clone of the same image (#171).
//!
//! A node taking its disk moves the goldens from the appliance's clone onto
//! it in the background. A power cut in the middle left the local records
//! naming extents on the *old* clone's slab; the next boot claims a new clone,
//! the old slab was "not attached", 9087 mappings were dropped and the root
//! came up with holes — the node could not boot again. A fresh clone of the
//! same sealed image carries the same slabs, by id, with the same bytes, so
//! `boot-local` now fetches the missing slab from one and maps onto it.
//!
//! Here: an "image" slab holding a volume, a copy of it standing in for the
//! next boot's fresh clone, and a local slab onto which half the extents were
//! moved before the "cut". `STORMBLOCK_RESUME_SOURCE` hands `boot-local` the
//! fresh clone the way a claim would.

use std::process::Command;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::placement::PlacementEngine;
use stormblock::volume::VolumeManager;
use tempfile::TempDir;

const SLOT: u64 = 64 * 1024;
const EXTENTS: u64 = 16;

async fn system_slab(path: &str) -> Slab {
    let dev = Arc::new(FileDevice::open_with_capacity(path, 16 * 1024 * 1024).await.unwrap()) as Arc<dyn BlockDevice>;
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::System)
        .with_auto_metadata(dev.capacity_bytes());
    Slab::format_with(dev, fmt).await.unwrap()
}

/// The image, a fresh clone of it, and a local disk half flowed over.
async fn cut_short(dir: &TempDir) -> (String, String) {
    let img = dir.path().join("image.slab").display().to_string();
    let clone = dir.path().join("fresh-clone.slab").display().to_string();
    let local = dir.path().join("local.slab").display().to_string();

    let img_slab = system_slab(&img).await;
    let img_id = img_slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(img_slab).await;
    vm.persist_to_slab(img_id);
    let root = vm.create_volume_any("root", EXTENTS * SLOT).await.unwrap();
    let v = vm.get_volume(&root).unwrap();
    for e in 0..EXTENTS {
        v.write(e * SLOT, &vec![0x40 + e as u8; SLOT as usize]).await.unwrap();
    }
    v.flush().await.unwrap();
    vm.persist().await;
    // The next boot's clone: the same sealed image, byte for byte.
    std::fs::copy(&img, &clone).unwrap();

    // The install boot lays the local disk and moves half the extents onto it
    // before the power goes. The records now live on the local disk.
    let local_slab = system_slab(&local).await;
    let local_id = local_slab.slab_id();
    vm.add_slab(local_slab).await;
    vm.persist_to_slab(local_id);
    let engine = PlacementEngine::new();
    for e in 0..EXTENTS / 2 {
        let mut gem = vm.gem().write().await;
        let mut reg = vm.registry().write().await;
        engine.migrate_extent(&mut gem, &mut reg, root, e, Some(local_id)).await.unwrap();
    }
    v.flush().await.unwrap();
    vm.persist().await;
    drop(v);
    drop(vm);
    (local, clone)
}

fn boot_local(local: &str, resume_from: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stormblock"));
    cmd.args(["boot-local", "--slab", local, "--volume", "root", "--check"])
        .env_remove("STORMBLOCK_BOOTHOST")
        .env("RUST_LOG", "stormblock=info");
    match resume_from {
        Some(src) => cmd.env("STORMBLOCK_RESUME_SOURCE", src),
        None => cmd.env_remove("STORMBLOCK_RESUME_SOURCE"),
    };
    let out = cmd.output().expect("spawn stormblock boot-local");
    (
        out.status.success(),
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    )
}

#[tokio::test]
async fn a_cut_short_flow_over_resumes_from_a_fresh_clone() {
    let dir = TempDir::new().unwrap();
    let (local, clone) = cut_short(&dir).await;

    let (ok, text) = boot_local(&local, Some(&clone));
    assert!(ok, "boot-local failed:\n{text}");
    assert!(text.contains("to finish the flow-over"), "the fresh clone was not attached:\n{text}");
    assert!(!text.contains("mapping dropped"), "extents were still dropped:\n{text}");
}

/// Without a source the old behaviour stands, loudly: what was not moved is
/// missing, and the log says why.
#[tokio::test]
async fn without_a_source_the_unmoved_extents_are_missing_and_said_so() {
    let dir = TempDir::new().unwrap();
    let (local, _clone) = cut_short(&dir).await;
    let (_, text) = boot_local(&local, None);
    assert!(text.contains("flow-over cut short"), "no warning about the missing slab:\n{text}");
    assert!(text.contains("mapping dropped"), "expected the unmoved extents to be dropped:\n{text}");
}
