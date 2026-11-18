//! Ship a snapshot's changed extents to a remote volume (#400,
//! stormstorage#46, stormvm#31).
//!
//! `POST /v1/snapshots/{id}/ship {target_uri, base?, max_bytes_per_sec}`
//! answers 202 with a job. The engine opens the target as a remote drive
//! (`nvme-tcp://host:port/<nqn>?nsid=N[&hostnqn=…]`, this node's host NQN
//! when the URI names none), writes **only** the ranges #399 reports for the
//! snapshot against `base` (every allocated extent without one) at the same
//! offsets, makes each `zero` range WRITE ZEROES with deallocate (so a thin
//! target stays thin; an older target that refuses the command gets zeros
//! written), flushes, and closes it. The cost is O(changed bytes), throttled
//! by `max_bytes_per_sec`, kept with the job.
//!
//! `GET /v1/ship-jobs/{id}` reads the job; `DELETE` cancels it (the target
//! is then inconsistent: the caller snapshots it only after `done`). Jobs
//! are kept in `<data_dir>/ship-jobs.json` with how far they got; after a
//! restart a copying job resumes from there, or reads `failed` when its
//! snapshot is gone. A job that existed is never a 404.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::snapshot::ChangedRange;
use crate::volume::VolumeId;

const FILE: &str = "ship-jobs.json";
/// The most one read and write moves.
const CHUNK: u64 = 4 << 20;
/// How often (bytes) the job's progress is written out.
const SAVE_EVERY: u64 = 64 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// `copying`, `done` or `failed`.
    pub state: String,
    /// The bytes of data to send (the `zero` ranges are not counted: they
    /// carry none).
    pub bytes_total: u64,
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// As asked, and the engine volume it named.
    pub snapshot: String,
    pub snapshot_volume: uuid::Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_volume: Option<uuid::Uuid>,
    pub target_uri: String,
    pub max_bytes_per_sec: u64,
    pub ranges: u64,
    pub zero_bytes: u64,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Everything before this offset is on the target (where a restart
    /// resumes).
    pub next_offset: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

static STORE: Mutex<Option<HashMap<String, BTreeMap<String, Job>>>> = Mutex::new(None);
static FILE_LOCK: Mutex<()> = Mutex::new(());
/// Running jobs: their cancel flags.
static RUNNING: Mutex<Option<HashMap<String, Arc<AtomicBool>>>> = Mutex::new(None);

fn dir_of(state: &AppState) -> String {
    state.config.management.data_dir.clone().unwrap_or_default()
}

fn with<R>(state: &AppState, f: impl FnOnce(&mut BTreeMap<String, Job>) -> R) -> R {
    let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let all = g.get_or_insert_with(HashMap::new);
    let k = dir_of(state);
    if !all.contains_key(&k) {
        let loaded = state
            .config
            .management
            .data_dir
            .as_ref()
            .and_then(|d| std::fs::read(std::path::Path::new(d).join(FILE)).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        all.insert(k.clone(), loaded);
    }
    f(all.get_mut(&k).expect("inserted"))
}

fn save(state: &AppState) {
    let Some(d) = state.config.management.data_dir.clone() else { return };
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let body = with(state, |m| serde_json::to_vec_pretty(m).unwrap_or_default());
    let p = std::path::Path::new(&d).join(FILE);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        tracing::warn!("ship: {} not written", p.display());
    }
}

/// Forget what is cached and read the file again (a restart, for tests).
pub fn reload(state: &AppState) {
    let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(all) = g.as_mut() {
        all.remove(&dir_of(state));
    }
}

pub fn get(state: &AppState, id: &str) -> Option<Job> {
    with(state, |m| m.get(id).cloned())
}

fn update(state: &AppState, id: &str, f: impl FnOnce(&mut Job)) {
    with(state, |m| {
        if let Some(j) = m.get_mut(id) {
            f(j);
        }
    });
}

fn running(id: &str) -> Option<Arc<AtomicBool>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(id).cloned()
}

/// What a ship asks for.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub target_uri: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub max_bytes_per_sec: u64,
}

