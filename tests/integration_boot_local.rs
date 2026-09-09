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

/// Run `boot-local` that is **expected to keep running**, and stop it once it
/// has said what it was going to say.
///
/// A flow-over failure no longer takes the boot down: the node is served from
/// the appliance and `boot-local` goes on being the server, forever, which is
/// right and is not something `Command::output()` can wait for — it waits for
/// a process that has no intention of exiting. So: capture to a file, watch
/// for the line under test, and kill it.
fn boot_local_until(args: &[&str], marker: &str) -> String {
    let out = tempfile::NamedTempFile::new().unwrap();
    let path = out.path().to_path_buf();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .arg("boot-local")
        .args(args)
        .stdout(std::fs::File::create(&path).unwrap())
        .stderr(std::fs::File::create(path.with_extension("err")).unwrap())
        .spawn()
        .expect("spawn stormblock boot-local");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let read = || {
        format!(
            "{}{}",
            std::fs::read_to_string(&path).unwrap_or_default(),
            std::fs::read_to_string(path.with_extension("err")).unwrap_or_default()
        )
    };
    loop {
        let text = read();
        if text.contains(marker) {
            break;
        }
        // It may also simply exit — a refusal that happens before anything is
        // exported, or a platform with no ublk.
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("boot-local never said '{marker}':\n{}", read());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    read()
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
    assert!(text.contains("Attached system slab"), "attach missing:\n{text}");
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

    // A file that is not a slab at all: say so, rather than blaming the
    // metadata for a device that was never formatted.
    let empty = TempDir::new().unwrap();
    let orphan = empty.path().join("orphan.slab");
    std::fs::write(&orphan, vec![0u8; 1024 * 1024]).unwrap();
    let (ok, text) = run_boot_local(&["--slab", orphan.to_str().unwrap(), "--volume", "x"]);
    assert!(!ok);
    assert!(text.contains("bad slab magic"), "orphan-slab error unclear:\n{text}");

    // A real slab with nothing to say about itself, and no directory beside
    // it either — the error has to name both places that were looked in.
    let bare_dir = TempDir::new().unwrap();
    let bare = bare_dir.path().join("bare.slab");
    let dev = FileDevice::open_with_capacity(bare.to_str().unwrap(), 8 * 1024 * 1024)
        .await
        .unwrap();
    stormblock::drive::slab::Slab::format(
        Arc::new(dev),
        SLOT,
        stormblock::placement::topology::StorageTier::Hot,
    )
    .await
    .unwrap();
    let (ok, text) = run_boot_local(&["--slab", bare.to_str().unwrap(), "--volume", "x"]);
    assert!(!ok);
    assert!(text.contains("volumes.dat"), "missing-meta error unclear:\n{text}");
    assert!(text.contains("carries any"), "missing-meta error unclear:\n{text}");
}

/// #88 — flow-over must not format the disk the node's identity is on.
///
/// A reinstall is "boot a fresh image, then flow over onto the disk the
/// previous install was on". That disk holds tier-0: the node CA private key
/// and the ServiceAccount token signing key, neither of which anything can
/// mint again. The operator supplies a path, and a path proves nothing — so
/// the check is asked of the device.
#[tokio::test]
async fn flow_over_refuses_a_target_that_carries_a_data_slab() {
    use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
    use stormblock::placement::topology::StorageTier;

    let tmp = TempDir::new().unwrap();
    let (slab, _meta, _uuid) = build_artifact(&tmp, "boot-machine-a").await;

    // The disk the previous install left behind: a bare data slab.
    let identity = tmp.path().join("identity.disk");
    let dev = FileDevice::open_with_capacity(identity.to_str().unwrap(), 32 * 1024 * 1024)
        .await
        .unwrap();
    Slab::format_with(
        Arc::new(dev),
        SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::Data),
    )
    .await
    .unwrap();

    // The refusal is a *warning* now, not a failure: an optimisation may not
    // decide whether a node boots, so boot-local says why it is not taking the
    // drive and goes on serving the root it already has. So watch for the
    // line rather than waiting for an exit that is not coming.
    let text = boot_local_until(
        &[
            "--slab",
            slab.to_str().unwrap(),
            "--volume",
            "boot-machine-a",
            "--local-disk",
            identity.to_str().unwrap(),
        ],
        "refusing to format",
    );
    assert!(text.contains("refusing to format"), "unclear refusal:\n{text}");
    assert!(text.contains("is itself a data slab"), "did not name why:\n{text}");
    assert!(
        text.contains("boots from the appliance") || text.contains("not taking"),
        "the refusal must say the boot carries on:\n{text}"
    );

    // The slab is still there: the refusal happened before the format.
    let reopened = Slab::open(Arc::new(
        FileDevice::open(identity.to_str().unwrap()).await.unwrap(),
    ))
    .await
    .unwrap();
    assert_eq!(reopened.role(), SlabRole::Data, "the target was formatted anyway");

    // A plain disk is still a fine target — the guard is about data slabs,
    // not about caution in general.
    let plain = tmp.path().join("plain.disk");
    FileDevice::open_with_capacity(plain.to_str().unwrap(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let (_ok, text) = run_boot_local(&[
        "--slab",
        slab.to_str().unwrap(),
        "--volume",
        "boot-machine-a",
        "--local-disk",
        plain.to_str().unwrap(),
        "--check",
    ]);
    assert!(!text.contains("refusing to format"), "guard fired on a blank disk:\n{text}");
}
