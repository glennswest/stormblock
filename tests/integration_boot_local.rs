//! boot-local CLI integration tests (issue #12).
//!
//! Builds the artifact stormcos produces — a slab file plus meta/volumes.dat
//! carrying a named boot volume — then drives the real `stormblock boot-local`
//! binary against it. The ublk export step needs Linux 6.0+ with ublk_drv and
//! root, so the assertion stops at the point the platform allows: the slab
//! must attach non-destructively, metadata must restore, and the boot volume
//! must resolve by name or UUID.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::raid::RaidArrayId;
use stormblock::volume::VolumeManager;

use tempfile::TempDir;

const SLOT: u64 = 4096;

/// Build a stormcos-style artifact: <dir>/root.slab + <dir>/meta/volumes.dat
/// with a named volume carrying a recognizable payload. Returns the volume id.
async fn build_artifact(dir: &TempDir, volume_name: &str) -> (PathBuf, PathBuf, uuid::Uuid) {
    let slab_path = dir.path().join("root.slab");
    let meta_dir = dir.path().join("meta");
    let array_id = RaidArrayId(uuid::Uuid::new_v4());

    let dev = FileDevice::open_with_capacity(slab_path.to_str().unwrap(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let mut mgr = VolumeManager::with_data_dir(SLOT, meta_dir.clone()).unwrap();
    mgr.add_backing_device(array_id, Arc::new(dev)).await;
    let vol_id = mgr
        .create_volume(volume_name, 4 * 1024 * 1024, array_id)
        .await
        .unwrap();
    let vol = mgr.get_volume(&vol_id).unwrap();
    vol.write(0, &vec![0x5A_u8; SLOT as usize]).await.unwrap();
    vol.flush().await.unwrap();

    mgr.persist().await;

    (slab_path, meta_dir, vol_id.0)
}

fn run_boot_local(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .arg("boot-local")
        .args(args)
        .output()
        .expect("spawn stormblock boot-local");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[tokio::test]
async fn boot_local_attaches_and_resolves_by_name() {
    let dir = TempDir::new().unwrap();
    let (slab, _meta, vol_uuid) = build_artifact(&dir, "boot-machine-a").await;

    // meta dir defaults to "meta" next to the slab — exercise the default.
    // --check validates attach/restore/resolve and exits before ublk export,
    // so this passes identically on Linux and macOS.
    let (ok, text) = run_boot_local(&[
        "--slab",
        slab.to_str().unwrap(),
        "--volume",
        "boot-machine-a",
        "--check",
    ]);
    assert!(ok, "check mode must exit 0:\n{text}");
    assert!(text.contains("Attached slab"), "attach missing:\n{text}");
    assert!(
        text.contains("Boot volume: boot-machine-a"),
        "resolve missing:\n{text}"
    );
    assert!(text.contains(&vol_uuid.to_string()), "uuid missing:\n{text}");
    assert!(text.contains("/dev/ublkb0"), "export plan missing:\n{text}");
    assert!(text.contains("boot-local check OK"), "check marker missing:\n{text}");
}

#[tokio::test]
async fn boot_local_resolves_from_boot_toml() {
    let dir = TempDir::new().unwrap();
    let (slab, meta, vol_uuid) = build_artifact(&dir, "boot-machine-b").await;

    // The initramfs handoff BootManager::initramfs_config generates.
    let boot_toml = dir.path().join("boot.toml");
    std::fs::write(
        &boot_toml,
        format!("[boot]\nvolume = \"{vol_uuid}\"\nserver = \"127.0.0.1:9090\"\n"),
    )
    .unwrap();

    let (ok, text) = run_boot_local(&[
        "--slab",
        slab.to_str().unwrap(),
        "--meta",
        meta.to_str().unwrap(),
        "--boot-config",
        boot_toml.to_str().unwrap(),
        "--check",
    ]);
    assert!(ok, "check mode must exit 0:\n{text}");
    assert!(
        text.contains("Boot volume: boot-machine-b"),
        "boot.toml resolve missing:\n{text}"
    );
}

#[tokio::test]
async fn boot_local_rejects_unknown_volume_and_missing_meta() {
    let dir = TempDir::new().unwrap();
    let (slab, _meta, _) = build_artifact(&dir, "boot-machine-c").await;

    let (ok, text) = run_boot_local(&[
        "--slab",
        slab.to_str().unwrap(),
        "--volume",
        "no-such-volume",
    ]);
    assert!(!ok);
    assert!(text.contains("not found"), "unexpected error:\n{text}");
    // The error must name what IS there, for debuggability at 3am in an initramfs.
    assert!(text.contains("boot-machine-c"), "no volume inventory:\n{text}");

    let empty = TempDir::new().unwrap();
    let orphan = empty.path().join("orphan.slab");
    std::fs::write(&orphan, vec![0u8; 1024 * 1024]).unwrap();
    let (ok, text) = run_boot_local(&["--slab", orphan.to_str().unwrap(), "--volume", "x"]);
    assert!(!ok);
    assert!(text.contains("volumes.dat"), "missing-meta error unclear:\n{text}");
}
