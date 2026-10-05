//! What one persist costs when one extent changed, metadata format v1 against
//! v2 (#158, #157).
//!
//! A volume of N extents on an emulated drive (4 KiB slots, so each extent is
//! one block of memory), persisted once; then, R times, one more extent is
//! written and the records persisted. v1 encodes and writes every volume's
//! whole record each time; v2 appends what changed to its log.
//!
//! ```text
//! cargo run --release --example persist_cost -- [N] [R]   # 100000 50
//! ```

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::slab::{Slab, SlabFormat, SlabRole, SLAB_VERSION, SLAB_VERSION_2};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::{MetadataStore, VolumeManager};

const SLOT: u64 = 4096;

async fn run(version: u32, n: u64, rounds: u64) -> anyhow::Result<()> {
    let size = ((n + rounds + 1024) * (SLOT + 64) * 2).next_power_of_two();
    let uri = format!("emulated://persist-cost-v{version}?size={size}");
    let dev = stormblock::drive::open_path(&uri, false).await?;
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(version)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev.clone(), fmt).await?;
    let sid = slab.slab_id();
    let region = slab.metadata_capacity();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    let id = vm.create_volume_any("v", (n + rounds + 1) * SLOT).await?;
    let v: Arc<dyn BlockDevice> = vm.get_volume(&id).unwrap();
    let block = vec![0x5Au8; SLOT as usize];
    for e in 0..n {
        v.write(e * SLOT, &block).await?;
    }
    v.flush().await?;
    let t = Instant::now();
    vm.persist().await;
    let first = t.elapsed();
    anyhow::ensure!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());

    let before = vm.metadata_v2_usage().first().map(|u| u.1);
    let mut total = std::time::Duration::ZERO;
    for r in 0..rounds {
        v.write((n + r) * SLOT, &block).await?;
        v.flush().await?;
        let t = Instant::now();
        vm.persist().await;
        total += t.elapsed();
    }
    anyhow::ensure!(vm.durability_fault().is_none(), "{:?}", vm.durability_fault());
    let per = total / rounds as u32;
    let written = if version == SLAB_VERSION_2 {
        // Log bytes per persist (a checkpoint, when one ran, is not counted).
        let after = vm.metadata_v2_usage().first().map(|u| u.1);
        match (before, after) {
            (Some(b), Some(a)) if a.log_used >= b.log_used => (a.log_used - b.log_used) / rounds,
            _ => 0,
        }
    } else {
        // The whole record, every time.
        let reg = vm.registry().read().await;
        let bytes = reg.get(&sid).unwrap().read_metadata().await?.unwrap_or_default();
        let _ = MetadataStore::decode(&bytes)?;
        bytes.len() as u64
    };
    println!(
        "format {version}: {n} extents, region {} KiB; first persist {:.1} ms; then one extent changed: \
         {:.2} ms and {} bytes written per persist",
        region / 1024,
        first.as_secs_f64() * 1e3,
        per.as_secs_f64() * 1e3,
        written
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let rounds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
    run(SLAB_VERSION, n, rounds).await?;
    run(SLAB_VERSION_2, n, rounds).await?;
    Ok(())
}
