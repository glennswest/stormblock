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
//! # Repeats, because one pass measures noise
//!
//! A single pass over the depths on a virtualised rig produces run-to-run
//! variance larger than the effect being looked for — two consecutive passes
//! put the peak at different depths and disagreed about whether there was an
//! inversion at all. Each depth is therefore sampled `--repeats` times and the
//! **median** reported, with the spread printed alongside so a reader can see
//! whether the numbers are worth believing rather than having to trust them.
//!
//! ```text
//! cargo run --release --example qd_sweep -- [--seconds N] [--repeats N] [--slab-path PATH]
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

#[derive(Clone)]
struct Sample {
    iops: f64,
    cpu_seconds: f64,
    /// CPU-seconds burned per million I/Os — the per-operation cost, which is
    /// what makes points at different throughputs comparable at all.
    cpu_per_mio: f64,
}

struct Point {
    depth: usize,
    /// Median of the samples: one pass is noise on a virtualised rig.
    iops: f64,
    cpu_seconds: f64,
    cpu_per_mio: f64,
    /// Lowest and highest IOPS seen, so the spread is visible rather than
    /// hidden behind the median.
    iops_lo: f64,
    iops_hi: f64,
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 }
}

impl Point {
    fn from_samples(depth: usize, s: &[Sample]) -> Point {
        let iops: Vec<f64> = s.iter().map(|x| x.iops).collect();
        Point {
            depth,
            iops: median(iops.clone()),
            cpu_seconds: median(s.iter().map(|x| x.cpu_seconds).collect()),
            cpu_per_mio: median(s.iter().map(|x| x.cpu_per_mio).collect()),
            iops_lo: iops.iter().cloned().fold(f64::INFINITY, f64::min),
            iops_hi: iops.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        }
    }

    /// How wide the samples were, as a fraction of the median.
    fn spread_pct(&self) -> f64 {
        if self.iops == 0.0 {
            return 0.0;
        }
        (self.iops_hi - self.iops_lo) / self.iops * 100.0
    }
}

/// Drive `depth` writers concurrently for `dur`, and report what happened.
async fn measure(
    volume: Arc<dyn BlockDevice>,
    depth: usize,
    dur: Duration,
    seed: u64,
    span_blocks: u64,
) -> Sample {
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
    Sample {
        iops: ops / wall,
        cpu_seconds: cpu,
        cpu_per_mio: if ops > 0.0 { cpu / (ops / 1_000_000.0) } else { 0.0 },
    }
}

