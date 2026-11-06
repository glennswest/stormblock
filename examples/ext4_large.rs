//! A very large ext4 filesystem written into (#512), on a bare emulated drive
//! and through the engine's volume, to tell the crates from the engine.
//!
//! ```text
//! cargo run --release --example ext4_large -- bare 256T [DIR] [FILES]
//! cargo run --release --example ext4_large -- volume 256T [DIR] [FILES]
//! ```
//!
//! `bare`: format and files straight on the drive (`VolumeDevice`, as the
//! engine does), no slab. `volume`: a template, a clone and the files through
//! the volume, as `POST /api/v1/fstemplates`, `…/clone` and `…/files` do. Each
//! is checked before and after the writes, and says which blocks differ.

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::slab::{Slab, SlabFormat};
use stormblock::drive::BlockDevice;
use stormblock::fs::ext4::{self, Ext4Params};
use stormblock::fs::files::{write_files, SeedFile};
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

const MIB: u64 = 1 << 20;

fn seeds(n: usize) -> Vec<SeedFile> {
    (0..n)
        .map(|i| SeedFile::new(format!("/scale/{i}.txt"), format!("file {i}\n").repeat(150).into_bytes()))
        .collect()
}

async fn check(what: &str, dev: &Arc<dyn BlockDevice>) -> anyhow::Result<bool> {
    let t = Instant::now();
    let r = ext4::check(dev).await?;
    println!("{what}: clean={} problems={} ({:.1} s)", r.is_clean(), r.problems.len(), t.elapsed().as_secs_f64());
    for p in r.problems.iter().take(12) {
        println!("   {p:?}");
    }
    Ok(r.is_clean())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bare").to_string();
    let size_s = args.get(2).cloned().unwrap_or_else(|| "256T".into());
    let dir = args.get(3).cloned().unwrap_or_else(|| "/tmp/ext4-large".into());
    let files: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(256);
    let size = stormblock::drive::emulated::parse_size(&size_s).ok_or_else(|| anyhow::anyhow!("size {size_s}"))?;
    std::fs::create_dir_all(&dir)?;

    let clean_after = if mode == "bare" {
        let dev: Arc<dyn BlockDevice> = stormblock::drive::open_path(&format!("emulated://ext4l?size={size_s}&backing={dir}"), false).await?;
        let t = Instant::now();
        // `META_BG=1`: the layout a filesystem this size is given anyway, asked
        // for at a size that formats in a moment.
        let mut params = Ext4Params::default();
        if std::env::var("META_BG").is_ok() {
            params.features = Some("meta_bg,^resize_inode".into());
        }
        ext4::format(&dev, &params).await?;
        println!("formatted {size_s} in {:.1} s", t.elapsed().as_secs_f64());
        check("after the format", &dev).await?;
        let t = Instant::now();
        write_files(&dev, &seeds(files)).await?;
        println!("{files} files in {:.1} s", t.elapsed().as_secs_f64());
        check("after the files", &dev).await?
    } else {
        let mut vm = VolumeManager::new(MIB);
        for d in 0..2 {
            let dev = stormblock::drive::open_path(&format!("emulated://ext4l{d}?size=1P&backing={dir}/{d}"), false).await?;
            let cap = dev.capacity_bytes();
            vm.add_slab(Slab::format_with(dev, SlabFormat::new(MIB, StorageTier::Hot).with_auto_metadata(cap)).await?).await;
        }
        let vm = Arc::new(stormblock::lockwatch::TrackedMutex::new(vm));
        let store = Arc::new(tokio::sync::Mutex::new(stormblock::fs::template::TemplateStore::default()));
        let t = Instant::now();
        let tmpl = stormblock::fs::template::create(&vm, &store, &stormblock::fs::template::TemplateSpec::new("tmpl", size)).await?;
        println!("template {size_s} in {:.1} s", t.elapsed().as_secs_f64());
        let t = Instant::now();
        let spec = stormblock::fs::template::CloneSpec::new("clone");
        let c = stormblock::fs::template::clone_template(&vm, &store, &tmpl.id.to_string(), &spec).await?;
        println!("cloned in {:.1} s", t.elapsed().as_secs_f64());
        let dev: Arc<dyn BlockDevice> = vm.lock().await.get_volume(&c.volume_id).unwrap();
        check("the clone", &dev).await?;
        let t = Instant::now();
        write_files(&dev, &seeds(files)).await?;
        println!("{files} files in {:.1} s", t.elapsed().as_secs_f64());
        check("after the files", &dev).await?
    };
    println!("RESULT {mode} {size_s}: {}", if clean_after { "clean" } else { "CORRUPT" });
    Ok(())
}
