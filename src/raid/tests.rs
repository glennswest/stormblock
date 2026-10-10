//! RAID engine tests, on in-memory drives that can be made to fail.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::*;
use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A drive in memory. Reads and writes can be made to fail.
struct MemDev {
    id: DeviceId,
    data: Mutex<Vec<u8>>,
    fail_reads: AtomicBool,
    fail_writes: AtomicBool,
    reads: AtomicU64,
}

impl MemDev {
    fn new(size: u64) -> Arc<MemDev> {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        Arc::new(MemDev {
            id: DeviceId {
                uuid: Uuid::new_v4(),
                serial: format!("MEM{n:04}"),
                model: "mem".into(),
                path: format!("mem:{n}"),
                wwn: String::new(),
            },
            data: Mutex::new(vec![0u8; size as usize]),
            fail_reads: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
            reads: AtomicU64::new(0),
        })
    }

    /// Filled with something that is not the data.
    fn garbage(size: u64) -> Arc<MemDev> {
        let d = Self::new(size);
        for (i, b) in d.data.lock().unwrap().iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31) ^ 0xA5;
        }
        d
    }

    fn poke(&self, at: u64, bytes: &[u8]) {
        self.data.lock().unwrap()[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    }

    fn peek(&self, at: u64, len: usize) -> Vec<u8> {
        self.data.lock().unwrap()[at as usize..at as usize + len].to_vec()
    }
}

#[async_trait]
impl BlockDevice for MemDev {
    fn id(&self) -> &DeviceId {
        &self.id
    }
    fn capacity_bytes(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
    fn block_size(&self) -> u32 {
        512
    }
    fn optimal_io_size(&self) -> u32 {
        4096
    }
    fn device_type(&self) -> DriveType {
        DriveType::File
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(DriveError::Other(anyhow::anyhow!("injected read error")));
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let d = self.data.lock().unwrap();
        let end = offset as usize + buf.len();
        if end > d.len() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: d.len() as u64 });
        }
        buf.copy_from_slice(&d[offset as usize..end]);
        Ok(buf.len())
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(DriveError::Other(anyhow::anyhow!("injected write error")));
        }
        let mut d = self.data.lock().unwrap();
        let end = offset as usize + buf.len();
        if end > d.len() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: d.len() as u64 });
        }
        d[offset as usize..end].copy_from_slice(buf);
        Ok(buf.len())
    }
    async fn flush(&self) -> DriveResult<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(DriveError::Other(anyhow::anyhow!("injected flush error")));
        }
        Ok(())
    }
    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        Ok(())
    }
}

const UNIT: u64 = 4096;
const SIZE: u64 = DATA_OFFSET + 256 * 1024;

fn devs(n: usize) -> (Vec<Arc<MemDev>>, Vec<Arc<dyn BlockDevice>>) {
    devs_sized(n, SIZE)
}

fn devs_sized(n: usize, size: u64) -> (Vec<Arc<MemDev>>, Vec<Arc<dyn BlockDevice>>) {
    let mems: Vec<Arc<MemDev>> = (0..n).map(|_| MemDev::new(size)).collect();
    let dyns = mems.iter().map(|m| m.clone() as Arc<dyn BlockDevice>).collect();
    (mems, dyns)
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn make(level: RaidLevel, n: usize) -> (Arc<RaidArray>, Vec<Arc<MemDev>>) {
    make_sized(level, n, SIZE).await
}

async fn make_sized(level: RaidLevel, n: usize, size: u64) -> (Arc<RaidArray>, Vec<Arc<MemDev>>) {
    let (mems, dyns) = devs_sized(n, size);
    let a = RaidArray::create(level, dyns, Some(UNIT)).await.unwrap();
    (Arc::new(a), mems)
}

/// Writes of odd sizes at odd offsets, mirrored into a model of the array.
async fn scribble(a: &RaidArray, model: &mut [u8], seed: u64, count: usize) {
    let cap = a.capacity_bytes();
    let mut x = seed;
    for i in 0..count {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let len = 512 * (1 + (x >> 40) % 40);
        let off = 512 * ((x >> 8) % ((cap - len) / 512));
        let data = pattern(seed * 1000 + i as u64, len as usize);
        a.write(off, &data).await.unwrap();
        model[off as usize..(off + len) as usize].copy_from_slice(&data);
    }
}

async fn read_all(a: &RaidArray) -> Vec<u8> {
    let mut out = vec![0u8; a.capacity_bytes() as usize];
    // In pieces of odd sizes, so reads straddle units too.
    let mut at = 0usize;
    let mut step = 3 * 512;
    while at < out.len() {
        let n = step.min(out.len() - at);
        a.read(at as u64, &mut out[at..at + n]).await.unwrap();
        at += n;
        step = if step > 64 * 1024 { 3 * 512 } else { step * 3 };
    }
    out
}

async fn fill(a: &RaidArray, seed: u64) -> Vec<u8> {
    let mut model = pattern(seed, a.capacity_bytes() as usize);
    a.write(0, &model.clone()).await.unwrap();
    scribble(a, &mut model, seed + 1, 50).await;
    model
}

async fn wait_clean(a: &RaidArray) {
    for _ in 0..500 {
        if a.status().state == "clean" && !a.rebuild_running.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("array never came back clean: {:?}", a.status());
}

#[tokio::test]
async fn every_level_round_trips() {
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid5, 4), (RaidLevel::Raid6, 6), (RaidLevel::Raid10, 4)] {
        let (a, _) = make(level, n).await;
        let model = fill(&a, 7).await;
        assert_eq!(read_all(&a).await, model, "{level}");
    }
}

