//! An ext4 template created as the API creates one (format in core, then
//! seal, which checks it end to end) on emulated drives, memory store
//! against directory backing (#300, stormcos#92): the time, and for the
//! directory store where it goes (calls and seconds per filesystem
//! operation, `drive::emulated::dir_stats`).
//!
//! ```text
//! cargo run --release --example emulated_format -- 64G,128G,256G,512G [DIR]
//! ```

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::emulated::{self, dir_stats, parse_size};
use stormblock::drive::slab::{Slab, SlabFormat};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

async fn one(size: u64, backing: Option<&str>, tag: &str) -> anyhow::Result<f64> {
    const MIB: u64 = 1 << 20;
    let mut vm = VolumeManager::new(MIB);
    for d in 0..2 {
        let name = format!("fmt-{tag}-{d}-{}", uuid::Uuid::new_v4().simple());
        let uri = match backing {
            Some(dir) => format!("emulated://{name}?size=256T&backing={dir}/{name}"),
            None => format!("emulated://{name}?size=256T"),
        };
        let spec = emulated::EmulatedSpec::parse(&uri).unwrap()?;
        let dev: Arc<dyn BlockDevice> = Arc::new(emulated::open(&spec)?);
        let slab = Slab::format_with(dev, SlabFormat::new(MIB, StorageTier::Hot).with_auto_metadata(256 << 40)).await?;
        vm.add_slab(slab).await;
    }
    // What `POST /api/v1/fstemplates` runs: create, format in core, seal
    // (which checks the filesystem end to end).
    let vm = tokio::sync::Mutex::new(vm);
    let store = tokio::sync::Mutex::new(stormblock::fs::template::TemplateStore::default());
    let _ = dir_stats::take();
    let t = Instant::now();
    stormblock::fs::template::create(&vm, &store, &stormblock::fs::template::TemplateSpec::new("tmpl", size)).await?;
    Ok(t.elapsed().as_secs_f64())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sizes = args.get(1).cloned().unwrap_or_else(|| "64G,128G,256G,512G".into());
    let dir = args.get(2).cloned().unwrap_or_else(|| {
        let d = std::env::temp_dir().join("emulated-format");
        d.to_string_lossy().into_owned()
    });
    std::fs::create_dir_all(&dir)?;
    for s in sizes.split(',') {
        let size = parse_size(s).ok_or_else(|| anyhow::anyhow!("size {s}"))?;
        let mem = one(size, None, "mem").await?;
        let _ = dir_stats::take();
        let on_dir = one(size, Some(&dir), "dir").await?;
        let stats = dir_stats::take();
        println!("{s}: memory {mem:.1} s, directory {on_dir:.1} s");
        for (k, calls, secs) in stats {
            if calls > 0 {
                println!("    {k:<7} {calls:>9} calls {secs:>9.2} s");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}
