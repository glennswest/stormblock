//! The blob store against a thin volume in memory (#732): slots mapped on
//! first write, given back by discard, a budget of slots for "the pool is
//! full", and what a flush made durable kept apart from what was only
//! written, so a crash can be had at any point.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use super::format::Digest;
use super::*;
use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

struct Thin {
    id: DeviceId,
    slot: u64,
    cap: AtomicU64,
    /// Slots the pool has for this volume.
    budget: AtomicU64,
    now: Mutex<HashMap<u64, Vec<u8>>>,
    durable: Mutex<HashMap<u64, Vec<u8>>>,
    writes: AtomicU64,
    /// Flushes that succeed before every later one fails.
    flushes_left: AtomicU64,
}

impl Thin {
    fn new(slot: u64, budget: u64) -> Arc<Thin> {
        Arc::new(Thin {
            id: DeviceId { uuid: Uuid::new_v4(), serial: "THIN".into(), model: "thin".into(), path: "mem:thin".into(), wwn: String::new() },
            slot,
            cap: AtomicU64::new(2 * slot),
            budget: AtomicU64::new(budget),
            now: Mutex::new(HashMap::new()),
            durable: Mutex::new(HashMap::new()),
            writes: AtomicU64::new(0),
            flushes_left: AtomicU64::new(u64::MAX),
        })
    }

    /// Power cut: what was not flushed is gone.
    fn crash(&self) {
        *self.now.lock().unwrap() = self.durable.lock().unwrap().clone();
        self.flushes_left.store(u64::MAX, Ordering::SeqCst);
    }

    fn mapped(&self) -> BTreeSet<u64> {
        self.now.lock().unwrap().keys().copied().collect()
    }

    fn poke(&self, at: u64, b: &[u8]) {
        let mut m = self.now.lock().unwrap();
        let s = m.get_mut(&(at / self.slot)).expect("mapped");
        let o = (at % self.slot) as usize;
        s[o..o + b.len()].copy_from_slice(b);
    }
}

#[async_trait]
impl BlockDevice for Thin {
    fn id(&self) -> &DeviceId {
        &self.id
    }
    fn capacity_bytes(&self) -> u64 {
        self.cap.load(Ordering::SeqCst)
    }
    fn block_size(&self) -> u32 {
        4096
    }
    fn optimal_io_size(&self) -> u32 {
        4096
    }
    fn device_type(&self) -> DriveType {
        DriveType::File
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        assert_eq!(offset % 4096, 0, "an unaligned read");
        assert_eq!(buf.len() % 4096, 0, "a read of part of a block");
        if offset + buf.len() as u64 > self.capacity_bytes() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: self.capacity_bytes() });
        }
        let m = self.now.lock().unwrap();
        let mut pos = offset;
        let mut done = 0;
        while done < buf.len() {
            let (s, o) = (pos / self.slot, (pos % self.slot) as usize);
            let n = (self.slot as usize - o).min(buf.len() - done);
            match m.get(&s) {
                Some(d) => buf[done..done + n].copy_from_slice(&d[o..o + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
            pos += n as u64;
        }
        Ok(buf.len())
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        assert_eq!(offset % 4096, 0, "an unaligned write");
        assert_eq!(buf.len() % 4096, 0, "a write of part of a block");
        if offset + buf.len() as u64 > self.capacity_bytes() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: self.capacity_bytes() });
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mut m = self.now.lock().unwrap();
        let mut pos = offset;
        let mut done = 0;
        while done < buf.len() {
            let (s, o) = (pos / self.slot, (pos % self.slot) as usize);
            let n = (self.slot as usize - o).min(buf.len() - done);
            if !m.contains_key(&s) {
                if m.len() as u64 >= self.budget.load(Ordering::SeqCst) {
                    return Err(DriveError::NoSpace("the pool is full".into()));
                }
                m.insert(s, vec![0u8; self.slot as usize]);
            }
            m.get_mut(&s).unwrap()[o..o + n].copy_from_slice(&buf[done..done + n]);
            done += n;
            pos += n as u64;
        }
        Ok(buf.len())
    }
    async fn flush(&self) -> DriveResult<()> {
        let left = self.flushes_left.load(Ordering::SeqCst);
        if left == 0 {
            return Err(DriveError::Other(anyhow::anyhow!("injected flush error")));
        }
        if left != u64::MAX {
            self.flushes_left.store(left - 1, Ordering::SeqCst);
        }
        *self.durable.lock().unwrap() = self.now.lock().unwrap().clone();
        Ok(())
    }
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let mut m = self.now.lock().unwrap();
        for s in offset.div_ceil(self.slot)..(offset + len) / self.slot {
            m.remove(&s);
        }
        Ok(())
    }
}

