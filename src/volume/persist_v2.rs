//! Persisting to format v2 metadata stores (#158, #157): `docs/metadata-v2.md`.
//!
//! A *sink* is one store: a v2 metadata slab's region, or `metadata.v2` in
//! the data directory. A persist hands each sink what changed since the last
//! one: the GEM's [`Changes`](super::gem::Changes), the headers whose bytes
//! moved, the volumes that came onto or left that sink. One log record and
//! one flush per sink, where v1 wrote every volume's whole record.
//!
//! A sink is brought up to date whole (one checkpoint) when what it holds is
//! not known:
//! its first persist after a start (restore rebuilds the maps from the slot
//! tables, so they may differ from what any record said), after a write to
//! it failed, and when the GEM was not recording changes. "Whole" is what is
//! compared, not what is written: the store's tree is read and only the
//! entries that differ are written (#546), so it never needs room for a
//! second copy of itself.
//!
//! **Order.** Records are taken in generation order (under the manager) and
//! written with no lock held (#269), so two persists may reach the device in
//! either order. A change list is not a snapshot: an older one written after
//! a newer one would put old values back. Batches are applied in the order
//! they were taken ([`Ticket`]), and a batch taken before a failure that
//! made its sink be written whole is dropped: the whole write covers it.
//!
//! **Group commit (#521).** A record is durable once it and every record
//! before it are flushed, and a flush is what costs seconds on a drive-managed
//! SMR disk. Eight clones at once took eight log flushes, one after the other.
//! Now a batch joins its sink's queue in its turn, and whoever gets to write
//! next writes everything queued as one record with one flush: never two
//! separate records with only the later one on the disk, which the log's replay
//! could read back after a second cut as if it were new.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use super::extent::VolumeId;
use super::metadata::{ArrayRecord, VolumeRecord};
use super::metav2::{self, Key, MetaV2, Op};
use crate::drive::slab::SlabId;
use crate::drive::BlockDevice;

/// The data directory's store.
pub const DIR_FILE: &str = "metadata.v2";
/// Its first size. It doubles when it fills ([`grow_dir`]): stormcos copies
/// the data directory into its state volume every ten seconds, a whole file
/// at a time, so the file stays in proportion to what it holds.
const DIR_INITIAL: u64 = 8 << 20;