fn report(title: &str, points: &[Point]) {
    println!("\n{title}");
    println!(
        "{:>6}  {:>12}  {:>10}  {:>14}  {:>10}",
        "QD", "IOPS (med)", "CPU (s)", "CPU-s / MIO", "spread"
    );
    println!("{:->6}  {:->12}  {:->10}  {:->14}  {:->10}", "", "", "", "", "");
    for p in points {
        println!(
            "{:>6}  {:>12.0}  {:>10.2}  {:>14.1}  {:>9.0}%",
            p.depth,
            p.iops,
            p.cpu_seconds,
            p.cpu_per_mio,
            p.spread_pct()
        );
    }

    let Some(qd1) = points.iter().find(|p| p.depth == 1) else { return };
    let peak = points.iter().max_by(|a, b| a.iops.total_cmp(&b.iops)).unwrap();
    let deepest = points.last().unwrap();
    let ratio = deepest.iops / qd1.iops;
    // The widest per-depth spread bounds what this rig can actually resolve:
    // a depth effect smaller than the run-to-run noise is not a finding.
    let noise = points.iter().map(|p| p.spread_pct()).fold(0.0, f64::max);

    println!();
    println!("  peak {:.0} IOPS at QD{}", peak.iops, peak.depth);
    println!("  QD{} / QD1 = {ratio:.2}x", deepest.depth);
    println!("  widest per-depth spread: {noise:.0}%  (the resolution floor of this rig)");

    let effect = (1.0 - ratio).abs() * 100.0;
    if effect < noise {
        println!(
            "  INCONCLUSIVE: the depth effect ({effect:.0}%) is inside the noise ({noise:.0}%). \
             Whatever this rig is measuring, it is not a {:.1}x inversion.",
            1.0 / ratio.max(0.0001)
        );
    } else if ratio < 1.0 {
        println!("  INVERTED: QD{} is {ratio:.2}x QD1, beyond the noise", deepest.depth);
        if deepest.cpu_seconds < qd1.cpu_seconds {
            println!("  and CPU went DOWN with depth — the serialization signature");
        } else {
            println!("  but CPU did not go down — saturation, not a lock");
        }
    } else {
        println!("  NO INVERSION: QD{} is {ratio:.2}x QD1 — scales then plateaus", deepest.depth);
    }

    // The stable number, and the one that actually discriminates: lock
    // contention makes each operation cost *more* CPU as depth rises.
    let cpu_lo = points.iter().map(|p| p.cpu_per_mio).fold(f64::INFINITY, f64::min);
    let cpu_hi = points.iter().map(|p| p.cpu_per_mio).fold(f64::NEG_INFINITY, f64::max);
    println!(
        "  CPU per million I/Os: {cpu_lo:.1}–{cpu_hi:.1} s across all depths ({:.0}% range)",
        (cpu_hi - cpu_lo) / cpu_lo * 100.0
    );
    if (cpu_hi - cpu_lo) / cpu_lo < 0.25 {
        println!("  → per-operation cost is flat with depth, which lock contention would not be");
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(6);
    let repeats: usize = arg("--repeats").and_then(|s| s.parse().ok()).unwrap_or(3);
    let slab_path = arg("--slab-path")
        .unwrap_or_else(|| std::env::temp_dir().join("qd-sweep.slab").display().to_string());
    let dur = Duration::from_secs(seconds);

    println!("stormblock QD sweep — 4K random write through ThinVolume (#36)");
    println!("  build      : {}", env!("CARGO_PKG_VERSION"));
    println!("  workers    : {} available parallelism", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));
    println!("  slab       : {slab_path}");
    println!("  slot size  : {} MiB", SLOT / (1 << 20));
    println!("  volume     : {} MiB", VOLUME_BYTES / (1 << 20));
    println!("  per sample : {seconds}s x {repeats} repeats, median reported");

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
    // Passes are interleaved rather than repeating each depth in place: if the
    // host drifts — another tenant, thermal, page cache — a block of repeats at
    // one depth absorbs all of it and that depth alone looks fast or slow.
    let mut cold: Vec<Vec<Sample>> = DEPTHS.iter().map(|_| Vec::new()).collect();
    for pass in 0..repeats {
        for (i, depth) in DEPTHS.iter().enumerate() {
            let id: VolumeId = vm
                .lock()
                .await
                .create_volume_any(&format!("cold-qd{depth}-p{pass}"), VOLUME_BYTES)
                .await
                .expect("volume");
            let vol = vm.lock().await.get_volume(&id).unwrap();
            let span = VOLUME_BYTES / BLOCK as u64;
            cold[i].push(measure(vol, *depth, dur, 0xC01D, span).await);
            // Drop it before the next sample so the pool does not fill up.
            let _ = vm.lock().await.delete_volume(id).await;
        }
        println!("  cold pass {}/{repeats} done", pass + 1);
    }
    let cold: Vec<Point> = DEPTHS
        .iter()
        .enumerate()
        .map(|(i, d)| Point::from_samples(*d, &cold[i]))
        .collect();
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
    let mut warm: Vec<Vec<Sample>> = DEPTHS.iter().map(|_| Vec::new()).collect();
    for pass in 0..repeats {
        for (i, depth) in DEPTHS.iter().enumerate() {
            warm[i].push(measure(vol.clone(), *depth, dur, 0x3A12, warm_span).await);
        }
        println!("  warm pass {}/{repeats} done", pass + 1);
    }
    let warm: Vec<Point> = DEPTHS
        .iter()
        .enumerate()
        .map(|(i, d)| Point::from_samples(*d, &warm[i]))
        .collect();
    report("WARM — fully allocated, no allocation on the write path", &warm);

    println!("\nCold inverts but warm does not  → the allocation path.");
    println!("Both invert                     → the general I/O path, which is #30.");
    println!("Neither inverts                 → the original number was the 2-vCPU rig.");
    println!("Effect inside the noise         → this rig cannot answer it; only the");
    println!("                                  absence of a 5.4x inversion is established.");

    let _ = std::fs::remove_file(&slab_path);
}