struct Mem(Arc<Thin>);

#[async_trait]
impl Backing for Mem {
    fn device(&self) -> Arc<dyn BlockDevice> {
        self.0.clone()
    }
    fn slot_size(&self) -> u64 {
        self.0.slot
    }
    async fn mapped_slots(&self) -> Result<Vec<u64>, String> {
        Ok(self.0.mapped().into_iter().collect())
    }
    async fn allocated_bytes(&self) -> u64 {
        self.0.mapped().len() as u64 * self.0.slot
    }
    async fn pool_free_bytes(&self) -> u64 {
        self.0.budget.load(Ordering::SeqCst).saturating_sub(self.0.mapped().len() as u64) * self.0.slot
    }
    async fn grow(&self, new_size: u64) -> Result<(), String> {
        self.0.cap.fetch_max(new_size, Ordering::SeqCst);
        Ok(())
    }
}

fn cfg() -> StoreConfig {
    StoreConfig { background: false, ..Default::default() }
}

async fn open(t: &Arc<Thin>) -> Arc<BlobStore> {
    open_with(t, cfg()).await
}

async fn open_with(t: &Arc<Thin>, c: StoreConfig) -> Arc<BlobStore> {
    BlobStore::open(Arc::new(Mem(t.clone())), c).await.expect("open")
}

fn sha(b: &[u8]) -> Digest {
    Sha256::digest(b).into()
}

/// Bytes that differ everywhere, so a misplaced block shows.
fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64).wrapping_mul(2654435761) >> 7) as u8 ^ seed).collect()
}

/// A body in pieces of an awkward size.
fn body(b: &[u8], piece: usize) -> futures_util::stream::Iter<std::vec::IntoIter<Result<Bytes, std::io::Error>>> {
    let v: Vec<Result<Bytes, std::io::Error>> = b.chunks(piece.max(1)).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
    futures_util::stream::iter(v)
}

async fn put(s: &BlobStore, b: &[u8]) -> Digest {
    let d = sha(b);
    let out = s.put(d, Some(b.len() as u64), body(b, 7777)).await.expect("put");
    assert_eq!(out, PutOutcome::Created);
    d
}

async fn read_all(s: &BlobStore, d: &Digest) -> Vec<u8> {
    let p = s.get(d).await.expect("held");
    let mut buf = vec![0u8; p.size() as usize];
    s.read(&p, 0, &mut buf).await.expect("read");
    buf
}

/// Every mapped slot is one the store names, and none it names is free.
async fn consistent(s: &BlobStore, t: &Thin) {
    s.settle().await;
    let st = s.state.lock().await;
    let used = st.used_slots();
    for m in t.mapped() {
        assert!(used.contains(&m), "slot {m} is mapped and nothing names it");
    }
    for f in &st.free {
        assert!(!used.contains(f), "slot {f} is free and named");
    }
}

const SLOT: u64 = 64 * 1024;

