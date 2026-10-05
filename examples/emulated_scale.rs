//! The engine at drive counts and sizes no test machine has (#208, stormcos#92).
//!
//! Formats N emulated drives of SIZE each (in memory: nothing is stored but
//! what is written) as 1 MiB-slot slabs, writes a few extents on each, then
//! opens every slab again as a restart would, and reports:
//!
//! * the time to format and to open a slab (opening reads its slot table:
//!   64 GiB of entries for 1 PiB at 1 MiB slots);
//! * resident memory per PiB of drive, with the slabs open: the free map is
//!   1 bit a slot, so 128 MiB a PiB at 1 MiB slots (#155);
//! * what the emulated drives actually hold.
//!
//! ```text
//! cargo run --release --example emulated_scale -- [COUNT] [SIZE]   # 4 256T
//! ```

use std::time::Instant;

use stormblock::drive::emulated::{self, parse_size};
use stormblock::drive::slab::{Slab, SlabFormat};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::extent::VolumeId;

fn rss() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 4096
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let count: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let size_s = args.get(2).cloned().unwrap_or_else(|| "256T".into());
    let size = parse_size(&size_s).ok_or_else(|| anyhow::anyhow!("size {size_s}"))?;
    const MIB: u64 = 1 << 20;
    let pib = size as f64 * count as f64 / (1u64 << 50) as f64;
    println!("{count} emulated drives of {size_s} ({pib:.3} PiB), 1 MiB slots");

    let r0 = rss();
    let mut uris = Vec::new();
    let mut slabs = Vec::new();
    let t = Instant::now();
    for i in 0..count {
        let uri = format!("emulated://scale{i}?size={size_s}");
        let dev = stormblock::drive::open_path(&uri, false).await?;
        let mut slab = Slab::format_with(dev, SlabFormat::new(MIB, StorageTier::Hot)).await?;
        let vol = VolumeId(uuid::Uuid::new_v4());
        for x in 0..64u64 {
            let s = slab.allocate(vol, x).await?;
            slab.write_slot(s, 0, &vec![(x as u8) | 1; 4096]).await?;
        }
        slabs.push(slab);
        uris.push(uri);
    }
    let format_s = t.elapsed().as_secs_f64();
    let r1 = rss();
    println!("format: {:.2} s per drive", format_s / count as f64);
    println!("open slabs, after format: {:.1} MiB per PiB resident", (r1 - r0) as f64 / (1 << 20) as f64 / pib);
    drop(slabs);

    // A restart: open every slab from its drive.
    let r2 = rss();
    let t = Instant::now();
    let mut reopened = Vec::new();
    for uri in &uris {
        let dev = stormblock::drive::open_path(uri, false).await?;
        let slab = Slab::open(dev).await?;
        assert_eq!(slab.allocated_slots(), 64);
        reopened.push(slab);
    }
    let open_s = t.elapsed().as_secs_f64();
    let r3 = rss();
    println!("open: {:.2} s per drive (reads its slot table)", open_s / count as f64);
    println!("open slabs, after a restart: {:.1} MiB per PiB resident", (r3.saturating_sub(r2)) as f64 / (1 << 20) as f64 / pib);
    let stored: u64 = (0..count).map(|i| emulated::get(&format!("scale{i}")).map(|d| d.stored_bytes()).unwrap_or(0)).sum();
    println!("the drives hold {:.1} MiB of {:.3} PiB", stored as f64 / (1 << 20) as f64, pib);
    let at_160 = (r3.saturating_sub(r2)) as f64 / pib * 160.0 * (size as f64 / (1u64 << 50) as f64);
    println!("extrapolated: 160 such drives open = {:.1} GiB resident", at_160 / (1u64 << 30) as f64);
    let _ = reopened.first().map(|s| s.device().capacity_bytes());
    Ok(())
}
