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

/// What one read of a volume cost (#475): logged, and kept for health.
#[derive(Debug, Default, Clone, Copy)]
pub struct Read {
    pub sha256: [u8; 32],
    /// Bytes read off the disk.
    pub bytes: u64,
    /// Time spent reading.
    pub reading: Duration,
    /// Time spent leaving the disk to the node (its I/O, its API, the duty).
    pub yielded: Duration,
}

/// The check or digester running now, for health and the log (#475).
#[derive(Debug, Default)]
struct Progress {
    running: std::sync::atomic::AtomicBool,
    what: std::sync::Mutex<&'static str>,
    started_at: std::sync::atomic::AtomicU64,
    finished_at: std::sync::atomic::AtomicU64,
    volumes: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    reading_ms: std::sync::atomic::AtomicU64,
    yielded_ms: std::sync::atomic::AtomicU64,
    elapsed_ms: std::sync::atomic::AtomicU64,
}

fn progress() -> &'static Progress {
    static P: std::sync::OnceLock<Progress> = std::sync::OnceLock::new();
    P.get_or_init(Progress::default)
}

fn mb_per_s(bytes: u64, d: Duration) -> f64 {
    let s = d.as_secs_f64();
    if s <= 0.0 {
        0.0
    } else {
        (bytes as f64 / 1_000_000.0 / s * 10.0).round() / 10.0
    }
}

/// A run of the check or the digester: said when it starts and ends, with
/// what it read and how fast (#475), and kept for health.
struct Run {
    what: &'static str,
    t: std::time::Instant,
    volumes: u64,
    bytes: u64,
    reading: Duration,
    yielded: Duration,
}

impl Run {
    fn start(what: &'static str, volumes: usize) -> Run {
        let p = progress();
        p.running.store(true, Relaxed);
        *p.what.lock().unwrap_or_else(|e| e.into_inner()) = what;
        p.started_at.store(now(), Relaxed);
        for a in [&p.volumes, &p.bytes, &p.reading_ms, &p.yielded_ms, &p.elapsed_ms] {
            a.store(0, Relaxed);
        }
        tracing::info!(volumes, "{what}: started (#475)");
        Run { what, t: std::time::Instant::now(), volumes: 0, bytes: 0, reading: Duration::ZERO, yielded: Duration::ZERO }
    }

    fn add(&mut self, r: &Read) {
        self.volumes += 1;
        self.bytes += r.bytes;
        self.reading += r.reading;
        self.yielded += r.yielded;
        let p = progress();
        p.volumes.store(self.volumes, Relaxed);
        p.bytes.store(self.bytes, Relaxed);
        p.reading_ms.store(self.reading.as_millis() as u64, Relaxed);
        p.yielded_ms.store(self.yielded.as_millis() as u64, Relaxed);
        p.elapsed_ms.store(self.t.elapsed().as_millis() as u64, Relaxed);
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let p = progress();
        let took = self.t.elapsed();
        p.elapsed_ms.store(took.as_millis() as u64, Relaxed);
        p.finished_at.store(now(), Relaxed);
        p.running.store(false, Relaxed);
        tracing::info!(
            volumes = self.volumes,
            mb = self.bytes / 1_000_000,
            secs = took.as_secs(),
            reading_secs = self.reading.as_secs(),
            yielded_secs = self.yielded.as_secs(),
            mb_per_s = mb_per_s(self.bytes, took),
            "{}: ended: {} volume(s), {} MB in {:.0} s ({} MB/s; {:.0} s reading, {:.0} s left to the node)",
            self.what,
            self.volumes,
            self.bytes / 1_000_000,
            took.as_secs_f64(),
            mb_per_s(self.bytes, took),
            self.reading.as_secs_f64(),
            self.yielded.as_secs_f64(),
        );
    }
}