#[tokio::test]
async fn blobs_of_every_size_read_back_whole_and_by_range() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let sizes = [0usize, 1, 4095, 4096, 4097, 65535, 65536, 65537, 200_000, 3 * 65536];
    let mut held = Vec::new();
    for (i, n) in sizes.iter().enumerate() {
        let b = data(*n, i as u8);
        let d = put(&s, &b).await;
        held.push((d, b));
    }
    for (d, b) in &held {
        assert_eq!(&read_all(&s, d).await, b);
        let p = s.get(d).await.unwrap();
        // Ranges that start and end anywhere, across the slots and the tail.
        for (a, z) in [(0usize, 1usize), (1, 5000), (4095, 4097), (65530, 65600), (100, 199_999)] {
            if z <= b.len() && a < z {
                let mut buf = vec![0u8; z - a];
                s.read(&p, a as u64, &mut buf).await.unwrap();
                assert_eq!(&buf[..], &b[a..z], "{a}..{z} of {}", b.len());
            }
        }
        // A blob's tail is a pack's, never a slot of its own.
        assert_eq!(p.layout.slots.len() as u64, p.size() / SLOT);
        assert_eq!(p.layout.tail.map(|t| t.1 as u64).unwrap_or(0), p.size() % SLOT);
    }
    let (page, next) = s.list(None, 4).await;
    assert_eq!(page.len(), 4);
    let (rest, end) = s.list(next, 100).await;
    assert_eq!(page.len() + rest.len(), sizes.len());
    assert!(end.is_none());
    assert!(page.windows(2).all(|w| w[0].0 < w[1].0));
    let st = s.stats().await;
    assert_eq!(st.blobs, sizes.len() as u64);
    assert_eq!(st.bytes, sizes.iter().sum::<usize>() as u64);
    consistent(&s, &t).await;

    // A restart reads the same store.
    drop(s);
    let s = open(&t).await;
    for (d, b) in &held {
        assert_eq!(&read_all(&s, d).await, b);
    }
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_mismatch_keeps_nothing_and_a_held_digest_writes_nothing() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    s.settle().await;
    let before = t.mapped();
    let b = data(300_000, 1);
    let wrong = sha(b"something else");
    match s.put(wrong, None, body(&b, 4000)).await {
        Err(BlobError::Mismatch { bytes, .. }) => assert_eq!(bytes, 300_000),
        other => panic!("a mismatch was {other:?}"),
    }
    assert!(s.get(&wrong).await.is_none());
    s.settle().await;
    // The open pack the tail went to may stay; the blob's slots do not.
    assert!(t.mapped().len() <= before.len() + 1, "{:?} after, {:?} before", t.mapped(), before);
    consistent(&s, &t).await;

    let d = put(&s, &b).await;
    let writes = t.writes.load(Ordering::SeqCst);
    assert_eq!(s.put(d, None, body(&b, 9999)).await.unwrap(), PutOutcome::Existed);
    assert_eq!(t.writes.load(Ordering::SeqCst), writes, "a held digest wrote");
}

