//! A content-addressed blob store in the engine (#732): write-once objects
//! keyed by their sha256, for the registry's OCI blobs
//! (stormblock-registry#241). Nothing to grow or fill a filesystem with:
//! a blob is put under its digest, read by it (with Range), listed and
//! deleted by it, and the space comes from the node's slab pool.
//!
//! # Where the bytes go
//!
//! One thin volume holds the store ([`engine::VOLUME_NAME`]), addressed in
//! its slots (`S` bytes, the volume's extent size):
//!
//! - slot 0: the superblocks; the journal's segments: [`format`].
//! - a blob's body in **whole slots** of its own, its **tail** (what is left
//!   under a slot, or the whole of a small blob) in a **pack**: a slot shared
//!   by many tails, cut in 4 KiB blocks. A 1 KB config takes one block, not
//!   a slot (the issue's "tens of thousands of 1 KB configs").
//! - an upload session's bytes in slots of its own; its commit moves the
//!   last partial slot's bytes into a pack, and the whole slots become the
//!   blob's, unread and uncopied.
//!
//! # Durability
//!
//! Data first, then the journal record that names it: a put's bytes are
//! flushed before its `Put` record is written, an append's before its
//! `SessionAppend`. What a crash leaves between the two is space nothing
//! names, and [`BlobStore::open`] discards every mapped slot nothing names.
//! A delete is journaled before its space can be reused.
//!
//! # Space given back
//!
//! A delete takes the digest out of the index at once; its slots are freed
//! when the last read of it in flight ends ([`Placement`]'s drop): "a read
//! already in flight finishes". Slots go back by discard, so the pool gets
//! them. A pack is given back when no tail in it lives, and a closed pack
//! that is mostly dead is **repacked**: its live tails copied to the open
//! pack, journaled, the old pack then freed like any other space.

pub mod engine;
pub mod format;
pub mod source;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::drive::{BlockDevice, DriveError};
use format::{Digest, Layout, Record, Superblock, BLOCK};

/// How a read or write is cut: large enough to be one I/O to a drive.
const IO_UNIT: usize = 1 << 20;
/// The journal's segments are at least this large, so a batch (a large
/// blob's slot runs, a checkpoint's slice of the index) fits one.
const SEGMENT_BYTES: u64 = 4 << 20;
/// A closed pack whose live blocks fall under this share of it is repacked.
const REPACK_BELOW: u64 = 4;

/// What the store's volume gives it.
#[async_trait]
pub trait Backing: Send + Sync {
    fn device(&self) -> Arc<dyn BlockDevice>;
    /// The volume's slot (extent) size: its allocation and discard unit.
    fn slot_size(&self) -> u64;
    /// The volume's id, for the stats.
    fn volume(&self) -> Option<Uuid> {
        None
    }
    /// Every slot of the volume that holds space.
    async fn mapped_slots(&self) -> Result<Vec<u64>, String>;
    /// Bytes the volume holds on the pool.
    async fn allocated_bytes(&self) -> u64;
    /// What the pool the volume allocates from has free, as this volume
    /// would use it (a mirrored volume's free is the pool's over its copies).
    async fn pool_free_bytes(&self) -> u64;
    /// Make the volume `new_size` bytes long.
    async fn grow(&self, new_size: u64) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Journal blocks an epoch may reach before a checkpoint, at least.
    pub checkpoint_min_blocks: u64,
    /// An upload session untouched this long is aborted.
    pub session_expiry: Duration,
    /// Give space back and expire sessions on a task of the store's own;
    /// off, only [`BlobStore::settle`] does (tests).
    pub background: bool,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig { checkpoint_min_blocks: 4096, session_expiry: Duration::from_secs(7 * 24 * 3600), background: true }
    }
}

