//! Build a stormcos-style boot artifact for testing `stormblock boot-local`:
//! <dir>/root.slab + <dir>/meta/volumes.dat with one named boot volume
//! carrying a recognizable payload.
//!
//! Usage: make_boot_artifact <dir> [volume-name] [slab-size-mb] [volume-size-mb]

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::raid::RaidArrayId;
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("usage: make_boot_artifact <dir> [name] [slab-mb] [vol-mb]"))?,
    );
    let name = args.next().unwrap_or_else(|| "boot-test".to_string());
    let slab_mb: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(256);
    let vol_mb: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(64);

    std::fs::create_dir_all(&dir)?;
    let slab_path = dir.join("root.slab");
    let meta_dir = dir.join("meta");
    let array_id = RaidArrayId(uuid::Uuid::new_v4());

    let dev = FileDevice::open_with_capacity(
        slab_path.to_str().unwrap(),
        slab_mb * 1024 * 1024,
    )
    .await?;
    let mut mgr = VolumeManager::with_data_dir(DEFAULT_EXTENT_SIZE, meta_dir.clone())?;
    mgr.add_backing_device(array_id, Arc::new(dev)).await;
    let vol_id = mgr
        .create_volume(&name, vol_mb * 1024 * 1024, array_id)
        .await
        .map_err(|e| anyhow::anyhow!("create volume: {e}"))?;

    // Recognizable payload in the first block so the exported device can be
    // verified from the initiator side.
    let vol = mgr.get_volume(&vol_id).expect("volume exists");
    let mut block = vec![0u8; 4096];
    block[..16].copy_from_slice(b"STORMBLOCK-BOOT!");
    vol.write(0, &block).await.map_err(|e| anyhow::anyhow!("write: {e}"))?;
    vol.flush().await.map_err(|e| anyhow::anyhow!("flush: {e}"))?;

    mgr.persist().await;

    println!("artifact ready: {}", dir.display());
    println!("  slab:   {} ({slab_mb} MB)", slab_path.display());
    println!("  volume: {name} = {} ({vol_mb} MB)", vol_id.0);
    println!("boot with: stormblock boot-local --slab {} --volume {name}", slab_path.display());
    Ok(())
}
