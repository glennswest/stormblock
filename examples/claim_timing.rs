//! What minting a clone of a sealed blank costs, step by step (#137).
//!
//! A standing clone per template (#55) exists because a claim was believed to
//! cost "seconds": a snapshot, a fresh filesystem identity, a check. This
//! measures each of those on real ext4 blanks of the sizes PVCs use, on a
//! file-backed slab, with nothing standing by:
//!
//! * **snapshot** — the copy-on-write clone: `VolumeManager::create_snapshot`
//! * **identity** — `ext4::stamp_uuid`, primary superblock only (the default)
//! * **check**    — `ext4::check`, the per-clone fsck
//! * **mint**     — `clone_volume` end to end, with and without the check
//!
//! ```text
//! cargo run --release --example claim_timing -- [DIR] [SIZES_MIB] [REPEATS]
//! cargo run --release --example claim_timing -- /tmp/ct 64,1024,10240 5
//! ```
//!
//! Medians, with min and max beside them: one pass is noise (see qd_sweep).

use std::sync::Arc;
use std::time::{Duration, Instant};

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
use stormblock::fs::template::{self, CloneSpec, TemplateSpec, TemplateStore, VmLock};
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

fn stats(mut v: Vec<Duration>) -> String {
    v.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "median {:>9.2} ms  (min {:>8.2}, max {:>8.2})",
        ms(v[v.len() / 2]),
        ms(v[0]),
        ms(v[v.len() - 1])
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        std::env::temp_dir().join("claim-timing").to_string_lossy().to_string()
    });
    let sizes: Vec<u64> = args
        .get(2)
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| vec![64, 1024, 10240]);
    let repeats: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    std::fs::create_dir_all(&dir)?;
    let slab_path = format!("{dir}/slab.img");
    let _ = std::fs::remove_file(&slab_path);
    let total: u64 = sizes.iter().sum::<u64>() * 1024 * 1024 * 2 + (1 << 30);
    let dev = FileDevice::open_with_capacity(&slab_path, total).await?;
    let slab = Slab::format(Arc::new(dev), DEFAULT_SLOT_SIZE, StorageTier::Hot).await?;
    let mut mgr = VolumeManager::new(DEFAULT_SLOT_SIZE);
    mgr.add_slab(slab).await;
    let vm: Arc<VmLock> = Arc::new(tokio::sync::Mutex::new(mgr));
    let store = Arc::new(tokio::sync::Mutex::new(TemplateStore::in_memory()));

    println!("claim timing — slot {} KiB, {repeats} repeats per step\n", DEFAULT_SLOT_SIZE / 1024);
    for mib in sizes {
        let t0 = Instant::now();
        let spec = TemplateSpec::new(format!("blank-{mib}m"), mib * 1024 * 1024);
        let t = template::create(&vm, &store, &spec).await?;
        let built = t0.elapsed();
        let source = t.clone_source().expect("sealed");
        println!("== {mib} MiB blank (built and sealed in {:.2} s)", built.as_secs_f64());

        let (mut snap, mut ident, mut check, mut mint_v, mut mint) = (vec![], vec![], vec![], vec![], vec![]);
        for i in 0..repeats {
            // The steps one at a time.
            let t = Instant::now();
            let id = vm.lock().await.create_snapshot(source, &format!("s-{mib}-{i}")).await?;
            snap.push(t.elapsed());
            let dev = vm.lock().await.get_volume(&id).expect("clone");
            let t = Instant::now();
            stormblock::fs::ext4::stamp_uuid(&dev, uuid::Uuid::new_v4(), false).await?;
            ident.push(t.elapsed());
            let t = Instant::now();
            let report = stormblock::fs::ext4::check(&dev).await?;
            check.push(t.elapsed());
            assert!(report.is_clean(), "a fresh clone must check clean");
            drop(dev);
            vm.lock().await.delete_volume(id).await?;

            // And end to end, as a claim mints.
            let t = Instant::now();
            let c = template::clone_volume(&vm, source, &CloneSpec::new(format!("v-{mib}-{i}"))).await?;
            mint_v.push(t.elapsed());
            vm.lock().await.delete_volume(c.volume_id).await?;
            let mut s = CloneSpec::new(format!("n-{mib}-{i}"));
            s.verify = false;
            let t = Instant::now();
            let c = template::clone_volume(&vm, source, &s).await?;
            mint.push(t.elapsed());
            vm.lock().await.delete_volume(c.volume_id).await?;
        }
        println!("  snapshot            {}", stats(snap));
        println!("  identity (primary)  {}", stats(ident));
        println!("  check (fsck)        {}", stats(check));
        println!("  mint, with check    {}", stats(mint_v));
        println!("  mint, no check      {}", stats(mint));
        println!();
    }
    let _ = std::fs::remove_file(&slab_path);
    Ok(())
}
