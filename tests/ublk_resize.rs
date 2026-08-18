//! Live ublk resize against a real kernel (#19).
//!
//! Layer 3 of the expand path: `resize_volume` moves the volume's virtual size,
//! and without `UBLK_U_CMD_UPDATE_SIZE` the block device keeps the capacity it
//! was given at `SET_PARAMS` — so `/dev/ublkbN` stays put, `xfs_growfs` finds
//! nothing to grow into, and the resize is invisible to everything above.
//!
//! Nothing here can be faked: it needs `ublk_drv` loaded, root, and a kernel
//! new enough to offer `UBLK_F_UPDATE_SIZE` (6.12+). Ignored by default.
//!
//! Run: `cargo test --test ublk_resize -- --ignored --nocapture`

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use stormblock::drive::ublk::UblkServer;
use stormblock::drive::BlockDevice;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

const SLOT: u64 = 4 * 1024 * 1024;
const START: u64 = 64 * 1024 * 1024;
const GROWN: u64 = 192 * 1024 * 1024;

/// What the kernel says the block device holds, in bytes.
fn kernel_capacity(dev_path: &str) -> Option<u64> {
    let name = dev_path.trim_start_matches("/dev/");
    // /sys/block/<name>/size is in 512-byte sectors, always.
    let raw = std::fs::read_to_string(format!("/sys/block/{name}/size")).ok()?;
    raw.trim().parse::<u64>().ok().map(|sectors| sectors * 512)
}

/// Grow a live ublk device and check the kernel agrees, with I/O never stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn a_live_ublk_device_follows_its_volume_growing() {
    if !std::path::Path::new("/dev/ublk-control").exists() {
        panic!("/dev/ublk-control missing — modprobe ublk_drv (this test is about the kernel)");
    }

    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 1024 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let vol_id = vm.create_volume("ublk-resize", START, array_id).await.unwrap();
    let volume = vm.get_volume(&vol_id).unwrap();

    // Export it. run() holds non-Send pointers, so it gets its own thread with
    // its own runtime, exactly like the CSI export path does.
    let server = Arc::new(UblkServer::new(volume.clone()).with_dev_id(9));
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let runner = server.clone();
    let thread = std::thread::Builder::new()
        .name("ublk-resize-test".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            if let Err(e) = rt.block_on(runner.run(rx)) {
                eprintln!("ublk server exited: {e}");
            }
        })
        .unwrap();

    // Wait for the device to appear.
    let dev_path = server.dev_path();
    let mut waited = 0;
    while kernel_capacity(&dev_path).is_none() && waited < 100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    let before = kernel_capacity(&dev_path)
        .unwrap_or_else(|| panic!("{dev_path} never appeared — is ublk_drv loaded?"));
    assert_eq!(before, START, "{dev_path} came up at the wrong size");
    assert!(
        server.resizable(),
        "this kernel did not offer UBLK_F_UPDATE_SIZE — 6.12+ is needed for the resize"
    );

    // Keep writing through the device for the whole resize: an expand that
    // needs a quiesce is not an expand, it is an outage on a live /var.
    let writer_path = dev_path.clone();
    let writer = tokio::task::spawn_blocking(move || {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&writer_path)?;
        let block = vec![0x5Au8; 4096];
        for i in 0..200u64 {
            f.seek(SeekFrom::Start((i % 16) * 4096))?;
            f.write_all(&block)?;
            f.flush()?;
            std::thread::sleep(Duration::from_millis(5));
        }
        std::io::Result::Ok(())
    });

    // Grow the volume, then tell the kernel — the two halves of the fix.
    vm.resize_volume(vol_id, GROWN).await.expect("volume grows");
    assert_eq!(volume.capacity_bytes(), GROWN);
    server.update_size(GROWN).expect("ublk device follows");

    // The kernel's own view is the assertion that matters.
    let after = kernel_capacity(&dev_path).expect("device still there");
    assert_eq!(after, GROWN, "{dev_path} did not follow the volume");

    // And the I/O that was in flight throughout came through unharmed.
    writer.await.unwrap().expect("writes kept flowing across the resize");

    // Read past the old end, which only exists because the device grew.
    let read_back = tokio::task::spawn_blocking({
        let p = dev_path.clone();
        move || {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&p)?;
            f.seek(SeekFrom::Start(START + 4096))?;
            let mut buf = vec![0u8; 4096];
            f.read_exact(&mut buf)?;
            std::io::Result::Ok(buf)
        }
    })
    .await
    .unwrap();
    assert!(read_back.is_ok(), "reading past the old end failed: {:?}", read_back.err());

    let _ = shutdown.send(true);
    let _ = thread.join();
}

/// A resize the volume layer refuses never reaches the kernel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn shrinking_is_refused_before_it_reaches_the_device() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 512 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let vol_id = vm.create_volume("ublk-shrink", GROWN, array_id).await.unwrap();

    let err = vm.resize_volume(vol_id, START).await.unwrap_err();
    assert!(
        matches!(err, stormblock::volume::VolumeError::ShrinkRefused { .. }),
        "{err}"
    );
    assert_eq!(vm.get_volume(&vol_id).unwrap().capacity_bytes(), GROWN);
}
