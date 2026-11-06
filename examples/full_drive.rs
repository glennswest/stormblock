//! What a drive really full costs in memory at v23 (#513, stormcos#93).
//!
//! `metadata_footprint` multiplies a small slab up. This fills one: a format 2
//! slab on an emulated drive of the real size and slot size, backed by a
//! directory, and volumes written through `ThinVolumeHandle::write` with their
//! flushes and persists. A block of zeros over nothing stores nothing, so what
//! lands on the job's disk is the slot table and the volume records. Heap is
//! counted by the allocator, so the emulated drive's page cache and the
//! kernel's are not in it; the peak says what a step needed at its worst.
//!
//! Reports, with the bytes per extent of each:
//!
//! * **formatted**: the drive open, nothing written;
//! * **hot**: every volume's map in memory, as a node that has touched them all;
//! * **cold**: every map out (`evict_idle`), what the node keeps for idle volumes;
//! * **restart**: slabs opened and volumes restored from the drive alone, its
//!   time and peak, and what stays;
//! * **run on**: the first persist after it, then every map out, in, and out;
//! * **one volume back**: the first read of a cold volume, its map loaded whole.
//!
//! ```text
//! cargo run --release --example full_drive -- DIR SIZE SLOT_MIB VOLUMES [FILL_PERCENT]
//! cargo run --release --example full_drive -- /work/fd 256T 8 1024
//! ```

use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::emulated::parse_size;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole, SLAB_VERSION_2};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

