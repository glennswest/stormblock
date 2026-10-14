//! A sealed volume's digest, and the check of a copy against it (#384,
//! #245 step 2).
//!
//! **The digest** is taken when a volume is sealed on the node that made it
//! (forge, for a release's goldens), in the background with no lock held.
//! It is the sha256 of the volume's content as 64 KiB chunks: every chunk
//! inside a mapped extent that is not all zeros, in offset order, each as its
//! byte offset (u64 little-endian) and its bytes, after [`SCHEME`]'s prefix.
//! So a 16 TiB blank costs what it holds, and the digest is the same whatever
//! the volume's extent size or wherever its extents are: a composed image's
//! golden (another extent size) and a flowed-over copy (zeros unmapped,
//! #401) hash as their source does.
//!
//! **The verdict** is what reading a copy found. A node reads its local
//! goldens once after a flow-over or a stage, in the background and giving
//! way to the node's own I/O, and records `intact` or `mismatch` beside the
//! digest. `slab holds` reads only the verdicts: no read of gigabytes at
//! boot.
//!
//! Both live in the volume's format-2 header, after the tier (#349's
//! extension), which every format-2 reader skips: an engine rolled back to
//! one without this still opens the disk.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::drive::BlockDevice;
use crate::lockwatch::TrackedMutex;
use crate::volume::extent::VolumeId;
use crate::volume::metadata::{SealDigest, Verdict};
use crate::volume::VolumeManager;

/// What a digest is of, in [`SealDigest::json`] and the hash's prefix.
pub const SCHEME: &str = "sb-seal-1";

/// The unit the content is hashed in.
pub const CHUNK: u64 = 64 * 1024;

/// The longest the reader gives way to the node's own I/O at a time.
const YIELD_MAX: Duration = Duration::from_millis(250);

/// The sha256 of a sealed volume's content (see the module). Reads with no
/// lock held but the volume manager's, briefly, to find the volume.
pub async fn compute(vm: &TrackedMutex<VolumeManager>, id: VolumeId) -> anyhow::Result<[u8; 32]> {
    let (dev, extent_size, capacity, gem) = {
        let m = vm.lock().await;
        let h = m.get_volume_handle(&id).ok_or_else(|| anyhow::anyhow!("volume {} is gone", id.0))?;
        let dev: Arc<dyn BlockDevice> = h.clone();
        (dev, h.extent_size(), h.capacity_bytes(), m.gem().clone())
    };
    let _pin = gem.read().await.pin();
    crate::volume::gem::ensure_resident(&gem, id).await.map_err(|e| anyhow::anyhow!("loading its extent map: {e}"))?;
    let mut extents: Vec<u64> = {
        let g = gem.read().await;
        g.volume_extents(&id).map(|it| it.map(|(v, _)| v).collect()).unwrap_or_default()
    };
    extents.sort_unstable();
    let mut hash = Sha256::new();
    hash.update(SCHEME.as_bytes());
    hash.update(b"\n");
    let mut buf = vec![0u8; extent_size as usize];
    for e in extents {
        let off = e * extent_size;
        if off >= capacity {
            continue;
        }
        let len = extent_size.min(capacity - off) as usize;
        let before = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        let t = std::time::Instant::now();
        dev.read(off, &mut buf[..len]).await.map_err(|err| anyhow::anyhow!("reading at {off}: {err}"))?;
        let took = t.elapsed();
        for (i, chunk) in buf[..len].chunks(CHUNK as usize).enumerate() {
            if chunk.iter().all(|b| *b == 0) {
                continue;
            }
            hash.update((off + i as u64 * CHUNK).to_le_bytes());
            hash.update(chunk);
        }
        // Our own read counts once; anything more is the node's (#269's
        // signal, as the flow-over reads it): give it the disk for as long
        // as one of our reads takes.
        if crate::volume::thin::FOREGROUND_IO.load(Relaxed).saturating_sub(before) > 1 {
            tokio::time::sleep(took.min(YIELD_MAX)).await;
        }
    }
    Ok(hash.finalize().into())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Record a volume's digest (or verdict) and persist it, owed (#364): never
/// with the manager held across the write.
async fn record(vm: &TrackedMutex<VolumeManager>, id: VolumeId, d: SealDigest) {
    {
        let mut m = vm.lock().await;
        // Still sealed: an unseal in between forgot it on purpose.
        if !m.is_sealed(&id) {
            return;
        }
        m.set_seal_digest(id, Some(d));
    }
    if let Err(e) = VolumeManager::persist_detached_checked(vm).await {
        tracing::warn!(volume = %id.0, "a sealed volume's digest was not written to the records: {e}");
    }
}

/// Digest every volume sealed here that waits for one.
pub async fn digest_sealed(vm: &TrackedMutex<VolumeManager>) -> usize {
    let ids = vm.lock().await.take_to_digest();
    let mut done = 0;
    for id in ids {
        match compute(vm, id).await {
            Ok(sha) => {
                // Taken of this copy just now: intact here. A copy of it
                // elsewhere carries it unchecked (`SealDigest::unchecked`).
                let d = SealDigest { verdict: Verdict::Intact, checked_at: now(), ..SealDigest::new(sha) };
                tracing::info!(volume = %id.0, digest = %d.hex(), "sealed volume's digest taken (#384)");
                record(vm, id, d).await;
                done += 1;
            }
            Err(e) => tracing::warn!(volume = %id.0, "no digest for a sealed volume: {e}"),
        }
    }
    done
}

/// What a check found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Checked {
    pub intact: usize,
    pub mismatch: usize,
    pub unreadable: usize,
}