#[tokio::test]
async fn small_blobs_share_packs() {
    let slot = 1 << 20;
    let t = Thin::new(slot, 1 << 16);
    let s = open(&t).await;
    s.settle().await;
    let before = t.mapped().len();
    let mut held = Vec::new();
    for i in 0..500u32 {
        let b = format!("{{\"config\": {i}, \"pad\": \"{}\"}}", "x".repeat(900)).into_bytes();
        held.push((put(&s, &b).await, b));
    }
    // 500 configs of ~1 KB: a block each, 256 to a 1 MiB pack; the rest of
    // what grew is the journal (a block a put).
    let st = s.stats().await;
    assert_eq!(st.packs, 2, "{st:?}");
    let journal_slots = (st.journal_blocks * 4096).div_ceil(slot);
    let grew = t.mapped().len() - before;
    assert!(grew as u64 <= st.packs + journal_slots, "500 small blobs took {grew} slots: {st:?}");
    for (d, b) in &held {
        assert_eq!(&read_all(&s, d).await, b);
    }
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_delete_waits_for_the_read_in_flight() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let b = data(5 * SLOT as usize + 100, 3);
    let d = put(&s, &b).await;
    let reader = s.get(&d).await.unwrap();
    let slots = reader.layout.slots.clone();
    assert!(s.delete(&d).await.unwrap());
    assert!(!s.delete(&d).await.unwrap(), "a second delete is still a success");
    assert!(s.get(&d).await.is_none());
    s.settle().await;
    for sl in &slots {
        assert!(t.mapped().contains(sl), "slot {sl} given back under a reader");
    }
    let mut buf = vec![0u8; b.len()];
    s.read(&reader, 0, &mut buf).await.unwrap();
    assert_eq!(buf, b);
    drop(reader);
    s.settle().await;
    for sl in &slots {
        assert!(!t.mapped().contains(sl), "slot {sl} kept after the last reader");
    }
    consistent(&s, &t).await;
    // And the delete outlives a restart.
    drop(s);
    let s = open(&t).await;
    assert!(s.get(&d).await.is_none());
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_session_appends_at_its_length_and_survives_a_restart() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let b = data(3 * SLOT as usize + 12_345, 9);
    let id = s.open_session().await.unwrap();
    let cuts = [0usize, 1000, 4096 + 7, 70_000, 140_001];
    for w in cuts.windows(2) {
        let n = s.append(&id, Some(w[0] as u64), body(&b[w[0]..w[1]], 3333)).await.unwrap();
        assert_eq!(n, w[1] as u64);
    }
    match s.append(&id, Some(5), body(b"late", 4)).await {
        Err(BlobError::BadOffset { asked: 5, length }) => assert_eq!(length, 140_001),
        other => panic!("{other:?}"),
    }
    // A restart mid-upload: the session and its bytes are there.
    drop(s);
    let s = open(&t).await;
    assert_eq!(s.session(&id).await.unwrap().length, 140_001);
    s.append(&id, Some(140_001), body(&b[140_001..], 5000)).await.unwrap();
    let d = sha(&b);
    match s.commit(&id, sha(b"no"), None::<futures_util::stream::Empty<Result<Bytes, std::io::Error>>>).await {
        Err(BlobError::Mismatch { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert!(s.session(&id).await.is_some(), "a mismatch keeps the session");
    assert_eq!(s.commit(&id, d, None::<futures_util::stream::Empty<Result<Bytes, std::io::Error>>>).await.unwrap(), PutOutcome::Created);
    assert!(s.session(&id).await.is_none());
    assert_eq!(read_all(&s, &d).await, b);
    consistent(&s, &t).await;
    drop(s);
    let s = open(&t).await;
    assert_eq!(read_all(&s, &d).await, b);
    assert!(s.sessions().await.is_empty());
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_session_committed_with_its_last_chunk_and_hashed_as_it_went() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let b = data(2 * SLOT as usize, 4);
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&b[..100_000], 4096)).await.unwrap();
    let out = s.commit(&id, sha(&b), Some(body(&b[100_000..], 10_000))).await.unwrap();
    assert_eq!(out, PutOutcome::Created);
    assert_eq!(read_all(&s, &sha(&b)).await, b);
    // An exact number of slots: no tail.
    assert!(s.get(&sha(&b)).await.unwrap().layout.tail.is_none());
    // A second upload of the same bytes ends as `Existed` and keeps nothing.
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&b, 50_000)).await.unwrap();
    assert_eq!(s.commit(&id, sha(&b), None::<futures_util::stream::Empty<Result<Bytes, std::io::Error>>>).await.unwrap(), PutOutcome::Existed);
    consistent(&s, &t).await;
    // An abort gives the space back.
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&b, 50_000)).await.unwrap();
    assert!(s.abort(&id).await.unwrap());
    assert!(!s.abort(&id).await.unwrap());
    consistent(&s, &t).await;
    assert_eq!(s.stats().await.slots_in_use, {
        let st = s.state.lock().await;
        st.used_slots().len() as u64
    });
}