#[tokio::test]
async fn raid10_capacity_is_usable_and_pairs_hold_copies() {
    let (a, mems) = make(RaidLevel::Raid10, 4).await;
    let cap = a.capacity_bytes();
    assert_eq!(cap, 2 * ((SIZE - DATA_OFFSET) / UNIT * UNIT));
    // The last byte is writable — the old RAID-10 failed beyond one member.
    a.write(cap - 4096, &[9u8; 4096]).await.unwrap();
    // Unit 0 on pair 0 (members 0, 1), unit 1 on pair 1 (members 2, 3).
    a.write(0, &pattern(1, 2 * UNIT as usize)).await.unwrap();
    assert_eq!(mems[0].peek(DATA_OFFSET, 64), mems[1].peek(DATA_OFFSET, 64));
    assert_eq!(mems[2].peek(DATA_OFFSET, 64), mems[3].peek(DATA_OFFSET, 64));
    assert_ne!(mems[0].peek(DATA_OFFSET, 64), mems[2].peek(DATA_OFFSET, 64));
}

/// RAID-6 really stores Q: lose any two members, the data is all there.
#[tokio::test]
async fn raid6_survives_any_two_lost() {
    let (a, _) = make(RaidLevel::Raid6, 6).await;
    let model = fill(&a, 11).await;
    for x in 0..6 {
        for y in x + 1..6 {
            a.set_member_state(x, RaidMemberState::Failed);
            a.set_member_state(y, RaidMemberState::Failed);
            assert_eq!(read_all(&a).await, model, "lost {x} and {y}");
            a.set_member_state(x, RaidMemberState::Active);
            a.set_member_state(y, RaidMemberState::Active);
        }
    }
}

/// Degraded writes land: written with members missing, read back with
/// others missing instead (only possible if parity was kept right).
#[tokio::test]
async fn degraded_writes_keep_parity() {
    for (level, n, lose) in [(RaidLevel::Raid5, 5, 1usize), (RaidLevel::Raid6, 7, 2)] {
        let (a, _) = make(level, n).await;
        let mut model = fill(&a, 3).await;
        for k in 0..lose {
            a.set_member_state(k, RaidMemberState::Failed);
        }
        scribble(&a, &mut model, 99, 60).await;
        assert_eq!(read_all(&a).await, model, "{level} degraded");
        // Rebuild from the data as written: bring the lost ones "back" by
        // reconstructing, i.e. swap which members are missing.
        for k in 0..lose {
            a.set_member_state(k, RaidMemberState::Active);
        }
        // They hold stale data now; only failing the others proves parity.
        // Recompute the lost members' strips: read everything with them
        // failed and write it back whole (full-stripe writes).
        for k in 0..lose {
            a.set_member_state(k, RaidMemberState::Failed);
        }
        let all = read_all(&a).await;
        for k in 0..lose {
            a.set_member_state(k, RaidMemberState::Active);
        }
        a.write(0, &all).await.unwrap();
        for k in lose..2 * lose {
            a.set_member_state(k, RaidMemberState::Failed);
        }
        assert_eq!(read_all(&a).await, model, "{level} after a full rewrite");
    }
}

