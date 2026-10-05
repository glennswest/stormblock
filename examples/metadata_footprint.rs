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

/// Heap bytes in use, counted by the allocator: exact, where RSS lags
/// frees and reuse (a map built after another is dropped reuses its pages).
struct Counting;
static HEAP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        HEAP.fetch_add(l.size() as i64, std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        HEAP.fetch_sub(l.size() as i64, std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        HEAP.fetch_add(n as i64 - l.size() as i64, std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn heap() -> u64 {
    HEAP.load(std::sync::atomic::Ordering::Relaxed).max(0) as u64
}

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
    let h0 = heap();
    let mut slab = Slab::format(Arc::new(dev), slot, StorageTier::Hot).await?;
    let slots = slab.total_slots();
    let r1 = rss();
    let h1 = heap();
    println!("\nslab of {slots} slots, all free:        {:>8.1} B/slot (heap {:.1})", per(r1 - r0, slots), per(h1 - h0, slots));

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
    println!("…every slot allocated (slab side): {:>8.1} B/slot more (heap {:.1})", per(r2 - r1, i), per(heap().saturating_sub(h1), i));
    print!("allocation µs/slot by tenth of fill:");
    for t in &times {
        print!(" {t:.1}");
    }
    println!();

    // The volume side: one extent per allocated slot, forward and reverse.
    let h2 = heap();
    let mut gem = GlobalExtentMap::new();
    let sid = slab.slab_id();
    for v in 0..i {
        gem.insert(vol, v, ExtentLocation::new(sid, v as u32));
    }
    let r3 = rss();
    println!("GEM, one extent per slot:          {:>8.1} B/extent (heap)", per(heap() - h2, i));

    // What a node really holds is not one volume written front to back: a
    // thin volume is written where its filesystem writes, and copy-on-write
    // puts each new slot wherever the slab has one. Scattered: the extents of
    // 64 volumes, every third virtual extent, in random order, on random
    // slots. Clones: 16 clones of one golden, each with every extent shared.
    {
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        let mut g2 = GlobalExtentMap::new();
        let ra = heap();
        let vols: Vec<VolumeId> = (0..64).map(|_| VolumeId(uuid::Uuid::new_v4())).collect();
        let mut keys: Vec<(usize, u64)> = (0..i).map(|k| ((k % 64) as usize, (k / 64) * 3)).collect();
        keys.shuffle(&mut rng);
        let mut slots_perm: Vec<u32> = (0..i as u32).collect();
        slots_perm.shuffle(&mut rng);
        for (n, (v, vext)) in keys.iter().enumerate() {
            g2.insert(vols[*v], *vext, ExtentLocation::new(sid, slots_perm[n]));
        }
        let rb = heap();
        println!("GEM, scattered (64 vols, random):  {:>8.1} B/extent (heap)", per(rb.saturating_sub(ra), i));
        drop(g2);

        let mut g3 = GlobalExtentMap::new();
        let golden = VolumeId(uuid::Uuid::new_v4());
        let per_golden = i / 17;
        for v in 0..per_golden {
            g3.insert(golden, v, ExtentLocation::new(sid, slots_perm[v as usize]));
        }
        let rc = heap();
        for _ in 0..16 {
            g3.clone_volume_map(golden, VolumeId(uuid::Uuid::new_v4()));
        }
        let rd = heap();
        println!("GEM, clone maps (16 of a golden):  {:>8.1} B/extent (heap)", per(rd.saturating_sub(rc), per_golden * 16));
        drop(g3);
    }

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
