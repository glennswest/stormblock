//! What the engine holds in memory per slot and per extent (#145).
//!
//! The scale target is 160 × 256 TB drives a node. Whatever the engine keeps
//! resident per slot is multiplied by ~244 million slots a drive at 1 MiB, and
//! by 160 drives — so this measures it rather than estimating it:
//!
//! * a slab formatted with N slots, all free — what an empty drive costs;
//! * every slot allocated — the slab's side of a full drive;
//! * N extents in the global extent map (forward + reverse) — the volume side;
//! * allocation time as the slab fills — `first_one()` scans the bitmap from
//!   the start on every allocation.
//!
//! RSS is read from /proc/self/statm before and after each step.
//!
//! ```text
//! cargo run --release --example metadata_footprint -- [DIR] [SLOTS]
//! ```

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, Slot};
use stormblock::placement::topology::StorageTier;
use stormblock::volume::extent::VolumeId;
use stormblock::volume::gem::{ExtentLocation, GlobalExtentMap};

fn rss() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = s.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    pages * 4096
}

fn per(bytes: u64, n: u64) -> f64 {
    bytes as f64 / n as f64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| std::env::temp_dir().join("mfp").to_string_lossy().to_string());
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1 << 20);
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/slab.img");
    let _ = std::fs::remove_file(&path);
    // Small slots: the metadata per slot is what is measured, and it does not
    // depend on how big the slot is.
    let slot = 4096u64;
    let dev = FileDevice::open_with_capacity(&path, n * slot + (n * 64) + (64 << 20)).await?;

    println!("size_of::<Slot>()           = {} B", std::mem::size_of::<Slot>());
    println!("size_of::<ExtentLocation>() = {} B", std::mem::size_of::<ExtentLocation>());

    let r0 = rss();
    let mut slab = Slab::format(Arc::new(dev), slot, StorageTier::Hot).await?;
    let slots = slab.total_slots();
    let r1 = rss();
    println!("\nslab of {slots} slots, all free:        {:>8.1} B/slot", per(r1 - r0, slots));

    // Allocate every slot, timing each tenth of the fill.
    let vol = VolumeId(uuid::Uuid::new_v4());
    let tenth = slots / 10;
    let mut times = Vec::new();
    let mut i = 0u64;
    for _ in 0..10 {
        let t = Instant::now();
        for _ in 0..tenth {
            slab.allocate(vol, i).await?;
            i += 1;
        }
        times.push(t.elapsed().as_secs_f64() * 1e6 / tenth as f64);
    }
    let r2 = rss();
    println!("…every slot allocated (slab side): {:>8.1} B/slot more", per(r2 - r1, i));
    print!("allocation µs/slot by tenth of fill:");
    for t in &times {
        print!(" {t:.1}");
    }
    println!();

    // The volume side: one extent per allocated slot, forward and reverse.
    let mut gem = GlobalExtentMap::new();
    let sid = slab.slab_id();
    for v in 0..i {
        gem.insert(vol, v, ExtentLocation::new(sid, v as u32));
    }
    let r3 = rss();
    println!("GEM, one extent per slot:          {:>8.1} B/extent", per(r3 - r2, i));

    let empty = per(r1 - r0, slots);
    let full = per(r3 - r0, slots);
    let drive_slots = 256e12 / 1048576.0;
    println!("\nextrapolated at 1 MiB slots:");
    println!("  256 TB drive, empty:  {:>8.1} GB", empty * drive_slots / 1e9);
    println!("  256 TB drive, full:   {:>8.1} GB", full * drive_slots / 1e9);
    println!("  160 drives, full:     {:>8.1} TB", full * drive_slots * 160.0 / 1e12);
    println!("  per PB, full:         {:>8.1} GB", full * (1e15 / 1048576.0) / 1e9);
    drop(gem);
    drop(slab);
    let _ = std::fs::remove_file(&path);
    Ok(())
}