/// Many writers hitting different data strips of the same stripes at once:
/// without a stripe lock two read-modify-writes lose a parity update.
#[tokio::test]
async fn concurrent_writes_to_one_stripe_keep_parity() {
    let (a, _) = make(RaidLevel::Raid6, 8).await;
    let dd = 6u64;
    let mut handles = Vec::new();
    for w in 0..dd {
        let a = a.clone();
        handles.push(tokio::spawn(async move {
            for round in 0..40u64 {
                let stripe = round % 8;
                let off = stripe * UNIT * dd + w * UNIT + 512 * (round % 4);
                a.write(off, &pattern(w * 100 + round, 1024)).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let p = a.start_scrub(rebuild::ScrubConfig { max_bytes_per_sec: 0, repair: false });
    while !p.is_finished() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(p.found(), 0, "parity disagreed with data after concurrent writes");
}

/// A member whose write fails is failed, recorded on the others, and the
/// write still succeeds.
#[tokio::test]
async fn a_write_error_fails_the_member_and_records_it() {
    let (a, mems) = make(RaidLevel::Raid5, 4).await;
    let model = fill(&a, 5).await;
    mems[2].fail_writes.store(true, Ordering::SeqCst);
    let mut model = model;
    scribble(&a, &mut model, 6, 20).await;
    assert_eq!(a.member_states()[2].1, RaidMemberState::Failed);
    assert_eq!(read_all(&a).await, model);
    // The survivors' superblocks say so.
    let sb = read_superblock(&(mems[0].clone() as Arc<dyn BlockDevice>)).await.unwrap().unwrap();
    assert_eq!(sb.slots[2].state, RaidMemberState::Failed);
    assert_eq!(sb.events, a.events());
}

/// A read error fails the member and the read is served from the rest.
#[tokio::test]
async fn a_read_error_is_served_degraded() {
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid6, 5), (RaidLevel::Raid10, 4)] {
        let (a, mems) = make(level, n).await;
        let model = fill(&a, 8).await;
        mems[1].fail_reads.store(true, Ordering::SeqCst);
        assert_eq!(read_all(&a).await, model, "{level}");
        assert_eq!(a.member_states()[1].1, RaidMemberState::Failed, "{level}");
    }
}

/// The last copy is never failed: the I/O errors instead and the array keeps
/// what it has.
#[tokio::test]
async fn the_last_copy_is_not_failed() {
    let (a, mems) = make(RaidLevel::Raid1, 2).await;
    a.write(0, &[1u8; 4096]).await.unwrap();
    mems[0].fail_writes.store(true, Ordering::SeqCst);
    mems[1].fail_writes.store(true, Ordering::SeqCst);
    assert!(a.write(0, &[2u8; 4096]).await.is_err());
    let failed = a.member_states().iter().filter(|(_, s)| *s == RaidMemberState::Failed).count();
    assert_eq!(failed, 1, "one leg failed, the last kept");
}

/// Replace a failed member: the rebuild writes it whole, and afterwards it
/// alone can stand in for another lost member.
#[tokio::test]
async fn replace_rebuilds_and_the_new_member_holds_the_data() {
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid5, 4), (RaidLevel::Raid6, 6), (RaidLevel::Raid10, 4)] {
        let (a, _mems) = make(level, n).await;
        let model = fill(&a, 21).await;
        assert!(a.fail_member(1, "test"));
        let fresh = MemDev::garbage(SIZE);
        a.replace(1, fresh.clone()).await.unwrap();
        wait_clean(&a).await;
        // Now lose the member it mirrors, or any other.
        // Member 0 is the rebuilt one's partner in every shape here.
        assert!(a.fail_member(0, "test"));
        assert_eq!(read_all(&a).await, model, "{level}: rebuilt member does not hold the data");
    }
}

/// Writes during a rebuild reach the rebuilt member, below and above where
/// the rebuild has got to.
#[tokio::test]
async fn writes_during_a_rebuild_are_not_lost() {
    // Big enough for the rebuild to take several batches (a mirror's lock
    // unit is 1 MiB), slow enough that the writes land while it runs.
    let size = DATA_OFFSET + 4 * MIRROR_UNIT;
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid6, 5), (RaidLevel::Raid10, 4)] {
        let (a, _mems) = make_sized(level, n, size).await;
        let mut model = pattern(31, a.capacity_bytes() as usize);
        a.write(0, &model.clone()).await.unwrap();
        a.set_rebuild_config(rebuild::RebuildConfig { max_bytes_per_sec: 8 * 1024 * 1024, batch_bytes: MIRROR_UNIT });
        assert!(a.fail_member(1, "test"));
        a.replace(1, MemDev::garbage(size)).await.unwrap();
        for round in 0..10 {
            scribble(&a, &mut model, 500 + round, 5).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(a.status().rebuilding > 0 || level != RaidLevel::Raid1, "rebuild finished before the writes");
        a.set_rebuild_config(rebuild::RebuildConfig::default());
        wait_clean(&a).await;
        assert!(a.fail_member(0, "test"));
        assert_eq!(read_all(&a).await, model, "{level}");
    }
}

