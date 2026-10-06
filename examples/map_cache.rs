//! What the extent map cache keeps in memory (#158 stage C).
//!
//! G goldens of E extents each on a format v2 slab (emulated, 64 KiB slots),
//! persisted; heap in use (counted by the allocator) before and after every
//! idle map leaves memory, and after one golden is read again.
//!
//! ```text
//! cargo run --release --example map_cache -- [G] [E]   # 64 2000
//! ```

use std::sync::Arc;

use stormblock::drive::slab::{Slab, SlabFormat, SlabRole, SLAB_VERSION_2};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

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
}
#[global_allocator]
static A: Counting = Counting;

fn heap() -> i64 {
    HEAP.load(std::sync::atomic::Ordering::Relaxed)
}

const SLOT: u64 = 64 * 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let goldens: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(64);
    let extents: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let size = ((goldens * extents + 4096) * (SLOT + 64) * 2).next_power_of_two();
    let dev = stormblock::drive::open_path(&format!("emulated://map-cache?size={size}"), false).await?;
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(SLAB_VERSION_2)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev.clone(), fmt).await?;
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    let block = vec![0x33u8; 4096];
    let mut first = None;
    for g in 0..goldens {
        let id = vm.create_volume_any(&format!("g{g}"), extents * SLOT).await?;
        first.get_or_insert(id);
        let v: Arc<dyn BlockDevice> = vm.get_volume(&id).unwrap();
        for e in 0..extents {
            v.write(e * SLOT, &block).await?;
        }
        v.flush().await?;
    }
    vm.persist().await;
    anyhow::ensure!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    let all = goldens * extents;
    let before = heap();
    let n = vm.evict_idle(0).await;
    let after = heap();
    println!(
        "{goldens} goldens x {extents} extents = {all} extents; {n} maps out of memory: \
         heap {:.1} MiB -> {:.1} MiB ({:.1} B an extent freed)",
        before as f64 / (1 << 20) as f64,
        after as f64 / (1 << 20) as f64,
        (before - after) as f64 / all as f64
    );
    let id = first.unwrap();
    let v = vm.get_volume(&id).unwrap();
    let mut buf = vec![0u8; 4096];
    let t = std::time::Instant::now();
    v.read(0, &mut buf).await?;
    println!(
        "first read of one golden loads its {extents} extents in {:.2} ms; heap {:.1} MiB",
        t.elapsed().as_secs_f64() * 1e3,
        heap() as f64 / (1 << 20) as f64
    );
    anyhow::ensure!(buf == block, "the golden read back wrong");
    Ok(())
}
