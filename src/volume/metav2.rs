//! Metadata format v2 (#158, with #157): `docs/metadata-v2.md`.
//!
//! A metadata region (a slab's, or `metadata.v2` in the data directory) is:
//!
//! ```text
//! | superblock A | superblock B | log (ring)            | pages (4 KiB each)   |
//! ```
//!
//! * **Pages** hold one copy-on-write B-tree of `(key → value)`, keys being
//!   `(volume, kind, index)`: the document (extent size, arrays), each
//!   volume's header, its extents and its parity groups. A checkpoint writes
//!   new pages for what changed, flushes, then writes the other superblock
//!   naming the new root: a cut leaves the previous root whole.
//! * **The log** is where a persist goes: the changes since the last persist,
//!   one record, one flush (#157). O(changes), where v1 rewrote every
//!   volume's whole record into both copies of every metadata slab.
//! * **Recovery**: the newer superblock that checks, its tree, then the log
//!   records after its checkpoint in sequence, stopping at the first that
//!   fails its checksum (a torn tail) or is out of sequence (a record of an
//!   earlier lap of the ring).
//!
//! Pages freed by a checkpoint are reused only once the superblock that no
//! longer names them is durable, so the older superblock's tree stays whole
//! until the newer one is: the pair is what makes a torn superblock safe.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::BlockDevice;

pub const PAGE: u64 = 4096;
const SB_MAGIC: [u8; 8] = *b"SMV2SUPR";
const SB_VERSION: u32 = 1;
const LOG_MAGIC: [u8; 8] = *b"SMV2LOG\0";
const LOG_HEADER: usize = 40;
const PAGE_MAGIC: u32 = u32::from_le_bytes(*b"SMPG");
const PAGE_HEADER: usize = 16;
const LEAF: u8 = 1;
const INTERNAL: u8 = 2;
/// No tree: the store holds nothing.
const EMPTY: u64 = u64::MAX;
/// Bytes of one key: volume (16), kind (1), index big-endian (8).
pub const KEY_LEN: usize = 25;
/// The largest value one entry holds; a volume header is chunked to this.
pub const MAX_VALUE: usize = 1536;
/// A leaf entry: key, value length, value.
const LEAF_ENTRY: usize = KEY_LEN + 2;
/// An internal entry: separator key, child page.
const INTERNAL_ENTRY: usize = KEY_LEN + 8;
const MAX_CHILDREN: usize = 1 + (PAGE as usize - PAGE_HEADER - 8) / INTERNAL_ENTRY;
/// Overlay entries (logged, not yet in the tree) before a checkpoint.
const CHECKPOINT_ENTRIES: usize = 256 * 1024;

/// What a key names.
pub mod kind {
    /// The document: extent size and arrays. Volume nil, index 0.
    pub const DOC: u8 = 0;
    /// A volume's header (its record without extents and parity), in chunks.
    pub const HEADER: u8 = 1;
    pub const EXTENT: u8 = 2;
    pub const PARITY: u8 = 3;
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Key(pub [u8; KEY_LEN]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", uuid::Uuid::from_bytes(self.vol()), self.kind(), self.idx())
    }
}

impl Key {
    pub fn new(vol: [u8; 16], kind: u8, idx: u64) -> Key {
        let mut k = [0u8; KEY_LEN];
        k[..16].copy_from_slice(&vol);
        k[16] = kind;
        k[17..].copy_from_slice(&idx.to_be_bytes());
        Key(k)
    }
    pub fn vol(&self) -> [u8; 16] {
        self.0[..16].try_into().unwrap()
    }
    pub fn kind(&self) -> u8 {
        self.0[16]
    }
    pub fn idx(&self) -> u64 {
        u64::from_be_bytes(self.0[17..].try_into().unwrap())
    }
    fn first_of(vol: [u8; 16]) -> Key {
        Key::new(vol, 0, 0)
    }
    fn last_of(vol: [u8; 16]) -> Key {
        Key::new(vol, u8::MAX, u64::MAX)
    }
}

/// One change, as the log keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Put(Key, Vec<u8>),
    Del(Key),
    /// Every key of a volume.
    DropVolume([u8; 16]),
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn dev_err(e: crate::drive::DriveError) -> io::Error {
    io::Error::other(e.to_string())
}

/// Where the region's parts are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    log_off: u64,
    log_len: u64,
    page_off: u64,
    page_count: u64,
}

impl Layout {
    fn for_size(size: u64) -> io::Result<Layout> {
        let size = size / PAGE * PAGE;
        if size < 64 * PAGE {
            return Err(err(format!("metadata region of {size} bytes is too small for format v2 (256 KiB)")));
        }
        let log_len = (size / 8).clamp(16 * PAGE, 64 << 20) / PAGE * PAGE;
        let page_off = 2 * PAGE + log_len;
        Ok(Layout { log_off: 2 * PAGE, log_len, page_off, page_count: (size - page_off) / PAGE })
    }
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    gen: u64,
    root: u64,
    log_start: u64,
    log_seq: u64,
    layout: Layout,
    nonce: u64,
}

impl Superblock {
    fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; PAGE as usize];
        b[0..8].copy_from_slice(&SB_MAGIC);
        b[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
        b[16..24].copy_from_slice(&self.gen.to_le_bytes());
        b[24..32].copy_from_slice(&self.root.to_le_bytes());
        b[32..40].copy_from_slice(&self.log_start.to_le_bytes());
        b[40..48].copy_from_slice(&self.log_seq.to_le_bytes());
        b[48..56].copy_from_slice(&self.layout.log_len.to_le_bytes());
        b[56..64].copy_from_slice(&self.layout.page_off.to_le_bytes());
        b[64..72].copy_from_slice(&self.layout.page_count.to_le_bytes());
        b[72..80].copy_from_slice(&self.nonce.to_le_bytes());
        let crc = crc32c::crc32c(&b[..80]);
        b[80..84].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Option<Superblock> {
        if b[0..8] != SB_MAGIC || u32::from_le_bytes(b[8..12].try_into().unwrap()) != SB_VERSION {
            return None;
        }
        if crc32c::crc32c(&b[..80]) != u32::from_le_bytes(b[80..84].try_into().unwrap()) {
            return None;
        }
        let u = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Some(Superblock {
            gen: u(16),
            root: u(24),
            log_start: u(32),
            log_seq: u(40),
            layout: Layout { log_off: 2 * PAGE, log_len: u(48), page_off: u(56), page_count: u(64) },
            nonce: u(72),
        })
    }
}

/// Whether a region holds a v2 store (a superblock that checks).
pub async fn is_formatted(dev: &Arc<dyn BlockDevice>, base: u64) -> bool {
    let mut b = vec![0u8; PAGE as usize];
    for i in 0..2 {
        if dev.read(base + i * PAGE, &mut b).await.is_ok() && Superblock::from_bytes(&b).is_some() {
            return true;
        }
    }
    false
}