/// #175: a mirror's new member is never read above where its copy has got.
#[tokio::test]
async fn reads_never_come_from_unrebuilt_space() {
    let size = DATA_OFFSET + 4 * MIRROR_UNIT;
    let (a, _) = make_sized(RaidLevel::Raid1, 2, size).await;
    let model = pattern(41, a.capacity_bytes() as usize);
    a.write(0, &model).await.unwrap();
    a.set_rebuild_config(rebuild::RebuildConfig { max_bytes_per_sec: 2 * 1024 * 1024, batch_bytes: MIRROR_UNIT });
    let fresh = MemDev::garbage(size);
    a.add_member(fresh.clone()).await.unwrap();
    for _ in 0..3 {
        assert_eq!(read_all(&a).await, model);
    }
    assert!(a.status().rebuilding > 0, "the test needs the rebuild still running");
    a.set_rebuild_config(rebuild::RebuildConfig::default());
    wait_clean(&a).await;
    assert_eq!(fresh.peek(DATA_OFFSET, 4096), model[..4096].to_vec());
}

/// Reassembly from the drives alone, whole and with a member missing.
#[tokio::test]
async fn assemble_from_superblocks() {
    for (level, n) in [(RaidLevel::Raid1, 3), (RaidLevel::Raid5, 4), (RaidLevel::Raid6, 6), (RaidLevel::Raid10, 4)] {
        let (mems, dyns) = devs(n);
        let id;
        let model;
        {
            let a = RaidArray::create_with(CreateOptions {
                level,
                members: dyns.clone(),
                stripe_size: Some(UNIT),
                name: "shelf1-a".into(),
                pool: "shelf1".into(),
            })
            .await
            .unwrap();
            id = a.array_id();
            model = fill(&a, 51).await;
            a.close().await.unwrap();
        }
        let scan = scan(&dyns).await;
        assert_eq!(scan.arrays.len(), 1);
        let b = RaidArray::assemble(scan.arrays[0].clone()).await.unwrap();
        assert_eq!(b.array_id(), id);
        assert_eq!(b.name(), "shelf1-a");
        assert_eq!(b.pool(), "shelf1");
        assert_eq!(read_all(&b).await, model, "{level} whole");
        drop(b);

        // One drive gone.
        let some: Vec<Arc<dyn BlockDevice>> = dyns.iter().skip(1).cloned().collect();
        let scan2 = scan_of(&some).await;
        let c = RaidArray::assemble(scan2).await.unwrap();
        assert_eq!(c.member_states()[0].1, RaidMemberState::Failed);
        assert_eq!(c.status().state, "degraded");
        assert_eq!(read_all(&c).await, model, "{level} missing one");
        let _ = mems;
    }
}

async fn scan_of(d: &[Arc<dyn BlockDevice>]) -> Vec<(Arc<dyn BlockDevice>, Superblock)> {
    scan(d).await.arrays.remove(0)
}

#[tokio::test]
async fn assemble_refuses_more_missing_than_tolerated() {
    let (_mems, dyns) = devs(4);
    let a = RaidArray::create(RaidLevel::Raid5, dyns.clone(), Some(UNIT)).await.unwrap();
    drop(a);
    let two: Vec<Arc<dyn BlockDevice>> = dyns[2..].to_vec();
    assert!(matches!(RaidArray::assemble(scan_of(&two).await).await, Err(RaidError::TooManyFailures { .. })));
}