/// [`DIR_INITIAL`], or `$STORMBLOCK_METADATA_DIR_INITIAL` bytes (tests: a
/// small file that grows).
fn dir_initial() -> u64 {
    std::env::var("STORMBLOCK_METADATA_DIR_INITIAL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| (v / metav2::PAGE * metav2::PAGE).max(64 * metav2::PAGE))
        .unwrap_or(DIR_INITIAL)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Sink {
    Slab(SlabId),
    Dir,
}

/// How to open a sink's store.
#[derive(Clone)]
pub(super) enum Opener {
    Region(Arc<dyn BlockDevice>, u64, u64),
    Dir(PathBuf),
}

impl Opener {
    async fn open(&self) -> std::io::Result<MetaV2> {
        let (dev, base, size) = match self {
            Opener::Region(d, b, s) => (d.clone(), *b, *s),
            Opener::Dir(path) => {
                let p = path.to_string_lossy().to_string();
                let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let size = if len >= dir_initial() { len / metav2::PAGE * metav2::PAGE } else { dir_initial() };
                let d = crate::drive::filedev::FileDevice::open_with_capacity(&p, size)
                    .await
                    .map_err(|e| std::io::Error::other(format!("{p}: {e}")))?;
                (Arc::new(d) as Arc<dyn BlockDevice>, 0, size)
            }
        };
        match MetaV2::open(dev.clone(), base, size).await? {
            Some(s) => Ok(s),
            None => MetaV2::format(dev, base, size).await,
        }
    }
}

/// A sink's changes waiting to be written (#521).
#[derive(Default)]
struct Commit {
    queue: std::sync::Mutex<Vec<Queued>>,
    /// Held by whoever is writing: those behind it are queueing their changes.
    running: tokio::sync::Mutex<()>,
}

impl Commit {
    /// Nothing waiting and nothing being written.
    fn idle(&self) -> bool {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty() && self.running.try_lock().is_ok()
    }
}

struct Queued {
    epoch: u64,
    ops: Vec<Op>,
    done: Done,
}

/// How a queued batch ended, set by whoever wrote it.
type Done = Arc<std::sync::OnceLock<Result<(), String>>>;

pub(super) struct SinkState {
    store: Arc<tokio::sync::Mutex<Option<MetaV2>>>,
    commit: Arc<Commit>,
    /// How to open the store, for a load before the first persist opened it.
    opener: Option<Opener>,
    /// The volumes the store holds: their header's hash and chunk count.
    held: HashMap<VolumeId, (u64, u64)>,
    /// The hash of the document entry it holds.
    doc: Option<u64>,
    need_full: bool,
    /// Bumped on every failed write: a batch taken before it is dropped.
    epoch: u64,
}

impl SinkState {
    fn new() -> SinkState {
        SinkState { store: Default::default(), commit: Default::default(), opener: None, held: HashMap::new(), doc: None, need_full: true, epoch: 0 }
    }
}

#[derive(Default)]
pub(super) struct V2State {
    pub(super) sinks: HashMap<Sink, SinkState>,
    /// The slabs each volume has a leg on, kept between persists and worked
    /// out again only for the volumes that changed.
    vol_slabs: HashMap<VolumeId, HashSet<SlabId>>,
    next_ticket: u64,
    order: Arc<Order>,
    /// Slabs whose own region filled while `metadata.v2` took the record
    /// (#583), each with how many volumes the node had then. The data
    /// directory's copy is the one restore reads, so a full region is a
    /// stale fallback, not a record that cannot be written. Tried again,
    /// whole, once the node holds a fifth fewer volumes (#587), and at a
    /// restart.
    stale: HashMap<SlabId, usize>,
    /// How many volumes the last persist was given.
    last_total: usize,
}

impl V2State {
    /// Every sink is written whole at its next persist.
    pub(super) fn forget(&mut self) {
        for s in self.sinks.values_mut() {
            s.need_full = true;
        }
        self.vol_slabs.clear();
    }

    /// Whether a slab's own copy stopped being written because its region
    /// is full and the data directory holds the record (#583).
    pub(super) fn is_stale(&self, id: &SlabId) -> bool {
        self.stale.contains_key(id)
    }

    /// The stale slabs a persist of `total` volumes into `sinks` leaves out
    /// (#583): all of them while the data directory takes the record, but
    /// one whose node has lost a fifth of its volumes since is tried again
    /// (#587). A try writes the slab whole, which reads every map, so it is
    /// not made at every persist: a failed try sets a new baseline.
    fn left_out(&self, sinks: &[Sink], total: usize) -> HashSet<SlabId> {
        if !sinks.contains(&Sink::Dir) {
            // With no data directory a slab's copy is the record: always tried.
            return HashSet::new();
        }
        self.stale.iter().filter(|(_, then)| total * 5 > **then * 4).map(|(id, _)| *id).collect()
    }

    /// The store a sink has open, for the pressure report.
    pub(super) fn usage(&self, sink: Sink) -> Option<metav2::Usage> {
        let s = self.sinks.get(&sink)?;
        let g = s.store.try_lock().ok()?;
        g.as_ref().map(|m| m.usage())
    }
}

impl V2State {
    /// Whether a persist with these sinks writes a store whole (which reads
    /// every map it carries) or drops one (whose maps only it may hold).
    ///
    /// A stale slab left out counts as neither (#587): with it counted, a
    /// node with one read every map at every persist.
    pub(super) fn rewrites(&self, sinks: &[Sink], total: usize) -> bool {
        let out = self.left_out(sinks, total);
        let sinks: Vec<&Sink> = sinks.iter().filter(|s| !matches!(s, Sink::Slab(id) if out.contains(id))).collect();
        sinks.len() != self.sinks.len()
            || sinks.iter().any(|s| self.sinks.get(s).is_none_or(|x| x.need_full))
    }

    /// Whether every record taken has been written, and every store holds
    /// what it was last given: the state in which a map may leave memory.
    pub(super) fn quiet(&self) -> bool {
        let next = self.order.state.lock().unwrap_or_else(|e| e.into_inner()).0;
        // Queued is not yet in the store, and a map that left memory is read
        // back from the store.
        next == self.next_ticket && self.sinks.values().all(|s| !s.need_full && s.commit.idle())
    }

    /// How many volumes a sink's store holds.
    pub(super) fn held_count(&self, sink: Sink) -> usize {
        self.sinks.get(&sink).map(|s| s.held.len()).unwrap_or(0)
    }

    /// Whether some store holds `id`.
    pub(super) fn held(&self, id: &VolumeId) -> bool {
        self.sinks.values().any(|s| s.held.contains_key(id))
    }

    /// The sinks whose stores hold every volume they carry.
    pub(super) fn sink_count(&self) -> usize {
        self.sinks.len()
    }
}

/// Loads a map that is not in memory from a store that holds it (#158
/// stage C). A map is taken out of memory only when every record taken has
/// been written ([`V2State::quiet`]) and nothing has changed in it since, so
/// any store that holds it holds its last state.
pub(super) struct StorePager {
    pub(super) state: Arc<std::sync::Mutex<V2State>>,
}

#[async_trait::async_trait]
impl super::gem::Pager for StorePager {
    async fn load(&self, id: VolumeId) -> std::io::Result<super::gem::VolumeExtentMap> {
        let candidates: Vec<(Arc<tokio::sync::Mutex<Option<MetaV2>>>, Option<Opener>)> = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut c: Vec<(bool, Arc<tokio::sync::Mutex<Option<MetaV2>>>, Option<Opener>)> = st
                .sinks
                .values()
                .filter(|s| s.held.contains_key(&id))
                .map(|s| (s.need_full, s.store.clone(), s.opener.clone()))
                .collect();
            // A store that took every write first.
            c.sort_by_key(|(full, _, _)| *full);
            c.into_iter().map(|(_, s, o)| (s, o)).collect()
        };
        let mut last = std::io::Error::other(format!("volume {}: no metadata store holds its map", id.0));
        for (store, opener) in candidates {
            let mut g = store.lock().await;
            if g.is_none() {
                match opener {
                    Some(o) => match o.open().await {
                        Ok(s) => *g = Some(s),
                        Err(e) => {
                            last = e;
                            continue;
                        }
                    },
                    None => continue,
                }
            }
            match g.as_mut().unwrap().scan_volume(*id.0.as_bytes()).await.and_then(metav2::map_of) {
                Ok(Some(map)) => return Ok(map),
                Ok(None) => last = std::io::Error::other(format!("volume {}: not in this store", id.0)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

#[derive(Default)]
struct Order {
    /// The next ticket to run, and the tickets finished out of turn.
    state: std::sync::Mutex<(u64, BTreeSet<u64>)>,
    notify: tokio::sync::Notify,
}

/// A place in the order batches are applied in. Dropped (run or not), it
/// lets the next one go.
pub(super) struct Ticket {
    n: u64,
    order: Arc<Order>,
}

impl Ticket {
    async fn turn(&self) {
        loop {
            let notified = self.order.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.order.state.lock().unwrap_or_else(|e| e.into_inner()).0 == self.n {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut st = self.order.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.0 == self.n {
            st.0 += 1;
            loop {
                let next = st.0;
                if !st.1.remove(&next) {
                    break;
                }
                st.0 += 1;
            }
        } else {
            st.1.insert(self.n);
        }
        drop(st);
        self.order.notify.notify_waiters();
    }
}

pub(super) struct Batch {
    sink: Sink,
    store: Arc<tokio::sync::Mutex<Option<MetaV2>>>,
    commit: Arc<Commit>,
    opener: Opener,
    epoch: u64,
    /// The whole store, sorted; and how many volumes it names.
    full: Option<(Vec<(Key, Vec<u8>)>, usize)>,
    ops: Vec<Op>,
}

pub(super) struct V2Records {
    /// Taken by [`apply`]; still here when the persist stopped before it.
    inner: Option<(Vec<Batch>, Ticket)>,
    state: Arc<std::sync::Mutex<V2State>>,
}

impl Drop for V2Records {
    /// A persist that stopped before writing (a slab flush failed): the
    /// changes taken for it are written by no one, so each of its sinks is
    /// written whole next time, and nothing taken after it is applied in its
    /// place. Marked before the ticket goes, so the next batch sees it.
    fn drop(&mut self) {
        if let Some((batches, ticket)) = self.inner.take() {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            for b in &batches {
                if let Some(s) = st.sinks.get_mut(&b.sink) {
                    s.need_full = true;
                    s.epoch += 1;
                }
            }
            drop(st);
            drop(ticket);
        }
    }
}

fn hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// What one persist hands the v2 sinks, worked out under the manager.
pub(super) struct Inputs<'a> {
    pub sinks: Vec<(Sink, Opener)>,
    pub headers: Vec<VolumeRecord>,
    /// The arrays each sink records.
    pub arrays: HashMap<Sink, Vec<ArrayRecord>>,
    pub extent_size: u64,
    pub gem: &'a super::gem::GlobalExtentMap,
    pub changes: super::gem::Changes,
    /// Which volumes each sink carries, given each volume's slabs.
    pub carries: &'a dyn Fn(Sink, &VolumeId, &HashSet<SlabId>) -> bool,
}

/// Work out each sink's batch. Called with the GEM read-locked, so the
/// changes taken and the values read are one state.
pub(super) fn take(state: &Arc<std::sync::Mutex<V2State>>, mut input: Inputs<'_>) -> Option<V2Records> {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    // A stale slab copy is left out while the data directory takes the
    // record (#583), until it is due a try (#587); with no data directory it
    // is the record, and is tried.
    let names: Vec<Sink> = input.sinks.iter().map(|(s, _)| *s).collect();
    let total = input.headers.len();
    let out = st.left_out(&names, total);
    input.sinks.retain(|(s, _)| !matches!(s, Sink::Slab(id) if out.contains(id)));
    st.last_total = total;
    for (s, _) in &input.sinks {
        if let Sink::Slab(id) = s {
            if let Some(then) = st.stale.get(id) {
                tracing::info!(
                    "slab {}: trying its own copy of the volume records again ({total} volumes, {then} when it \
                     filled; #587)",
                    id.0
                );
            }
        }
    }
    if input.sinks.is_empty() {
        return None;
    }
    let configured: HashSet<Sink> = input.sinks.iter().map(|(s, _)| *s).collect();
    st.sinks.retain(|s, _| configured.contains(s));
    for (s, o) in &input.sinks {
        st.sinks.entry(*s).or_insert_with(SinkState::new).opener = Some(o.clone());
    }

    // Each volume's slabs: again for what changed or is new.
    let changed = input.changes.volumes();
    let live: HashSet<VolumeId> = input.headers.iter().map(|h| h.id).collect();
    st.vol_slabs.retain(|id, _| live.contains(id));
    for id in &live {
        if changed.contains(id) || !st.vol_slabs.contains_key(id) {
            let slabs = match input.gem.cold(id) {
                Some(c) => c.slabs.iter().copied().collect(),
                None => input
                    .gem
                    .get_volume_map(id)
                    .map(|m| m.all_legs().map(|l| l.slab_id).collect())
                    .unwrap_or_default(),
            };
            st.vol_slabs.insert(*id, slabs);
        }
    }
    let empty = HashSet::new();
    let headers: Vec<(&VolumeRecord, Vec<u8>)> =
        input.headers.iter().map(|h| (h, metav2::header_bytes(h, input.extent_size))).collect();

    let order = st.order.clone();
    let ticket = Ticket { n: st.next_ticket, order };
    st.next_ticket += 1;

    let mut batches = Vec::new();
    let vol_slabs = std::mem::take(&mut st.vol_slabs);
    for (sink, opener) in &input.sinks {
        let s = st.sinks.get_mut(sink).expect("registered above");
        let members: Vec<&(&VolumeRecord, Vec<u8>)> = headers
            .iter()
            .filter(|(h, _)| (input.carries)(*sink, &h.id, vol_slabs.get(&h.id).unwrap_or(&empty)))
            .collect();
        let arrays = input.arrays.get(sink).cloned().unwrap_or_default();
        let (doc_key, doc_val) = metav2::doc_entry(input.extent_size, &arrays);
        let doc_hash = hash(&doc_val);

        let all_of = |h: &VolumeRecord, bytes: &[u8]| -> Vec<(Key, Vec<u8>)> {
            let mut out = metav2::header_entries(h.id, bytes);
            if let Some(m) = input.gem.get_volume_map(&h.id) {
                out.extend(m.extents.iter().map(|(v, l)| metav2::extent_entry(h.id, v, &l)));
                out.extend(m.parity.iter().map(|(st, g)| metav2::parity_entry(h.id, *st, g)));
            }
            out
        };

        if s.need_full {
            let mut entries = vec![(doc_key, doc_val)];
            let mut held = HashMap::new();
            for (h, bytes) in &members {
                entries.extend(all_of(h, bytes));
                held.insert(h.id, (hash(bytes), metav2::header_chunks(bytes.len())));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            s.need_full = false;
            s.held = held;
            s.doc = Some(doc_hash);
            batches.push(Batch {
                sink: *sink,
                store: s.store.clone(),
                commit: s.commit.clone(),
                opener: opener.clone(),
                epoch: s.epoch,
                full: Some((entries, members.len())),
                ops: Vec::new(),
            });
            continue;
        }

        let mut ops = Vec::new();
        if s.doc != Some(doc_hash) {
            ops.push(Op::Put(doc_key, doc_val));
            s.doc = Some(doc_hash);
        }
        let now: HashSet<VolumeId> = members.iter().map(|(h, _)| h.id).collect();
        let gone: Vec<VolumeId> = s.held.keys().filter(|id| !now.contains(id)).copied().collect();
        for id in gone {
            ops.push(Op::DropVolume(*id.0.as_bytes()));
            s.held.remove(&id);
        }
        for (h, bytes) in &members {
            let hh = hash(bytes);
            let chunks = metav2::header_chunks(bytes.len());
            let whole = input.changes.whole.contains(&h.id);
            match s.held.get(&h.id).copied() {
                Some(_) if !whole => {
                    let (old_hash, old_chunks) = s.held[&h.id];
                    if old_hash != hh {
                        for (k, v) in metav2::header_entries(h.id, bytes) {
                            ops.push(Op::Put(k, v));
                        }
                        for i in chunks..old_chunks {
                            ops.push(Op::Del(Key::new(*h.id.0.as_bytes(), metav2::kind::HEADER, i)));
                        }
                    }
                    if let Some(vexts) = input.changes.extents.get(&h.id) {
                        for v in vexts {
                            match input.gem.lookup(h.id, *v) {
                                Some(l) => {
                                    let (k, val) = metav2::extent_entry(h.id, *v, &l);
                                    ops.push(Op::Put(k, val));
                                }
                                None => ops.push(Op::Del(Key::new(*h.id.0.as_bytes(), metav2::kind::EXTENT, *v))),
                            }
                        }
                    }
                    if let Some(stripes) = input.changes.parity.get(&h.id) {
                        for stripe in stripes {
                            match input.gem.lookup_parity(h.id, *stripe) {
                                Some(g) => {
                                    let (k, val) = metav2::parity_entry(h.id, *stripe, g);
                                    ops.push(Op::Put(k, val));
                                }
                                None => {
                                    ops.push(Op::Del(Key::new(*h.id.0.as_bytes(), metav2::kind::PARITY, *stripe)))
                                }
                            }
                        }
                    }
                }
                _ => {
                    // New to this sink, or changed as a whole: written again
                    // entirely, after anything it held.
                    ops.push(Op::DropVolume(*h.id.0.as_bytes()));
                    ops.extend(all_of(h, bytes).into_iter().map(|(k, v)| Op::Put(k, v)));
                }
            }
            s.held.insert(h.id, (hh, chunks));
        }
        if ops.is_empty() {
            continue;
        }
        batches.push(Batch {
            sink: *sink,
            store: s.store.clone(),
            commit: s.commit.clone(),
            opener: opener.clone(),
            epoch: s.epoch,
            full: None,
            ops,
        });
    }
    st.vol_slabs = vol_slabs;
    drop(st);
    Some(V2Records { inner: Some((batches, ticket)), state: state.clone() })
}

/// Write the batches, in the order they were taken. What failed, by sink.
pub(super) async fn apply(mut records: V2Records) -> Vec<String> {
    let Some((batches, ticket)) = records.inner.take() else { return Vec::new() };
    ticket.turn().await;
    let state = &records.state;
    let mut failed = Vec::new();
    let mut waiting = Vec::new();
    let tried: Vec<Sink> = batches.iter().map(|b| b.sink).collect();
    for mut b in batches {
        if b.full.is_some() {
            // In its turn, with what was queued before it written first.
            write_queued(&b, state).await;
            if let Err(e) = apply_one(&b, state).await {
                failed.push((b.sink, sink_failed(&b, state, e.to_string())));
            }
        } else {
            let done = Done::default();
            let queued = Queued { epoch: b.epoch, ops: std::mem::take(&mut b.ops), done: done.clone() };
            b.commit.queue.lock().unwrap_or_else(|e| e.into_inner()).push(queued);
            waiting.push((b, done));
        }
    }
    // The order is settled: what comes next queues behind these, and shares
    // the write that is waiting for this one's.
    drop(ticket);
    let ended = futures_util::future::join_all(waiting.iter().map(|(b, done)| async move {
        if done.get().is_none() {
            write_queued(b, state).await;
        }
        done.get().cloned().unwrap_or_else(|| Err("not written".to_string()))
    }))
    .await;
    for ((b, _), end) in waiting.iter().zip(ended) {
        if let Err(e) = end {
            failed.push((b.sink, sink_failed(b, state, e)));
        }
    }
    excuse_full_regions(state, &tried, failed)
}

/// A slab region that is full is not a record that cannot be written when
/// `metadata.v2` took this write (#583): restore reads the data directory
/// first, and its file grows. Such a slab is marked stale and left out until
/// a restart, and only the other failures are reported. Forge, 2026-11-11:
/// its hot slab's region at 96 % refused every create and every delete for
/// a day, with the data directory's copy written all along.
///
/// A stale slab written whole again (#587) is current from then on.
fn excuse_full_regions(
    state: &Arc<std::sync::Mutex<V2State>>,
    tried: &[Sink],
    failed: Vec<(Sink, String)>,
) -> Vec<String> {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    for sink in tried {
        if let Sink::Slab(id) = sink {
            if !failed.iter().any(|(s, _)| s == sink) && st.stale.remove(id).is_some() {
                tracing::info!(
                    "slab {}: its own copy of the volume records is current again: written whole with \
                     what {DIR_FILE} holds (#587)",
                    id.0
                );
            }
        }
    }
    let dir_written = st.sinks.contains_key(&Sink::Dir) && !failed.iter().any(|(s, _)| *s == Sink::Dir);
    let total = st.last_total;
    let mut out = Vec::new();
    for (sink, e) in failed {
        match sink {
            Sink::Slab(id) if dir_written && e.contains("metadata region full") => {
                match st.stale.insert(id, total) {
                    None => tracing::warn!(
                        "slab {}: its own copy of the volume records is stale from now on ({e}); \
                         {DIR_FILE} holds the record and restore reads it first (#583). It is tried \
                         again once the node holds a fifth fewer than {total} volumes (#587)",
                        id.0
                    ),
                    Some(_) => tracing::info!(
                        "slab {}: still too full for the record ({e}); tried again once the node holds \
                         a fifth fewer than {total} volumes (#587)",
                        id.0
                    ),
                }
                st.sinks.remove(&sink);
            }
            _ => out.push(e),
        }
    }
    out
}

/// The sink is written whole next time; what to report.
fn sink_failed(b: &Batch, state: &Arc<std::sync::Mutex<V2State>>, e: String) -> String {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = st.sinks.get_mut(&b.sink) {
        s.need_full = true;
        s.epoch += 1;
    }
    match b.sink {
        Sink::Slab(id) => format!("slab {} (metadata v2): {e}", id.0),
        Sink::Dir => format!("{DIR_FILE}: {e}"),
    }
}

/// Write everything queued on `b`'s sink as one record, once whoever is
/// writing now has finished.
async fn write_queued(b: &Batch, state: &Arc<std::sync::Mutex<V2State>>) {
    let _writing = b.commit.running.lock().await;
    let queued = std::mem::take(&mut *b.commit.queue.lock().unwrap_or_else(|e| e.into_inner()));
    if queued.is_empty() {
        return;
    }
    let epoch = state.lock().unwrap_or_else(|e| e.into_inner()).sinks.get(&b.sink).map(|s| s.epoch);
    let mut ops = Vec::new();
    let mut in_record = Vec::new();
    for q in queued {
        if Some(q.epoch) == epoch {
            ops.extend(q.ops);
            in_record.push(q.done);
        } else {
            // Taken before a failed write; the whole write that follows it
            // covers this.
            let _ = q.done.set(Ok(()));
        }
    }
    if in_record.is_empty() {
        return;
    }
    let record = Batch {
        sink: b.sink,
        store: b.store.clone(),
        commit: b.commit.clone(),
        opener: b.opener.clone(),
        epoch: epoch.unwrap_or(b.epoch),
        full: None,
        ops,
    };
    let end = apply_one(&record, state).await.map_err(|e| e.to_string());
    for done in in_record {
        let _ = done.set(end.clone());
    }
}

async fn apply_one(b: &Batch, state: &Arc<std::sync::Mutex<V2State>>) -> std::io::Result<()> {
    let r = match apply_store(b, state).await {
        Err(e) if b.sink == Sink::Dir && e.to_string().contains("metadata region full") => {
            let Opener::Dir(path) = &b.opener else { return Err(e) };
            grow_dir(b, path).await
        }
        r => r,
    };
    if r.is_ok() && b.full.is_some() {
        if let Opener::Dir(path) = &b.opener {
            retire_v1_record(path);
        }
    }
    r
}

/// Once `metadata.v2` holds the record, the v1 one beside it is set aside
/// (`volumes.dat.pre-v2`): left as it was, an older engine after a rollback
/// would trust a record that stopped being written; set aside, it finds none
/// and says so.
fn retire_v1_record(path: &std::path::Path) {
    let Some(dir) = path.parent() else { return };
    for name in ["volumes.dat", "volumes.dat.bak"] {
        let p = dir.join(name);
        if p.exists() {
            let to = dir.join(format!("{name}.pre-v2"));
            match std::fs::rename(&p, &to) {
                Ok(()) => tracing::info!("{} set aside as {} (the record is {DIR_FILE} now)", p.display(), to.display()),
                Err(e) => tracing::warn!("setting {} aside: {e}", p.display()),
            }
        }
    }
}

/// `metadata.v2` is full: write what it holds and this batch into a file of
/// twice the size beside it, flush it, and rename it over the old one.
async fn grow_dir(b: &Batch, path: &std::path::Path) -> std::io::Result<()> {
    let mut g = b.store.lock().await;
    let Some(old) = g.as_mut() else { return Err(std::io::Error::other("metadata.v2: not open")) };
    let mut all: std::collections::BTreeMap<Key, Vec<u8>> = match &b.full {
        Some((entries, _)) => entries.iter().cloned().collect(),
        None => old.scan().await?.into_iter().collect(),
    };
    if b.full.is_none() {
        for op in &b.ops {
            match op {
                Op::Put(k, v) => {
                    all.insert(*k, v.clone());
                }
                Op::Del(k) => {
                    all.remove(k);
                }
                Op::DropVolume(v) => all.retain(|k, _| k.vol() != *v),
            }
        }
    }
    let bytes: u64 = all.iter().map(|(_, v)| v.len() as u64 + 32).sum();
    let mut size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0).max(dir_initial()) * 2;
    while size < bytes * 4 {
        size *= 2;
    }
    let tmp = path.with_file_name(format!(".{DIR_FILE}.grow"));
    let _ = std::fs::remove_file(&tmp);
    let t = tmp.to_string_lossy().to_string();
    let d: Arc<dyn BlockDevice> = Arc::new(
        crate::drive::filedev::FileDevice::open_with_capacity(&t, size)
            .await
            .map_err(|e| std::io::Error::other(format!("{t}: {e}")))?,
    );
    let mut fresh = MetaV2::format(d, 0, size).await?;
    fresh.replace_all(all.into_iter().collect()).await?;
    drop(fresh);
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
    *g = Some(Opener::Dir(path.to_path_buf()).open().await?);
    tracing::info!("{DIR_FILE} grown to {} MiB", size >> 20);
    Ok(())
}

async fn apply_store(b: &Batch, state: &Arc<std::sync::Mutex<V2State>>) -> std::io::Result<()> {
    let mut g = b.store.lock().await;
    if g.is_none() {
        *g = Some(b.opener.open().await?);
        if b.full.is_none() {
            return Err(std::io::Error::other("the store was not open: it is written whole at the next persist"));
        }
    }
    let store = g.as_mut().unwrap();
    let epoch = state.lock().unwrap_or_else(|e| e.into_inner()).sinks.get(&b.sink).map(|s| s.epoch);
    match &b.full {
        Some((entries, volumes)) => {
            if *volumes == 0 && b.sink == Sink::Dir {
                // Knowing about no volumes is not the same as there being
                // none (see `records`): never an empty record over one that
                // names volumes.
                let had = store.scan().await?.iter().filter(|(k, _)| k.kind() == metav2::kind::HEADER && k.idx() == 0).count();
                if had > 0 {
                    tracing::warn!(
                        "not overwriting {DIR_FILE} naming {had} volume(s) with an empty one — this manager \
                         has no volumes, which usually means its slabs are not attached"
                    );
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(s) = st.sinks.get_mut(&b.sink) {
                        s.need_full = true;
                    }
                    return Ok(());
                }
            }
            // Only what differs is written (#546): a whole second tree beside
            // the first needed room for both.
            let n = match store.reconcile(entries.clone()).await {
                Err(e) if matches!(b.sink, Sink::Slab(_)) && e.to_string().contains("metadata region full") => {
                    // Copy on write needs free pages to free pages (#587): let
                    // go of the volumes the store should no longer hold first,
                    // a few at a time, then try again.
                    if drop_in_steps(store, entries).await? == 0 {
                        return Err(e);
                    }
                    store.reconcile(entries.clone()).await?
                }
                r => r?,
            };
            if n > 0 {
                tracing::debug!("metadata v2: {n} change(s) to bring the store up to date");
            }
            Ok(())
        }
        None => {
            if epoch != Some(b.epoch) {
                // Taken before a failed write; the whole write that follows
                // it covers this.
                return Ok(());
            }
            store.append(b.ops.clone()).await
        }
    }
}

/// Drop from a full store, a few volumes per checkpoint, every volume it
/// holds that `entries` does not name (#587). One reconcile that drops them
/// all and adds what is new needs its new pages before the old ones are
/// free; each step here frees pages for the next. The steps halve when one
/// does not fit, down to one volume. How many volumes were dropped.
///
/// Every step is a record a restore could read after a cut: the store as
/// it was, less volumes that are gone from it anyway.
async fn drop_in_steps(store: &mut MetaV2, entries: &[(Key, Vec<u8>)]) -> std::io::Result<usize> {
    let wanted: HashSet<[u8; 16]> = entries.iter().map(|(k, _)| k.vol()).collect();
    let mut held = store.scan().await?;
    let extra: Vec<[u8; 16]> = held
        .iter()
        .map(|(k, _)| k.vol())
        .filter(|v| !wanted.contains(v))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut step = 64usize;
    let mut done = 0;
    while done < extra.len() {
        let these: HashSet<[u8; 16]> = extra[done..(done + step).min(extra.len())].iter().copied().collect();
        let target: Vec<(Key, Vec<u8>)> = held.iter().filter(|(k, _)| !these.contains(&k.vol())).cloned().collect();
        match store.reconcile(target.clone()).await {
            Ok(_) => {
                done += these.len();
                held = target;
            }
            Err(e) if step > 1 && e.to_string().contains("metadata region full") => step /= 2,
            Err(e) if done > 0 => {
                tracing::debug!("metadata v2: {done} volume(s) dropped before the region was full again: {e}");
                break;
            }
            Err(e) => return Err(e),
        }
    }
    if done > 0 {
        tracing::info!("metadata v2: {done} volume(s) the region no longer holds dropped first, in steps (#587)");
    }
    Ok(done)
}

/// The document `metadata.v2` in `dir` holds, if there is one.
pub(super) async fn load_dir(dir: &std::path::Path) -> std::io::Result<Option<super::metadata::VolumeMetadata>> {
    let path = dir.join(DIR_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let mut store = Opener::Dir(path).open().await?;
    let entries = store.scan().await?;
    metav2::document_of(entries).map(Some)
}