/// One page of the tree, decoded.
#[derive(Debug, Clone)]
enum Node {
    Leaf(Vec<(Key, Vec<u8>)>),
    /// `level` ≥ 1 (1: the children are leaves). Children with the lowest
    /// key each may hold; the first one's is `None` (anything below the
    /// second's).
    Internal { level: u8, children: Vec<(Option<Key>, u64)> },
}

impl Node {
    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; PAGE as usize];
        b[0..4].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
        let mut o = PAGE_HEADER;
        match self {
            Node::Leaf(entries) => {
                b[4] = LEAF;
                b[6..8].copy_from_slice(&(entries.len() as u16).to_le_bytes());
                for (k, v) in entries {
                    b[o..o + KEY_LEN].copy_from_slice(&k.0);
                    b[o + KEY_LEN..o + LEAF_ENTRY].copy_from_slice(&(v.len() as u16).to_le_bytes());
                    o += LEAF_ENTRY;
                    b[o..o + v.len()].copy_from_slice(v);
                    o += v.len();
                }
            }
            Node::Internal { level, children } => {
                b[4] = INTERNAL;
                b[5] = *level;
                b[6..8].copy_from_slice(&(children.len() as u16).to_le_bytes());
                b[o..o + 8].copy_from_slice(&children[0].1.to_le_bytes());
                o += 8;
                for (k, c) in &children[1..] {
                    b[o..o + KEY_LEN].copy_from_slice(&k.expect("a separator").0);
                    b[o + KEY_LEN..o + INTERNAL_ENTRY].copy_from_slice(&c.to_le_bytes());
                    o += INTERNAL_ENTRY;
                }
            }
        }
        let crc = crc32c::crc32c(&b);
        b[8..12].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn decode(page: u64, b: &[u8]) -> io::Result<Node> {
        let bad = |why: &str| err(format!("metadata page {page}: {why}"));
        if u32::from_le_bytes(b[0..4].try_into().unwrap()) != PAGE_MAGIC {
            return Err(bad("not a page"));
        }
        let stored = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let mut c = b.to_vec();
        c[8..12].fill(0);
        if crc32c::crc32c(&c) != stored {
            return Err(bad("checksum"));
        }
        let count = u16::from_le_bytes(b[6..8].try_into().unwrap()) as usize;
        let mut o = PAGE_HEADER;
        let key = |o: usize| Key(b[o..o + KEY_LEN].try_into().unwrap());
        match b[4] {
            LEAF => {
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    if o + LEAF_ENTRY > b.len() {
                        return Err(bad("leaf overruns"));
                    }
                    let k = key(o);
                    let len = u16::from_le_bytes(b[o + KEY_LEN..o + LEAF_ENTRY].try_into().unwrap()) as usize;
                    o += LEAF_ENTRY;
                    if o + len > b.len() {
                        return Err(bad("leaf overruns"));
                    }
                    entries.push((k, b[o..o + len].to_vec()));
                    o += len;
                }
                Ok(Node::Leaf(entries))
            }
            INTERNAL => {
                if count == 0 || count > MAX_CHILDREN {
                    return Err(bad("internal page count"));
                }
                let mut children = Vec::with_capacity(count);
                children.push((None, u64::from_le_bytes(b[o..o + 8].try_into().unwrap())));
                o += 8;
                for _ in 1..count {
                    let k = key(o);
                    let c = u64::from_le_bytes(b[o + KEY_LEN..o + INTERNAL_ENTRY].try_into().unwrap());
                    children.push((Some(k), c));
                    o += INTERNAL_ENTRY;
                }
                Ok(Node::Internal { level: b[5], children })
            }
            _ => Err(bad("unknown page type")),
        }
    }
}

/// Changes logged and not yet in the tree.
#[derive(Default, Debug)]
struct Overlay {
    dropped: BTreeSet<[u8; 16]>,
    puts: BTreeMap<Key, Option<Vec<u8>>>,
}

impl Overlay {
    fn apply(&mut self, op: Op) {
        match op {
            Op::Put(k, v) => {
                self.puts.insert(k, Some(v));
            }
            Op::Del(k) => {
                self.puts.insert(k, None);
            }
            Op::DropVolume(v) => {
                let keys: Vec<Key> = self.puts.range(Key::first_of(v)..=Key::last_of(v)).map(|(k, _)| *k).collect();
                for k in keys {
                    self.puts.remove(&k);
                }
                self.dropped.insert(v);
            }
        }
    }
    fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.puts.is_empty()
    }
    fn touches(&self, lo: Option<Key>, hi: Option<Key>) -> bool {
        let lo_k = lo.unwrap_or(Key([0; KEY_LEN]));
        let puts = match hi {
            Some(h) => self.puts.range(lo_k..h).next().is_some(),
            None => self.puts.range(lo_k..).next().is_some(),
        };
        puts || self.dropped.iter().any(|v| {
            let (a, b) = (Key::first_of(*v), Key::last_of(*v));
            // The volume's range [a, b] meets [lo, hi).
            lo.is_none_or(|l| b >= l) && hi.is_none_or(|h| a < h)
        })
    }
    /// Whether every key in [lo, hi) is of a dropped volume and no put lands
    /// there: the subtree goes without being read.
    fn drops_whole(&self, lo: Option<Key>, hi: Option<Key>) -> bool {
        let (Some(lo), Some(hi)) = (lo, hi) else { return false };
        let v = lo.vol();
        if !self.dropped.contains(&v) {
            return false;
        }
        // [lo, hi) inside [first_of(v), first_of(v + 1)).
        let within = hi.vol() == v || next_vol(v).is_some_and(|n| hi == Key::first_of(n));
        within && self.puts.range(lo..hi).next().is_none()
    }
}

fn next_vol(v: [u8; 16]) -> Option<[u8; 16]> {
    let n = u128::from_be_bytes(v).checked_add(1)?;
    Some(n.to_be_bytes())
}

/// What a store holds, for the pressure report.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub pages_used: u64,
    pub pages_total: u64,
    pub log_used: u64,
    pub log_len: u64,
    pub pending: usize,
}

/// A v2 metadata store over one region. See the module documentation.
pub struct MetaV2 {
    dev: Arc<dyn BlockDevice>,
    base: u64,
    layout: Layout,
    nonce: u64,
    sb_gen: u64,
    root: u64,
    /// Pages the durable tree uses, one bit each.
    used: Vec<u64>,
    /// Pages written for the next checkpoint (also set in `used`).
    fresh: Vec<u64>,
    /// Pages the next checkpoint stops naming: free once its superblock is.
    retired: Vec<u64>,
    /// Where the log the superblock names starts, and its first sequence.
    log_start: u64,
    /// Where the next record goes, and its sequence.
    log_tail: u64,
    next_seq: u64,
    /// Bytes of the ring from the start to the tail.
    log_used: u64,
    overlay: Overlay,
    /// Internal pages, decoded (about one in a hundred of the tree's).
    internal: HashMap<u64, Arc<Node>>,
    /// The first word of `used` that may have a free page.
    hint: usize,
}