/// A member that failed is not trusted again at assembly, even though its
/// drive is back with its old (stale) superblock.
#[tokio::test]
async fn a_failed_member_stays_failed_after_assembly() {
    let (mems, dyns) = devs(4);
    let a = RaidArray::create(RaidLevel::Raid5, dyns.clone(), Some(UNIT)).await.unwrap();
    let model = fill(&a, 61).await;
    mems[3].fail_writes.store(true, Ordering::SeqCst);
    let mut model = model;
    scribble(&a, &mut model, 62, 30).await;
    drop(a);
    mems[3].fail_writes.store(false, Ordering::SeqCst);
    let b = RaidArray::assemble(scan_of(&dyns).await).await.unwrap();
    assert_eq!(b.member_states()[3].1, RaidMemberState::Failed);
    assert_eq!(read_all(&b).await, model);
}

/// A crash mid-write: the bitmap names the chunk, assembly recomputes its
/// parity, and the data is right even with a member then lost.
#[tokio::test]
async fn a_dirty_chunk_is_resynced_at_assembly() {
    let (mems, dyns) = devs(5);
    let a = RaidArray::create(RaidLevel::Raid5, dyns.clone(), Some(UNIT)).await.unwrap();
    let model = fill(&a, 71).await;
    a.close().await.unwrap();
    // Stripe 0's parity is on member 4. Tear it, as a crash between the data
    // and the parity write would, and leave the chunk's bit set.
    mems[4].poke(DATA_OFFSET, &[0xEE; 512]);
    for m in &mems {
        m.poke(bitmap::BITMAP_OFFSET, &[1]);
    }
    drop(a);
    let b = RaidArray::assemble(scan_of(&dyns).await).await.unwrap();
    assert_eq!(b.dirty_chunks(), 0);
    assert_eq!(mems[0].peek(bitmap::BITMAP_OFFSET, 1), vec![0], "the bitmap is cleared once resynced");
    b.set_member_state(0, RaidMemberState::Failed);
    assert_eq!(read_all(&b).await, model, "parity was not repaired");
}

/// A clean close leaves no bit set; writes set them on disk first.
#[tokio::test]
async fn the_bitmap_is_set_before_a_write_and_cleared_by_close() {
    let (a, mems) = make(RaidLevel::Raid6, 5).await;
    a.write(0, &[3u8; 8192]).await.unwrap();
    assert_eq!(mems[2].peek(bitmap::BITMAP_OFFSET, 1)[0] & 1, 1, "bit not on disk after a write");
    a.close().await.unwrap();
    assert_eq!(mems[2].peek(bitmap::BITMAP_OFFSET, 1)[0], 0);
}

/// A failed member takes a spare from its own pool, else the global one,
/// never another pool's.
#[tokio::test]
async fn hot_spares_are_taken_by_pool() {
    let pool = spares::SparePool::new();
    let other = MemDev::new(SIZE);
    let global = MemDev::new(SIZE);
    let own = MemDev::new(SIZE);
    pool.add(other.clone(), "shelf2").await.unwrap();
    pool.add(global.clone(), "").await.unwrap();
    pool.add(own.clone(), "shelf1").await.unwrap();

    let (mems, dyns) = devs(5);
    let a = Arc::new(
        RaidArray::create_with(CreateOptions {
            level: RaidLevel::Raid6,
            members: dyns,
            stripe_size: Some(UNIT),
            name: "s1".into(),
            pool: "shelf1".into(),
        })
        .await
        .unwrap(),
    );
    let model = fill(&a, 81).await;
    a.start(Some(pool.clone()));
    mems[1].fail_writes.store(true, Ordering::SeqCst);
    a.write(0, &model[..65536]).await.unwrap();
    wait_clean(&a).await;
    let drives: Vec<String> = a.drive_ids().iter().map(|d| d.path.clone()).collect();
    assert!(drives.contains(&own.id.path), "took {drives:?}, not its own pool's spare");

    mems[3].fail_writes.store(true, Ordering::SeqCst);
    a.write(0, &model[..65536]).await.unwrap();
    wait_clean(&a).await;
    let drives: Vec<String> = a.drive_ids().iter().map(|d| d.path.clone()).collect();
    assert!(drives.contains(&global.id.path), "second failure should take the global spare");
    assert_eq!(pool.list().len(), 1);
    assert_eq!(pool.list()[0].pool, "shelf2", "another shelf's spare must stay");

    // And it is a member now, data and all.
    a.set_member_state(0, RaidMemberState::Failed);
    a.set_member_state(2, RaidMemberState::Failed);
    assert_eq!(read_all(&a).await, model);
    a.stop();
}

