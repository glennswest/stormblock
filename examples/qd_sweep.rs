//! Queue-depth sweep over the engine's own write path (#36).
//!
//! The measurement that currently justifies #30 — 4K random write inverting
//! from 16,781 IOPS at QD1 to 3,093 at QD32, with the engine burning *less*
//! CPU — was taken on a 2 vCPU VM. That rig cannot tell "our lock is bad" from
//! "the queue is deeper than the machine is wide", because 32 outstanding
//! writes on two cores produce that shape with no lock pathology at all.
//!
//! This measures the same thing directly against a `ThinVolume`, with no iSCSI
//! or NVMe-oF in the path: the transport is not what #30 is about, and leaving
//! it in would measure it.
//!
//! Two phases, because they answer different questions and only one of them is
//! about the allocation path:
//!
//! - **cold** — writes to a fresh volume, so every write allocates a slab slot
//! - **warm** — rewrites the same range once it is fully allocated, so no
//!   allocation happens and only the steady-state I/O path is measured
//!
//! An inversion that appears cold but not warm is the allocation path. One that
//! appears in both is the general I/O path, which is what #30 proposes to fix.
//!
//! ```text
//! cargo run --release --example qd_sweep -- [--seconds N] [--slab-path PATH]
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::raid::RaidArrayId;
use stormblock::volume::{VolumeId, VolumeManager};

const BLOCK: usize = 4096;
const SLOT: u64 = 4 * 1024 * 1024;
const VOLUME_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SLAB_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const DEPTHS: [usize; 6] = [1, 4, 8, 16, 32, 64];

/// A deterministic LBA sequence. Not `rand`, so a rerun measures the same
/// access pattern rather than a different one — the comparison between depths
/// is the point, and two runs that touched different blocks are not comparable.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}

/// CPU time this process has burned, in seconds.
///
/// The signal that matters is CPU going *down* as depth goes up: that is
/// serialization (threads parked on a lock or a round trip), not saturation.
/// Saturation looks like CPU flat or rising while IOPS plateaus.
fn cpu_seconds() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            let f: Vec<&str> = stat.split_whitespace().collect();
            if f.len() > 16 {
                let ticks = 100.0; // USER_HZ, fixed at 100 on every Linux we run on
                let utime: f64 = f[13].parse().unwrap_or(0.0);
                let stime: f64 = f[14].parse().unwrap_or(0.0);
                return (utime + stime) / ticks;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // getrusage is portable enough for a local sanity run on macOS.
        unsafe {
            let mut ru: libc_rusage = std::mem::zeroed();
            if getrusage(0, &mut ru) == 0 {
                return ru.ru_utime.tv_sec as f64
                    + ru.ru_utime.tv_usec as f64 / 1e6
                    + ru.ru_stime.tv_sec as f64
                    + ru.ru_stime.tv_usec as f64 / 1e6;
            }
        }
    }
    0.0
}

#[cfg(not(target_os = "linux"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i32,
}

#[cfg(not(target_os = "linux"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct libc_rusage {
    ru_utime: Timeval,
    ru_stime: Timeval,
    rest: [i64; 14],
}

#[cfg(not(target_os = "linux"))]
extern "C" {
    fn getrusage(who: i32, usage: *mut libc_rusage) -> i32;
}

struct Point {
    depth: usize,
    iops: f64,
    cpu_seconds: f64,
    /// CPU-seconds burned per million I/Os — the per-operation cost, which is
    /// what makes points at different throughputs comparable at all.
    cpu_per_mio: f64,
}

