//! The background eraser (#286): overwrites the slots slabs marked
//! `Erasing`, then frees them.
//!
//! A slab marks a slot `Erasing` when its last reference goes and an erase
//! level is set (`drive::erase`); the slot is out of the free pool from that
//! moment, and its entry on the device says so, so a stop or a power cut
//! resumes the erase rather than handing the bytes out. This task takes
//! batches of them, writes each level's passes with no lock held (a flush
//! after every pass, the last pass read back for the DoD levels, a discard
//! after on anything but a spinning disk), and then frees them the ordinary
//! durable way (#171). Foreground I/O comes first, as for the flow-over
//! (#269).
//!
//! What it erased is recorded per volume (the volume a slot was allocated
//! for): `<data_dir>/erasures.json`, `GET /api/v1/erasures`, a log line, and
//! the `stormblock_erase_*` metrics.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::drive::erase::{EraseLevel, Pass};
use crate::drive::slab::EraseJob;
use crate::drive::slab_registry::SlabRegistry;
use crate::drive::{DriveError, DriveResult, DriveType};
use crate::volume::extent::VolumeId;

/// Slots taken per round.
const BATCH: usize = 16;
/// Bytes written per request.
const CHUNK: u64 = 1024 * 1024;
/// Longest the eraser steps aside for foreground I/O between slots.
const YIELD_MAX: Duration = Duration::from_millis(500);
/// How often an idle eraser looks for work.
const IDLE: Duration = Duration::from_secs(1);
/// Audit records kept.
const KEEP: usize = 1000;

/// One volume's erase, finished: the audit record (#286).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Erasure {
    /// The volume the slots were allocated for.
    pub volume: String,
    pub level: EraseLevel,
    pub passes: usize,
    pub slots: u64,
    pub bytes: u64,
    /// Every slot's last pass read back as written (DoD levels).
    pub verified: bool,
    /// Discarded after the passes (not on a spinning disk).
    pub discarded: bool,
    /// Unix seconds.
    pub started: u64,
    pub finished: u64,
    pub duration_ms: u64,
    /// Slots that failed and were put back to retry.
    pub retries: u64,
}

struct Running {
    level: EraseLevel,
    slots: u64,
    bytes: u64,
    verified: bool,
    discarded: bool,
    started: Instant,
    started_unix: u64,
    retries: u64,
}

/// What `GET /api/v1/erasures` reports.
#[derive(Debug, Clone, Serialize)]
pub struct EraseStatus {
    /// The node's level for a delete that does not name one.
    pub default: EraseLevel,
    /// Slots waiting to be overwritten.
    pub pending_slots: u64,
    /// Volumes being erased, with slots done so far.
    pub running: Vec<RunningView>,
    /// Finished erases, newest last.
    pub finished: Vec<Erasure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningView {
    pub volume: String,
    pub level: EraseLevel,
    pub slots_done: u64,
    pub slots_left: u64,
}

/// The eraser: its log and the task that drains the queue.
pub struct Eraser {
    registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    path: Option<PathBuf>,
    running: std::sync::Mutex<HashMap<VolumeId, Running>>,
    finished: std::sync::Mutex<Vec<Erasure>>,
    kick: tokio::sync::Notify,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Eraser {
    /// An eraser over `registry`, keeping its log in `data_dir` when given.
    pub fn new(registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>, data_dir: Option<PathBuf>) -> Arc<Self> {
        let path = data_dir.map(|d| d.join("erasures.json"));
        let finished = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Eraser {
            registry,
            path,
            running: Default::default(),
            finished: std::sync::Mutex::new(finished),
            kick: tokio::sync::Notify::new(),
        })
    }

    /// Start the task. It runs until the process ends.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(crate::lockwatch::named("the eraser", async move { me.run().await }))
    }

    /// Look for work now rather than at the next idle tick.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// The queue, what is running and what finished.
    pub async fn status(&self) -> EraseStatus {
        let reg = self.registry.read().await;
        let running = self
            .running
            .lock()
            .unwrap()
            .iter()
            .map(|(v, r)| RunningView {
                volume: v.0.to_string(),
                level: r.level,
                slots_done: r.slots,
                slots_left: reg.erasing_for(*v),
            })
            .collect();
        EraseStatus {
            default: reg.erase_default(),
            pending_slots: reg.erasing_slots(),
            running,
            finished: self.finished.lock().unwrap().clone(),
        }
    }