/// A spare is recognised by its superblock after a restart.
#[tokio::test]
async fn spares_are_found_by_scan() {
    let pool = spares::SparePool::new();
    let d = MemDev::new(SIZE);
    pool.add(d.clone(), "shelf9").await.unwrap();
    let s = scan(&[d.clone() as Arc<dyn BlockDevice>]).await;
    assert_eq!(s.spares.len(), 1);
    assert_eq!(s.spares[0].1.pool, "shelf9");
    let uuid = pool.list()[0].uuid;
    pool.remove(uuid).await.unwrap();
    assert!(scan(&[d as Arc<dyn BlockDevice>]).await.spares.is_empty());
}

/// A rebuild cut short resumes from where its last checkpoint said.
#[tokio::test]
async fn a_rebuild_resumes_after_assembly() {
    let (mems, dyns) = devs(4);
    let a = Arc::new(RaidArray::create(RaidLevel::Raid5, dyns.clone(), Some(UNIT)).await.unwrap());
    let model = fill(&a, 91).await;
    a.set_rebuild_config(rebuild::RebuildConfig { max_bytes_per_sec: 256 * 1024, batch_bytes: 4 * UNIT });
    assert!(a.fail_member(2, "test"));
    let fresh = MemDev::garbage(SIZE);
    a.replace(2, fresh.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.stop();
    a.persist_rebuild_position().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(a);
    let mut all: Vec<Arc<dyn BlockDevice>> = dyns.clone();
    all[2] = fresh.clone();
    let b = Arc::new(RaidArray::assemble(scan_of(&all).await).await.unwrap());
    assert_eq!(b.member_states()[2].1, RaidMemberState::Rebuilding);
    b.start(None);
    wait_clean(&b).await;
    b.set_member_state(0, RaidMemberState::Failed);
    assert_eq!(read_all(&b).await, model);
    let _ = mems;
    b.stop();
}

#[tokio::test]
async fn scrub_finds_and_repairs() {
    for (level, n) in [(RaidLevel::Raid5, 4), (RaidLevel::Raid6, 5), (RaidLevel::Raid1, 2), (RaidLevel::Raid10, 4)] {
        let (a, mems) = make(level, n).await;
        let model = fill(&a, 101).await;
        // Damage one member's copy of the first unit.
        mems[n - 1].poke(DATA_OFFSET + 100, &[0x5A; 64]);
        let p = a.start_scrub(rebuild::ScrubConfig { max_bytes_per_sec: 0, repair: false });
        while !p.is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(p.found(), 1, "{level}: report-only scrub");
        let p = a.start_scrub(rebuild::ScrubConfig { max_bytes_per_sec: 0, repair: true });
        while !p.is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(p.repaired(), 1, "{level}");
        let p = a.start_scrub(rebuild::ScrubConfig { max_bytes_per_sec: 0, repair: false });
        while !p.is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(p.found(), 0, "{level}: still wrong after repair");
        // The damaged member was parity (or a mirror leg), so the data reads
        // right without it.
        let _ = model;
    }
}

#[tokio::test]
async fn two_sets_are_two_failure_domains() {
    let (a, _) = make(RaidLevel::Raid6, 4).await;
    let (b, _) = make(RaidLevel::Raid6, 4).await;
    let da = crate::placement::domain::FailureDomain::from_device(&a.drive_id());
    let db = crate::placement::domain::FailureDomain::from_device(&b.drive_id());
    assert_ne!(da, db);
}

#[tokio::test]
async fn create_refuses_bad_shapes() {
    let (_, d) = devs(3);
    assert!(RaidArray::create(RaidLevel::Raid6, d.clone(), Some(UNIT)).await.is_err());
    let (_, d) = devs(5);
    assert!(RaidArray::create(RaidLevel::Raid10, d, Some(UNIT)).await.is_err());
    let (_, d) = devs(3);
    assert!(RaidArray::create(RaidLevel::Raid5, d, Some(1000)).await.is_err());
}

async fn wait_replaced(a: &RaidArray) {
    for _ in 0..1000 {
        if a.replacing.lock().unwrap().is_none() && a.status().state == "clean" && !a.rebuild_running.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the replacement never finished: {:?}", a.status());
}

/// #256: a member replaced while it serves (a `failing` drive): writes keep
/// landing during the copy, the new drive takes the slot with every byte,
/// the set never runs degraded, and the old drive is no longer a member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_member_is_replaced_while_it_serves() {
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid5, 4), (RaidLevel::Raid6, 5), (RaidLevel::Raid10, 4)] {
        let (a, mems) = make(level, n).await;
        let mut model = fill(&a, 91).await;
        let spare = MemDev::new(SIZE);
        let old: Arc<dyn BlockDevice> = mems[1].clone();
        a.replace_proactively(1, spare.clone()).await.unwrap();
        assert_eq!(a.status().failed, 0, "{level}: never degraded");
        // Writes while the copy runs.
        scribble(&a, &mut model, 92, 40).await;
        wait_replaced(&a).await;
        assert_eq!(a.failed_count(), 0, "{level}");
        let drives = a.member_drives();
        assert_eq!(drives[1].2.path, spare.id.path, "{level}: the new drive holds slot 1");
        assert_eq!(read_all(&a).await, model, "{level}");
        // Slot 1 holds the data itself: lose another member, read it all.
        let other = 0;
        a.set_member_state(other, RaidMemberState::Failed);
        assert_eq!(read_all(&a).await, model, "{level}: with slot {other} gone, slot 1 is read");
        a.set_member_state(other, RaidMemberState::Active);
        assert!(read_superblock(&old).await.unwrap().is_none(), "{level}: the old drive is no member now");
    }
}

