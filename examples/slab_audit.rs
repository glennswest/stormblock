//! Read a device or a slab out to files the real e2fsprogs can judge (#239).
//!
//! ```text
//! cargo run --example slab_audit -- fetch URI OUT      # a device (nvme-tcp://…, a file) → sparse file
//! cargo run --example slab_audit -- extract SLAB DIR [MAX]  # every volume in a slab (or a disk of slabs) → DIR/<name>.img
//! ```
//!
//! `SLAB` may also be `http://host:port/path` — a release image read with
//! HTTP `Range` GETs, so nothing is downloaded but what the slabs map, and
//! nothing on the server changes. Writes `extract` makes (records an adopt
//! rewrites) stay in this process's memory. Volumes larger than `MAX` bytes
//! (default 2 GiB) are compared but not written out.
//!
//! `fetch` only reads. `extract` adopts the slabs of a *copy* (it may write
//! records), so point it at a file `fetch` made, never at a served device.
//! For each volume whose parent is in the same slabs, `extract` also lists
//! the 4 KiB blocks where the two differ — a clone differs from its golden
//! where it was written (stamped, mounted), and nowhere else.

use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use stormblock::drive::httpdev::HttpDevice;
use stormblock::drive::BlockDevice;
use stormblock::volume::VolumeManager;

async fn open_any(path: &str, read_only: bool) -> anyhow::Result<Arc<dyn BlockDevice>> {
    if path.starts_with("http://") {
        return Ok(Arc::new(HttpDevice::open(path).await?));
    }
    Ok(stormblock::drive::open_path(path, read_only).await?)
}

async fn dump(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<u64> {
    let size = dev.capacity_bytes();
    let mut f = std::fs::File::create(path)?;
    f.set_len(size)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut off, mut written) = (0u64, 0u64);
    while off < size {
        let n = ((size - off) as usize).min(buf.len());
        dev.read(off, &mut buf[..n]).await?;
        if buf[..n].iter().any(|&b| b != 0) {
            f.seek(SeekFrom::Start(off))?;
            f.write_all(&buf[..n])?;
            written += n as u64;
        }
        off += n as u64;
    }
    Ok(written)
}

/// The 4 KiB blocks where `a` and `b` differ, read 1 MiB at a time.
async fn diff_blocks(a: &Arc<dyn BlockDevice>, b: &Arc<dyn BlockDevice>) -> anyhow::Result<Vec<u64>> {
    let size = a.capacity_bytes().min(b.capacity_bytes());
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut out = Vec::new();
    let mut off = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(x.len());
        a.read(off, &mut x[..n]).await?;
        b.read(off, &mut y[..n]).await?;
        for (i, (p, q)) in x[..n].chunks(4096).zip(y[..n].chunks(4096)).enumerate() {
            if p != q {
                out.push(off / 4096 + i as u64);
            }
        }
        off += n as u64;
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fetch") if args.len() == 4 => {
            let dev = open_any(&args[2], true).await?;
            let t = std::time::Instant::now();
            let n = dump(&dev, &args[3]).await?;
            println!(
                "fetched {} ({} bytes, {} non-zero) in {:.1}s",
                args[2],
                dev.capacity_bytes(),
                n,
                t.elapsed().as_secs_f64()
            );
        }
        Some("extract") if args.len() == 4 || args.len() == 5 => {
            let max: u64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(2 << 30);
            let dev = open_any(&args[2], false).await?;
            let found = stormblock::drive::discover::slabs_in_partitions(&dev).await;
            anyhow::ensure!(!found.is_empty(), "no slab in {}", args[2]);
            let slot = found[0].slab.slot_size();
            for f in &found {
                println!("slab {} ({}): role {}", f.slab.slab_id().0, f.label, f.slab.role());
            }
            let mut mgr = VolumeManager::new(slot);
            let r = mgr.adopt_slabs(found).await?;
            println!("adopted {} slab(s), {} volume(s)", r.slabs.len(), r.volumes.len());
            std::fs::create_dir_all(&args[3])?;
            let mut vols = mgr.list_volumes().await;
            vols.sort_by(|a, b| a.1.cmp(&b.1));
            let names: std::collections::HashMap<_, _> =
                vols.iter().map(|(id, n, ..)| (*id, n.clone())).collect();
            for (id, name, size, used) in &vols {
                let dev = mgr.get_volume(id).expect("listed");
                let file = format!("{}/{}.img", args[3], name.replace('/', "_"));
                if *size <= max {
                    dump(&dev, &file).await?;
                }
                let parent = mgr.parent(id);
                println!(
                    "volume {name} {id:?} size {size} mapped {used} parent {}",
                    parent.and_then(|p| names.get(&p).cloned()).unwrap_or_else(|| "-".into())
                );
                if let Some(p) = parent.and_then(|p| mgr.get_volume(&p)) {
                    let diff = diff_blocks(&dev, &p).await?;
                    println!("  differs from its parent in {} block(s): {:?}", diff.len(), &diff[..diff.len().min(64)]);
                }
            }
        }
        _ => anyhow::bail!("usage: slab_audit fetch URI OUT | extract SLAB DIR [MAX]"),
    }
    Ok(())
}