    async fn run(self: Arc<Self>) {
        let mut foreground = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        let mut last = Duration::ZERO;
        loop {
            match self.round(&mut foreground, &mut last).await {
                0 => {
                    let _ = tokio::time::timeout(IDLE, self.kick.notified()).await;
                }
                _ => tokio::task::yield_now().await,
            }
        }
    }

    /// Erase up to one batch. How many slots it took.
    pub async fn round(&self, foreground: &mut u64, last: &mut Duration) -> usize {
        let jobs: Vec<EraseJob> = {
            let reg = self.registry.read().await;
            let mut jobs = Vec::new();
            for (_, slab) in reg.iter() {
                if jobs.len() >= BATCH {
                    break;
                }
                jobs.extend(slab.take_erasing(BATCH - jobs.len()));
            }
            jobs
        };
        let pending = jobs.len();
        if jobs.is_empty() {
            metrics::gauge!("stormblock_erase_pending_slots").set(0.0);
            return 0;
        }
        let mut done: Vec<(EraseJob, DriveResult<Outcome>)> = Vec::with_capacity(jobs.len());
        for job in jobs {
            // Foreground first (#269): when a volume was read or written
            // since the last slot, give the disk back for as long as that
            // slot took (capped).
            if crate::volume::thin::FOREGROUND_IO.load(Relaxed) != *foreground {
                tokio::time::sleep((*last).min(YIELD_MAX)).await;
            }
            *foreground = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
            let started = Instant::now();
            let res = erase_one(&job).await;
            *last = started.elapsed();
            done.push((job, res));
        }
        // Free what was overwritten; put back what failed. The free entries
        // are written with no registry lock (#521): under its write lock, a
        // table write a slot held every reader 1-2 s a batch on pvetest3.
        let mut by_slab: HashMap<crate::drive::slab::SlabId, Vec<u64>> = HashMap::new();
        for (job, res) in &done {
            if res.is_ok() {
                by_slab.entry(job.slab).or_default().push(job.slot);
            }
        }
        let finishers: Vec<_> = {
            let reg = self.registry.read().await;
            by_slab.into_iter().filter_map(|(id, slots)| reg.get(&id).map(|s| (id, s.erase_finisher(), slots))).collect()
        };
        let mut freed: std::collections::HashSet<(crate::drive::slab::SlabId, u64)> = Default::default();
        for (id, finisher, slots) in finishers {
            match finisher.finish(&slots).await {
                Ok(f) => freed.extend(f.into_iter().map(|s| (id, s))),
                Err(e) => tracing::warn!(slab = %id.0, slots = slots.len(), "erased slots not freed: {e}"),
            }
        }
        let mut results: Vec<(EraseJob, Option<Outcome>)> = Vec::with_capacity(done.len());
        let still: HashMap<VolumeId, u64>;
        {
            let reg = self.registry.read().await;
            for (job, res) in done {
                let Some(slab) = reg.get(&job.slab) else { continue };
                let ok = match res {
                    Ok(o) if freed.contains(&(job.slab, job.slot)) => Some(o),
                    Ok(_) => {
                        slab.return_erase(job.slot);
                        None
                    }
                    Err(e) => {
                        tracing::warn!(slab = %job.slab.0, slot = job.slot, level = %job.level, "erase failed, retrying later: {e}");
                        slab.return_erase(job.slot);
                        None
                    }
                };
                results.push((job, ok));
            }
            let vols: Vec<VolumeId> = {
                let running = self.running.lock().unwrap();
                running.keys().copied().chain(results.iter().map(|(j, _)| j.volume)).collect()
            };
            still = vols.into_iter().map(|v| (v, reg.erasing_for(v))).collect();
            metrics::gauge!("stormblock_erase_pending_slots").set(reg.erasing_slots() as f64);
        }
        let mut finished_vols = Vec::new();
        {
            let mut running = self.running.lock().unwrap();
            for (job, ok) in results {
                let r = running.entry(job.volume).or_insert_with(|| Running {
                    level: job.level,
                    slots: 0,
                    bytes: 0,
                    verified: true,
                    discarded: true,
                    started: Instant::now(),
                    started_unix: unix_now(),
                    retries: 0,
                });
                r.level = r.level.max(job.level);
                match ok {
                    Some(o) => {
                        r.slots += 1;
                        r.bytes += job.len;
                        r.verified &= o.verified;
                        r.discarded &= o.discarded;
                        metrics::counter!("stormblock_erased_slots_total", "level" => job.level.as_str()).increment(1);
                        metrics::counter!("stormblock_erased_bytes_total").increment(job.len);
                    }
                    None => r.retries += 1,
                }
            }
            for vol in running.keys() {
                if still.get(vol).copied().unwrap_or(0) == 0 {
                    finished_vols.push(*vol);
                }
            }
        }
        if !finished_vols.is_empty() {
            self.finish(finished_vols).await;
        }
        pending
    }