struct Counting;
static HEAP: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);
fn grew(by: i64) {
    let now = HEAP.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        grew(l.size() as i64);
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        HEAP.fetch_sub(l.size() as i64, Relaxed);
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        grew(n as i64 - l.size() as i64);
        unsafe { std::alloc::System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn heap() -> i64 {
    HEAP.load(Relaxed)
}
fn peak_reset() {
    PEAK.store(heap(), Relaxed);
}
fn peak() -> i64 {
    PEAK.load(Relaxed)
}
fn rss() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 4096
}
fn mib(b: i64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

fn line(what: &str, base: i64, extents: u64, secs: f64) {
    let now = heap() - base;
    println!(
        "{what:<22} heap {:>10.1} MiB ({:>6.2} B/extent)  peak {:>10.1} MiB ({:>6.2} B/extent)  rss {:>9.1} MiB  {secs:>8.1} s",
        mib(now),
        now as f64 / extents.max(1) as f64,
        mib(peak() - base),
        (peak() - base) as f64 / extents.max(1) as f64,
        rss() as f64 / (1u64 << 20) as f64,
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| "/tmp/full-drive".into());
    let size_s = args.get(2).cloned().unwrap_or_else(|| "256T".into());
    let slot_mib: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    let volumes: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let fill: u64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(100);
    parse_size(&size_s).ok_or_else(|| anyhow::anyhow!("size {size_s}"))?;
    let slot = slot_mib << 20;
    std::fs::create_dir_all(&dir)?;
    let uri = format!("emulated://full-drive?size={size_s}&backing={dir}");
    let base = heap();
    // A step that takes long says so, whatever it is doing.
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let avail = mem.lines().find(|l| l.starts_with("MemAvailable")).unwrap_or("").to_string();
        eprintln!("  [every 2 min] heap {:.1} MiB, peak {:.1} MiB, rss {:.1} MiB, {avail}", mib(heap() - base), mib(peak() - base), rss() as f64 / (1u64 << 20) as f64);
    });

    peak_reset();
    let t = Instant::now();
    let dev: Arc<dyn BlockDevice> = stormblock::drive::open_path(&uri, false).await?;
    // The default region is too small to measure this: a fill stops at about
    // 94 % of the drive, and a restart's first persist, which writes the store
    // whole beside the old one, past about half (#513's findings). Four times
    // it, or METADATA_X times (1: the default), so a fill reaches the end.
    let mut fmt = SlabFormat::new(slot, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(SLAB_VERSION_2)
        .with_auto_metadata(dev.capacity_bytes());
    fmt.metadata_bytes *= std::env::var("METADATA_X").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(4);
    println!("metadata region {:.1} MiB", fmt.metadata_bytes as f64 / (1u64 << 20) as f64);
    let slab = Slab::format_with(dev.clone(), fmt).await?;
    let sid = slab.slab_id();
    let total = slab.total_slots();
    let mut vm = VolumeManager::new(slot);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    println!("{size_s} drive, {slot_mib} MiB extents: {total} slots, {volumes} volumes, filled {fill}%");
    line("formatted", base, total, t.elapsed().as_secs_f64());

    // Leave what a volume's records and a little slack need.
    let target = total * fill / 100 - total * fill / 100 / 1000;
    let per_volume = (target / volumes).max(1);
    let extents = per_volume * volumes;

    let mut handles = Vec::new();
    for i in 0..volumes {
        let id = vm.create_volume_any(&format!("v{i}"), per_volume * slot).await?;
        handles.push(vm.get_volume(&id).unwrap());
    }

    // Written a batch at a time, a batch from every volume at once; each batch
    // flushed and persisted, as a busy node's are.
    peak_reset();
    let t = Instant::now();
    let zero = Arc::new(vec![0u8; 4096]);
    // About 256 Ki extents between flushes: a write costs ~2.3 KiB until its
    // flush has published it, so 4 M at once took 10 GiB.
    let batch_extents = (262_144 / volumes).max(1);
    let mut done = 0u64;
    let mut last = Instant::now();
    let mut at = 0u64;
    while at < per_volume {
        let end = (at + batch_extents).min(per_volume);
        let batch = Instant::now();
        let mut tasks = Vec::with_capacity(handles.len());
        for h in &handles {
            let (h, zero) = (h.clone(), zero.clone());
            tasks.push(tokio::spawn(async move {
                for e in at..end {
                    h.write(e * slot, &zero).await?;
                }
                h.flush().await
            }));
        }
        for task in tasks {
            task.await??;
        }
        done += (end - at) * volumes;
        at = end;
        let wrote = batch.elapsed().as_secs_f64();
        vm.persist().await;
        let persisted = batch.elapsed().as_secs_f64() - wrote;
        anyhow::ensure!(vm.durability_fault().is_none(), "records: {:?} at {done} extents", vm.durability_fault());
        if last.elapsed().as_secs() >= 60 || persisted > 30.0 {
            println!(
                "  {done} of {extents} extents, {:.0} /s, heap {:.1} MiB, peak so far {:.1} MiB; this batch: write+flush {wrote:.1} s, persist {persisted:.1} s",
                done as f64 / t.elapsed().as_secs_f64(),
                mib(heap() - base),
                mib(peak() - base),
            );
            last = Instant::now();
        }
    }
    let fill_s = t.elapsed().as_secs_f64();
    println!("{extents} extents written in {fill_s:.0} s ({:.0} /s)", extents as f64 / fill_s);
    line("hot, every map resident", base, extents, fill_s);

    // Cold: the handles here are what a served volume's export would be, and
    // keep a map in; idle volumes have none.
    drop(handles);
    peak_reset();
    let t = Instant::now();
    let out = vm.evict_idle(0).await;
    println!("{out} of {volumes} maps out of memory");
    line("cold, every map out", base, extents, t.elapsed().as_secs_f64());

    drop(vm);
    let after_drop = heap() - base;
    println!("everything dropped: heap {:.1} MiB", mib(after_drop));

    // A restart, from the drive alone.
    peak_reset();
    let t = Instant::now();
    let slab = Slab::open(dev.clone()).await?;
    anyhow::ensure!(slab.allocated_slots() >= extents, "{} slots allocated of {extents}", slab.allocated_slots());
    line("restart: slab open", base, extents, t.elapsed().as_secs_f64());
    let mut vm = VolumeManager::new(slot);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    vm.restore().await?;
    line("restart: restored", base, extents, t.elapsed().as_secs_f64());
    peak_reset();
    let t = Instant::now();
    let out = vm.evict_idle(0).await;
    println!("{out} maps out after the restart");
    line("restart: every map out", base, extents, t.elapsed().as_secs_f64());

    // A node that has run on: its first persist after the restart (which turns
    // the pager on), every map out, every map back, every map out again.
    peak_reset();
    let t = Instant::now();
    vm.persist().await;
    anyhow::ensure!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    line("first persist", base, extents, t.elapsed().as_secs_f64());
    peak_reset();
    let t = Instant::now();
    let out = vm.evict_idle(0).await;
    println!("{out} of {volumes} maps out");
    line("run on: every map out", base, extents, t.elapsed().as_secs_f64());
    peak_reset();
    let t = Instant::now();
    let before = heap();
    stormblock::volume::gem::ensure_all_resident(vm.gem()).await?;
    let loaded = heap() - before;
    line("run on: every map in", base, extents, t.elapsed().as_secs_f64());
    println!("  a map in memory: {:.2} B/extent", loaded as f64 / extents as f64);
    peak_reset();
    let t = Instant::now();
    let out = vm.evict_idle(0).await;
    println!("{out} of {volumes} maps out");
    line("run on: out again", base, extents, t.elapsed().as_secs_f64());

    // One volume comes back: its first read loads its map whole.
    peak_reset();
    let t = Instant::now();
    let before = heap();
    let id = vm.find_volume("v0").await.ok_or_else(|| anyhow::anyhow!("v0 did not come back"))?;
    let v = vm.get_volume(&id).unwrap();
    let mut b = vec![0u8; 4096];
    v.read((per_volume - 1) * slot, &mut b).await?;
    println!(
        "one volume back: {per_volume} extents loaded in {:.1} ms, {:.2} B/extent",
        t.elapsed().as_secs_f64() * 1e3,
        (heap() - before) as f64 / per_volume as f64
    );
    let stored: u64 = stormblock::drive::emulated::get("full-drive").map(|d| d.stored_bytes()).unwrap_or(0);
    println!("the drive holds {:.1} GiB (slot table and records)", stored as f64 / (1u64 << 30) as f64);
    Ok(())
}
