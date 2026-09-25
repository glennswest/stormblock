//! How fast a failed drive's volumes are rebuilt, and how that scales with
//! the rebuild queue's two knobs (#146).
//!
//! Each run lays out `DRIVES` slabs on O_DIRECT files (no page cache), fills
//! `VOLUMES` `mirror:2` volumes, fails the drive holding the most legs, and
//! rebuilds the volumes it touched with `parallel` volumes at once and
//! `extents_in_flight` extents per volume. It prints the bytes copied and
//! the rate, for several settings, so the scaling is measured rather than
//! assumed.
//!
//! ```text
//! cargo run --release --example rebuild_rate -- [DIR] [--buffered] [REPEATS]
//! ```
//!
//! `--buffered` puts the slabs on the page cache instead (`FileDevice`), so
//! the drive is out of the picture and what is left is the engine: whether
//! rebuilding more at once goes faster, or queues on a lock.
//!
//! On a virtual disk this measures the engine's scheduling, not a drive:
//! every "drive" here is a file on the same underlying device.

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::sas::SasDevice;
use stormblock::drive::BlockDevice;
use stormblock::drive::slab::Slab;
use stormblock::placement::topology::StorageTier;
use stormblock::rebuild::{RebuildConfig, Rebuilds};
use stormblock::volume::{CreateOptions, RedundancyPolicy, VolumeManager};

const DRIVES: usize = 8;
const VOLUMES: usize = 16;
const EXTENTS: u64 = 16;
const SLOT: u64 = 1 << 20;

async fn run(dir: &str, buffered: bool, parallel: usize, in_flight: usize) -> anyhow::Result<(u64, f64)> {
    let mut vm = VolumeManager::new(SLOT);
    let mut paths = Vec::new();
    let mut sids = Vec::new();
    for i in 0..DRIVES {
        let path = format!("{dir}/drive{i}.img");
        let _ = std::fs::remove_file(&path);
        let f = std::fs::File::create(&path)?;
        f.set_len(160 << 20)?;
        drop(f);
        let dev: Arc<dyn BlockDevice> = if buffered {
            Arc::new(FileDevice::open(&path).await?)
        } else {
            Arc::new(SasDevice::open_file_direct(&path, 4096).await?)
        };
        let slab = Slab::format(dev, SLOT, StorageTier::Hot).await?;
        sids.push(slab.slab_id());
        vm.add_slab(slab).await;
        paths.push(path);
    }
    let buf: Vec<u8> = (0..SLOT as usize).map(|i| (i % 251) as u8).collect();
    for v in 0..VOLUMES {
        let id = vm
            .create_volume_with(&format!("v{v}"), EXTENTS * SLOT, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
            .await?;
        let h = vm.get_volume(&id).unwrap();
        for e in 0..EXTENTS {
            h.write(e * SLOT, &buf).await?;
        }
    }
    let lost = {
        let g = vm.gem().read().await;
        let s = *sids.iter().max_by_key(|s| g.slab_extents(**s).len()).unwrap();
        s
    };
    let touched = vm.distrust_slab(lost).await;
    let volumes = Arc::new(tokio::sync::Mutex::new(vm));
    let rb = Rebuilds::new(
        volumes.clone(),
        &RebuildConfig { parallel, extents_in_flight: in_flight, ..Default::default() },
    );
    let start = Instant::now();
    let job = rb.start("bench".into(), None, touched).await;
    rb.wait(job).await;
    let secs = start.elapsed().as_secs_f64();
    let j = rb.job(job).unwrap();
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    Ok((j.bytes_copied, secs))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let buffered = args.iter().any(|a| a == "--buffered");
    let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let dir = rest.first().map(|s| s.to_string()).unwrap_or_else(|| std::env::temp_dir().join("rebuild-rate").to_string_lossy().to_string());
    let repeats: usize = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    std::fs::create_dir_all(&dir)?;
    println!(
        "{DRIVES} drives ({}), {VOLUMES} mirror:2 volumes x {EXTENTS} MiB, one drive fails; median of {repeats}",
        if buffered { "page cache" } else { "O_DIRECT" }
    );
    println!("{:>9} {:>10} {:>10} {:>8} {:>9}  {}", "parallel", "in_flight", "copied", "secs", "MiB/s", "each run, MiB/s");
    let configs = [(1, 1), (4, 1), (1, 4), (4, 4), (8, 8)];
    let mut rates: Vec<Vec<(u64, f64)>> = vec![Vec::new(); configs.len()];
    // Interleaved, so drift on a shared host does not land on one setting.
    for _ in 0..repeats {
        for (i, (p, f)) in configs.iter().enumerate() {
            rates[i].push(run(&dir, buffered, *p, *f).await?);
        }
    }
    for (i, (p, f)) in configs.iter().enumerate() {
        let mut r: Vec<f64> = rates[i].iter().map(|(b, s)| (*b as f64 / (1 << 20) as f64) / s).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = r[r.len() / 2];
        let (bytes, _) = rates[i][0];
        let secs = (bytes as f64 / (1 << 20) as f64) / med;
        let each: Vec<String> = rates[i].iter().map(|(b, s)| format!("{:.0}", (*b as f64 / (1 << 20) as f64) / s)).collect();
        println!("{p:>9} {f:>10} {:>9}M {secs:>8.2} {med:>9.1}  {}", bytes >> 20, each.join(" "));
    }
    Ok(())
}