/// A new job (the snapshot and base resolved by the caller), started.
pub fn start(state: Arc<AppState>, snapshot: String, snap: VolumeId, base: Option<(String, VolumeId)>, req: Request) -> Result<Job, String> {
    if crate::drive::nvmeof_dev::NvmeTcpSpec::parse(req.target_uri.trim()).is_none() {
        return Err("target_uri: an nvme-tcp://host:port/<nqn>?nsid=N URI".into());
    }
    let job = Job {
        id: format!("ship-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        state: "copying".into(),
        bytes_total: 0,
        bytes_done: 0,
        message: None,
        snapshot,
        snapshot_volume: snap.0,
        base: base.as_ref().map(|b| b.0.clone()),
        base_volume: base.map(|b| b.1 .0),
        target_uri: req.target_uri.trim().to_string(),
        max_bytes_per_sec: req.max_bytes_per_sec,
        ranges: 0,
        zero_bytes: 0,
        created_at: now(),
        finished_at: None,
        next_offset: 0,
    };
    with(&state, |m| m.insert(job.id.clone(), job.clone()));
    save(&state);
    spawn(state, job.id.clone());
    Ok(job)
}

/// Cancel a job: it stops at its next chunk and reads `failed`.
pub fn cancel(state: &AppState, id: &str) -> Option<Job> {
    let j = get(state, id)?;
    if j.state == "copying" {
        if let Some(flag) = running(id) {
            flag.store(true, Ordering::SeqCst);
        }
        update(state, id, |j| {
            j.state = "failed".into();
            j.message = Some("cancelled: the target holds part of the copy and is not consistent".into());
            j.finished_at = Some(now());
        });
        save(state);
    }
    get(state, id)
}

/// At the engine's start: every job still copying resumes where it got to,
/// or fails when its snapshot is gone.
pub async fn resume(state: Arc<AppState>) {
    let copying: Vec<Job> = with(&state, |m| m.values().filter(|j| j.state == "copying").cloned().collect());
    for j in copying {
        if running(&j.id).is_some() {
            continue;
        }
        let here = state.volume_manager.lock().await.get_volume_handle(&VolumeId(j.snapshot_volume)).is_some();
        if here {
            tracing::info!("ship {}: resumed at byte {} of {} to {} (#400)", j.id, j.bytes_done, j.bytes_total, j.target_uri);
            spawn(state.clone(), j.id.clone());
        } else {
            update(&state, &j.id, |x| {
                x.state = "failed".into();
                x.message = Some(format!("the snapshot {} is gone: the job cannot resume", j.snapshot));
                x.finished_at = Some(now());
            });
            save(&state);
        }
    }
}

fn spawn(state: Arc<AppState>, id: String) {
    let flag = Arc::new(AtomicBool::new(false));
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id.clone(), flag.clone());
    tokio::spawn(async move {
        let r = run(&state, &id, &flag).await;
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).remove(&id);
        if flag.load(Ordering::SeqCst) {
            tracing::warn!("ship {id}: cancelled (#400)");
            return;
        }
        match r {
            Ok(()) => {
                update(&state, &id, |j| {
                    j.state = "done".into();
                    j.finished_at = Some(now());
                    j.message = None;
                });
                if let Some(j) = get(&state, &id) {
                    tracing::info!("ship {id}: done, {} bytes and {} zeroed to {} (#400)", j.bytes_done, j.zero_bytes, j.target_uri);
                }
            }
            Err(e) => {
                tracing::warn!("ship {id}: failed: {e} (#400)");
                update(&state, &id, |j| {
                    j.state = "failed".into();
                    j.message = Some(e);
                    j.finished_at = Some(now());
                });
            }
        }
        save(&state);
    });
}

/// Where to pick up in `ranges`: the part of each at or after `from`.
pub fn remaining(ranges: &[ChangedRange], from: u64) -> Vec<ChangedRange> {
    ranges
        .iter()
        .filter(|r| r.offset + r.length > from)
        .map(|r| {
            let start = r.offset.max(from);
            ChangedRange { offset: start, length: r.offset + r.length - start, zero: r.zero }
        })
        .collect()
}

