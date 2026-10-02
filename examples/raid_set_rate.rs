//! How fast a RAID set writes, reads degraded and rebuilds (#252).
//!
//! `cargo run --release --example raid_set_rate -- [members] [MiB per member] [dir]`
//!
//! Members are sparse files opened O_DIRECT where the filesystem allows it, so
//! the numbers are the engine's on this box's disk, not a shelf's. Defaults:
//! an 11-drive RAID-6 (the DS2246 set) of 256 MiB each, in `$TMPDIR`.

use std::sync::Arc;
use std::time::Instant;

use stormblock::drive::BlockDevice;
use stormblock::raid::{RaidArray, RaidLevel};

fn mib(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / secs.max(1e-9)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(11);
    let per: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(256) * 1024 * 1024;
    let dir = args.get(3).cloned().unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let run = uuid::Uuid::new_v4().simple().to_string();

    let mut devs: Vec<Arc<dyn BlockDevice>> = Vec::new();
    let mut paths = Vec::new();
    for i in 0..=n {
        let p = format!("{dir}/raidrate-{run}-{i}.bin");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(per + stormblock::raid::DATA_OFFSET).unwrap();
        drop(f);
        // O_DIRECT, as a real drive is opened: the page cache would
        // otherwise be what is measured.
        #[cfg(target_os = "linux")]
        devs.push(Arc::new(stormblock::drive::sas::SasDevice::open_file_direct(&p, 4096).await.unwrap()));
        #[cfg(not(target_os = "linux"))]
        devs.push(Arc::from(stormblock::drive::open_one_drive(&p).await.unwrap()));
        paths.push(p);
    }
    let spare = devs.pop().unwrap();
    let a = Arc::new(RaidArray::create(RaidLevel::Raid6, devs, None).await.unwrap());
    let cap = a.capacity_bytes();
    println!("RAID-6 of {n}, 64 KiB unit: {} MiB usable", cap >> 20);

    // Sequential 1 MiB writes over the whole set.
    let buf: Vec<u8> = (0..1 << 20).map(|i| (i as u32).wrapping_mul(2654435761) as u8).collect();
    let t = Instant::now();
    let mut off = 0;
    while off + buf.len() as u64 <= cap {
        a.write(off, &buf).await.unwrap();
        off += buf.len() as u64;
    }
    a.flush().await.unwrap();
    println!("sequential write 1 MiB: {:.0} MiB/s", mib(off, t.elapsed().as_secs_f64()));

    // Random 4 KiB writes (read-modify-write of data, P and Q).
    let small = vec![0x5Au8; 4096];
    let count = 4000u64;
    let t = Instant::now();
    let mut x = 12345u64;
    for _ in 0..count {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        a.write((x >> 20) % (cap / 4096) * 4096, &small).await.unwrap();
    }
    a.flush().await.unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("random write 4 KiB: {:.0} IOPS", count as f64 / s);

    // Sequential read healthy, then with two members lost.
    let mut rbuf = vec![0u8; 1 << 20];
    for (label, lose) in [("healthy", 0usize), ("two members lost", 2)] {
        for k in 0..lose {
            a.set_member_state(k, stormblock::raid::RaidMemberState::Failed);
        }
        let t = Instant::now();
        let mut off = 0;
        while off + rbuf.len() as u64 <= cap {
            a.read(off, &mut rbuf).await.unwrap();
            off += rbuf.len() as u64;
        }
        println!("sequential read 1 MiB, {label}: {:.0} MiB/s", mib(off, t.elapsed().as_secs_f64()));
        for k in 0..lose {
            a.set_member_state(k, stormblock::raid::RaidMemberState::Active);
        }
    }

    // What the set holds now, to check the rebuilt member against.
    let sample = (16u64 << 20).min(cap) as usize;
    let mut expect = vec![0u8; sample];
    a.read(0, &mut expect).await.unwrap();

    // A member fails; the spare rebuilds it.
    assert!(a.fail_member(3, "rate test"));
    let t = Instant::now();
    a.replace(3, spare).await.unwrap();
    loop {
        if let Some(p) = a.rebuild_progress() {
            if p.is_finished() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let s = t.elapsed().as_secs_f64();
    println!(
        "rebuild of one member ({} MiB): {:.1} s, {:.0} MiB/s per member; state {}",
        a.data_size() >> 20,
        s,
        mib(a.data_size(), s),
        a.status().state
    );
    // Proof it holds the data: lose two others and read the sample back.
    a.set_member_state(0, stormblock::raid::RaidMemberState::Failed);
    a.set_member_state(1, stormblock::raid::RaidMemberState::Failed);
    let mut got = vec![0u8; sample];
    a.read(0, &mut got).await.unwrap();
    assert!(got == expect, "rebuilt set does not read back");
    println!("rebuilt member verified (read with two others lost)");
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}