/// Drive `depth` writers concurrently for `dur`, and report what happened.
async fn measure(
    volume: Arc<dyn BlockDevice>,
    depth: usize,
    dur: Duration,
    seed: u64,
    span_blocks: u64,
) -> Point {
    let done = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + dur;
    let cpu0 = cpu_seconds();
    let wall0 = Instant::now();

    let mut tasks = Vec::with_capacity(depth);
    for w in 0..depth {
        let vol = volume.clone();
        let done = done.clone();
        // Each writer walks its own sequence, so `depth` writers do not all
        // hammer the same block and measure lock contention on one slot.
        let mut rng = Lcg(seed.wrapping_add(w as u64 * 0x9E37_79B9_7F4A_7C15));
        tasks.push(tokio::spawn(async move {
            let buf = vec![0xA5u8; BLOCK];
            while Instant::now() < deadline {
                let lba = rng.next() % span_blocks;
                if vol.write(lba * BLOCK as u64, &buf).await.is_ok() {
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }

    let wall = wall0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    let ops = done.load(Ordering::Relaxed) as f64;
    Point {
        depth,
        iops: ops / wall,
        cpu_seconds: cpu,
        cpu_per_mio: if ops > 0.0 { cpu / (ops / 1_000_000.0) } else { 0.0 },
    }
}

fn report(title: &str, points: &[Point]) {
    println!("\n{title}");
    println!("{:>6}  {:>12}  {:>10}  {:>14}", "QD", "IOPS", "CPU (s)", "CPU-s / MIO");
    println!("{:->6}  {:->12}  {:->10}  {:->14}", "", "", "", "");
    for p in points {
        println!(
            "{:>6}  {:>12.0}  {:>10.2}  {:>14.1}",
            p.depth, p.iops, p.cpu_seconds, p.cpu_per_mio
        );
    }

    let Some(qd1) = points.iter().find(|p| p.depth == 1) else { return };
    let peak = points.iter().max_by(|a, b| a.iops.total_cmp(&b.iops)).unwrap();
    let deepest = points.last().unwrap();

    println!();
    println!("  peak {:.0} IOPS at QD{}", peak.iops, peak.depth);
    if deepest.iops < qd1.iops {
        println!(
            "  INVERTED: QD{} is {:.2}x QD1 ({:.0} vs {:.0})",
            deepest.depth,
            deepest.iops / qd1.iops,
            deepest.iops,
            qd1.iops
        );
        if deepest.cpu_seconds < qd1.cpu_seconds {
            println!("  and CPU went DOWN with depth — the serialization signature");
        } else {
            println!("  but CPU did not go down — this looks like saturation, not a lock");
        }
    } else {
        println!(
            "  NO INVERSION: QD{} is {:.2}x QD1 — scales then plateaus",
            deepest.depth,
            deepest.iops / qd1.iops
        );
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(6);
    let slab_path = arg("--slab-path")
        .unwrap_or_else(|| std::env::temp_dir().join("qd-sweep.slab").display().to_string());
    let dur = Duration::from_secs(seconds);

    println!("stormblock QD sweep — 4K random write through ThinVolume (#36)");
    println!("  build      : {}", env!("CARGO_PKG_VERSION"));
    println!("  workers    : {} available parallelism", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));
    println!("  slab       : {slab_path}");
    println!("  slot size  : {} MiB", SLOT / (1 << 20));
    println!("  volume     : {} MiB", VOLUME_BYTES / (1 << 20));
    println!("  per point  : {seconds}s");

    let _ = std::fs::remove_file(&slab_path);
    let dev = FileDevice::open_with_capacity(&slab_path, SLAB_BYTES)
        .await
        .expect("creating the slab backing file");
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), Arc::new(dev)).await;
    let vm = Arc::new(tokio::sync::Mutex::new(vm));

    // ---- cold: every write allocates ----
    // A fresh volume per depth, so each point genuinely allocates rather than
    // the first point paying for all of them.
    let mut cold = Vec::new();
    for depth in DEPTHS {
        let id: VolumeId = vm
            .lock()
            .await
            .create_volume_any(&format!("cold-qd{depth}"), VOLUME_BYTES)
            .await
            .expect("volume");
        let vol = vm.lock().await.get_volume(&id).unwrap();
        let span = VOLUME_BYTES / BLOCK as u64;
        cold.push(measure(vol, depth, dur, 0xC01D, span).await);
        // Drop it before the next point so the pool does not fill up.
        let _ = vm.lock().await.delete_volume(id).await;
    }
    report("COLD — every write allocates a slab slot", &cold);

    // ---- warm: nothing allocates ----
    // One volume, its whole range pre-allocated, reused for every depth. The
    // range is deliberately small so it fits comfortably and the allocation is
    // genuinely done before the measurement starts.
    let warm_span_bytes: u64 = 256 * 1024 * 1024;
    let id = vm
        .lock()
        .await
        .create_volume_any("warm", VOLUME_BYTES)
        .await
        .expect("volume");
    let vol = vm.lock().await.get_volume(&id).unwrap();
    println!("\npre-allocating {} MiB …", warm_span_bytes / (1 << 20));
    {
        let buf = vec![0x5Au8; SLOT as usize];
        let mut off = 0;
        while off < warm_span_bytes {
            vol.write(off, &buf).await.expect("pre-allocate");
            off += SLOT;
        }
        vol.flush().await.ok();
    }
    let warm_span = warm_span_bytes / BLOCK as u64;
    let mut warm = Vec::new();
    for depth in DEPTHS {
        warm.push(measure(vol.clone(), depth, dur, 0x3A12, warm_span).await);
    }
    report("WARM — fully allocated, no allocation on the write path", &warm);

    println!("\nCold inverts but warm does not  → the allocation path.");
    println!("Both invert                     → the general I/O path, which is #30.");
    println!("Neither inverts                 → the original number was the 2-vCPU rig.");

    let _ = std::fs::remove_file(&slab_path);
}