impl MetaV2 {
    /// Lay an empty store over the region: both superblocks, the log's first
    /// page. Whatever the region held is gone.
    pub async fn format(dev: Arc<dyn BlockDevice>, base: u64, size: u64) -> io::Result<MetaV2> {
        let layout = Layout::for_size(size)?;
        let nonce = rand::random::<u64>() | 1;
        let sb = Superblock { gen: 1, root: EMPTY, log_start: 0, log_seq: 1, layout, nonce };
        let zero = vec![0u8; PAGE as usize];
        dev.write(base + PAGE, &zero).await.map_err(dev_err)?;
        dev.write(base + layout.log_off, &zero).await.map_err(dev_err)?;
        dev.write(base, &sb.to_bytes()).await.map_err(dev_err)?;
        dev.flush().await.map_err(dev_err)?;
        Ok(MetaV2 {
            dev,
            base,
            layout,
            nonce,
            sb_gen: 1,
            root: EMPTY,
            used: vec![0; (layout.page_count as usize).div_ceil(64)],
            fresh: Vec::new(),
            retired: Vec::new(),
            log_start: 0,
            log_tail: 0,
            next_seq: 1,
            log_used: 0,
            overlay: Overlay::default(),
            internal: HashMap::new(),
            hint: 0,
        })
    }

    /// Open the store in the region: the newer superblock that checks, then
    /// the log after it. `None` when neither superblock checks (a region
    /// never formatted for v2).
    pub async fn open(dev: Arc<dyn BlockDevice>, base: u64, size: u64) -> io::Result<Option<MetaV2>> {
        let layout = Layout::for_size(size)?;
        let mut best: Option<Superblock> = None;
        let mut b = vec![0u8; PAGE as usize];
        for i in 0..2 {
            if dev.read(base + i * PAGE, &mut b).await.is_err() {
                continue;
            }
            if let Some(sb) = Superblock::from_bytes(&b) {
                if sb.layout != layout {
                    return Err(err(format!(
                        "metadata v2 superblock describes another region ({:?}, here {:?})",
                        sb.layout, layout
                    )));
                }
                if best.is_none_or(|x| sb.gen > x.gen) {
                    best = Some(sb);
                }
            }
        }
        let Some(sb) = best else { return Ok(None) };
        let mut s = MetaV2 {
            dev,
            base,
            layout,
            nonce: sb.nonce,
            sb_gen: sb.gen,
            root: sb.root,
            used: vec![0; (layout.page_count as usize).div_ceil(64)],
            fresh: Vec::new(),
            retired: Vec::new(),
            log_start: sb.log_start,
            log_tail: sb.log_start,
            next_seq: sb.log_seq,
            log_used: 0,
            overlay: Overlay::default(),
            internal: HashMap::new(),
            hint: 0,
        };
        if s.root != EMPTY {
            s.mark_reachable().await?;
        }
        s.replay().await?;
        Ok(Some(s))
    }

    pub fn usage(&self) -> Usage {
        Usage {
            pages_used: self.used.iter().map(|w| w.count_ones() as u64).sum(),
            pages_total: self.layout.page_count,
            log_used: self.log_used,
            log_len: self.layout.log_len,
            pending: self.overlay.puts.len() + self.overlay.dropped.len(),
        }
    }

    // ------------------------------------------------------------ pages

    fn is_used(&self, p: u64) -> bool {
        self.used[(p / 64) as usize] & (1 << (p % 64)) != 0
    }
    fn set_used(&mut self, p: u64, on: bool) {
        let (w, b) = ((p / 64) as usize, 1u64 << (p % 64));
        if on {
            self.used[w] |= b;
        } else {
            self.used[w] &= !b;
            self.hint = self.hint.min(w);
        }
    }

    fn alloc_page(&mut self) -> io::Result<u64> {
        for (w, word) in self.used.iter().enumerate().skip(self.hint) {
            if *word != u64::MAX {
                self.hint = w;
                let p = w as u64 * 64 + (!word).trailing_zeros() as u64;
                if p >= self.layout.page_count {
                    break;
                }
                self.set_used(p, true);
                self.fresh.push(p);
                return Ok(p);
            }
        }
        Err(err(format!(
            "metadata region full: {} pages of {} KiB in use; format the slab with a larger metadata region",
            self.layout.page_count,
            PAGE / 1024
        )))
    }

    async fn read_page(&self, p: u64) -> io::Result<Node> {
        if p >= self.layout.page_count {
            return Err(err(format!("metadata page {p} is past the region ({} pages)", self.layout.page_count)));
        }
        let mut b = vec![0u8; PAGE as usize];
        self.dev.read(self.base + self.layout.page_off + p * PAGE, &mut b).await.map_err(dev_err)?;
        Node::decode(p, &b)
    }

    async fn node(&mut self, p: u64) -> io::Result<Arc<Node>> {
        if let Some(n) = self.internal.get(&p) {
            return Ok(n.clone());
        }
        let n = Arc::new(self.read_page(p).await?);
        if matches!(*n, Node::Internal { .. }) {
            self.internal.insert(p, n.clone());
        }
        Ok(n)
    }

    async fn write_node(&mut self, node: Node) -> io::Result<u64> {
        let p = self.alloc_page()?;
        let b = node.encode();
        self.dev.write(self.base + self.layout.page_off + p * PAGE, &b).await.map_err(dev_err)?;
        if matches!(node, Node::Internal { .. }) {
            self.internal.insert(p, Arc::new(node));
        }
        Ok(p)
    }

    fn retire(&mut self, p: u64) {
        self.retired.push(p);
        self.internal.remove(&p);
    }

    /// Mark every page the tree names; leaves are named by their parents, so
    /// only internal pages are read.
    async fn mark_reachable(&mut self) -> io::Result<()> {
        let mut stack = vec![self.root];
        let root_node = self.node(self.root).await?;
        if matches!(*root_node, Node::Leaf(_)) {
            self.set_used(self.root, true);
            return Ok(());
        }
        while let Some(p) = stack.pop() {
            if p >= self.layout.page_count || self.is_used(p) {
                return Err(err(format!("metadata page {p} is named twice or past the region")));
            }
            self.set_used(p, true);
            let n = self.node(p).await?;
            if let Node::Internal { level, children } = &*n {
                for (_, c) in children {
                    if *level == 1 {
                        if *c >= self.layout.page_count || self.is_used(*c) {
                            return Err(err(format!("metadata page {c} is named twice or past the region")));
                        }
                        self.set_used(*c, true);
                    } else {
                        stack.push(*c);
                    }
                }
            }
        }
        Ok(())
    }

