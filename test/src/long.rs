//! `long` (the night window): waves of the engine's main workload — claim a
//! clone of a blank, attach it, write and read it back, detach, delete — sized
//! from the pod's own CPUs and memory, repeated until the window closes.
//! What it measures across waves is the point: a wave slower than the first
//! ones, or residue that grows (volumes, allocated slots, the engine's memory
//! and file descriptors), fails even when every operation passed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::engine::{read_check, write_check, Engine, MIB};
use crate::env::Env;
use crate::flows::*;
use crate::node;
use crate::report::{ensure, Report, Why};

/// Memory this pod may use, in MiB: the cgroup's limit, else the machine's.
fn memory_mib() -> u64 {
    let cg = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let total = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
            .map(|kib| kib * 1024)
    });
    cg.or(total).unwrap_or(1 << 30) / MIB
}

fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

/// One volume's life in a wave. Answers its duration.
async fn one(e: Arc<Engine>, blank: String, seed: u64) -> Result<u64, String> {
    let t = Instant::now();
    let id = claim(&e, &blank).await?;
    let dev = e.attach(&id).await?;
    let r = async {
        write_check(&dev, 8 * MIB, seed, MIB as usize).await?;
        read_check(&dev, 8 * MIB, seed, MIB as usize).await
    }
    .await;
    drop(dev);
    let d = e.detach(&id).await;
    let del = delete_volume(&e, &id).await;
    r.and(d).and(del)?;
    Ok(t.elapsed().as_millis() as u64)
}

pub async fn run(env: &Env, r: &mut Report) -> Result<(), String> {
    let started = Instant::now();
    r.run("node-health", node::health(env)).await;
    if !env.bin.exists() {
        return Err(format!("no stormblock binary at {}", env.bin.display()));
    }
    let cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(2);
    let mem = memory_mib();
    // Four volumes in flight per CPU, a volume per 64 MiB of memory, 4..=64.
    let wave = (cpus * 4).min(mem / 64).clamp(4, 64);
    // The window, less a margin to clean up and report.
    let window = env.timeout.saturating_sub(Duration::from_secs(600)).max(Duration::from_secs(60));

    let mut engine: Option<Engine> = None;
    r.run("engine-up", async {
        engine = Some(Engine::start(env, "long", 4096 * MIB).await?);
        Ok(format!("{cpus} cpu(s), {mem} MiB: waves of up to {wave}"))
    })
    .await;
    let Some(e) = engine else { return Ok(()) };
    let e = Arc::new(e);
    let blank = make_blank(&e, "t-long-blank", "64M").await?;
    let base_vols = volume_count(&e).await?;
    let base_slots = allocated_slots(&e).await?;

    let (mut times, mut worst, mut rss, mut fds) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut failures, mut residue) = (Vec::<String>::new(), Vec::<String>::new());
    let mut n = 0u64;
    while started.elapsed() < window {
        n += 1;
        // Vary the size: full, half, full, three quarters.
        let size = match n % 4 {
            2 => wave / 2,
            0 => wave * 3 / 4,
            _ => wave,
        }
        .max(1);
        let t = Instant::now();
        let mut set = tokio::task::JoinSet::new();
        for i in 0..size {
            set.spawn(one(e.clone(), blank.clone(), n * 1000 + i));
        }
        let mut max_ms = 0;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(ms)) => max_ms = max_ms.max(ms),
                Ok(Err(err)) => failures.push(format!("wave {n}: {err}")),
                Err(err) => failures.push(format!("wave {n}: {err}")),
            }
        }
        let wave_ms = t.elapsed().as_millis() as u64;
        let vols = volume_count(&e).await?;
        let slots = allocated_slots(&e).await?;
        let (kib, fd) = e.footprint();
        if vols != base_vols || slots != base_slots {
            residue.push(format!("wave {n}: {vols} volume(s) (was {base_vols}), {slots} slot(s) (was {base_slots})"));
        }
        // Per volume, so a smaller wave does not look faster.
        times.push(wave_ms * wave / size);
        worst.push(max_ms);
        rss.push(kib);
        fds.push(fd);
        r.metric(json!({
            "wave": n, "volumes": size, "wave_ms": wave_ms, "slowest_volume_ms": max_ms,
            "engine_rss_kib": kib, "engine_fds": fd, "volumes_left": vols - base_vols.min(vols),
            "slots_allocated": slots, "failures": failures.len(),
        }));
        if failures.len() > 50 {
            break;
        }
    }

    r.run("waves-complete", async {
        ensure(n >= 2, format!("only {n} wave(s) fit in the window"))?;
        ensure(failures.is_empty(), format!("{} failure(s); first: {}", failures.len(), failures.first().cloned().unwrap_or_default()))?;
        Ok(format!("{n} wave(s) of up to {wave} volume(s)"))
    })
    .await;
    r.run("no-residue", async {
        ensure(residue.is_empty(), format!("{} wave(s) left something; first: {}", residue.len(), residue[0]))?;
        Ok("every wave cleaned up after itself".to_string())
    })
    .await;
    r.run("no-slowdown", async {
        if times.len() < 6 {
            return Err(Why::Skip(format!("{} wave(s): too few to compare", times.len())));
        }
        // The first wave warms caches; compare the next three with the last three.
        let early = median(&times[1..4]);
        let late = median(&times[times.len() - 3..]);
        ensure(
            late <= early * 2 || late < early + 2000,
            format!("waves slowed from {early} ms to {late} ms (per full wave)"),
        )?;
        Ok(format!("{early} ms → {late} ms per full wave"))
    })
    .await;
    r.run("no-leak", async {
        if rss.len() < 3 {
            return Err(Why::Skip("too few waves".into()));
        }
        let (r0, r1) = (rss[1], *rss.last().unwrap());
        let (f0, f1) = (fds[1], *fds.last().unwrap());
        ensure(r1 <= r0 + r0 / 2 + 64 * 1024, format!("engine memory grew {r0} → {r1} KiB"))?;
        ensure(f1 <= f0 + 16, format!("engine file descriptors grew {f0} → {f1}"))?;
        Ok(format!("rss {r0} → {r1} KiB, fds {f0} → {f1}"))
    })
    .await;

    let _ = e.ok("DELETE", &format!("/fstemplates/{blank}"), None).await;
    if let Ok(e) = Arc::try_unwrap(e) {
        e.remove().await;
    }
    Ok(())
}