#[tokio::test]
async fn a_crash_between_the_data_and_its_record_leaves_nothing_named_and_nothing_kept() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let kept = put(&s, &data(70_000, 1)).await;
    let b = data(4 * SLOT as usize + 5, 2);
    // The blob's data flush succeeds, the journal's does not.
    t.flushes_left.store(1, Ordering::SeqCst);
    assert!(s.put(sha(&b), None, body(&b, 8192)).await.is_err());
    drop(s);
    t.crash();
    let s = open(&t).await;
    assert!(s.get(&sha(&b)).await.is_none());
    assert_eq!(read_all(&s, &kept).await, data(70_000, 1));
    // The open's sweep gave back the four slots nothing names.
    consistent(&s, &t).await;

    // A session's append cut the same way: the length is what was journaled.
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&b[..1000], 1000)).await.unwrap();
    t.flushes_left.store(1, Ordering::SeqCst);
    assert!(s.append(&id, Some(1000), body(&b[1000..], 9000)).await.is_err());
    drop(s);
    t.crash();
    let s = open(&t).await;
    assert_eq!(s.session(&id).await.unwrap().length, 1000);
    consistent(&s, &t).await;
    s.append(&id, Some(1000), body(&b[1000..], 9000)).await.unwrap();
    s.commit(&id, sha(&b), None::<futures_util::stream::Empty<Result<Bytes, std::io::Error>>>).await.unwrap();
    assert_eq!(read_all(&s, &sha(&b)).await, b);
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_torn_last_batch_is_where_the_journal_ends() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let a = put(&s, &data(10_000, 1)).await;
    let pos = s.state.lock().await.journal.pos;
    let b = put(&s, &data(10_000, 2)).await;
    drop(s);
    // The second put's record, torn.
    t.poke(pos + 100, &[0xff; 16]);
    let s = open(&t).await;
    assert!(s.get(&a).await.is_some());
    assert!(s.get(&b).await.is_none());
    consistent(&s, &t).await;
    // The journal goes on from there.
    let c = put(&s, &data(10_000, 3)).await;
    drop(s);
    let s = open(&t).await;
    assert!(s.get(&a).await.is_some() && s.get(&c).await.is_some());
}

#[tokio::test]
async fn checkpoints_keep_the_store_and_let_the_old_journal_go() {
    let t = Thin::new(4096 * 4, 1 << 20);
    let c = StoreConfig { checkpoint_min_blocks: 16, ..cfg() };
    let s = open_with(&t, c.clone()).await;
    let mut held = Vec::new();
    for i in 0..300u32 {
        let b = data(5000 + i as usize * 37, i as u8);
        let d = put(&s, &b).await;
        if i % 3 == 0 {
            s.delete(&d).await.unwrap();
        } else {
            held.push((d, b));
        }
    }
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&data(50_000, 7), 7000)).await.unwrap();
    let epoch = s.state.lock().await.journal.epoch;
    assert!(epoch > 2, "no checkpoint in 400 records (epoch {epoch})");
    consistent(&s, &t).await;
    drop(s);
    let s = open_with(&t, c).await;
    assert_eq!(s.state.lock().await.journal.epoch, epoch);
    assert_eq!(s.list(None, 10_000).await.0.len(), held.len());
    for (d, b) in &held {
        assert_eq!(&read_all(&s, d).await, b);
    }
    assert_eq!(s.session(&id).await.unwrap().length, 50_000);
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_full_pool_is_no_space_and_keeps_nothing() {
    let t = Thin::new(SLOT, 12);
    let s = open(&t).await;
    s.settle().await;
    let b = data(20 * SLOT as usize, 5);
    // Said up front: refused before a byte is read.
    let writes = t.writes.load(Ordering::SeqCst);
    assert!(matches!(s.put(sha(&b), Some(b.len() as u64), body(&b, 65536)).await, Err(BlobError::NoSpace(_))));
    assert_eq!(t.writes.load(Ordering::SeqCst), writes);
    // Not said: found out on the way, and what was written goes back.
    assert!(matches!(s.put(sha(&b), None, body(&b, 65536)).await, Err(BlobError::NoSpace(_))));
    consistent(&s, &t).await;
    // And there is room again for what fits.
    put(&s, &data(3 * SLOT as usize, 6)).await;
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_mostly_dead_pack_is_repacked_and_given_back() {
    // 16 blocks a pack.
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let mut held = Vec::new();
    for i in 0..40u32 {
        let b = data(3000, i as u8);
        held.push((put(&s, &b).await, b));
    }
    let first_pack = s.get(&held[0].0).await.unwrap().layout.tail.unwrap().0 / SLOT;
    let mut in_first: Vec<usize> = Vec::new();
    for (i, (d, _)) in held.iter().enumerate() {
        if s.get(d).await.unwrap().layout.tail.unwrap().0 / SLOT == first_pack {
            in_first.push(i);
        }
    }
    assert_eq!(in_first.len(), 16);
    // All but two of the first pack's go.
    for i in &in_first[..14] {
        s.delete(&held[*i].0).await.unwrap();
    }
    s.settle().await;
    assert!(!t.mapped().contains(&first_pack), "the sparse pack was kept");
    for (i, (d, b)) in held.iter().enumerate() {
        if in_first[..14].contains(&i) {
            assert!(s.get(d).await.is_none());
        } else {
            assert_eq!(&read_all(&s, d).await, b);
        }
    }
    consistent(&s, &t).await;
    drop(s);
    let s = open(&t).await;
    for i in in_first[14..].iter() {
        assert_eq!(read_all(&s, &held[*i].0).await, held[*i].1);
    }
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_scrub_finds_a_blob_that_does_not_read_as_its_digest() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let good = put(&s, &data(100_000, 1)).await;
    let bad = put(&s, &data(100_000, 2)).await;
    let at = s.get(&bad).await.unwrap().layout.slots[0] * SLOT + 77;
    t.poke(at, b"rot");
    let report = s.clone().run_scrub().await;
    assert_eq!(report.checked, 2);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].digest, format::digest_string(&bad));
    assert!(!report.running);
    let _ = good;
}