    async fn finish(&self, vols: Vec<VolumeId>) {
        let records: Vec<Erasure> = {
            let mut running = self.running.lock().unwrap();
            vols.iter()
                .filter_map(|v| running.remove(v).map(|r| (v, r)))
                .map(|(v, r)| Erasure {
                    volume: v.0.to_string(),
                    level: r.level,
                    passes: r.level.passes().len(),
                    slots: r.slots,
                    bytes: r.bytes,
                    verified: r.verified && r.level.verifies(),
                    discarded: r.discarded,
                    started: r.started_unix,
                    finished: unix_now(),
                    duration_ms: r.started.elapsed().as_millis() as u64,
                    retries: r.retries,
                })
                .collect()
        };
        for e in &records {
            tracing::info!(
                volume = %e.volume, level = %e.level, passes = e.passes, slots = e.slots,
                bytes = e.bytes, verified = e.verified, duration_ms = e.duration_ms,
                "erased the freed data of a volume (#286)"
            );
        }
        let snapshot = {
            let mut f = self.finished.lock().unwrap();
            f.extend(records);
            let excess = f.len().saturating_sub(KEEP);
            f.drain(..excess);
            f.clone()
        };
        if let Some(path) = &self.path {
            let tmp = path.with_extension("json.tmp");
            let body = serde_json::to_vec_pretty(&snapshot).unwrap_or_default();
            let res = async {
                tokio::fs::write(&tmp, &body).await?;
                tokio::fs::rename(&tmp, path).await
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("could not write {}: {e}", path.display());
            }
        }
    }

    /// Drain the queue now, in the caller's task (tests, and a stop that
    /// wants the queue empty). Returns the slots erased.
    pub async fn drain(&self) -> usize {
        let mut fg = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        let mut last = Duration::ZERO;
        let mut total = 0;
        let mut left = self.registry.read().await.erasing_slots();
        loop {
            if self.round(&mut fg, &mut last).await == 0 {
                return total;
            }
            let now = self.registry.read().await.erasing_slots();
            if now >= left {
                // Nothing came off the queue: every slot failed. Leave them
                // for the background task rather than spin here.
                return total;
            }
            total += (left - now) as usize;
            left = now;
        }
    }
}

/// What one slot's erase did.
pub struct Outcome {
    /// Its last pass was read back as written.
    pub verified: bool,
    /// It was discarded after the passes.
    pub discarded: bool,
}

