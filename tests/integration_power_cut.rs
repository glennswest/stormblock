//! Power cut: what a consumer fsync'd must be there after the engine restores
//! from its slabs alone (#171).
//!
//! fastetcd's redb came back "All roots are corrupted" after every hard
//! power-off of a stormcos node: writes it had fsync'd were not on the disk.
//! This reproduces that shape without hardware. A `CrashDevice` holds every
//! write in a volatile cache until a flush and, at the crash, keeps a random
//! subset of the unflushed ones — what a drive with a write-back cache may do
//! when the power goes. On it:
//!
//! 1. a blank, fully written (a formatted filesystem's metadata), and a
//!    copy-on-write clone of it — the shape of a claim;
//! 2. random 4 KiB writes into the clone, numbered, with flushes (the
//!    consumer's fsync), discards, and a second volume churning allocations so
//!    slots are freed and reused;
//! 3. a crash at a random point, then a fresh engine that restores from the
//!    slabs alone — no data directory, as on a stormcos node;
//! 4. every block of the clone checked: a block acknowledged by a flush reads
//!    what was written before that flush or anything written to it since;
//!    a block never written by the clone reads the blank's content.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rand::{Rng, SeedableRng};
use stormblock::drive::crashdev::CrashDevice;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

const SLOT: u64 = 64 * 1024;
const BLOCK: u64 = 4096;
const VOL: u64 = 2 * 1024 * 1024; // 32 extents, 512 blocks
const BLOCKS: u64 = VOL / BLOCK;

/// A block's content: its index and a value, repeated, so a block read from
/// the wrong place is caught as surely as a stale one.
fn block(idx: u64, value: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK as usize];
    if value == 0 {
        return b;
    }
    for c in b.chunks_mut(16) {
        c[..8].copy_from_slice(&idx.to_le_bytes());
        c[8..].copy_from_slice(&value.to_le_bytes());
    }
    b
}

fn value_of(idx: u64, b: &[u8]) -> Result<u64, String> {
    if b.iter().all(|&x| x == 0) {
        return Ok(0);
    }
    let i = u64::from_le_bytes(b[..8].try_into().unwrap());
    let v = u64::from_le_bytes(b[8..16].try_into().unwrap());
    if i != idx || block(i, v) != b {
        return Err(format!("block {idx} holds garbage (claims index {i}, value {v})"));
    }
    Ok(v)
}

fn blank_value(idx: u64) -> u64 {
    1_000_000 + idx
}

/// What a block may read after a crash.
#[derive(Clone)]
enum Allowed {
    /// One of these values.
    Values(HashSet<u64>),
    /// Anything that is not garbage: a discard since the last write.
    Any,
}

async fn trial(seed: u64) -> Result<(), String> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let dev = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev.clone() as Arc<dyn BlockDevice>, fmt).await.map_err(|e| e.to_string())?;
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);

    // The blank: every block written, as a formatted filesystem's metadata
    // would be, then made durable.
    let blank = vm.create_volume_any("blank", VOL).await.map_err(|e| e.to_string())?;
    let bv = vm.get_volume(&blank).unwrap();
    for i in 0..BLOCKS {
        bv.write(i * BLOCK, &block(i, blank_value(i))).await.map_err(|e| e.to_string())?;
    }
    bv.flush().await.map_err(|e| e.to_string())?;
    vm.persist().await;
    // The claim: a copy-on-write clone of the blank.
    let clone = vm.create_snapshot(blank, "clone").await.map_err(|e| e.to_string())?;
    let cv = vm.get_volume(&clone).unwrap();
    cv.flush().await.map_err(|e| e.to_string())?;
    // A second volume, to free and reuse slots under the clone.
    let other = vm.create_volume_any("other", VOL).await.map_err(|e| e.to_string())?;
    let ov = vm.get_volume(&other).unwrap();

    // What each block of the clone may read after a crash.
    let mut allowed: HashMap<u64, Allowed> =
        (0..BLOCKS).map(|i| (i, Allowed::Values([blank_value(i)].into_iter().collect()))).collect();
    // The value each block holds now.
    let mut current: HashMap<u64, Option<u64>> = (0..BLOCKS).map(|i| (i, Some(blank_value(i)))).collect();

    let ops = rng.gen_range(20..200);
    let mut seq = 1u64;
    for _ in 0..ops {
        let r: f64 = rng.gen();
        if r < 0.70 {
            let i = rng.gen_range(0..BLOCKS);
            seq += 1;
            cv.write(i * BLOCK, &block(i, seq)).await.map_err(|e| e.to_string())?;
            current.insert(i, Some(seq));
            if let Allowed::Values(s) = allowed.get_mut(&i).unwrap() {
                s.insert(seq);
            }
        } else if r < 0.85 {
            // The consumer's fsync: from here on, a block may read only what
            // it holds now.
            cv.flush().await.map_err(|e| e.to_string())?;
            for i in 0..BLOCKS {
                let a = match current[&i] {
                    Some(v) => Allowed::Values([v].into_iter().collect()),
                    None => Allowed::Any,
                };
                allowed.insert(i, a);
            }
        } else if r < 0.90 {
            // Discard a whole extent: its blocks are undefined until rewritten.
            let e = rng.gen_range(0..VOL / SLOT);
            cv.discard(e * SLOT, SLOT).await.map_err(|e| e.to_string())?;
            for i in (e * SLOT / BLOCK)..((e + 1) * SLOT / BLOCK) {
                current.insert(i, None);
                allowed.insert(i, Allowed::Any);
            }
        } else if r < 0.97 {
            // Churn: the other volume takes and gives back slots.
            let e = rng.gen_range(0..VOL / SLOT);
            if rng.gen_bool(0.5) {
                ov.write(e * SLOT, &vec![0xEE; SLOT as usize]).await.map_err(|e| e.to_string())?;
            } else {
                ov.discard(e * SLOT, SLOT).await.map_err(|e| e.to_string())?;
            }
        } else {
            // Something that rewrites the volume records.
            vm.persist().await;
        }
    }

    // The power goes.
    let keep = rng.gen_range(0.0..1.0);
    let after = Arc::new(dev.crash(seed, keep));
    let slab = Slab::open(after.clone() as Arc<dyn BlockDevice>).await.map_err(|e| format!("reopen: {e}"))?;
    let mut vm2 = VolumeManager::new(SLOT);
    vm2.add_slab(slab).await;
    vm2.persist_to_slab(sid);
    vm2.restore().await.map_err(|e| format!("restore: {e}"))?;
    let id = vm2.find_volume("clone").await.ok_or("the clone did not come back")?;
    let rv = vm2.get_volume(&id).unwrap();
    let mut buf = vec![0u8; BLOCK as usize];
    for i in 0..BLOCKS {
        rv.read(i * BLOCK, &mut buf).await.map_err(|e| format!("read block {i}: {e}"))?;
        let v = value_of(i, &buf).map_err(|e| format!("seed {seed}: {e}"))?;
        match &allowed[&i] {
            Allowed::Values(s) if !s.contains(&v) => {
                return Err(format!(
                    "seed {seed} (keep {keep:.2}, {ops} ops): block {i} reads {v}, allowed {:?}",
                    s
                ))
            }
            _ => {}
        }
    }
    Ok(())
}

#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_at_any_point() {
    let mut failures = Vec::new();
    for seed in 0..300u64 {
        if let Err(e) = trial(seed).await {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of 300 power cuts lost acknowledged data; first: {}",
        failures.len(),
        failures.first().unwrap()
    );
}