/// The sha256 of a sealed volume's content (see the module). Reads with no
/// lock held but the volume manager's, briefly, to find the volume.
///
/// A background read (#475): before each extent it waits for the API's
/// changes to be answered ([`bgio::wait_clear`](crate::volume::bgio)), after
/// it gives way to the node's own volume I/O, and on a rotating disk it keeps
/// to its share of the disk's time.
pub async fn compute(vm: &TrackedMutex<VolumeManager>, id: VolumeId) -> anyhow::Result<Read> {
    use crate::volume::bgio;
    let (dev, extent_size, capacity, gem) = {
        let m = vm.lock().await;
        let h = m.get_volume_handle(&id).ok_or_else(|| anyhow::anyhow!("volume {} is gone", id.0))?;
        let dev: Arc<dyn BlockDevice> = h.clone();
        (dev, h.extent_size(), h.capacity_bytes(), m.gem().clone())
    };
    let rotational = dev.rotational();
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
    let mut out = Read::default();
    for e in extents {
        let off = e * extent_size;
        if off >= capacity {
            continue;
        }
        let len = extent_size.min(capacity - off) as usize;
        out.yielded += bgio::wait_clear(rotational).await;
        let before = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        let t = std::time::Instant::now();
        dev.read(off, &mut buf[..len]).await.map_err(|err| anyhow::anyhow!("reading at {off}: {err}"))?;
        let took = t.elapsed();
        out.reading += took;
        out.bytes += len as u64;
        for (i, chunk) in buf[..len].chunks(CHUNK as usize).enumerate() {
            if chunk.iter().all(|b| *b == 0) {
                continue;
            }
            hash.update((off + i as u64 * CHUNK).to_le_bytes());
            hash.update(chunk);
        }
        // Our own read counts once; anything more is the node's (#269's
        // signal, as the flow-over reads it): give it the disk for as long
        // as one of our reads takes. On a rotating disk, keep to the
        // reader's share of it besides (#475).
        let mut pause = bgio::duty_pause(took, rotational);
        if crate::volume::thin::FOREGROUND_IO.load(Relaxed).saturating_sub(before) > 1 {
            pause = pause.max(took.min(YIELD_MAX));
        }
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
            out.yielded += pause;
        }
    }
    out.sha256 = hash.finalize().into();
    Ok(out)
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
    if ids.is_empty() {
        return 0;
    }
    let mut run = Run::start("digester", ids.len());
    let mut done = 0;
    for id in ids {
        match compute(vm, id).await {
            Ok(r) => {
                run.add(&r);
                // Taken of this copy just now: intact here. A copy of it
                // elsewhere carries it unchecked (`SealDigest::unchecked`).
                let d = SealDigest { verdict: Verdict::Intact, checked_at: now(), ..SealDigest::new(r.sha256) };
                tracing::info!(volume = %id.0, digest = %d.hex(), mb = r.bytes / 1_000_000, mb_per_s = mb_per_s(r.bytes, r.reading + r.yielded), "sealed volume's digest taken (#384)");
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
    if todo.is_empty() {
        return out;
    }
    let mut run = Run::start("golden check", todo.len());
    for (id, d) in todo {
        let handle = vm.lock().await.get_volume_handle(&id);
        let name = match handle {
            Some(h) => h.name().await,
            None => continue,
        };
        let r = compute(vm, id).await;
        if let Ok(r) = &r {
            run.add(r);
        }
        match r {
            Ok(r) if r.sha256 == d.sha256 => {
                out.intact += 1;
                tracing::info!(
                    volume = %id.0, name, mb = r.bytes / 1_000_000,
                    reading_ms = r.reading.as_millis() as u64, yielded_ms = r.yielded.as_millis() as u64,
                    "golden intact: its content is the digest it was sealed with"
                );
                record(vm, id, SealDigest { verdict: Verdict::Intact, checked_at: now(), ..d }).await;
            }
            Ok(r) => {
                out.mismatch += 1;
                let got: String = r.sha256.iter().map(|b| format!("{b:02x}")).collect();
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
    let p = progress();
    let (bytes, elapsed) = (p.bytes.load(Relaxed), Duration::from_millis(p.elapsed_ms.load(Relaxed)));
    let started = p.started_at.load(Relaxed);
    let reading = if started == 0 {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "what": *p.what.lock().unwrap_or_else(|e| e.into_inner()),
            "running": p.running.load(Relaxed),
            "started_at": started,
            "finished_at": if p.running.load(Relaxed) { 0 } else { p.finished_at.load(Relaxed) },
            "volumes": p.volumes.load(Relaxed),
            "bytes": bytes,
            "reading_ms": p.reading_ms.load(Relaxed),
            "yielded_ms": p.yielded_ms.load(Relaxed),
            "mb_per_s": mb_per_s(bytes, elapsed),
        })
    };
    Some(serde_json::json!({
        "intact": intact, "mismatch": mismatch, "unchecked": unchecked,
        "to_digest": waiting, "mismatched": bad, "reading": reading,
    }))
}

/// How long the node's volume I/O must be still before the golden check
/// starts (#475).
const CHECK_QUIET: Duration = Duration::from_secs(10);

/// The most the golden check waits for the node to settle (#475):
/// `STORMBLOCK_GOLDEN_CHECK_GRACE_SECS`, 120 by default, 0 = no wait.
fn check_grace() -> Duration {
    Duration::from_secs(
        std::env::var("STORMBLOCK_GOLDEN_CHECK_GRACE_SECS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(120),
    )
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
                    // Not while the node boots (#475): the check is asked
                    // for at the start (or after a flow-over), when every
                    // unit reads its volumes and the kubelet clones roots.
                    let (waited, quiet) = crate::volume::bgio::wait_for_quiet(check_grace(), CHECK_QUIET).await;
                    if !waited.is_zero() {
                        tracing::info!(
                            "golden check: waited {:.0} s for the node's I/O to settle ({})",
                            waited.as_secs_f64(),
                            if quiet { "it did" } else { "it did not; checking anyway, paced" }
                        );
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::{Slab, SlabFormat};
    use crate::placement::topology::StorageTier;
    use crate::raid::RaidArrayId;

    const SLOT: u64 = 1024 * 1024;

    /// A manager over one file slab whose records live on it (format 2).
    async fn manager(dir: &tempfile::TempDir) -> (Arc<TrackedMutex<VolumeManager>>, Arc<dyn BlockDevice>) {
        let path = dir.path().join("slab.bin").to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_with_capacity(&path, 256 * SLOT).await.unwrap());
        let slab = Slab::format_with(dev.clone(), SlabFormat::new(SLOT, StorageTier::Hot).with_metadata(16 * SLOT).with_version(2))
            .await
            .unwrap();
        let sid = slab.slab_id();
        let mut m = VolumeManager::new(SLOT);
        m.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), slab).await.unwrap();
        m.persist_to_slab(sid);
        (Arc::new(TrackedMutex::new(m)), dev)
    }

    /// The scheme, written out independently of `compute`.
    fn expected(chunks: &[(u64, Vec<u8>)]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"sb-seal-1\n");
        for (off, bytes) in chunks {
            for (i, c) in bytes.chunks(CHUNK as usize).enumerate() {
                if c.iter().all(|b| *b == 0) {
                    continue;
                }
                h.update((off + i as u64 * CHUNK).to_le_bytes());
                h.update(c);
            }
        }
        h.finalize().into()
    }

    async fn write(vm: &TrackedMutex<VolumeManager>, id: VolumeId, off: u64, bytes: &[u8]) {
        let dev = vm.lock().await.get_volume(&id).unwrap();
        dev.write(off, bytes).await.unwrap();
        dev.flush().await.unwrap();
    }

    /// #384: a seal takes the digest of what was sealed (its non-zero 64 KiB
    /// chunks with their offsets), the same whether an all-zero extent is
    /// mapped or not; it is on the slab's records; an unseal forgets it; and
    /// a release's volume is never digested here.
    #[tokio::test]
    async fn a_seal_takes_the_digest_of_what_was_sealed_and_an_unseal_forgets_it() {
        let dir = tempfile::tempdir().unwrap();
        let (vm, dev) = manager(&dir).await;
        let mut content = vec![0u8; 3 * SLOT as usize];
        content[..4096].fill(0x11);
        content[2 * SLOT as usize + 70_000..2 * SLOT as usize + 80_000].fill(0x22);
        let (a, b) = {
            let mut m = vm.lock().await;
            (m.create_volume_any("a", 8 * SLOT).await.unwrap(), m.create_volume_any("b", 8 * SLOT).await.unwrap())
        };
        // `a` maps extents 0 and 2; `b` the same bytes and extent 5 written
        // with zeros (mapped, as a flow-over before #401 left one).
        write(&vm, a, 0, &content[..SLOT as usize]).await;
        write(&vm, a, 2 * SLOT, &content[2 * SLOT as usize..]).await;
        write(&vm, b, 0, &content[..SLOT as usize]).await;
        write(&vm, b, 2 * SLOT, &content[2 * SLOT as usize..]).await;
        write(&vm, b, 5 * SLOT, &vec![0u8; SLOT as usize]).await;
        for id in [a, b] {
            vm.lock().await.seal_volume(id, None).await.unwrap();
        }
        assert_eq!(vm.lock().await.digests_waiting(), 2, "both sealed volumes wait for a digest");
        assert_eq!(digest_sealed(&vm).await, 2);
        let want = expected(&[(0, content[..SLOT as usize].to_vec()), (2 * SLOT, content[2 * SLOT as usize..].to_vec())]);
        let da = vm.lock().await.seal_digest(&a).unwrap();
        let db = vm.lock().await.seal_digest(&b).unwrap();
        assert_eq!(da.sha256, want, "the digest is the scheme's");
        assert_eq!(db.sha256, want, "an all-zero extent, mapped or not, hashes the same");
        assert_eq!(da.verdict, Verdict::Intact, "taken of this copy: intact here");

        // On the slab's own records, read offline as `slab holds` reads them.
        let slab = Slab::open(dev.clone()).await.unwrap();
        let doc = crate::volume::metav2::read_slab(&slab).await.unwrap().unwrap();
        let rec = doc.volumes.iter().find(|v| v.id == a).unwrap();
        assert_eq!(rec.digest, Some(da), "the digest is in the volume's header on the slab");

        // Unsealed: it says nothing any more, on the slab too.
        vm.lock().await.unseal_volume(a).await.unwrap();
        assert!(vm.lock().await.seal_digest(&a).is_none());
        let doc = crate::volume::metav2::read_slab(&Slab::open(dev.clone()).await.unwrap()).await.unwrap().unwrap();
        assert!(doc.volumes.iter().find(|v| v.id == a).unwrap().digest.is_none());

        // A release's volume keeps what it came with: none is taken here.
        let r = vm.lock().await.create_volume_any("release", 4 * SLOT).await.unwrap();
        write(&vm, r, 0, &[7u8; 4096]).await;
        vm.lock().await.set_origin(r, crate::volume::metadata::Origin::Release);
        vm.lock().await.seal_volume(r, None).await.unwrap();
        assert_eq!(vm.lock().await.digests_waiting(), 0, "a release's volume is not digested from its own copy");
    }

    /// #384: a copy that came with a digest is read against it: intact when
    /// it is what was sealed, a mismatch when a byte under it changed.
    #[tokio::test]
    async fn a_copy_is_checked_against_the_digest_it_came_with() {
        let dir = tempfile::tempdir().unwrap();
        let (vm, dev) = manager(&dir).await;
        let id = vm.lock().await.create_volume_any("golden", 4 * SLOT).await.unwrap();
        write(&vm, id, 0, &vec![0x5Au8; SLOT as usize]).await;
        write(&vm, id, 3 * SLOT, &vec![0xA5u8; 8192]).await;
        vm.lock().await.seal_volume(id, None).await.unwrap();
        digest_sealed(&vm).await;
        let sealed = vm.lock().await.seal_digest(&id).unwrap();

        // As a compose or a stage carries it: unchecked.
        vm.lock().await.set_seal_digest(id, Some(sealed.unchecked()));
        assert_eq!(check_unchecked(&vm).await, Checked { intact: 1, mismatch: 0, unreadable: 0 });
        let d = vm.lock().await.seal_digest(&id).unwrap();
        assert_eq!((d.verdict, d.sha256), (Verdict::Intact, sealed.sha256));
        assert!(d.checked_at > 0);
        assert_eq!(check_unchecked(&vm).await, Checked::default(), "an intact copy is not read again");

        // A byte under the golden changes on the drive.
        let (slot, slab_off) = {
            let m = vm.lock().await;
            let loc = m.gem().read().await.lookup(id, 0).unwrap();
            let reg = m.registry().read().await;
            (loc.slot_idx, reg.get(&loc.slab_id).unwrap().data_offset())
        };
        dev.write(slab_off + slot * SLOT + 4096, &[0u8; 512]).await.unwrap();
        dev.flush().await.unwrap();
        vm.lock().await.set_seal_digest(id, Some(sealed.unchecked()));
        assert_eq!(check_unchecked(&vm).await, Checked { intact: 0, mismatch: 1, unreadable: 0 });
        assert_eq!(vm.lock().await.seal_digest(&id).unwrap().verdict, Verdict::Mismatch);
        let s = summary(&vm).await.unwrap();
        assert_eq!(s["mismatch"], 1);
        assert_eq!(s["mismatched"][0], id.0.to_string());
    }
}