    /// Every page of a subtree, for a drop that does not read its leaves.
    async fn subtree_pages(&mut self, p: u64, out: &mut Vec<u64>) -> io::Result<()> {
        let n = self.node(p).await?;
        out.push(p);
        if let Node::Internal { level, children } = &*n {
            for (_, c) in children {
                if *level == 1 {
                    out.push(*c);
                } else {
                    Box::pin(self.subtree_pages(*c, out)).await?;
                }
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- log

    fn rec_size(payload: usize) -> u64 {
        ((LOG_HEADER + payload) as u64).div_ceil(PAGE) * PAGE
    }

    fn record(&self, seq: u64, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; Self::rec_size(payload.len()) as usize];
        b[0..8].copy_from_slice(&LOG_MAGIC);
        b[8..16].copy_from_slice(&self.nonce.to_le_bytes());
        b[16..24].copy_from_slice(&seq.to_le_bytes());
        b[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        b[28] = kind;
        b[LOG_HEADER..LOG_HEADER + payload.len()].copy_from_slice(payload);
        let mut c = b[0..32].to_vec();
        c.extend_from_slice(payload);
        b[32..36].copy_from_slice(&crc32c::crc32c(&c).to_le_bytes());
        b
    }

    async fn replay(&mut self) -> io::Result<()> {
        let mut pos = self.log_start;
        let mut seq = self.next_seq;
        let mut used = 0u64;
        let mut replayed = 0usize;
        let mut b = vec![0u8; PAGE as usize];
        loop {
            if pos >= self.layout.log_len {
                used += self.layout.log_len - pos;
                pos = 0;
            }
            if used >= self.layout.log_len {
                break;
            }
            if self.dev.read(self.base + self.layout.log_off + pos, &mut b).await.is_err() {
                break;
            }
            if b[0..8] != LOG_MAGIC
                || u64::from_le_bytes(b[8..16].try_into().unwrap()) != self.nonce
                || u64::from_le_bytes(b[16..24].try_into().unwrap()) != seq
            {
                break;
            }
            let len = u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize;
            let size = Self::rec_size(len);
            if pos + size > self.layout.log_len {
                break;
            }
            let mut rec = b.clone();
            if size > PAGE {
                rec = vec![0u8; size as usize];
                if self.dev.read(self.base + self.layout.log_off + pos, &mut rec).await.is_err() {
                    break;
                }
            }
            let mut c = rec[0..32].to_vec();
            c.extend_from_slice(&rec[LOG_HEADER..LOG_HEADER + len]);
            if crc32c::crc32c(&c) != u32::from_le_bytes(rec[32..36].try_into().unwrap()) {
                break;
            }
            let kind = rec[28];
            if kind == 1 {
                // A wrap: the rest of the ring was skipped.
                used += self.layout.log_len - pos;
                pos = 0;
                seq += 1;
                continue;
            }
            let (ops, _): (Vec<Op>, _) =
                bincode::serde::decode_from_slice(&rec[LOG_HEADER..LOG_HEADER + len], bincode::config::standard())
                    .map_err(|e| err(format!("metadata log record {seq}: {e}")))?;
            for op in ops {
                self.overlay.apply(op);
            }
            replayed += 1;
            pos += size;
            used += size;
            seq += 1;
        }
        self.log_tail = pos;
        self.next_seq = seq;
        self.log_used = used;
        if replayed > 0 {
            tracing::debug!("metadata v2: {replayed} log record(s) replayed");
        }
        Ok(())
    }

    /// Make `ops` durable: one log record and one flush, or a checkpoint
    /// when the log has no room for it.
    pub async fn append(&mut self, ops: Vec<Op>) -> io::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        for op in &ops {
            if let Op::Put(k, v) = op {
                if v.len() > MAX_VALUE {
                    return Err(err(format!("metadata value for {k:?} is {} bytes (most {MAX_VALUE})", v.len())));
                }
            }
        }
        let payload = bincode::serde::encode_to_vec(&ops, bincode::config::standard())
            .map_err(|e| err(format!("encode: {e}")))?;
        let size = Self::rec_size(payload.len());
        if size > self.layout.log_len / 4 {
            // Bigger than the log is for: straight into the tree.
            for op in ops {
                self.overlay.apply(op);
            }
            return self.checkpoint().await;
        }
        if !self.fits(size) {
            self.checkpoint().await?;
        }
        let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut tail = self.log_tail;
        let mut used = self.log_used;
        let mut seq = self.next_seq;
        if tail + size > self.layout.log_len {
            if tail + PAGE <= self.layout.log_len {
                writes.push((tail, self.record(seq, 1, &[])));
                seq += 1;
            }
            used += self.layout.log_len - tail;
            tail = 0;
        }
        writes.push((tail, self.record(seq, 0, &payload)));
        for (at, b) in &writes {
            self.dev.write(self.base + self.layout.log_off + at, b).await.map_err(dev_err)?;
        }
        self.dev.flush().await.map_err(dev_err)?;
        self.log_tail = tail + size;
        self.log_used = used + size;
        self.next_seq = seq + 1;
        for op in ops {
            self.overlay.apply(op);
        }
        if self.overlay.puts.len() > CHECKPOINT_ENTRIES || self.log_used > self.layout.log_len / 2 {
            self.checkpoint().await?;
        }
        Ok(())
    }

    /// Whether a record of `size` fits behind the tail, wrapping if it has
    /// to, with a page to spare so the tail never meets the start.
    fn fits(&self, size: u64) -> bool {
        let wasted = if self.log_tail + size > self.layout.log_len { self.layout.log_len - self.log_tail } else { 0 };
        self.log_used + wasted + size + PAGE <= self.layout.log_len
    }

    // ------------------------------------------------------- checkpoint

    /// Fold what the log holds into the tree: new pages, flush, the other
    /// superblock, flush; then the pages no longer named are free.
    pub async fn checkpoint(&mut self) -> io::Result<()> {
        if self.overlay.is_empty() && self.log_used == 0 {
            return Ok(());
        }
        let overlay = std::mem::take(&mut self.overlay);
        match self.write_tree(&overlay).await {
            Ok(root) => {
                let sb = Superblock {
                    gen: self.sb_gen + 1,
                    root,
                    log_start: self.log_tail,
                    log_seq: self.next_seq,
                    layout: self.layout,
                    nonce: self.nonce,
                };
                let at = self.base + (sb.gen % 2) * PAGE;
                let r = async {
                    self.dev.flush().await.map_err(dev_err)?;
                    self.dev.write(at, &sb.to_bytes()).await.map_err(dev_err)?;
                    self.dev.flush().await.map_err(dev_err)
                }
                .await;
                if let Err(e) = r {
                    self.undo_fresh();
                    self.overlay = overlay;
                    return Err(e);
                }
                self.sb_gen = sb.gen;
                self.root = root;
                self.log_start = self.log_tail;
                self.log_used = 0;
                self.fresh.clear();
                for p in std::mem::take(&mut self.retired) {
                    self.set_used(p, false);
                }
                Ok(())
            }
            Err(e) => {
                self.undo_fresh();
                self.overlay = overlay;
                Err(e)
            }
        }
    }

    fn undo_fresh(&mut self) {
        for p in std::mem::take(&mut self.fresh) {
            self.set_used(p, false);
            self.internal.remove(&p);
        }
        self.retired.clear();
    }

    /// Replace everything the store holds with `entries` (sorted by key), as
    /// one checkpoint. What a first persist after a restart writes.
    pub async fn replace_all(&mut self, entries: Vec<(Key, Vec<u8>)>) -> io::Result<()> {
        // Every page of the old tree goes, and so does what the log holds.
        let mut old = Vec::new();
        if self.root != EMPTY {
            let r = self.root;
            let n = self.node(r).await?;
            if matches!(*n, Node::Leaf(_)) {
                old.push(r);
            } else {
                self.subtree_pages(r, &mut old).await?;
            }
        }
        let saved = std::mem::take(&mut self.overlay);
        let built = async {
            let leaves = self.pack_leaves(entries).await?;
            self.build_up(leaves, 1).await
        }
        .await;
        let root = match built {
            Ok(r) => r,
            Err(e) => {
                self.undo_fresh();
                self.overlay = saved;
                return Err(e);
            }
        };
        for p in old {
            self.retire(p);
        }
        // An empty overlay over the new tree, and the log emptied.
        let sb = Superblock {
            gen: self.sb_gen + 1,
            root,
            log_start: self.log_tail,
            log_seq: self.next_seq,
            layout: self.layout,
            nonce: self.nonce,
        };
        let at = self.base + (sb.gen % 2) * PAGE;
        let r = async {
            self.dev.flush().await.map_err(dev_err)?;
            self.dev.write(at, &sb.to_bytes()).await.map_err(dev_err)?;
            self.dev.flush().await.map_err(dev_err)
        }
        .await;
        if let Err(e) = r {
            self.undo_fresh();
            self.overlay = saved;
            return Err(e);
        }
        self.sb_gen = sb.gen;
        self.root = root;
        self.log_start = self.log_tail;
        self.log_used = 0;
        self.fresh.clear();
        for p in std::mem::take(&mut self.retired) {
            self.set_used(p, false);
        }
        Ok(())
    }

    /// Leaves for `entries`, written; each with its lowest key.
    async fn pack_leaves(&mut self, entries: Vec<(Key, Vec<u8>)>) -> io::Result<Vec<(Option<Key>, u64)>> {
        let mut out = Vec::new();
        let mut cur: Vec<(Key, Vec<u8>)> = Vec::new();
        let mut bytes = PAGE_HEADER;
        for (k, v) in entries {
            let need = LEAF_ENTRY + v.len();
            if bytes + need > PAGE as usize && !cur.is_empty() {
                let first = cur[0].0;
                let p = self.write_node(Node::Leaf(std::mem::take(&mut cur))).await?;
                out.push((Some(first), p));
                bytes = PAGE_HEADER;
            }
            bytes += need;
            cur.push((k, v));
        }
        if !cur.is_empty() {
            let first = cur[0].0;
            let p = self.write_node(Node::Leaf(cur)).await?;
            out.push((Some(first), p));
        }
        Ok(out)
    }

    /// Internal nodes of `level` over `children`, written.
    async fn pack_internal(&mut self, level: u8, children: Vec<(Option<Key>, u64)>) -> io::Result<Vec<(Option<Key>, u64)>> {
        let mut out = Vec::new();
        // Even chunks, so no node is left with a single child.
        let n = children.len().div_ceil(MAX_CHILDREN).max(1);
        let per = children.len().div_ceil(n);
        let mut it = children.into_iter().peekable();
        while it.peek().is_some() {
            let mut chunk: Vec<(Option<Key>, u64)> = it.by_ref().take(per).collect();
            let first = chunk[0].0;
            chunk[0].0 = None;
            let p = self.write_node(Node::Internal { level, children: chunk }).await?;
            out.push((first, p));
        }
        Ok(out)
    }

    /// The root over `nodes` of `level - 1`.
    async fn build_up(&mut self, mut nodes: Vec<(Option<Key>, u64)>, mut level: u8) -> io::Result<u64> {
        if nodes.is_empty() {
            return Ok(EMPTY);
        }
        while nodes.len() > 1 {
            nodes = self.pack_internal(level, nodes).await?;
            level += 1;
        }
        Ok(nodes[0].1)
    }

    async fn write_tree(&mut self, ov: &Overlay) -> io::Result<u64> {
        if self.root == EMPTY {
            let entries: Vec<(Key, Vec<u8>)> =
                ov.puts.iter().filter_map(|(k, v)| v.clone().map(|v| (*k, v))).collect();
            let leaves = self.pack_leaves(entries).await?;
            return self.build_up(leaves, 1).await;
        }
        let root = self.root;
        let root_level = match &*self.node(root).await? {
            Node::Leaf(_) => 0,
            Node::Internal { level, .. } => *level,
        };
        let nodes = self.apply_node(root, None, None, ov).await?;
        let mut root = self.build_up(nodes, root_level + 1).await?;
        // A root with one child is that child.
        while root != EMPTY {
            let n = self.node(root).await?;
            match &*n {
                Node::Internal { children, .. } if children.len() == 1 => {
                    let only = children[0].1;
                    self.retire(root);
                    // The page was written by this checkpoint: give it back.
                    if let Some(i) = self.fresh.iter().position(|p| *p == root) {
                        self.fresh.swap_remove(i);
                        self.retired.pop();
                        self.set_used(root, false);
                    }
                    root = only;
                }
                _ => break,
            }
        }
        Ok(root)
    }

    /// Apply the overlay to the subtree at `page` holding keys in [lo, hi):
    /// the nodes that replace it (none when it ends up empty).
    async fn apply_node(
        &mut self,
        page: u64,
        lo: Option<Key>,
        hi: Option<Key>,
        ov: &Overlay,
    ) -> io::Result<Vec<(Option<Key>, u64)>> {
        let node = self.node(page).await?;
        match &*node {
            Node::Leaf(entries) => {
                let mut merged: BTreeMap<Key, Vec<u8>> = entries
                    .iter()
                    .filter(|(k, _)| !ov.dropped.contains(&k.vol()))
                    .map(|(k, v)| (*k, v.clone()))
                    .collect();
                let lo_k = lo.unwrap_or(Key([0; KEY_LEN]));
                let range: Vec<(Key, Option<Vec<u8>>)> = match hi {
                    Some(h) => ov.puts.range(lo_k..h).map(|(k, v)| (*k, v.clone())).collect(),
                    None => ov.puts.range(lo_k..).map(|(k, v)| (*k, v.clone())).collect(),
                };
                for (k, v) in range {
                    match v {
                        Some(v) => {
                            merged.insert(k, v);
                        }
                        None => {
                            merged.remove(&k);
                        }
                    }
                }
                self.retire(page);
                let mut out = self.pack_leaves(merged.into_iter().collect()).await?;
                if let Some(first) = out.first_mut() {
                    // Keep the bound the parent placed this subtree by.
                    first.0 = lo;
                }
                Ok(out)
            }
            Node::Internal { level, children } => {
                let level = *level;
                let children = children.clone();
                let mut out: Vec<(Option<Key>, u64)> = Vec::with_capacity(children.len() + 1);
                let mut changed = false;
                for (i, (sep, child)) in children.iter().enumerate() {
                    let c_lo = if i == 0 { lo } else { *sep };
                    let c_hi = children.get(i + 1).and_then(|(s, _)| *s).or(hi);
                    if !ov.touches(c_lo, c_hi) {
                        out.push((c_lo, *child));
                        continue;
                    }
                    changed = true;
                    if ov.drops_whole(c_lo, c_hi) {
                        let mut pages = Vec::new();
                        if level == 1 {
                            pages.push(*child);
                        } else {
                            self.subtree_pages(*child, &mut pages).await?;
                        }
                        for p in pages {
                            self.retire(p);
                        }
                        continue;
                    }
                    let mut repl = Box::pin(self.apply_node(*child, c_lo, c_hi, ov)).await?;
                    if let Some(first) = repl.first_mut() {
                        first.0 = c_lo;
                    }
                    // A level-1 child came back as leaves; a deeper one as
                    // nodes of its level, or of a level above (a split root
                    // of the subtree is just more children here).
                    out.extend(repl);
                }
                if !changed {
                    return Ok(vec![(lo, page)]);
                }
                self.retire(page);
                if out.is_empty() {
                    return Ok(Vec::new());
                }
                let mut nodes = self.pack_internal(level, out).await?;
                if let Some(first) = nodes.first_mut() {
                    first.0 = lo;
                }
                Ok(nodes)
            }
        }
    }

    // ------------------------------------------------------------ read

    /// Every entry, in key order: the tree with the log applied.
    pub async fn scan(&mut self) -> io::Result<Vec<(Key, Vec<u8>)>> {
        let mut out = Vec::new();
        if self.root != EMPTY {
            let mut stack = vec![self.root];
            // Depth first, children pushed right to left.
            while let Some(p) = stack.pop() {
                let n = self.node(p).await?;
                match &*n {
                    Node::Leaf(entries) => {
                        out.extend(entries.iter().filter(|(k, _)| !self.overlay.dropped.contains(&k.vol())).cloned())
                    }
                    Node::Internal { children, .. } => {
                        for (_, c) in children.iter().rev() {
                            stack.push(*c);
                        }
                    }
                }
            }
        }
        if self.overlay.puts.is_empty() {
            return Ok(out);
        }
        let mut merged: BTreeMap<Key, Vec<u8>> = out.into_iter().collect();
        for (k, v) in &self.overlay.puts {
            match v {
                Some(v) => {
                    merged.insert(*k, v.clone());
                }
                None => {
                    merged.remove(k);
                }
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// Tree depth and the page counts, for tests and the pressure report.
    #[cfg(test)]
    async fn check(&mut self) -> io::Result<usize> {
        // Every key in order, every page named once.
        let all = self.scan().await?;
        for w in all.windows(2) {
            assert!(w[0].0 < w[1].0, "keys out of order: {:?} {:?}", w[0].0, w[1].0);
        }
        Ok(all.len())
    }
}

// ----------------------------------------------------- the volume document

/// The volume document a slab keeps, in either format. `None`: the slab keeps
/// none (no region), or keeps one that was never written.
pub async fn read_slab(slab: &crate::drive::slab::Slab) -> io::Result<Option<VolumeMetadata>> {
    if slab.format_version() != crate::drive::slab::SLAB_VERSION_2 {
        return match slab.read_metadata().await {
            Ok(Some(bytes)) => super::metadata::MetadataStore::decode(&bytes).map(Some),
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::other(e.to_string())),
        };
    }
    let Some((dev, off, size)) = slab.metadata_region() else { return Ok(None) };
    let Some(mut store) = MetaV2::open(dev, off, size).await? else {
        return Err(err(format!("slab {}: no metadata v2 superblock checks", slab.slab_id())));
    };
    let entries = store.scan().await?;
    if entries.is_empty() {
        return Ok(None);
    }
    document_of(entries).map(Some)
}

use super::metadata::{ArrayRecord, VolumeMetadata, VolumeRecord};
use super::extent::VolumeId;

/// The shape of a volume header in v2: a version, then the record.
const HEADER_VERSION: u32 = 9;

#[derive(Serialize, Deserialize)]
struct DocValue {
    extent_size: u64,
    arrays: Vec<ArrayRecord>,
}

pub fn doc_entry(extent_size: u64, arrays: &[ArrayRecord]) -> (Key, Vec<u8>) {
    let v = DocValue { extent_size, arrays: arrays.to_vec() };
    (Key::new([0; 16], kind::DOC, 0), bincode::serde::encode_to_vec(&v, bincode::config::standard()).unwrap_or_default())
}

/// A volume's header bytes: its record with no extents or parity.
pub fn header_bytes(rec: &VolumeRecord) -> Vec<u8> {
    let mut h = rec.clone();
    h.extents.clear();
    h.parity.clear();
    let mut out = HEADER_VERSION.to_le_bytes().to_vec();
    out.extend(bincode::serde::encode_to_vec(&h, bincode::config::standard()).unwrap_or_default());
    out
}

/// The header's entries: chunks of at most [`MAX_VALUE`], the last one
/// shorter (empty when the bytes divide evenly), which is how a reader knows
/// where the header ends.
pub fn header_entries(vol: VolumeId, bytes: &[u8]) -> Vec<(Key, Vec<u8>)> {
    let n = bytes.len() / MAX_VALUE + 1;
    (0..n)
        .map(|i| {
            let c = &bytes[(i * MAX_VALUE).min(bytes.len())..((i + 1) * MAX_VALUE).min(bytes.len())];
            (Key::new(*vol.0.as_bytes(), kind::HEADER, i as u64), c.to_vec())
        })
        .collect()
}

/// How many entries [`header_entries`] makes of `len` bytes.
pub fn header_chunks(len: usize) -> u64 {
    (len / MAX_VALUE + 1) as u64
}

pub fn extent_entry(vol: VolumeId, vext: u64, loc: &super::gem::ExtentLocation) -> (Key, Vec<u8>) {
    (
        Key::new(*vol.0.as_bytes(), kind::EXTENT, vext),
        bincode::serde::encode_to_vec(loc, bincode::config::standard()).unwrap_or_default(),
    )
}

pub fn parity_entry(vol: VolumeId, stripe: u64, g: &super::gem::ParityGroup) -> (Key, Vec<u8>) {
    (
        Key::new(*vol.0.as_bytes(), kind::PARITY, stripe),
        bincode::serde::encode_to_vec(g, bincode::config::standard()).unwrap_or_default(),
    )
}

/// Every entry of one volume.
pub fn volume_entries(rec: &VolumeRecord) -> Vec<(Key, Vec<u8>)> {
    let mut out = header_entries(rec.id, &header_bytes(rec));
    out.extend(rec.extents.iter().map(|(v, l)| extent_entry(rec.id, *v, l)));
    out.extend(rec.parity.iter().map(|(s, g)| parity_entry(rec.id, *s, g)));
    out
}

/// Every entry of a document, in key order.
pub fn document_entries(doc: &VolumeMetadata) -> Vec<(Key, Vec<u8>)> {
    let mut out = vec![doc_entry(doc.extent_size, &doc.arrays)];
    for v in &doc.volumes {
        out.extend(volume_entries(v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The document the entries describe.
pub fn document_of(entries: Vec<(Key, Vec<u8>)>) -> io::Result<VolumeMetadata> {
    let cfg = bincode::config::standard();
    let mut doc = VolumeMetadata { extent_size: 0, arrays: Vec::new(), volumes: Vec::new() };
    // The volume being read: its header bytes so far, and its record once
    // the header is complete.
    let mut cur: Option<(VolumeId, Vec<u8>, Option<VolumeRecord>)> = None;
    for (k, v) in entries {
        let vol = VolumeId(uuid::Uuid::from_bytes(k.vol()));
        if k.kind() == kind::DOC {
            let (d, _): (DocValue, _) =
                bincode::serde::decode_from_slice(&v, cfg).map_err(|e| err(format!("document: {e}")))?;
            doc.extent_size = d.extent_size;
            doc.arrays = d.arrays;
            continue;
        }
        if cur.as_ref().is_none_or(|(id, _, _)| *id != vol) {
            if let Some((id, _, rec)) = cur.take() {
                doc.volumes.push(rec.ok_or_else(|| err(format!("volume {}: header incomplete", id.0)))?);
            }
            cur = Some((vol, Vec::new(), None));
        }
        let (_, buf, rec) = cur.as_mut().unwrap();
        match k.kind() {
            kind::HEADER => {
                if rec.is_some() || k.idx() != buf.len() as u64 / MAX_VALUE as u64 {
                    // A chunk past the end (left by a longer header) or out
                    // of place: not part of this header.
                    continue;
                }
                buf.extend_from_slice(&v);
                if v.len() < MAX_VALUE {
                    if buf.len() < 4 {
                        return Err(err(format!("volume {}: header too short", vol.0)));
                    }
                    let ver = u32::from_le_bytes(buf[..4].try_into().unwrap());
                    if ver != HEADER_VERSION {
                        return Err(err(format!("volume {}: header version {ver}", vol.0)));
                    }
                    let (r, _): (VolumeRecord, _) = bincode::serde::decode_from_slice(&buf[4..], cfg)
                        .map_err(|e| err(format!("volume {} header: {e}", vol.0)))?;
                    *rec = Some(r);
                }
            }
            kind::EXTENT | kind::PARITY => {
                let Some(rec) = rec.as_mut() else {
                    return Err(err(format!("volume {}: extents before its header", vol.0)));
                };
                if k.kind() == kind::EXTENT {
                    let (l, _) = bincode::serde::decode_from_slice(&v, cfg)
                        .map_err(|e| err(format!("volume {} extent {}: {e}", vol.0, k.idx())))?;
                    rec.extents.insert(k.idx(), l);
                } else {
                    let (g, _) = bincode::serde::decode_from_slice(&v, cfg)
                        .map_err(|e| err(format!("volume {} stripe {}: {e}", vol.0, k.idx())))?;
                    rec.parity.insert(k.idx(), g);
                }
            }
            other => return Err(err(format!("unknown metadata key kind {other}"))),
        }
    }
    if let Some((id, _, rec)) = cur.take() {
        doc.volumes.push(rec.ok_or_else(|| err(format!("volume {}: header incomplete", id.0)))?);
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::slab::SlabId;
    use crate::volume::gem::{ExtentLocation, Leg};

    async fn mem(size: u64) -> Arc<dyn BlockDevice> {
        let name = format!("metav2-{}", uuid::Uuid::new_v4());
        crate::drive::open_path(&format!("emulated://{name}?size={size}"), false).await.unwrap()
    }

    fn k(v: u8, i: u64) -> Key {
        Key::new([v; 16], kind::EXTENT, i)
    }

    #[tokio::test]
    async fn a_store_keeps_what_was_logged_and_checkpointed_across_reopens() {
        let size = 4 << 20;
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        let mut want: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        for round in 0..40u64 {
            let mut ops = Vec::new();
            for i in 0..200u64 {
                let key = k((i % 5) as u8 + 1, (round * 37 + i * 11) % 3000);
                if (i + round) % 7 == 0 {
                    ops.push(Op::Del(key));
                    want.remove(&key);
                } else {
                    let v = vec![(round as u8) ^ (i as u8); 20 + (i as usize % 60)];
                    ops.push(Op::Put(key, v.clone()));
                    want.insert(key, v);
                }
            }
            if round % 9 == 8 {
                ops.push(Op::DropVolume([3; 16]));
                want.retain(|key, _| key.vol() != [3; 16]);
            }
            s.append(ops).await.unwrap();
            if round % 13 == 12 {
                s.checkpoint().await.unwrap();
            }
            if round % 6 == 5 {
                s = MetaV2::open(dev.clone(), 0, size).await.unwrap().unwrap();
            }
            let got: BTreeMap<Key, Vec<u8>> = s.scan().await.unwrap().into_iter().collect();
            assert_eq!(got, want, "round {round}");
        }
        s.checkpoint().await.unwrap();
        let mut s = MetaV2::open(dev, 0, size).await.unwrap().unwrap();
        assert_eq!(s.check().await.unwrap(), want.len());
        assert!(s.usage().pages_used > 1);
    }

    #[tokio::test]
    async fn a_torn_log_tail_loses_only_the_record_being_written() {
        let size = 1 << 20;
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        s.append(vec![Op::Put(k(1, 1), vec![1; 10])]).await.unwrap();
        s.append(vec![Op::Put(k(1, 2), vec![2; 10])]).await.unwrap();
        let tail = s.log_tail;
        // A third record whose write tore: its first page has the header,
        // the checksum does not match.
        let mut rec = s.record(s.next_seq, 0, &bincode::serde::encode_to_vec(vec![Op::Put(k(1, 3), vec![3; 10])], bincode::config::standard()).unwrap());
        rec[LOG_HEADER] ^= 0xFF;
        dev.write(s.layout.log_off + tail, &rec).await.unwrap();
        let mut s = MetaV2::open(dev.clone(), 0, size).await.unwrap().unwrap();
        let got: Vec<Key> = s.scan().await.unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec![k(1, 1), k(1, 2)]);
        // And the next append goes where the torn one was.
        assert_eq!(s.log_tail, tail);
        s.append(vec![Op::Put(k(1, 4), vec![4; 10])]).await.unwrap();
        let mut s = MetaV2::open(dev, 0, size).await.unwrap().unwrap();
        assert_eq!(s.scan().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_torn_superblock_leaves_the_previous_checkpoint() {
        let size = 1 << 20;
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        s.append(vec![Op::Put(k(1, 1), vec![1; 10])]).await.unwrap();
        s.checkpoint().await.unwrap();
        s.append(vec![Op::Put(k(1, 2), vec![2; 10])]).await.unwrap();
        s.checkpoint().await.unwrap();
        // The newer superblock torn: the older names a tree with key 1 only,
        // and its log still holds key 2's record.
        let newer = (s.sb_gen % 2) * PAGE;
        dev.write(newer, &vec![0xAB; PAGE as usize]).await.unwrap();
        let mut s = MetaV2::open(dev, 0, size).await.unwrap().unwrap();
        let got: Vec<Key> = s.scan().await.unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec![k(1, 1), k(1, 2)]);
    }

    #[tokio::test]
    async fn the_log_wraps_and_checkpoints_when_full() {
        let size = 512 * 1024; // log of 16 pages
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        for i in 0..300u64 {
            s.append(vec![Op::Put(k(2, i), vec![i as u8; 100])]).await.unwrap();
            if i % 50 == 49 {
                s = MetaV2::open(dev.clone(), 0, size).await.unwrap().unwrap();
                assert_eq!(s.scan().await.unwrap().len() as u64, i + 1);
            }
        }
        assert!(s.sb_gen > 2, "checkpointed {} times", s.sb_gen - 1);
    }

    #[tokio::test]
    async fn a_big_tree_splits_and_a_dropped_volume_frees_its_pages() {
        let size = 64 << 20;
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        let mut entries = Vec::new();
        for v in 1..=3u8 {
            for i in 0..20_000u64 {
                entries.push((k(v, i), vec![v; 30]));
            }
        }
        s.replace_all(entries).await.unwrap();
        let full = s.usage().pages_used;
        assert!(full > 400, "{full} pages");
        s.append(vec![Op::DropVolume([2; 16])]).await.unwrap();
        s.checkpoint().await.unwrap();
        let after = s.usage().pages_used;
        assert!(after < full * 3 / 4, "{after} of {full}");
        let mut s = MetaV2::open(dev, 0, size).await.unwrap().unwrap();
        let all = s.scan().await.unwrap();
        assert_eq!(all.len(), 40_000);
        assert!(all.iter().all(|(k, _)| k.vol() != [2; 16]));
        assert_eq!(s.usage().pages_used, after);
    }

    /// Random puts, deletes and volume drops over a tree three levels deep,
    /// checked against a model; after every checkpoint the pages in use are
    /// exactly the pages a reopen finds named (nothing leaked, nothing named
    /// twice).
    #[tokio::test]
    async fn a_deep_tree_matches_a_model_and_leaks_no_page() {
        use rand::{Rng, SeedableRng};
        let size = 256 << 20;
        let dev = mem(size).await;
        let mut s = MetaV2::format(dev.clone(), 0, size).await.unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(158);
        let mut want: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        // ~20k entries a volume at ~60 B: ~300 leaves each, two internal levels.
        let mut bulk = Vec::new();
        for v in 1..=8u8 {
            for i in 0..20_000u64 {
                let val = vec![v ^ (i as u8); 30 + (i as usize % 7)];
                bulk.push((k(v, i * 3), val.clone()));
                want.insert(k(v, i * 3), val);
            }
        }
        s.replace_all(bulk).await.unwrap();
        for round in 0..30u32 {
            let mut ops = Vec::new();
            for _ in 0..rng.gen_range(1..3000) {
                let key = k(rng.gen_range(1..=9u8), rng.gen_range(0..70_000u64));
                if rng.gen_bool(0.3) {
                    ops.push(Op::Del(key));
                    want.remove(&key);
                } else {
                    let val = vec![rng.gen(); rng.gen_range(0..200)];
                    ops.push(Op::Put(key, val.clone()));
                    want.insert(key, val);
                }
            }
            if rng.gen_bool(0.2) {
                let v = rng.gen_range(1..=9u8);
                ops.push(Op::DropVolume([v; 16]));
                want.retain(|key, _| key.vol() != [v; 16]);
                // And some of it written again after the drop.
                for i in 0..rng.gen_range(0..500u64) {
                    let val = vec![7; 9];
                    ops.push(Op::Put(k(v, i), val.clone()));
                    want.insert(k(v, i), val);
                }
            }
            s.append(ops).await.unwrap();
            if rng.gen_bool(0.5) {
                s.checkpoint().await.unwrap();
                let used = s.usage().pages_used;
                let mut again = MetaV2::open(dev.clone(), 0, size).await.unwrap().unwrap();
                assert_eq!(again.usage().pages_used, used, "round {round}: pages in use vs named");
                assert_eq!(again.usage().pending, 0);
                let got: BTreeMap<Key, Vec<u8>> = again.scan().await.unwrap().into_iter().collect();
                assert_eq!(got.len(), want.len(), "round {round}");
                assert!(got == want, "round {round}: contents differ");
            }
            if rng.gen_bool(0.3) {
                s = MetaV2::open(dev.clone(), 0, size).await.unwrap().unwrap();
            }
            let got: BTreeMap<Key, Vec<u8>> = s.scan().await.unwrap().into_iter().collect();
            assert!(got == want, "round {round}: contents differ ({} vs {})", got.len(), want.len());
        }
    }

    #[test]
    fn a_header_that_shrank_ignores_the_chunk_left_behind() {
        let id = VolumeId(uuid::Uuid::new_v4());
        let rec = |name: &str| VolumeRecord {
            id,
            name: name.into(),
            virtual_size: 1,
            array_id: None,
            extents: BTreeMap::new(),
            retention: Default::default(),
            redundancy: Default::default(),
            parity: BTreeMap::new(),
            failed_slabs: vec![],
            parent: None,
            sealed: false,
            template: false,
            access: Default::default(),
            fs: None,
            owner: None,
            lba: 4096,
        };
        let long = header_entries(id, &header_bytes(&rec(&"a".repeat(5000))));
        let short = header_entries(id, &header_bytes(&rec("b")));
        assert_eq!(long.len(), 4);
        assert_eq!(short.len(), 1);
        let mut store: BTreeMap<Key, Vec<u8>> = long.into_iter().collect();
        store.extend(short);
        let doc = document_of(store.into_iter().collect()).unwrap();
        assert_eq!(doc.volumes[0].name, "b");
        assert_eq!(header_chunks(MAX_VALUE * 2), 3);
    }

    #[test]
    fn a_document_round_trips_through_its_entries() {
        let sid = SlabId(uuid::Uuid::new_v4());
        let mut v = VolumeRecord {
            id: VolumeId(uuid::Uuid::new_v4()),
            name: "x".repeat(4000),
            virtual_size: 1 << 30,
            array_id: None,
            extents: BTreeMap::new(),
            retention: Default::default(),
            redundancy: Default::default(),
            parity: BTreeMap::new(),
            failed_slabs: vec![],
            parent: None,
            sealed: true,
            template: false,
            access: crate::volume::metadata::Access::ReadWrite,
            fs: None,
            owner: None,
            lba: 4096,
        };
        v.extents.insert(0, ExtentLocation::with_legs(Leg::new(sid, 5_000_000_000), vec![Leg::new(sid, 7)]));
        v.extents.insert(9, ExtentLocation::new(sid, 3));
        let mut w = v.clone();
        w.id = VolumeId(uuid::Uuid::new_v4());
        w.name = "y".into();
        let doc = VolumeMetadata { extent_size: 1 << 20, arrays: vec![], volumes: vec![v, w] };
        let back = document_of(document_entries(&doc)).unwrap();
        assert_eq!(back.extent_size, 1 << 20);
        assert_eq!(back.volumes.len(), 2);
        for a in &doc.volumes {
            let b = back.volumes.iter().find(|x| x.id == a.id).unwrap();
            assert_eq!(a.name, b.name);
            assert_eq!(a.extents.len(), b.extents.len());
            for (i, l) in &a.extents {
                assert!(l.same_slots(&b.extents[i]));
            }
        }
    }
}