/// Read every sealed volume with a digest and no verdict on this copy, and
/// record what it found (after a flow-over or a stage).
pub async fn check_unchecked(vm: &TrackedMutex<VolumeManager>) -> Checked {
    let todo: Vec<(VolumeId, SealDigest)> = {
        let m = vm.lock().await;
        let mut v: Vec<_> =
            m.seal_digests().into_iter().filter(|(id, d)| d.verdict == Verdict::Unchecked && m.is_sealed(id)).collect();
        v.sort_by_key(|(id, _)| id.0);
        v
    };
    let mut out = Checked::default();
    for (id, d) in todo {
        let handle = vm.lock().await.get_volume_handle(&id);
        let name = match handle {
            Some(h) => h.name().await,
            None => continue,
        };
        match compute(vm, id).await {
            Ok(sha) if sha == d.sha256 => {
                out.intact += 1;
                tracing::info!(volume = %id.0, name, "golden intact: its content is the digest it was sealed with");
                record(vm, id, SealDigest { verdict: Verdict::Intact, checked_at: now(), ..d }).await;
            }
            Ok(sha) => {
                out.mismatch += 1;
                let got: String = sha.iter().map(|b| format!("{b:02x}")).collect();
                tracing::error!(
                    volume = %id.0,
                    name,
                    sealed = %d.hex(),
                    read = %got,
                    "golden NOT intact: this copy does not read as what was sealed; `slab holds` says not held, so the next install lays the system half again"
                );
                record(vm, id, SealDigest { verdict: Verdict::Mismatch, checked_at: now(), ..d }).await;
            }
            Err(e) => {
                out.unreadable += 1;
                tracing::warn!(volume = %id.0, name, "golden not checked: {e}");
            }
        }
    }
    out
}

/// The node's digests at a glance, for health.
pub async fn summary(vm: &TrackedMutex<VolumeManager>) -> Option<serde_json::Value> {
    let m = vm.try_lock().ok()?;
    let (mut intact, mut mismatch, mut unchecked) = (0, 0, 0);
    let mut bad = Vec::new();
    for (id, d) in m.seal_digests() {
        match d.verdict {
            Verdict::Intact => intact += 1,
            Verdict::Unchecked => unchecked += 1,
            Verdict::Mismatch => {
                mismatch += 1;
                bad.push(id.0);
            }
        }
    }
    let waiting = m.digests_waiting();
    Some(serde_json::json!({
        "intact": intact, "mismatch": mismatch, "unchecked": unchecked,
        "to_digest": waiting, "mismatched": bad,
    }))
}

/// The digester: digests what is sealed here when woken by a seal, and
/// checks what arrived with a digest when told to (a finished flow-over or
/// stage, or a start with no flow-over to wait for).
pub fn spawn(vm: Arc<TrackedMutex<VolumeManager>>, check: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        let wake = vm.lock().await.digest_wake();
        loop {
            digest_sealed(&vm).await;
            tokio::select! {
                _ = wake.notified() => {}
                _ = check.notified() => {
                    let c = check_unchecked(&vm).await;
                    if c != Checked::default() {
                        tracing::info!(intact = c.intact, mismatch = c.mismatch, unreadable = c.unreadable, "goldens checked against their digests (#384)");
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
        }
    });
}
