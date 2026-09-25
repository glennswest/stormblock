//! An XFS blank and two claims of it, as files the real xfsprogs can check
//! (#147). Driven by `ci-xfs-verify.sh`.
//!
//! Builds an XFS template on a thin volume through the engine's own template
//! lifecycle, claims two clones of it, and writes the bytes of all three into
//! `DIR/{blank,claim-a,claim-b}.img`, then prints each one's UUID as the engine
//! recorded it. `xfs_repair -n`, `blkid` and `xfs_db` then judge them from the
//! outside.
//!
//! ```text
//! cargo run --release --example xfs_verify -- DIR [SIZE_MIB]
//! ```

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::fs::template::{self, ClaimSpec, FsKind, TemplateSpec, TemplateStore};
use stormblock::raid::RaidArrayId;
use stormblock::volume::{VolumeId, VolumeManager};

async fn dump(vm: &tokio::sync::Mutex<VolumeManager>, id: VolumeId, path: &str) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dev = vm.lock().await.get_volume(&id).expect("volume");
    let size = dev.capacity_bytes();
    let mut f = std::fs::File::create(path)?;
    f.set_len(size)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(buf.len());
        dev.read(off, &mut buf[..n]).await?;
        // Sparse where the volume reads zeros, as an image file should be.
        if buf[..n].iter().any(|&b| b != 0) {
            f.seek(SeekFrom::Start(off))?;
            f.write_all(&buf[..n])?;
        }
        off += n as u64;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| "xfs-verify".into());
    let mib: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2048);
    std::fs::create_dir_all(&dir)?;
    let slab = format!("{dir}/slab.img");
    let _ = std::fs::remove_file(&slab);
    let dev = FileDevice::open_with_capacity(&slab, (mib + 512) << 20).await?;
    let mut vm = VolumeManager::new(1 << 20);
    vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), Arc::new(dev)).await;
    let vm = Arc::new(tokio::sync::Mutex::new(vm));
    let store = Arc::new(tokio::sync::Mutex::new(TemplateStore::in_memory()));

    let spec = TemplateSpec { fs: FsKind::Xfs, label: "blank".into(), ..TemplateSpec::new("xfs-blank", mib << 20) };
    let t = template::create(&vm, &store, &spec).await?;
    let blank = t.clone_source().expect("sealed");
    let allocated = vm.lock().await.get_volume_handle(&blank).unwrap().allocated().await;
    println!("blank {} uuid={} allocated={}", blank.0, t.fs_uuid.unwrap(), allocated);
    dump(&vm, blank, &format!("{dir}/blank.img")).await?;

    for name in ["claim-a", "claim-b"] {
        let c = template::claim(&vm, &store, "xfs-blank", &ClaimSpec { size_bytes: None, label: Some(name.into()) }).await?;
        println!("{name} {} uuid={} verified={}", c.volume_id.0, c.fs_uuid.unwrap(), c.verified);
        dump(&vm, c.volume_id, &format!("{dir}/{name}.img")).await?;
    }
    let _ = std::fs::remove_file(&slab);
    Ok(())
}