#[tokio::test]
async fn an_untouched_session_expires() {
    let t = Thin::new(SLOT, 1 << 20);
    let s = open_with(&t, StoreConfig { session_expiry: Duration::from_secs(1), ..cfg() }).await;
    let id = s.open_session().await.unwrap();
    s.append(&id, None, body(&data(70_000, 1), 9000)).await.unwrap();
    assert_eq!(s.expire_sessions().await, 0);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(s.expire_sessions().await, 1);
    assert!(s.session(&id).await.is_none());
    consistent(&s, &t).await;
}

#[tokio::test]
async fn a_volume_that_holds_something_else_is_not_formatted() {
    let t = Thin::new(SLOT, 1 << 20);
    t.write(0, &[0x5a; 4096]).await.unwrap();
    assert!(matches!(BlobStore::open(Arc::new(Mem(t.clone())), cfg()).await, Err(BlobError::Invalid(_))));
}

/// #733: a blob as the source of a pallet member, an import or an image
/// member: read in place, short at its end, and held against a delete
/// until the source is dropped.
#[tokio::test]
async fn a_blob_source_reads_in_place_and_holds_the_blob_against_a_delete() {
    use super::source::BlobSource;
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let b = data(3 * SLOT as usize + 5_000, 11);
    let d = put(&s, &b).await;
    let name = format::digest_string(&d);

    // Not held, and spelt wrong.
    let other = format::digest_string(&sha(b"never put"));
    match BlobSource::open(&s, &other).await {
        Err(BlobError::NotFound(m)) => assert!(m.contains(&other), "{m}"),
        e => panic!("{e:?}"),
    }
    assert!(matches!(BlobSource::open(&s, "sha256:XYZ").await, Err(BlobError::Invalid(_))));

    let src = BlobSource::open(&s, &name).await.unwrap();
    assert_eq!((src.len(), src.name()), (b.len() as u64, name.clone()));
    // Exact reads at awkward offsets, across a slot and into the tail.
    let mut buf = vec![0u8; 70_001];
    src.read_exact_at(SLOT - 3, &mut buf).await.unwrap();
    assert_eq!(&buf[..], &b[SLOT as usize - 3..SLOT as usize - 3 + 70_001]);
    // Short at the end, nothing past it, an error for an exact read past it.
    let mut tail = vec![0u8; 10_000];
    assert_eq!(src.read_at(b.len() as u64 - 100, &mut tail).await.unwrap(), 100);
    assert_eq!(&tail[..100], &b[b.len() - 100..]);
    assert_eq!(src.read_at(b.len() as u64, &mut tail).await.unwrap(), 0);
    assert!(src.read_exact_at(b.len() as u64 - 100, &mut tail).await.is_err());
    // As a pallet member: past the end is OutOfRange, not a short read.
    {
        use crate::pallet::MemberContent;
        assert_eq!(src.byte_len(), b.len() as u64);
        let mut x = vec![0u8; 200];
        assert!(MemberContent::read_at(&src, b.len() as u64 - 100, &mut x).await.is_err());
    }

    // Deleted while held: gone from the index, its slots kept, its bytes
    // the same even after other blobs are put where they could have gone.
    let slots = s.get(&d).await.unwrap().layout.slots.clone();
    assert!(s.delete(&d).await.unwrap());
    assert!(s.get(&d).await.is_none());
    s.settle().await;
    for i in 0..4u8 {
        put(&s, &data(SLOT as usize + 17, 100 + i)).await;
    }
    for sl in &slots {
        assert!(t.mapped().contains(sl), "slot {sl} given back under a source");
    }
    let mut all = vec![0u8; b.len()];
    src.read_exact_at(0, &mut all).await.unwrap();
    assert_eq!(all, b, "the source reads what it began on");
    // Its clone holds it too; the last one dropped frees it.
    let second = src.clone();
    drop(src);
    s.settle().await;
    assert!(t.mapped().contains(&slots[0]));
    drop(second);
    s.settle().await;
    for sl in &slots {
        assert!(!t.mapped().contains(sl), "slot {sl} kept after the last source");
    }
    consistent(&s, &t).await;
}