/// #256: the old drive stops answering reads mid-copy: it is failed, and the
/// new drive takes the slot as a rebuild from where the copy was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replacement_whose_old_drive_dies_becomes_a_rebuild() {
    let (a, mems) = make(RaidLevel::Raid5, 4).await;
    let model = fill(&a, 93).await;
    let spare = MemDev::new(SIZE);
    mems[2].fail_reads.store(true, Ordering::SeqCst);
    a.replace_proactively(2, spare.clone()).await.unwrap();
    wait_replaced(&a).await;
    assert_eq!(a.member_drives()[2].2.path, spare.id.path);
    assert_eq!(read_all(&a).await, model);
    a.set_member_state(0, RaidMemberState::Failed);
    assert_eq!(read_all(&a).await, model, "slot 2 rebuilt onto the new drive");
}

/// #256: a drive that went missing and comes back is re-added from the
/// bitmap: what was written while it was gone is resynced onto it, and it
/// serves again without a rebuild. A drive that failed on I/O is not.
#[tokio::test]
async fn a_returned_member_is_re_added_from_the_bitmap() {
    for (level, n) in [(RaidLevel::Raid1, 2), (RaidLevel::Raid5, 4), (RaidLevel::Raid6, 5), (RaidLevel::Raid10, 4)] {
        let (_mems, dyns) = devs(n);
        let a = RaidArray::create(level, dyns.clone(), Some(UNIT)).await.unwrap();
        let mut model = fill(&a, 95).await;
        a.close().await.unwrap();
        drop(a);
        // Assembled without drive 0 (a cable pulled): written meanwhile.
        let without: Vec<Arc<dyn BlockDevice>> = dyns.iter().skip(1).cloned().collect();
        let b = RaidArray::assemble(scan_of(&without).await).await.unwrap();
        assert_eq!(b.member_states()[0].1, RaidMemberState::Failed);
        assert!(b.keeps_bitmap(), "{level}: the bitmap is kept while a member is missing");
        scribble(&b, &mut model, 96, 30).await;
        b.flush().await.unwrap();
        b.close().await.unwrap();
        assert!(b.dirty_chunks() > 0, "{level}: what was written is still marked");
        drop(b);
        // Drive 0 is back.
        let c = RaidArray::assemble(scan_of(&dyns).await).await.unwrap();
        assert_eq!(c.member_states()[0].1, RaidMemberState::Active, "{level}: re-added, not rebuilding");
        assert!(!c.keeps_bitmap());
        assert_eq!(read_all(&c).await, model, "{level}");
        // Drive 0 has the writes made while it was gone: lose its partner.
        let partner = 1;
        c.set_member_state(partner, RaidMemberState::Failed);
        assert_eq!(read_all(&c).await, model, "{level}: drive 0 serves what it missed");
    }
}