#[derive(Debug)]
pub enum BlobError {
    /// The pool is full: `507`, and nothing of the put is kept.
    NoSpace(String),
    /// The bytes do not hash to the digest they were put under.
    Mismatch { want: String, got: String, bytes: u64 },
    NotFound(String),
    /// An append not at the session's current length: `416`.
    BadOffset { asked: u64, length: u64 },
    Invalid(String),
    /// The request's body could not be read.
    Body(String),
    Io(String),
    /// The store's journal failed and the store is being opened again: `503`.
    Unavailable(String),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::NoSpace(m) => write!(f, "no space in the pool: {m}"),
            BlobError::Mismatch { want, got, bytes } => {
                write!(f, "the {bytes} byte(s) hash to {got}, not {want}: nothing kept")
            }
            BlobError::NotFound(m) => write!(f, "{m}"),
            BlobError::BadOffset { asked, length } => {
                write!(f, "an append at {asked}, but the session holds {length} byte(s): append at {length}")
            }
            BlobError::Invalid(m) | BlobError::Body(m) | BlobError::Io(m) | BlobError::Unavailable(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BlobError {}

fn io(what: &str, e: DriveError) -> BlobError {
    match e {
        DriveError::NoSpace(m) => BlobError::NoSpace(m),
        e => BlobError::Io(format!("{what}: {e}")),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn blocks_of(len: u64) -> u64 {
    len.div_ceil(BLOCK)
}

// ---------------------------------------------------------------------------
// Space given back once nothing reads it
// ---------------------------------------------------------------------------

/// Space to give back: whole slots, and a tail's blocks in a pack.
#[derive(Debug, Default)]
struct Freed {
    slots: Vec<u64>,
    tail: Option<(u64, u32)>,
}

#[derive(Default)]
struct Frees {
    q: std::sync::Mutex<Vec<Freed>>,
    notify: tokio::sync::Notify,
}

impl Frees {
    fn push(&self, f: Freed) {
        if f.slots.is_empty() && f.tail.is_none() {
            return;
        }
        self.q.lock().unwrap_or_else(|e| e.into_inner()).push(f);
        self.notify.notify_one();
    }
}

const KEEP: u8 = 0;
const RELEASE_ALL: u8 = 1;
const RELEASE_TAIL: u8 = 2;

/// A blob's layout, shared by the index and every read of it. When a delete
/// has released it, its space is given back as the last holder lets go.
pub struct Placement {
    pub layout: Layout,
    release: AtomicU8,
    frees: Arc<Frees>,
}

impl Placement {
    fn new(layout: Layout, frees: &Arc<Frees>) -> Arc<Placement> {
        Arc::new(Placement { layout, release: AtomicU8::new(KEEP), frees: frees.clone() })
    }

    pub fn size(&self) -> u64 {
        self.layout.size
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        match self.release.load(Ordering::Acquire) {
            RELEASE_ALL => self.frees.push(Freed { slots: std::mem::take(&mut self.layout.slots), tail: self.layout.tail }),
            RELEASE_TAIL => self.frees.push(Freed { slots: Vec::new(), tail: self.layout.tail }),
            _ => {}
        }
    }
}

/// Space a put or an append has taken and not yet journaled: given back if
/// it ends any other way (an error, a mismatch, the client gone and the
/// request's future dropped).
struct Pending {
    slots: Vec<u64>,
    tail: Option<(u64, u32)>,
    frees: Arc<Frees>,
}

impl Pending {
    fn new(frees: &Arc<Frees>) -> Pending {
        Pending { slots: Vec::new(), tail: None, frees: frees.clone() }
    }
    /// Journaled: the space is someone's now.
    fn keep(mut self) -> (Vec<u64>, Option<(u64, u32)>) {
        (std::mem::take(&mut self.slots), self.tail.take())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.frees.push(Freed { slots: std::mem::take(&mut self.slots), tail: self.tail.take() });
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Pack {
    /// Blocks taken by tails that live, or are being written.
    live: u64,
    open: bool,
}

struct Journal {
    epoch: u64,
    seq: u64,
    /// Where the next batch goes, and where its segment ends (bytes).
    pos: u64,
    seg_end: u64,
    /// The first slot of each segment of this epoch.
    segments: Vec<u64>,
    /// Blocks written this epoch.
    blocks: u64,
}

/// An upload session as journaled.
struct SessionMeta {
    len: u64,
    slots: Vec<u64>,
    touched: u64,
    work: Arc<tokio::sync::Mutex<SessionWork>>,
}

/// What one append at a time holds: the running hash, while it covers every
/// byte since the session opened (not after a restart: the commit then
/// reads the session back).
struct SessionWork {
    hasher: Option<Sha256>,
}

struct State {
    sb: Superblock,
    index: BTreeMap<Digest, Arc<Placement>>,
    sessions: HashMap<Uuid, SessionMeta>,
    /// Slots at or past this were never handed out.
    high_water: u64,
    free: BTreeSet<u64>,
    packs: HashMap<u64, Pack>,
    /// The pack tails go to, and its next free block.
    open_pack: Option<(u64, u64)>,
    journal: Journal,
    logical_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScrubStatus {
    pub running: bool,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub checked: u64,
    pub bytes: u64,
    /// Blobs whose bytes do not hash to their digest (or could not be read).
    pub failed: Vec<ScrubFailure>,
    /// Blobs deleted while the scrub ran, not checked.
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScrubFailure {
    pub digest: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub volume: Option<Uuid>,
    pub store: Uuid,
    pub slot_size: u64,
    pub blobs: u64,
    /// The blobs' own bytes.
    pub bytes: u64,
    pub sessions: u64,
    pub session_bytes: u64,
    pub packs: u64,
    pub pack_live_bytes: u64,
    /// Slots in use: blobs, sessions, packs, the journal, the superblock.
    pub slots_in_use: u64,
    pub volume_capacity: u64,
    /// What the store's volume holds on the pool.
    pub used_bytes: u64,
    /// What the pool has free for it: a put larger than this is a `507`.
    pub pool_free_bytes: u64,
    pub journal_epoch: u64,
    pub journal_blocks: u64,
    pub pending_frees: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PutOutcome {
    Created,
    /// The digest was held already: nothing was written.
    Existed,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub length: u64,
    pub touched: u64,
}

pub struct BlobStore {
    backing: Arc<dyn Backing>,
    dev: Arc<dyn BlockDevice>,
    slot: u64,
    cfg: StoreConfig,
    state: tokio::sync::Mutex<State>,
    frees: Arc<Frees>,
    /// One drain of `frees` at a time, so `settle` waits for one under way.
    freeing: tokio::sync::Mutex<()>,
    repacking: std::sync::Mutex<HashSet<u64>>,
    scrub: std::sync::Mutex<ScrubStatus>,
    /// A journal write or flush failed: what is durable is not known, so
    /// nothing more is journaled or given back; the store is opened again
    /// (replay, sweep) once nothing holds this one.
    broken: std::sync::atomic::AtomicBool,
}

impl BlobStore {
    // -----------------------------------------------------------------------
    // Open, format, replay
    // -----------------------------------------------------------------------

    /// Open the store on its volume, formatting a volume that never held
    /// one. Replays the journal, then discards every mapped slot nothing
    /// names.
    pub async fn open(backing: Arc<dyn Backing>, cfg: StoreConfig) -> Result<Arc<BlobStore>, BlobError> {
        let dev = backing.device();
        let slot = backing.slot_size();
        if slot < BLOCK || slot % BLOCK != 0 {
            return Err(BlobError::Invalid(format!("a slot of {slot} bytes: the store needs whole 4 KiB blocks")));
        }
        if dev.capacity_bytes() < 2 * slot {
            backing.grow(2 * slot).await.map_err(BlobError::Io)?;
        }
        let mut b0 = vec![0u8; 2 * BLOCK as usize];
        dev.read(0, &mut b0).await.map_err(|e| io("reading the superblocks", e))?;
        let (a, b) = b0.split_at(BLOCK as usize);
        let sb = match Superblock::pick(Superblock::decode(a), Superblock::decode(b)) {
            Some(sb) => sb,
            None if Superblock::has_magic(a) || Superblock::has_magic(b) => {
                return Err(BlobError::Io("both of the store's superblocks are damaged".into()))
            }
            None if b0.iter().any(|&x| x != 0) => {
                return Err(BlobError::Invalid("the volume holds something that is not a blob store".into()))
            }
            None => {
                let segment_slots = SEGMENT_BYTES.div_ceil(slot).max(1);
                let sb = Superblock { generation: 1, epoch: 1, head: 1, slot_size: slot, segment_slots, store: Uuid::new_v4() };
                let need = (1 + segment_slots) * slot;
                if dev.capacity_bytes() < need {
                    backing.grow(need).await.map_err(BlobError::Io)?;
                }
                dev.write(sb.copy() * BLOCK, &sb.encode()).await.map_err(|e| io("writing the superblock", e))?;
                dev.flush().await.map_err(|e| io("flushing the superblock", e))?;
                tracing::info!("blob store {} formatted: {slot}-byte slots, journal segments of {segment_slots}", sb.store);
                sb
            }
        };
        if sb.slot_size != slot {
            return Err(BlobError::Invalid(format!(
                "the store was made with {}-byte slots and its volume has {slot}",
                sb.slot_size
            )));
        }
        let frees = Arc::new(Frees::default());
        let state = replay(&dev, sb, &frees).await?;
        let store = Arc::new(BlobStore {
            backing,
            dev,
            slot,
            cfg,
            state: tokio::sync::Mutex::new(state),
            frees,
            freeing: tokio::sync::Mutex::new(()),
            repacking: Default::default(),
            scrub: Default::default(),
            broken: Default::default(),
        });
        store.sweep().await;
        if store.cfg.background {
            store.clone().spawn_background();
        }
        let sparse: Vec<u64> = {
            let st = store.state.lock().await;
            st.packs.iter().filter(|(_, p)| p.live * REPACK_BELOW < slot / BLOCK).map(|(s, _)| *s).collect()
        };
        if !sparse.is_empty() {
            let s = store.clone();
            let work = async move {
                for p in sparse {
                    s.repack(p).await;
                }
            };
            if store.cfg.background {
                tokio::spawn(work);
            } else {
                work.await;
            }
        }
        Ok(store)
    }

    /// Every mapped slot nothing names is discarded: a put or append cut
    /// short by a crash, a delete journaled and not yet discarded.
    async fn sweep(&self) {
        let mapped = match self.backing.mapped_slots().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("blob store: the volume's mapped slots could not be read, nothing swept: {e}");
                return;
            }
        };
        let used = self.state.lock().await.used_slots();
        let stray: Vec<u64> = mapped.into_iter().filter(|s| !used.contains(s)).collect();
        if stray.is_empty() {
            return;
        }
        for s in &stray {
            if let Err(e) = self.dev.discard(s * self.slot, self.slot).await {
                tracing::warn!("blob store: discarding stray slot {s}: {e}");
            }
        }
        tracing::info!("blob store: {} slot(s) nothing names given back", stray.len());
    }

    fn spawn_background(self: Arc<Self>) {
        let weak: Weak<BlobStore> = Arc::downgrade(&self);
        let frees = self.frees.clone();
        drop(self);
        tokio::spawn(async move {
            let mut last_expiry = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = frees.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
                let Some(store) = weak.upgrade() else { break };
                store.settle().await;
                if last_expiry.elapsed() >= Duration::from_secs(3600) {
                    last_expiry = std::time::Instant::now();
                    store.expire_sessions().await;
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Make the volume reach slot `upto` (exclusive), growing it by a
    /// quarter at least: the volume's size is what the pool counts as
    /// promised (#152), so it follows what the store uses.
    async fn ensure_slots(&self, upto: u64) -> Result<(), BlobError> {
        let cap = self.dev.capacity_bytes() / self.slot;
        if upto <= cap {
            return Ok(());
        }
        let step = (cap / 4).max((256 << 20) / self.slot).max(1);
        let want = upto.max(cap + step);
        self.backing
            .grow(want * self.slot)
            .await
            .map_err(|e| BlobError::NoSpace(format!("the store's volume could not grow to {} slots: {e}", want)))
    }

    /// One slot, next to `near` when that one is free.
    async fn alloc_slot(&self, st: &mut State, near: Option<u64>) -> Result<u64, BlobError> {
        if let Some(n) = near.map(|n| n + 1) {
            if st.free.remove(&n) {
                return Ok(n);
            }
            if n == st.high_water {
                self.ensure_slots(n + 1).await?;
                st.high_water += 1;
                return Ok(n);
            }
        }
        if let Some(s) = st.free.pop_first() {
            return Ok(s);
        }
        let s = st.high_water;
        self.ensure_slots(s + 1).await?;
        st.high_water += 1;
        Ok(s)
    }

    /// A journal segment: contiguous slots, a free run of them or from the
    /// high water.
    async fn alloc_segment(&self, st: &mut State) -> Result<u64, BlobError> {
        let n = st.sb.segment_slots;
        if let Some(s) = take_run(&mut st.free, n) {
            return Ok(s);
        }
        let s = st.high_water;
        self.ensure_slots(s + n).await?;
        st.high_water += n;
        Ok(s)
    }

    /// Blocks for a tail of `len` bytes in the open pack, or a new one.
    async fn alloc_tail(&self, st: &mut State, len: u64) -> Result<(u64, u32), BlobError> {
        let need = blocks_of(len);
        let per = self.slot / BLOCK;
        if let Some((p, next)) = st.open_pack {
            if next + need <= per {
                st.open_pack = Some((p, next + need));
                if let Some(pk) = st.packs.get_mut(&p) {
                    pk.live += need;
                }
                return Ok((p * self.slot + next * BLOCK, len as u32));
            }
            self.close_pack(st, p);
        }
        let p = self.alloc_slot(st, None).await?;
        st.packs.insert(p, Pack { live: need, open: true });
        st.open_pack = Some((p, need));
        Ok((p * self.slot, len as u32))
    }

    fn close_pack(&self, st: &mut State, p: u64) {
        st.open_pack = None;
        let empty = match st.packs.get_mut(&p) {
            Some(pk) => {
                pk.open = false;
                pk.live == 0
            }
            None => false,
        };
        if empty {
            st.packs.remove(&p);
            self.frees.push(Freed { slots: vec![p], tail: None });
        }
    }

    /// Whether the pool has room for `len` more bytes: a `507` before any
    /// byte is read, when the caller says how many come.
    pub async fn space_for(&self, len: u64) -> Result<(), BlobError> {
        self.space_check(Some(len)).await
    }

    async fn space_check(&self, len: Option<u64>) -> Result<(), BlobError> {
        let free = self.backing.pool_free_bytes().await;
        let need = match len {
            Some(n) => (n / self.slot + 1) * self.slot,
            None => self.slot,
        };
        if free < need {
            return Err(BlobError::NoSpace(format!("the pool has {free} byte(s) free, {need} wanted")));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Freeing
    // -----------------------------------------------------------------------

    /// Give back everything released so far: discard, then free.
    pub async fn settle(&self) {
        let _one = self.freeing.lock().await;
        loop {
            if self.is_broken() {
                return;
            }
            let items: Vec<Freed> = std::mem::take(&mut *self.frees.q.lock().unwrap_or_else(|e| e.into_inner()));
            if items.is_empty() {
                break;
            }
            let mut slots = Vec::new();
            let mut sparse = Vec::new();
            {
                let mut st = self.state.lock().await;
                let per = self.slot / BLOCK;
                for it in &items {
                    slots.extend_from_slice(&it.slots);
                    if let Some((off, len)) = it.tail {
                        let p = off / self.slot;
                        if let Some(pk) = st.packs.get_mut(&p) {
                            pk.live = pk.live.saturating_sub(blocks_of(len as u64));
                            if !pk.open && pk.live == 0 {
                                st.packs.remove(&p);
                                slots.push(p);
                            } else if !pk.open && pk.live * REPACK_BELOW < per {
                                sparse.push(p);
                            }
                        }
                    }
                }
            }
            for s in &slots {
                if let Err(e) = self.dev.discard(s * self.slot, self.slot).await {
                    tracing::warn!("blob store: discarding slot {s}: {e}");
                }
            }
            {
                let mut st = self.state.lock().await;
                for s in slots {
                    st.free.insert(s);
                }
            }
            sparse.sort_unstable();
            sparse.dedup();
            for p in sparse {
                self.repack(p).await;
            }
        }
    }

    /// Copy a sparse closed pack's live tails to the open pack; the old
    /// pack is freed when the last of them is let go.
    async fn repack(&self, p: u64) {
        if !self.repacking.lock().unwrap_or_else(|e| e.into_inner()).insert(p) {
            return;
        }
        let moved = self.repack_inner(p).await;
        self.repacking.lock().unwrap_or_else(|e| e.into_inner()).remove(&p);
        match moved {
            Ok(0) => {}
            Ok(n) => tracing::info!("blob store: pack {p} repacked, {n} tail(s) moved"),
            Err(e) => tracing::warn!("blob store: repacking pack {p}: {e}"),
        }
    }

    async fn repack_inner(&self, p: u64) -> Result<usize, BlobError> {
        let tails: Vec<(Digest, Arc<Placement>)> = {
            let st = self.state.lock().await;
            if st.open_pack.map(|(o, _)| o) == Some(p) {
                return Ok(0);
            }
            st.index
                .iter()
                .filter(|(_, pl)| pl.layout.tail.is_some_and(|(off, _)| off / self.slot == p))
                .map(|(d, pl)| (*d, pl.clone()))
                .collect()
        };
        let mut moved = 0;
        for chunk in tails.chunks(256) {
            let mut fresh: Vec<(Digest, Arc<Placement>, Pending)> = Vec::new();
            for (d, old) in chunk {
                let (off, len) = old.layout.tail.expect("filtered on a tail");
                let mut buf = vec![0u8; (blocks_of(len as u64) * BLOCK) as usize];
                self.dev.read(off, &mut buf).await.map_err(|e| io("reading a tail", e))?;
                let mut pend = Pending::new(&self.frees);
                let at = {
                    let mut st = self.state.lock().await;
                    self.alloc_tail(&mut st, len as u64).await?
                };
                pend.tail = Some(at);
                self.dev.write(at.0, &buf).await.map_err(|e| io("writing a tail", e))?;
                fresh.push((*d, old.clone(), pend));
            }
            self.dev.flush().await.map_err(|e| io("flushing", e))?;
            let mut st = self.state.lock().await;
            let mut records = Vec::new();
            let mut swaps = Vec::new();
            for (d, old, pend) in fresh {
                // Deleted or replaced since: the copy goes back (`pend` drops).
                if st.index.get(&d).is_some_and(|cur| Arc::ptr_eq(cur, &old)) {
                    let (_, tail) = pend.keep();
                    let layout = Layout { size: old.layout.size, slots: old.layout.slots.clone(), tail };
                    records.push(Record::Put { digest: d, layout: layout.clone() });
                    swaps.push((d, layout));
                }
            }
            if records.is_empty() {
                continue;
            }
            if let Err(e) = self.journal(&mut st, &records).await {
                for (_, layout) in swaps {
                    self.frees.push(Freed { slots: Vec::new(), tail: layout.tail });
                }
                return Err(e);
            }
            for (d, layout) in swaps {
                let new = Placement::new(layout, &self.frees);
                if let Some(old) = st.index.insert(d, new) {
                    old.release.store(RELEASE_TAIL, Ordering::Release);
                }
                moved += 1;
            }
            self.maybe_checkpoint(&mut st).await;
        }
        Ok(moved)
    }

    // -----------------------------------------------------------------------
    // The journal
    // -----------------------------------------------------------------------

    /// Write one batch at the journal's end, a new segment first if it does
    /// not fit this one. Not flushed.
    async fn write_batch(&self, st: &mut State, j: &mut Journal, store: Uuid, records: &[Record]) -> Result<(), BlobError> {
        let seg_bytes = st.sb.segment_slots * self.slot;
        let mut b = format::encode_batch(&store, j.epoch, j.seq, records);
        if b.len() as u64 + BLOCK > seg_bytes {
            return Err(BlobError::Invalid(format!(
                "a journal batch of {} bytes does not fit a {seg_bytes}-byte segment",
                b.len()
            )));
        }
        if j.pos + b.len() as u64 + BLOCK > j.seg_end {
            let next = self.alloc_segment(st).await?;
            let nb = format::encode_batch(&store, j.epoch, j.seq, &[Record::Next { slot: next }]);
            self.dev.write(j.pos, &nb).await.map_err(|e| io("writing the journal", e))?;
            j.seq += 1;
            j.blocks += 1;
            j.pos = next * self.slot;
            j.seg_end = j.pos + seg_bytes;
            j.segments.push(next);
            b = format::encode_batch(&store, j.epoch, j.seq, records);
        }
        self.dev.write(j.pos, &b).await.map_err(|e| io("writing the journal", e))?;
        j.pos += b.len() as u64;
        j.seq += 1;
        j.blocks += b.len() as u64 / BLOCK;
        Ok(())
    }

    /// Whether a journal failure has stopped this store.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Acquire)
    }

    fn refuse_broken(&self) -> Result<(), BlobError> {
        if self.is_broken() {
            return Err(BlobError::Unavailable("the blob store's journal failed; it is opened again once the requests on it end".into()));
        }
        Ok(())
    }

    fn break_on(&self, what: &str, e: &BlobError) {
        if !self.broken.swap(true, Ordering::AcqRel) {
            tracing::error!("blob store: {what} failed ({e}): nothing more is journaled until the store is opened again");
        }
    }

    /// Records made durable, in one batch. A failure breaks the store: the
    /// batch may yet reach the disk, so nothing may act as if it had not.
    async fn journal(&self, st: &mut State, records: &[Record]) -> Result<(), BlobError> {
        self.refuse_broken()?;
        let mut j = std::mem::replace(&mut st.journal, empty_journal());
        let store = st.sb.store;
        let r = self.write_batch(st, &mut j, store, records).await;
        st.journal = j;
        let r = match r {
            Ok(()) => self.dev.flush().await.map_err(|e| io("flushing the journal", e)),
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            self.break_on("a journal write", e);
        }
        r
    }

    async fn maybe_checkpoint(&self, st: &mut State) {
        let live_blocks = (st.index.len() as u64 * 80 + st.sessions.len() as u64 * 64) / BLOCK;
        if st.journal.blocks <= self.cfg.checkpoint_min_blocks.max(2 * live_blocks) {
            return;
        }
        if let Err(e) = self.checkpoint(st).await {
            // Whether the new superblock reached the disk is not known.
            self.break_on("a checkpoint", &e);
        }
    }

    /// The live set as a fresh chain under the next epoch, then the
    /// superblock that names it; the old chain is then free.
    async fn checkpoint(&self, st: &mut State) -> Result<(), BlobError> {
        let head = self.alloc_segment(st).await?;
        let seg_bytes = st.sb.segment_slots * self.slot;
        let mut j = Journal { epoch: st.journal.epoch + 1, seq: 0, pos: head * self.slot, seg_end: head * self.slot + seg_bytes, segments: vec![head], blocks: 0 };
        let mut records: Vec<Record> = st
            .index
            .iter()
            .map(|(d, p)| Record::Put { digest: *d, layout: p.layout.clone() })
            .collect();
        for (id, m) in &st.sessions {
            records.push(Record::SessionOpen { id: *id, at: m.touched });
            records.push(Record::SessionAppend { id: *id, len: m.len, at: m.touched, slots: m.slots.clone() });
        }
        let limit = (seg_bytes / 4) as usize;
        let store = st.sb.store;
        let mut batch = Vec::new();
        let mut size = 0usize;
        let result: Result<(), BlobError> = async {
            for r in records {
                let mut enc = Vec::new();
                r.encode(&mut enc);
                if size + enc.len() > limit && !batch.is_empty() {
                    self.write_batch(st, &mut j, store, &batch).await?;
                    batch.clear();
                    size = 0;
                }
                size += enc.len();
                batch.push(r);
            }
            if !batch.is_empty() {
                self.write_batch(st, &mut j, store, &batch).await?;
            }
            self.dev.flush().await.map_err(|e| io("flushing the checkpoint", e))?;
            let sb = Superblock { generation: st.sb.generation + 1, epoch: j.epoch, head, ..st.sb };
            self.dev.write(sb.copy() * BLOCK, &sb.encode()).await.map_err(|e| io("writing the superblock", e))?;
            self.dev.flush().await.map_err(|e| io("flushing the superblock", e))?;
            st.sb = sb;
            Ok(())
        }
        .await;
        result?;
        let old = std::mem::replace(&mut st.journal, j);
        let mut slots = Vec::new();
        for s in &old.segments {
            slots.extend(*s..*s + st.sb.segment_slots);
        }
        tracing::info!(
            "blob store: checkpoint to epoch {} ({} blob(s), {} session(s)); {} journal block(s) let go",
            st.journal.epoch,
            st.index.len(),
            st.sessions.len(),
            old.blocks
        );
        self.frees.push(Freed { slots, tail: None });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    pub async fn get(&self, d: &Digest) -> Option<Arc<Placement>> {
        self.state.lock().await.index.get(d).cloned()
    }

    /// `buf.len()` bytes of a blob from `off`.
    pub async fn read(&self, p: &Placement, off: u64, buf: &mut [u8]) -> Result<(), BlobError> {
        read_layout(&self.dev, self.slot, &p.layout, off, buf).await
    }

    /// A blob's bytes `[start, end)`, as a stream of pieces of an I/O each.
    pub fn stream(
        self: &Arc<Self>,
        p: Arc<Placement>,
        start: u64,
        end: u64,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
        let store = self.clone();
        futures_util::stream::unfold((store, p, start), move |(store, p, pos)| async move {
            if pos >= end {
                return None;
            }
            let n = (end - pos).min(IO_UNIT as u64) as usize;
            let mut buf = vec![0u8; n];
            match store.read(&p, pos, &mut buf).await {
                Ok(()) => Some((Ok(Bytes::from(buf)), (store, p, pos + n as u64))),
                Err(e) => {
                    tracing::warn!("blob store: reading a blob at {pos}: {e}");
                    Some((Err(std::io::Error::other(e.to_string())), (store, p, end)))
                }
            }
        })
    }

    async fn hash_layout(&self, layout: &Layout) -> Result<Digest, BlobError> {
        let mut h = Sha256::new();
        let mut pos = 0u64;
        let mut buf = vec![0u8; IO_UNIT];
        while pos < layout.size {
            let n = (layout.size - pos).min(IO_UNIT as u64) as usize;
            read_layout(&self.dev, self.slot, layout, pos, &mut buf[..n]).await?;
            h.update(&buf[..n]);
            pos += n as u64;
        }
        Ok(h.finalize().into())
    }

    // -----------------------------------------------------------------------
    // Put
    // -----------------------------------------------------------------------

    /// A blob put under `digest`, streamed: hashed as it lands, kept only if
    /// it matches. A digest held already answers `Existed` and writes
    /// nothing (the body is read and dropped).
    pub async fn put<S, E>(&self, digest: Digest, len: Option<u64>, mut body: S) -> Result<PutOutcome, BlobError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        if self.state.lock().await.index.contains_key(&digest) {
            while let Some(c) = body.next().await {
                c.map_err(|e| BlobError::Body(e.to_string()))?;
            }
            return Ok(PutOutcome::Existed);
        }
        self.space_check(len).await?;
        let mut pend = Pending::new(&self.frees);
        let mut hasher = Sha256::new();
        let mut buf: Vec<u8> = Vec::with_capacity(self.slot.min(64 << 20) as usize);
        let mut size = 0u64;
        while let Some(c) = body.next().await {
            let c = c.map_err(|e| BlobError::Body(format!("reading the blob: {e}")))?;
            hasher.update(&c);
            size += c.len() as u64;
            let mut rest: &[u8] = &c;
            while !rest.is_empty() {
                let take = (self.slot as usize - buf.len()).min(rest.len());
                buf.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if buf.len() as u64 == self.slot {
                    let s = {
                        let mut st = self.state.lock().await;
                        self.alloc_slot(&mut st, pend.slots.last().copied()).await?
                    };
                    pend.slots.push(s);
                    self.dev.write(s * self.slot, &buf).await.map_err(|e| io("writing a blob", e))?;
                    buf.clear();
                }
            }
        }
        if !buf.is_empty() {
            let at = {
                let mut st = self.state.lock().await;
                self.alloc_tail(&mut st, buf.len() as u64).await?
            };
            pend.tail = Some(at);
            buf.resize((blocks_of(buf.len() as u64) * BLOCK) as usize, 0);
            self.dev.write(at.0, &buf).await.map_err(|e| io("writing a blob's tail", e))?;
        }
        let got: Digest = hasher.finalize().into();
        if got != digest {
            return Err(BlobError::Mismatch { want: format::digest_string(&digest), got: format::digest_string(&got), bytes: size });
        }
        self.dev.flush().await.map_err(|e| io("flushing a blob", e))?;
        self.commit_layout(digest, size, pend).await
    }

    /// Journal a written blob and index it. A digest that came to be held
    /// meanwhile: `Existed`, and what was written goes back.
    async fn commit_layout(&self, digest: Digest, size: u64, pend: Pending) -> Result<PutOutcome, BlobError> {
        let mut st = self.state.lock().await;
        if st.index.contains_key(&digest) {
            drop(pend);
            return Ok(PutOutcome::Existed);
        }
        self.journal(&mut st, &[Record::Put { digest, layout: Layout { size, slots: pend.slots.clone(), tail: pend.tail } }]).await?;
        let (slots, tail) = pend.keep();
        st.index.insert(digest, Placement::new(Layout { size, slots, tail }, &self.frees));
        st.logical_bytes += size;
        self.maybe_checkpoint(&mut st).await;
        Ok(PutOutcome::Created)
    }

    // -----------------------------------------------------------------------
    // Delete, list, stats
    // -----------------------------------------------------------------------

    /// Whether the digest was held. Not held is no error: a delete that is
    /// retried, or races another, succeeds.
    pub async fn delete(&self, d: &Digest) -> Result<bool, BlobError> {
        let mut st = self.state.lock().await;
        if !st.index.contains_key(d) {
            return Ok(false);
        }
        self.journal(&mut st, &[Record::Delete { digest: *d }]).await?;
        if let Some(p) = st.index.remove(d) {
            st.logical_bytes -= p.layout.size;
            p.release.store(RELEASE_ALL, Ordering::Release);
        }
        self.maybe_checkpoint(&mut st).await;
        Ok(true)
    }

    /// Digests in order, after `after`, `limit` of them; the last one given
    /// when there are more.
    pub async fn list(&self, after: Option<Digest>, limit: usize) -> (Vec<(Digest, u64)>, Option<Digest>) {
        let st = self.state.lock().await;
        let range = match after {
            Some(a) => st.index.range((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded)),
            None => st.index.range::<Digest, _>(..),
        };
        let mut out: Vec<(Digest, u64)> = range.take(limit + 1).map(|(d, p)| (*d, p.layout.size)).collect();
        let more = out.len() > limit;
        out.truncate(limit);
        let next = if more { out.last().map(|(d, _)| *d) } else { None };
        (out, next)
    }

    pub async fn stats(&self) -> Stats {
        let (mut s, pending) = {
            let st = self.state.lock().await;
            let pending = self.frees.q.lock().unwrap_or_else(|e| e.into_inner()).len() as u64;
            (
                Stats {
                    volume: self.backing.volume(),
                    store: st.sb.store,
                    slot_size: self.slot,
                    blobs: st.index.len() as u64,
                    bytes: st.logical_bytes,
                    sessions: st.sessions.len() as u64,
                    session_bytes: st.sessions.values().map(|m| m.len).sum(),
                    packs: st.packs.len() as u64,
                    pack_live_bytes: st.packs.values().map(|p| p.live * BLOCK).sum(),
                    slots_in_use: st.high_water - st.free.len() as u64,
                    volume_capacity: self.dev.capacity_bytes(),
                    used_bytes: 0,
                    pool_free_bytes: 0,
                    journal_epoch: st.journal.epoch,
                    journal_blocks: st.journal.blocks,
                    pending_frees: 0,
                },
                pending,
            )
        };
        s.pending_frees = pending;
        s.used_bytes = self.backing.allocated_bytes().await;
        s.pool_free_bytes = self.backing.pool_free_bytes().await;
        s
    }

    // -----------------------------------------------------------------------
    // Upload sessions
    // -----------------------------------------------------------------------

    pub async fn open_session(&self) -> Result<Uuid, BlobError> {
        let id = Uuid::new_v4();
        let at = now();
        let mut st = self.state.lock().await;
        self.journal(&mut st, &[Record::SessionOpen { id, at }]).await?;
        st.sessions.insert(id, SessionMeta::new(0, Vec::new(), at));
        Ok(id)
    }

    pub async fn session(&self, id: &Uuid) -> Option<SessionInfo> {
        let st = self.state.lock().await;
        st.sessions.get(id).map(|m| SessionInfo { id: *id, length: m.len, touched: m.touched })
    }

    pub async fn sessions(&self) -> Vec<SessionInfo> {
        let st = self.state.lock().await;
        let mut v: Vec<SessionInfo> =
            st.sessions.iter().map(|(id, m)| SessionInfo { id: *id, length: m.len, touched: m.touched }).collect();
        v.sort_by_key(|s| s.touched);
        v
    }

    fn work_of(st: &State, id: &Uuid) -> Result<Arc<tokio::sync::Mutex<SessionWork>>, BlobError> {
        st.sessions
            .get(id)
            .map(|m| m.work.clone())
            .ok_or_else(|| BlobError::NotFound(format!("no upload session {id}")))
    }

    /// Bytes appended at `offset`, which must be the session's length. The
    /// new length once durable.
    pub async fn append<S, E>(&self, id: &Uuid, offset: Option<u64>, body: S) -> Result<u64, BlobError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        let work = Self::work_of(&*self.state.lock().await, id)?;
        let mut w = work.lock().await;
        self.append_locked(&mut w, id, offset, body).await
    }

    async fn append_locked<S, E>(&self, w: &mut SessionWork, id: &Uuid, offset: Option<u64>, mut body: S) -> Result<u64, BlobError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        let (len, mut slots) = {
            let st = self.state.lock().await;
            let m = st.sessions.get(id).ok_or_else(|| BlobError::NotFound(format!("no upload session {id}")))?;
            (m.len, m.slots.clone())
        };
        if let Some(o) = offset {
            if o != len {
                return Err(BlobError::BadOffset { asked: o, length: len });
            }
        }
        let mut hasher = w.hasher.clone();
        let mut pend = Pending::new(&self.frees);
        // Writes start at the block `len` is in, with what is already there.
        let mut base = len / BLOCK * BLOCK;
        let mut buf: Vec<u8> = Vec::with_capacity(IO_UNIT + BLOCK as usize);
        if len % BLOCK != 0 {
            let mut b = vec![0u8; BLOCK as usize];
            read_slots(&self.dev, self.slot, &slots, base, &mut b).await?;
            buf.extend_from_slice(&b[..(len % BLOCK) as usize]);
        }
        let mut new_len = len;
        while let Some(c) = body.next().await {
            let c = c.map_err(|e| BlobError::Body(format!("reading the chunk: {e}")))?;
            if let Some(h) = hasher.as_mut() {
                h.update(&c);
            }
            new_len += c.len() as u64;
            buf.extend_from_slice(&c);
            while buf.len() >= IO_UNIT {
                self.write_session(&mut slots, &mut pend, base, &buf[..IO_UNIT]).await?;
                buf.drain(..IO_UNIT);
                base += IO_UNIT as u64;
            }
        }
        if new_len == len {
            return Ok(len);
        }
        if !buf.is_empty() {
            buf.resize((blocks_of(buf.len() as u64) * BLOCK) as usize, 0);
            self.write_session(&mut slots, &mut pend, base, &buf).await?;
        }
        self.dev.flush().await.map_err(|e| io("flushing an append", e))?;
        let mut st = self.state.lock().await;
        if !st.sessions.contains_key(id) {
            return Err(BlobError::NotFound(format!("upload session {id} ended during the append")));
        }
        let at = now();
        self.journal(&mut st, &[Record::SessionAppend { id: *id, len: new_len, at, slots: pend.slots.clone() }]).await?;
        let (added, _) = pend.keep();
        if let Some(m) = st.sessions.get_mut(id) {
            m.len = new_len;
            m.slots.extend(added);
            m.touched = at;
        }
        w.hasher = hasher;
        self.maybe_checkpoint(&mut st).await;
        Ok(new_len)
    }

    /// Write whole blocks at a session's logical `base`, taking slots past
    /// the ones it has.
    async fn write_session(&self, slots: &mut Vec<u64>, pend: &mut Pending, mut base: u64, mut data: &[u8]) -> Result<(), BlobError> {
        while !data.is_empty() {
            let k = (base / self.slot) as usize;
            while slots.len() <= k {
                let s = {
                    let mut st = self.state.lock().await;
                    self.alloc_slot(&mut st, slots.last().copied()).await?
                };
                slots.push(s);
                pend.slots.push(s);
            }
            let within = base % self.slot;
            let n = ((self.slot - within) as usize).min(data.len());
            self.dev.write(slots[k] * self.slot + within, &data[..n]).await.map_err(|e| io("writing an append", e))?;
            data = &data[n..];
            base += n as u64;
        }
        Ok(())
    }

    /// Verify the session's bytes against `digest` and seal them under it.
    /// `body`, when given, is appended first. A mismatch keeps the session.
    pub async fn commit<S, E>(&self, id: &Uuid, digest: Digest, body: Option<S>) -> Result<PutOutcome, BlobError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        let work = Self::work_of(&*self.state.lock().await, id)?;
        let mut w = work.lock().await;
        if let Some(b) = body {
            self.append_locked(&mut w, id, None, b).await?;
        }
        let (len, slots) = {
            let st = self.state.lock().await;
            let m = st.sessions.get(id).ok_or_else(|| BlobError::NotFound(format!("no upload session {id}")))?;
            (m.len, m.slots.clone())
        };
        let full = (len / self.slot) as usize;
        let rest = len % self.slot;
        let got: Digest = match w.hasher.clone() {
            Some(h) => h.finalize().into(),
            None => {
                let mut whole = slots[..full].to_vec();
                if rest > 0 {
                    whole.push(slots[full]);
                }
                // Read as whole slots: the tail is the session's last slot.
                let l = Layout { size: len, slots: whole, tail: None };
                self.hash_layout(&l).await?
            }
        };
        if got != digest {
            return Err(BlobError::Mismatch { want: format::digest_string(&digest), got: format::digest_string(&got), bytes: len });
        }
        let mut pend = Pending::new(&self.frees);
        if rest > 0 {
            let mut buf = vec![0u8; (blocks_of(rest) * BLOCK) as usize];
            self.dev.read(slots[full] * self.slot, &mut buf).await.map_err(|e| io("reading the session's tail", e))?;
            let at = {
                let mut st = self.state.lock().await;
                self.alloc_tail(&mut st, rest).await?
            };
            pend.tail = Some(at);
            self.dev.write(at.0, &buf).await.map_err(|e| io("writing a blob's tail", e))?;
            self.dev.flush().await.map_err(|e| io("flushing a blob's tail", e))?;
        }
        // The session's whole slots become the blob's: not `pend`'s to give
        // back if the commit fails, since the session still holds them.
        self.commit_session(digest, len, slots[..full].to_vec(), pend, *id).await
    }

    /// Journal a session's blob and the session's end in one batch. A digest
    /// held meanwhile: only the end, and all the session's space (the tail
    /// copied out with it) goes back.
    async fn commit_session(&self, digest: Digest, size: u64, body: Vec<u64>, tail: Pending, id: Uuid) -> Result<PutOutcome, BlobError> {
        let mut st = self.state.lock().await;
        let existed = st.index.contains_key(&digest);
        let mut records = Vec::new();
        if !existed {
            records.push(Record::Put { digest, layout: Layout { size, slots: body.clone(), tail: tail.tail } });
        }
        records.push(Record::SessionEnd { id });
        self.journal(&mut st, &records).await?;
        let kept: HashSet<u64> = if existed { HashSet::new() } else { body.iter().copied().collect() };
        if let Some(m) = st.sessions.remove(&id) {
            let rest: Vec<u64> = m.slots.into_iter().filter(|s| !kept.contains(s)).collect();
            self.frees.push(Freed { slots: rest, tail: None });
        }
        let outcome = if existed {
            drop(tail);
            PutOutcome::Existed
        } else {
            let (_, tail) = tail.keep();
            st.index.insert(digest, Placement::new(Layout { size, slots: body, tail }, &self.frees));
            st.logical_bytes += size;
            PutOutcome::Created
        };
        self.maybe_checkpoint(&mut st).await;
        Ok(outcome)
    }

    /// End a session and give its space back. Not there is no error.
    pub async fn abort(&self, id: &Uuid) -> Result<bool, BlobError> {
        let work = match Self::work_of(&*self.state.lock().await, id) {
            Ok(w) => w,
            Err(_) => return Ok(false),
        };
        let _w = work.lock().await;
        let mut st = self.state.lock().await;
        if !st.sessions.contains_key(id) {
            return Ok(false);
        }
        self.journal(&mut st, &[Record::SessionEnd { id: *id }]).await?;
        if let Some(m) = st.sessions.remove(id) {
            self.frees.push(Freed { slots: m.slots, tail: None });
        }
        self.maybe_checkpoint(&mut st).await;
        Ok(true)
    }

    /// Abort every session untouched for longer than the expiry.
    pub async fn expire_sessions(&self) -> usize {
        let cutoff = now().saturating_sub(self.cfg.session_expiry.as_secs());
        let old: Vec<Uuid> = {
            let st = self.state.lock().await;
            st.sessions.iter().filter(|(_, m)| m.touched < cutoff).map(|(id, _)| *id).collect()
        };
        let mut n = 0;
        for id in old {
            match self.abort(&id).await {
                Ok(true) => {
                    n += 1;
                    tracing::info!("blob store: upload session {id} untouched for {:?}, aborted", self.cfg.session_expiry);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("blob store: aborting expired session {id}: {e}"),
            }
        }
        n
    }

    // -----------------------------------------------------------------------
    // Scrub
    // -----------------------------------------------------------------------

    pub fn scrub_status(&self) -> ScrubStatus {
        self.scrub.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Start a scrub unless one runs; whether this call started it.
    pub fn start_scrub(self: &Arc<Self>) -> bool {
        {
            let mut s = self.scrub.lock().unwrap_or_else(|e| e.into_inner());
            if s.running {
                return false;
            }
            *s = ScrubStatus { running: true, started_at: Some(now()), ..Default::default() };
        }
        let me = self.clone();
        tokio::spawn(async move { me.run_scrub().await });
        true
    }

    /// Read every blob and hash it against its digest.
    pub async fn run_scrub(self: Arc<Self>) -> ScrubStatus {
        {
            let mut s = self.scrub.lock().unwrap_or_else(|e| e.into_inner());
            if !s.running {
                *s = ScrubStatus { running: true, started_at: Some(now()), ..Default::default() };
            }
        }
        let digests: Vec<Digest> = self.state.lock().await.index.keys().copied().collect();
        for d in digests {
            let Some(p) = self.get(&d).await else {
                self.scrub.lock().unwrap_or_else(|e| e.into_inner()).skipped += 1;
                continue;
            };
            let r = self.hash_layout(&p.layout).await;
            let mut s = self.scrub.lock().unwrap_or_else(|e| e.into_inner());
            s.checked += 1;
            s.bytes += p.layout.size;
            match r {
                Ok(got) if got == d => {}
                Ok(got) => {
                    tracing::error!("blob store scrub: {} reads as {}", format::digest_string(&d), format::digest_string(&got));
                    s.failed.push(ScrubFailure { digest: format::digest_string(&d), error: format!("reads as {}", format::digest_string(&got)) });
                }
                Err(e) => {
                    tracing::error!("blob store scrub: {}: {e}", format::digest_string(&d));
                    s.failed.push(ScrubFailure { digest: format::digest_string(&d), error: e.to_string() });
                }
            }
        }
        let mut s = self.scrub.lock().unwrap_or_else(|e| e.into_inner());
        s.running = false;
        s.finished_at = Some(now());
        tracing::info!("blob store scrub: {} blob(s), {} byte(s), {} failed", s.checked, s.bytes, s.failed.len());
        s.clone()
    }
}

impl SessionMeta {
    fn new(len: u64, slots: Vec<u64>, touched: u64) -> SessionMeta {
        // A session's hash is kept from its first byte: one opened here, or
        // one with nothing in it yet.
        let hasher = (len == 0).then(Sha256::new);
        SessionMeta { len, slots, touched, work: Arc::new(tokio::sync::Mutex::new(SessionWork { hasher })) }
    }
}

impl State {
    /// Every slot something names.
    fn used_slots(&self) -> HashSet<u64> {
        let mut used = HashSet::new();
        used.insert(0);
        for s in &self.journal.segments {
            used.extend(*s..*s + self.sb.segment_slots);
        }
        for p in self.index.values() {
            used.extend(p.layout.slots.iter().copied());
        }
        used.extend(self.packs.keys().copied());
        for m in self.sessions.values() {
            used.extend(m.slots.iter().copied());
        }
        used
    }
}

/// The first of `n` consecutive free slots, taken.
fn take_run(free: &mut BTreeSet<u64>, n: u64) -> Option<u64> {
    let mut start = 0;
    let mut len = 0;
    let mut prev: Option<u64> = None;
    let mut found = None;
    for &s in free.iter() {
        if prev.is_some_and(|p| p + 1 == s) {
            len += 1;
        } else {
            start = s;
            len = 1;
        }
        prev = Some(s);
        if len == n {
            found = Some(start);
            break;
        }
    }
    let start = found?;
    for x in start..start + n {
        free.remove(&x);
    }
    Some(start)
}

fn empty_journal() -> Journal {
    Journal { epoch: 0, seq: 0, pos: 0, seg_end: 0, segments: Vec::new(), blocks: 0 }
}

/// The state the journal describes.
async fn replay(dev: &Arc<dyn BlockDevice>, sb: Superblock, frees: &Arc<Frees>) -> Result<State, BlobError> {
    let slot = sb.slot_size;
    let seg_bytes = sb.segment_slots * slot;
    let mut index: BTreeMap<Digest, Layout> = BTreeMap::new();
    let mut sessions: HashMap<Uuid, (u64, Vec<u64>, u64)> = HashMap::new();
    let mut j = Journal { epoch: sb.epoch, seq: 0, pos: sb.head * slot, seg_end: sb.head * slot + seg_bytes, segments: vec![sb.head], blocks: 0 };
    let cap = dev.capacity_bytes();
    loop {
        if j.pos + BLOCK > j.seg_end || j.pos + BLOCK > cap {
            break;
        }
        let mut first = vec![0u8; BLOCK as usize];
        dev.read(j.pos, &mut first).await.map_err(|e| io("reading the journal", e))?;
        let Some(h) = format::decode_batch_header(&first, &sb.store, j.epoch, j.seq) else { break };
        let len = h.nblocks as u64 * BLOCK;
        if j.pos + len > j.seg_end {
            break;
        }
        let whole = if h.nblocks == 1 {
            first
        } else {
            let mut b = vec![0u8; len as usize];
            dev.read(j.pos, &mut b).await.map_err(|e| io("reading the journal", e))?;
            b
        };
        let Some(records) = format::decode_batch_payload(&whole, &h) else { break };
        j.seq += 1;
        j.blocks += h.nblocks as u64;
        let mut next = None;
        for r in records {
            match r {
                Record::Put { digest, layout } => {
                    index.insert(digest, layout);
                }
                Record::Delete { digest } => {
                    index.remove(&digest);
                }
                Record::SessionOpen { id, at } => {
                    sessions.insert(id, (0, Vec::new(), at));
                }
                Record::SessionAppend { id, len, at, slots } => {
                    if let Some(s) = sessions.get_mut(&id) {
                        s.0 = len;
                        s.1.extend(slots);
                        s.2 = at;
                    }
                }
                Record::SessionEnd { id } => {
                    sessions.remove(&id);
                }
                Record::Next { slot: s } => next = Some(s),
            }
        }
        match next {
            Some(s) => {
                j.pos = s * slot;
                j.seg_end = j.pos + seg_bytes;
                j.segments.push(s);
            }
            None => j.pos += len,
        }
    }
    let mut packs: HashMap<u64, Pack> = HashMap::new();
    let mut logical = 0u64;
    let mut idx = BTreeMap::new();
    for (d, l) in index {
        if let Some((off, len)) = l.tail {
            packs.entry(off / slot).or_insert(Pack { live: 0, open: false }).live += blocks_of(len as u64);
        }
        logical += l.size;
        idx.insert(d, Placement::new(l, frees));
    }
    let sess: HashMap<Uuid, SessionMeta> =
        sessions.into_iter().map(|(id, (len, slots, at))| (id, SessionMeta::new(len, slots, at))).collect();
    let mut st = State {
        sb,
        index: idx,
        sessions: sess,
        high_water: 1,
        free: BTreeSet::new(),
        packs,
        open_pack: None,
        journal: j,
        logical_bytes: logical,
    };
    let used = st.used_slots();
    st.high_water = used.iter().max().map(|m| m + 1).unwrap_or(1);
    st.free = (1..st.high_water).filter(|s| !used.contains(s)).collect();
    tracing::info!(
        "blob store {}: epoch {}, {} journal batch(es); {} blob(s), {} byte(s), {} session(s), {} pack(s)",
        sb.store,
        st.journal.epoch,
        st.journal.seq,
        st.index.len(),
        st.logical_bytes,
        st.sessions.len(),
        st.packs.len()
    );
    Ok(st)
}

/// Read `out.len()` bytes at device offset `abs`, whatever their alignment.
async fn read_abs(dev: &Arc<dyn BlockDevice>, abs: u64, out: &mut [u8]) -> Result<(), BlobError> {
    let start = abs / BLOCK * BLOCK;
    let end = (abs + out.len() as u64).div_ceil(BLOCK) * BLOCK;
    if start == abs && end == abs + out.len() as u64 {
        dev.read(abs, out).await.map_err(|e| io("reading a blob", e))?;
        return Ok(());
    }
    let mut tmp = vec![0u8; (end - start) as usize];
    dev.read(start, &mut tmp).await.map_err(|e| io("reading a blob", e))?;
    let from = (abs - start) as usize;
    out.copy_from_slice(&tmp[from..from + out.len()]);
    Ok(())
}

/// Bytes at a logical offset of whole slots in order.
async fn read_slots(dev: &Arc<dyn BlockDevice>, slot: u64, slots: &[u64], mut off: u64, mut out: &mut [u8]) -> Result<(), BlobError> {
    while !out.is_empty() {
        let k = (off / slot) as usize;
        let Some(s) = slots.get(k) else {
            return Err(BlobError::Io(format!("offset {off} is past the slots ({})", slots.len())));
        };
        let within = off % slot;
        let n = ((slot - within) as usize).min(out.len());
        let (a, b) = out.split_at_mut(n);
        read_abs(dev, s * slot + within, a).await?;
        out = b;
        off += n as u64;
    }
    Ok(())
}

async fn read_layout(dev: &Arc<dyn BlockDevice>, slot: u64, l: &Layout, off: u64, out: &mut [u8]) -> Result<(), BlobError> {
    if off + out.len() as u64 > l.size {
        return Err(BlobError::Invalid(format!("bytes {off}+{} are past the blob's {}", out.len(), l.size)));
    }
    let body = l.slots.len() as u64 * slot;
    let in_body = if off < body { ((body - off) as usize).min(out.len()) } else { 0 };
    let (a, b) = out.split_at_mut(in_body);
    if !a.is_empty() {
        read_slots(dev, slot, &l.slots, off, a).await?;
    }
    if !b.is_empty() {
        let (toff, tlen) = l.tail.ok_or_else(|| BlobError::Io("bytes past the blob's slots and no tail".into()))?;
        let within = off + in_body as u64 - body;
        if within + b.len() as u64 > tlen as u64 {
            return Err(BlobError::Io("bytes past the blob's tail".into()));
        }
        read_abs(dev, toff + within, b).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
