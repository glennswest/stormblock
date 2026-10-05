//! Persisting to format v2 metadata stores (#158, #157): `docs/metadata-v2.md`.
//!
//! A *sink* is one store: a v2 metadata slab's region, or `metadata.v2` in
//! the data directory. A persist hands each sink what changed since the last
//! one: the GEM's [`Changes`](super::gem::Changes), the headers whose bytes
//! moved, the volumes that came onto or left that sink. One log record and
//! one flush per sink, where v1 wrote every volume's whole record.
//!
//! A sink is written whole (one checkpoint) when what it holds is not known:
//! its first persist after a start (restore rebuilds the maps from the slot
//! tables, so they may differ from what any record said), after a write to
//! it failed, and when the GEM was not recording changes.
//!
//! **Order.** Records are taken in generation order (under the manager) and
//! written with no lock held (#269), so two persists may reach the device in
//! either order. A change list is not a snapshot: an older one written after
//! a newer one would put old values back. Batches are applied in the order
//! they were taken ([`Ticket`]), and a batch taken before a failure that
//! made its sink be written whole is dropped: the whole write covers it.

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
/// Its size: sparse, so only what is written is stored.
const DIR_SIZE: u64 = 16 << 30;

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
                let d = crate::drive::filedev::FileDevice::open_with_capacity(&p, DIR_SIZE)
                    .await
                    .map_err(|e| std::io::Error::other(format!("{p}: {e}")))?;
                (Arc::new(d) as Arc<dyn BlockDevice>, 0, DIR_SIZE)
            }
        };
        match MetaV2::open(dev.clone(), base, size).await? {
            Some(s) => Ok(s),
            None => MetaV2::format(dev, base, size).await,
        }
    }
}

pub(super) struct SinkState {
    store: Arc<tokio::sync::Mutex<Option<MetaV2>>>,
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
        SinkState { store: Default::default(), opener: None, held: HashMap::new(), doc: None, need_full: true, epoch: 0 }
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
}

impl V2State {
    /// Every sink is written whole at its next persist.
    pub(super) fn forget(&mut self) {
        for s in self.sinks.values_mut() {
            s.need_full = true;
        }
        self.vol_slabs.clear();
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
    pub(super) fn rewrites(&self, sinks: &[Sink]) -> bool {
        sinks.len() != self.sinks.len()
            || sinks.iter().any(|s| self.sinks.get(s).is_none_or(|x| x.need_full))
    }

    /// Whether every record taken has been written, and every store holds
    /// what it was last given: the state in which a map may leave memory.
    pub(super) fn quiet(&self) -> bool {
        let next = self.order.state.lock().unwrap_or_else(|e| e.into_inner()).0;
        next == self.next_ticket && self.sinks.values().all(|s| !s.need_full)
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
pub(super) fn take(state: &Arc<std::sync::Mutex<V2State>>, input: Inputs<'_>) -> Option<V2Records> {
    if input.sinks.is_empty() {
        return None;
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut failed = Vec::new();
    for b in batches {
        if let Err(e) = apply_one(&b, &records.state).await {
            let mut st = records.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = st.sinks.get_mut(&b.sink) {
                s.need_full = true;
                s.epoch += 1;
            }
            failed.push(match b.sink {
                Sink::Slab(id) => format!("slab {} (metadata v2): {e}", id.0),
                Sink::Dir => format!("{DIR_FILE}: {e}"),
            });
        }
    }
    drop(ticket);
    failed
}

async fn apply_one(b: &Batch, state: &Arc<std::sync::Mutex<V2State>>) -> std::io::Result<()> {
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
            store.replace_all(entries.clone()).await
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