/// Write `job`'s passes over its slot, flush after each, read the last back
/// when the level verifies, then discard (not on a spinning disk).
pub async fn erase_one(job: &EraseJob) -> DriveResult<Outcome> {
    let passes = job.level.passes();
    let chunk = CHUNK.min(job.len) as usize;
    let mut buf = vec![0u8; chunk];
    let mut seed = 0u64;
    for (i, pass) in passes.iter().enumerate() {
        if *pass == Pass::Random {
            seed = rand::thread_rng().next_u64();
        }
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut done = 0u64;
        while done < job.len {
            let n = (job.len - done).min(chunk as u64) as usize;
            match pass {
                Pass::Byte(b) => buf[..n].fill(*b),
                Pass::Random => rng.fill_bytes(&mut buf[..n]),
            }
            job.device.write(job.offset + done, &buf[..n]).await?;
            done += n as u64;
        }
        job.device.flush().await?;
        let last = i + 1 == passes.len();
        if last && job.level.verifies() {
            let mut want = vec![0u8; chunk];
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut done = 0u64;
            while done < job.len {
                let n = (job.len - done).min(chunk as u64) as usize;
                match pass {
                    Pass::Byte(b) => want[..n].fill(*b),
                    Pass::Random => rng.fill_bytes(&mut want[..n]),
                }
                job.device.read(job.offset + done, &mut buf[..n]).await?;
                if buf[..n] != want[..n] {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "erase verify failed at byte {} of slot {} on slab {}",
                        done,
                        job.slot,
                        job.slab.0
                    )));
                }
                done += n as u64;
            }
        }
    }
    let discarded = job.device.device_type() != DriveType::SasHdd
        && job.device.discard(job.offset, job.len).await.is_ok();
    Ok(Outcome { verified: job.level.verifies(), discarded })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::raid::RaidArrayId;
    use crate::volume::VolumeManager;

    const MIB: u64 = 1024 * 1024;

    fn holds(raw: &[u8], pat: &[u8]) -> bool {
        raw.windows(pat.len()).any(|w| w == pat)
    }

    /// A deleted volume's data is on the media until the eraser runs, then
    /// gone; the collector never takes an erasing slot; the audit says what
    /// was done.
    #[tokio::test]
    async fn a_deleted_volumes_data_is_overwritten_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 32 * MIB)
                .await
                .unwrap(),
        );
        let mut vm = VolumeManager::new(MIB);
        vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
        vm.registry().write().await.set_erase_default(EraseLevel::Once);

        let id = vm.create_volume_any("secret", 4 * MIB).await.unwrap();
        let pat = b"SECRET-286-DO-NOT-KEEP-";
        let data: Vec<u8> = pat.iter().copied().cycle().take(2 * MIB as usize).collect();
        let h = vm.get_volume(&id).unwrap();
        h.write(0, &data).await.unwrap();
        h.flush().await.unwrap();
        drop(h);

        let mut all = vec![0u8; dev.capacity_bytes() as usize];
        dev.read(0, &mut all).await.unwrap();
        assert!(holds(&all, pat));

        vm.delete_volume(id).await.unwrap();
        let reg = vm.registry().clone();
        assert_eq!(reg.read().await.erasing_slots(), 2);
        assert_eq!(reg.read().await.erasing_for(id), 2);

        // The collector sees nobody's slots, and frees none of them.
        let report = {
            let gem = vm.gem().read().await;
            let mut r = reg.write().await;
            crate::volume::gc::collect(&gem, &mut r, crate::volume::gc::GcOptions::default()).await
        };
        assert_eq!(report.reclaimed, 0);
        assert_eq!(reg.read().await.erasing_slots(), 2);

        let eraser = Eraser::new(reg.clone(), Some(dir.path().to_path_buf()));
        assert_eq!(eraser.drain().await, 2);
        assert_eq!(reg.read().await.erasing_slots(), 0);
        dev.read(0, &mut all).await.unwrap();
        assert!(!holds(&all, pat), "the deleted volume's data is still on the device");

        let st = eraser.status().await;
        assert_eq!(st.pending_slots, 0);
        assert!(st.running.is_empty());
        assert_eq!(st.finished.len(), 1);
        let e = &st.finished[0];
        assert_eq!(e.volume, id.0.to_string());
        assert_eq!((e.level, e.passes, e.slots, e.bytes), (EraseLevel::Once, 1, 2, 2 * MIB));
        assert!(!e.verified);
        let kept: Vec<Erasure> =
            serde_json::from_slice(&std::fs::read(dir.path().join("erasures.json")).unwrap()).unwrap();
        assert_eq!(kept, st.finished);
        // Kept across a restart.
        let again = Eraser::new(reg.clone(), Some(dir.path().to_path_buf()));
        assert_eq!(again.status().await.finished, st.finished);
    }

    /// `delete_volume_erasing` asks for more than the node's default.
    #[tokio::test]
    async fn a_delete_may_ask_for_more_passes_than_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 32 * MIB)
                .await
                .unwrap(),
        );
        let mut vm = VolumeManager::new(MIB);
        vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
        vm.registry().write().await.set_erase_default(EraseLevel::Once);
        let id = vm.create_volume_any("v", 2 * MIB).await.unwrap();
        let h = vm.get_volume(&id).unwrap();
        h.write(0, &vec![0x5A; MIB as usize]).await.unwrap();
        h.flush().await.unwrap();
        drop(h);
        vm.delete_volume_erasing(id, Some(EraseLevel::Dod7)).await.unwrap();
        let eraser = Eraser::new(vm.registry().clone(), None);
        assert_eq!(eraser.drain().await, 1);
        let e = &eraser.status().await.finished[0];
        assert_eq!((e.level, e.passes, e.verified), (EraseLevel::Dod7, 7, true));
    }

    /// A device whose writes take `ms` once it is set.
    struct SlowWrites {
        inner: Arc<dyn BlockDevice>,
        ms: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl BlockDevice for SlowWrites {
        fn id(&self) -> &crate::drive::DeviceId {
            self.inner.id()
        }
        fn capacity_bytes(&self) -> u64 {
            self.inner.capacity_bytes()
        }
        fn block_size(&self) -> u32 {
            self.inner.block_size()
        }
        fn optimal_io_size(&self) -> u32 {
            self.inner.optimal_io_size()
        }
        fn device_type(&self) -> DriveType {
            self.inner.device_type()
        }
        async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
            self.inner.read(offset, buf).await
        }
        async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
            let ms = self.ms.load(Relaxed);
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            self.inner.write(offset, buf).await
        }
        async fn flush(&self) -> DriveResult<()> {
            self.inner.flush().await
        }
        async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
            self.inner.discard(offset, len).await
        }
    }

    /// #521: pvetest3 12.74, `the slab registry (write) was held 1.0-2.0s by
    /// the eraser`, seven times in ten minutes, and `GET /volumes` and
    /// `/slabs` waited 1.0-1.9 s for it. The eraser freed a batch's slots
    /// under the registry's write lock, one table write after another. A
    /// reader now never waits on a slot's table write, and every slot is
    /// still freed, on the device too.
    #[tokio::test]
    async fn freeing_erased_slots_keeps_no_reader_waiting() {
        const WRITE_MS: u64 = 50;
        let dir = tempfile::tempdir().unwrap();
        let file: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 64 * MIB)
                .await
                .unwrap(),
        );
        let ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dev: Arc<dyn BlockDevice> = Arc::new(SlowWrites { inner: file.clone(), ms: ms.clone() });
        let mut vm = VolumeManager::new(MIB);
        vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
        vm.registry().write().await.set_erase_default(EraseLevel::Once);
        let id = vm.create_volume_any("v", BATCH as u64 * MIB).await.unwrap();
        let h = vm.get_volume(&id).unwrap();
        h.write(0, &vec![0x5A; BATCH * MIB as usize]).await.unwrap();
        h.flush().await.unwrap();
        drop(h);
        vm.delete_volume(id).await.unwrap();
        let reg = vm.registry().clone();
        assert_eq!(reg.read().await.erasing_slots(), BATCH as u64);

        ms.store(WRITE_MS, Relaxed);
        let eraser = Eraser::new(reg.clone(), None);
        let done = std::sync::atomic::AtomicBool::new(false);
        let reader = async {
            let mut worst = Duration::ZERO;
            while !done.load(Relaxed) {
                let asked = Instant::now();
                drop(reg.read().await);
                worst = worst.max(asked.elapsed());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            worst
        };
        let erasing = async {
            let n = eraser.drain().await;
            done.store(true, Relaxed);
            n
        };
        let (erased, worst) = tokio::join!(erasing, reader);
        assert_eq!(erased, BATCH, "every slot erased and freed");
        // A table write a slot under the write lock: 16 × 50 ms = 0.8 s.
        assert!(worst < Duration::from_millis(300), "a reader waited {worst:?}");
        assert_eq!(reg.read().await.erasing_slots(), 0);

        // On the device: a slab opened again finds every slot free.
        ms.store(0, Relaxed);
        let slab_id = reg.read().await.iter().map(|(id, _)| *id).next().unwrap();
        crate::drive::slab::sync_registered(&reg, slab_id).await.unwrap();
        let again = crate::drive::slab::Slab::open(file.clone()).await.unwrap();
        assert_eq!(again.erasing_slots(), 0, "an erased slot still says erasing on the device");
        assert_eq!(again.allocated_slots(), 0);
        let mut all = vec![0u8; file.capacity_bytes() as usize];
        file.read(0, &mut all).await.unwrap();
        assert!(!holds(&all, &[0x5A; 4096]), "the deleted volume's data is still on the device");
    }
}