async fn run(state: &Arc<AppState>, id: &str, cancel: &AtomicBool) -> Result<(), String> {
    use crate::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
    let job = get(state, id).ok_or("the job is gone")?;
    let snap = VolumeId(job.snapshot_volume);
    let (extent, size, changed) = state
        .volume_manager
        .lock()
        .await
        .changes(snap, job.base_volume.map(VolumeId))
        .await
        .map_err(|e| format!("the changes of {}: {e}", job.snapshot))?;
    let ranges = crate::volume::snapshot::changed_ranges(&changed, extent, size);
    let total: u64 = ranges.iter().filter(|r| !r.zero).map(|r| r.length).sum();
    let zeros: u64 = ranges.iter().filter(|r| r.zero).map(|r| r.length).sum();
    update(state, id, |j| {
        j.bytes_total = total;
        j.zero_bytes = zeros;
        j.ranges = ranges.len() as u64;
    });
    save(state);
    let src = state.volume_manager.lock().await.get_volume(&snap).ok_or(format!("the snapshot {} is gone", job.snapshot))?;
    let mut spec = NvmeTcpSpec::parse(&job.target_uri).ok_or("target_uri: not an nvme-tcp:// URI")?;
    if spec.host_nqn.is_none() {
        spec.host_nqn = Some(crate::mgmt::peers::host_nqn(state).await);
    }
    let dst = NvmeofDevice::connect(&spec).await.map_err(|e| format!("connecting to {}: {e}", job.target_uri))?;
    if dst.capacity_bytes() < size {
        return Err(format!("the target is {} bytes; the snapshot is {size}: make it at least that size", dst.capacity_bytes()));
    }
    tracing::info!("ship {id}: {} range(s), {} bytes of data and {} to zero, from byte {} to {} (#400)", ranges.len(), total, zeros, job.next_offset, job.target_uri);
    let started = std::time::Instant::now();
    let mut sent_now = 0u64;
    let mut done = job.bytes_done;
    let mut unsaved = 0u64;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut refused_wz = false;
    for r in remaining(&ranges, job.next_offset) {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        if r.zero {
            if refused_wz || !dst.write_zeroes_deallocate(r.offset, r.length).await.map_err(|e| format!("zeroing {} bytes at {}: {e}", r.length, r.offset))? {
                if !refused_wz {
                    tracing::warn!("ship {id}: the target does not do WRITE ZEROES: zeros written instead (it is not kept thin) (#400)");
                }
                refused_wz = true;
                dst.write_zeroes(r.offset, r.length).await.map_err(|e| format!("zeroing {} bytes at {}: {e}", r.length, r.offset))?;
            }
            update(state, id, |j| j.next_offset = r.offset + r.length);
            continue;
        }
        let mut at = r.offset;
        let end = r.offset + r.length;
        while at < end {
            if cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            let n = (end - at).min(CHUNK) as usize;
            src.read(at, &mut buf[..n]).await.map_err(|e| format!("reading the snapshot at {at}: {e}"))?;
            dst.write(at, &buf[..n]).await.map_err(|e| format!("writing the target at {at}: {e}"))?;
            at += n as u64;
            done += n as u64;
            sent_now += n as u64;
            unsaved += n as u64;
            metrics::counter!("storm_ship_bytes_total").increment(n as u64);
            update(state, id, |j| {
                j.bytes_done = done;
                j.next_offset = at;
            });
            if unsaved >= SAVE_EVERY {
                // What is recorded as sent is on the target first.
                dst.flush().await.map_err(|e| format!("flushing the target: {e}"))?;
                save(state);
                unsaved = 0;
            }
            if job.max_bytes_per_sec > 0 {
                let want = sent_now as f64 / job.max_bytes_per_sec as f64;
                let el = started.elapsed().as_secs_f64();
                if want > el {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(want - el)).await;
                }
            }
        }
    }
    dst.flush().await.map_err(|e| format!("flushing the target: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_mid_range() {
        let r = vec![
            ChangedRange { offset: 0, length: 8, zero: false },
            ChangedRange { offset: 16, length: 8, zero: true },
            ChangedRange { offset: 32, length: 16, zero: false },
        ];
        assert_eq!(remaining(&r, 0), r);
        assert_eq!(
            remaining(&r, 36),
            vec![ChangedRange { offset: 36, length: 12, zero: false }],
            "a restart picks up inside the range it was in"
        );
        assert_eq!(remaining(&r, 20), vec![ChangedRange { offset: 20, length: 4, zero: true }, r[2]]);
        assert!(remaining(&r, 48).is_empty());
    }
}