/// #733: the image decoders read a blob as they read a file: a qcow2 and a
/// raw image, through the import's own opener.
#[tokio::test]
async fn an_image_in_a_blob_decodes_as_a_file_does() {
    use crate::image::decode::qcow2::testimg as q;
    use crate::image::decode::{Input, SourceFormat};
    use super::source::BlobSource;
    let t = Thin::new(SLOT, 1 << 20);
    let s = open(&t).await;
    let a: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let c: Vec<u8> = (0..4096).map(|i| ((i * 7) % 253) as u8).collect();
    let img = q::build(12, &[q::C::Data(a.clone()), q::C::Zero, q::C::Hole, q::C::Compressed(c.clone())]);
    let d = put(&s, &img).await;
    let src = BlobSource::open(&s, &format::digest_string(&d)).await.unwrap();
    let (mut image, format) = crate::image::import::open_input(Input::Blob(src.clone()), None, None).await.unwrap();
    assert_eq!(format, SourceFormat::Qcow2);
    assert_eq!(image.virtual_size(), 4 * 4096);
    let mut buf = vec![0u8; 4 * 4096];
    image.read_at(0, &mut buf).await.unwrap();
    assert_eq!(&buf[..4096], &a[..]);
    assert!(buf[4096..3 * 4096].iter().all(|&b| b == 0));
    assert_eq!(&buf[3 * 4096..], &c[..], "a compressed cluster inflates from a blob");

    // The same blob forced raw: its bytes as they are, zeros past the end.
    let (mut raw, format) = crate::image::import::open_input(Input::Blob(src), Some("raw"), None).await.unwrap();
    assert_eq!((format, raw.virtual_size()), (SourceFormat::Raw, img.len() as u64));
    let mut buf = vec![0xAAu8; 8192];
    raw.read_at(img.len() as u64 - 4096, &mut buf).await.unwrap();
    assert_eq!(&buf[..4096], &img[img.len() - 4096..]);
    assert!(buf[4096..].iter().all(|&b| b == 0));
}
