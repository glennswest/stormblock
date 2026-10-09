//! Namespace IDs are never reused (#96).
//!
//! An attach hands out `nvme-tcp://…/<subsystem nqn>?nsid=N`, and in a
//! subsystem shared by several volumes the NSID is the only thing that tells
//! them apart. Allocation was lowest-free, so a released volume's NSID went
//! to the next attach, and anyone still holding the old address attached a
//! different volume with no error. So each subsystem hands out NSIDs from a
//! high-water mark that only rises, and the mark is kept across restarts in
//! `<data_dir>/nsid_high.json`: a stale address then finds nothing, never
//! someone else's volume.
//!
//! 32 bits is not a constraint: a node mints a handful of NSIDs a boot. Should
//! a subsystem ever reach the top, it falls back to the lowest free ID and
//! says so.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// The highest NSID the protocol allows (0xFFFFFFFF is the broadcast value).
pub const MAX_NSID: u32 = 0xFFFF_FFFE;

struct Record {
    path: Option<PathBuf>,
    high: HashMap<String, u32>,
}

fn record() -> &'static Mutex<Record> {
    static R: std::sync::OnceLock<Mutex<Record>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(Record { path: None, high: HashMap::new() }))
}

/// Keep the marks in `path` from now on, starting from what it holds. Marks
/// raised before this (none, in an engine that calls it at start) are kept
/// too, whichever is higher.
pub fn load(path: PathBuf) {
    let saved: HashMap<String, u32> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut r = record().lock().unwrap_or_else(|e| e.into_inner());
    for (nqn, n) in saved {
        let e = r.high.entry(nqn).or_insert(0);
        *e = (*e).max(n);
    }
    r.path = Some(path);
}

/// The highest NSID `nqn` has handed out.
pub fn high(nqn: &str) -> u32 {
    record().lock().unwrap_or_else(|e| e.into_inner()).high.get(nqn).copied().unwrap_or(0)
}

/// `nsid` has been handed out in `nqn`: raise the mark, and keep it.
pub fn raise(nqn: &str, nsid: u32) {
    let mut r = record().lock().unwrap_or_else(|e| e.into_inner());
    let cur = r.high.get(nqn).copied().unwrap_or(0);
    if nsid <= cur {
        return;
    }
    r.high.insert(nqn.to_string(), nsid);
    if let Some(path) = r.path.clone() {
        let body = serde_json::to_vec_pretty(&r.high).unwrap_or_default();
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let ok = std::fs::write(&tmp, &body).and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = ok {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("nvmeof: keeping the NSID high-water mark in {}: {e}", path.display());
        }
    }
}

/// The next NSID for `nqn`: above every one it has handed out and every one
/// in `used` (#96). The lowest free ID only if the subsystem has reached the
/// top of the range, said.
pub fn next(nqn: &str, used: impl Iterator<Item = u32> + Clone) -> u32 {
    let top = used.clone().max().unwrap_or(0).max(high(nqn));
    if top < MAX_NSID {
        return top + 1;
    }
    let taken: std::collections::HashSet<u32> = used.collect();
    let n = (1..=MAX_NSID).find(|n| !taken.contains(n)).unwrap_or(1);
    tracing::warn!("nvmeof: {nqn} has handed out every NSID up to {MAX_NSID}; reusing {n}");
    n
}
