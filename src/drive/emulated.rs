//! Emulated drives for scale tests (#208, stormcos#92).
//!
//! The scale target is 160 drives of 256 TB or 1 PB a node, and no test
//! machine has them. An emulated drive reports any capacity and stores only
//! what is written: in memory (pages of 64 KiB, for metadata-scale tests), or
//! in a directory of 1 GiB sparse files (for data that should outlive the
//! process). A range never written, or written with zeros, or discarded,
//! stores nothing and reads back as zeros, so formatting a 1 PiB slab, whose
//! slot table alone is 64 GiB of zeros, costs nothing.
//!
//! It is a drive like any other: `emulated://<name>?size=1P[&backing=<dir>]`
//! is accepted wherever a device path is (`[[drives]]`, `POST /api/v1/drives`,
//! `slab format`), and `[[drives]] kind = "emulated"` spells the same thing in
//! a config. One name is one drive for the life of the process: opening the
//! same name twice returns the same drive, which is what makes the in-memory
//! kind usable at all. It reports `DriveType::Emulated`, so nothing mistakes
//! it for media.
//!
//! **A volatile write cache** (`&volatile=1`, memory only) is the power-cut
//! test at node scale (#172): writes and zeroes are held until a flush, as a
//! drive's cache holds them, and [`crash`] cuts the power — every flushed
//! write kept, each one still cached kept or lost at random, the drive then
//! reading what survived. A `CrashDevice` does the same for a small device
//! held whole in memory; an install disk is tens of GiB.
//!
//! **Directory backing** costs what is written, not the range touched (#300):
//! each chunk file is opened once and its handle kept (up to
//! [`OPEN_CHUNKS`]), which chunks exist is known without asking the
//! filesystem, and the bytes held are counted per chunk from an `fstat` after
//! each change, never by rescanning the directory.
//!
//! **Failing one on command** is the rebuild test: [`set_failed`] (or
//! `POST /api/v1/drives/{id}/emulate {"failed": true}`) makes every I/O
//! answer EIO until it is cleared, as a drive that died would.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// Bytes a memory page holds.
const PAGE: u64 = 64 * 1024;
/// Bytes a backing file holds.
const CHUNK: u64 = 1 << 30;
/// Memory pages are spread over this many locks, so parallel I/O to one
/// drive does not queue on one mutex.
const SHARDS: usize = 64;
/// Chunk files a directory-backed drive keeps open at once.
pub const OPEN_CHUNKS: usize = 256;

/// What an `emulated://` path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmulatedSpec {
    pub name: String,
    pub size: u64,
    /// A directory of chunk files; `None` keeps the data in memory.
    pub backing: Option<PathBuf>,
    pub block_size: u32,
    /// Writes held in a cache until a flush, for power-cut tests (#172).
    pub volatile: bool,
}

/// A size: digits with an optional K/M/G/T/P/E suffix (powers of 1024).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => {
            let unit = s[i..].trim_end_matches(['B', 'b', 'i']).to_ascii_uppercase();
            let m: u64 = match unit.as_str() {
                "" => 1,
                "K" => 1 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                "T" => 1 << 40,
                "P" => 1 << 50,
                "E" => 1 << 60,
                _ => return None,
            };
            (&s[..i], m)
        }
        None => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

impl EmulatedSpec {
    /// `emulated://<name>?size=<n>[&backing=<dir>][&lbs=512|4096][&volatile=1]`.
    pub fn parse(uri: &str) -> Option<DriveResult<EmulatedSpec>> {
        let rest = uri.strip_prefix("emulated://")?;
        let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
        let bad = |why: String| Some(Err(DriveError::Other(anyhow::anyhow!("{uri}: {why}"))));
        if name.is_empty() || name.contains('/') {
            return bad("an emulated drive needs a name: emulated://<name>?size=…".into());
        }
        let mut size = None;
        let mut backing = None;
        let mut block_size = 4096u32;
        let mut volatile = false;
        for kv in query.split('&').filter(|kv| !kv.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "size" => match parse_size(v) {
                    Some(n) if n > 0 => size = Some(n),
                    _ => return bad(format!("size {v:?} is not a size (e.g. 256T, 1P)")),
                },
                "backing" if !v.is_empty() => backing = Some(PathBuf::from(v)),
                "lbs" => match v {
                    "512" => block_size = 512,
                    "4096" => block_size = 4096,
                    _ => return bad(format!("lbs {v:?}: 512 or 4096")),
                },
                "volatile" => match v {
                    "1" | "true" => volatile = true,
                    "0" | "false" => volatile = false,
                    _ => return bad(format!("volatile {v:?}: 1 or 0")),
                },
                _ => return bad(format!("unknown option {k:?} (size, backing, lbs, volatile)")),
            }
        }
        let Some(size) = size else { return bad("size= is required".into()) };
        if volatile && backing.is_some() {
            return bad("a volatile cache is for a drive held in memory, not one with a backing".into());
        }
        let size = size - size % block_size as u64;
        Some(Ok(EmulatedSpec { name: name.to_string(), size, backing, block_size, volatile }))
    }

    pub fn uri(&self) -> String {
        let mut u = format!("emulated://{}?size={}", self.name, self.size);
        if let Some(b) = &self.backing {
            u.push_str(&format!("&backing={}", b.display()));
        }
        if self.block_size != 4096 {
            u.push_str(&format!("&lbs={}", self.block_size));
        }
        if self.volatile {
            u.push_str("&volatile=1");
        }
        u
    }
}

enum Store {
    Memory(Vec<Mutex<HashMap<u64, Box<[u8]>>>>),
    Dir(Arc<DirStore>),
}

/// A directory of chunk files and what is known about them.
struct DirStore {
    dir: PathBuf,
    state: Mutex<DirState>,
}

#[derive(Default)]
struct DirState {
    /// Every chunk file there is, with the bytes it holds (`st_blocks`).
    chunks: HashMap<u64, u64>,
    /// Open handles and when each was last used.
    open: HashMap<u64, (Arc<std::fs::File>, u64)>,
    tick: u64,
}

impl DirStore {
    /// Open `dir`, learning its chunk files and what they hold (once).
    fn open(dir: &std::path::Path) -> std::io::Result<(DirStore, u64)> {
        std::fs::create_dir_all(dir)?;
        let mut st = DirState::default();
        let mut total = 0;
        for e in std::fs::read_dir(dir)?.flatten() {
            let name = e.file_name();
            let Some(chunk) = name.to_str().and_then(|n| n.strip_suffix(".chunk")).and_then(|h| u64::from_str_radix(h, 16).ok())
            else {
                continue;
            };
            let held = e.metadata().map(|m| held_bytes(&m)).unwrap_or(0);
            st.chunks.insert(chunk, held);
            total += held;
        }
        Ok((DirStore { dir: dir.to_path_buf(), state: Mutex::new(st) }, total))
    }

    fn path(&self, chunk: u64) -> PathBuf {
        self.dir.join(format!("{chunk:012x}.chunk"))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DirState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The chunk's open file: `None` when it does not exist and `create` is
    /// false. A handle is opened once and kept; past [`OPEN_CHUNKS`] the one
    /// used longest ago is closed.
    fn file(&self, chunk: u64, create: bool) -> std::io::Result<Option<Arc<std::fs::File>>> {
        let mut st = self.state();
        st.tick += 1;
        let tick = st.tick;
        if let Some((f, used)) = st.open.get_mut(&chunk) {
            *used = tick;
            return Ok(Some(Arc::clone(f)));
        }
        if !create && !st.chunks.contains_key(&chunk) {
            return Ok(None);
        }
        let path = self.path(chunk);
        let f = Arc::new(dtime(T_OPEN, || {
            std::fs::OpenOptions::new().create(create).read(true).write(true).open(&path)
        })?);
        st.chunks.entry(chunk).or_insert(0);
        if st.open.len() >= OPEN_CHUNKS {
            if let Some(oldest) = st.open.iter().min_by_key(|(_, (_, u))| *u).map(|(c, _)| *c) {
                st.open.remove(&oldest);
                dir_stats::add(T_CLOSE, std::time::Duration::ZERO);
            }
        }
        st.open.insert(chunk, (Arc::clone(&f), tick));
        Ok(Some(f))
    }

    /// After a write or a punch: the chunk's held bytes from one `fstat`,
    /// and the drive's total moved by the difference.
    fn account(&self, chunk: u64, f: &std::fs::File, stored: &AtomicU64) {
        let mut st = self.state();
        let held = dtime(T_STAT, || f.metadata()).map(|m| held_bytes(&m)).unwrap_or(0);
        let old = st.chunks.insert(chunk, held).unwrap_or(0);
        if held >= old {
            stored.fetch_add(held - old, Ordering::Relaxed);
        } else {
            stored.fetch_sub(old - held, Ordering::Relaxed);
        }
    }

    /// A whole chunk zeroed: its file goes.
    fn remove(&self, chunk: u64, stored: &AtomicU64) -> std::io::Result<()> {
        let mut st = self.state();
        st.open.remove(&chunk);
        if let Some(old) = st.chunks.remove(&chunk) {
            dtime(T_REMOVE, || std::fs::remove_file(self.path(chunk)))?;
            stored.fetch_sub(old, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Bytes a file holds on disk (its allocated blocks; holes hold none).
fn held_bytes(m: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        m.len()
    }
}

struct Inner {
    spec: EmulatedSpec,
    id: DeviceId,
    store: Store,
    failed: AtomicBool,
    /// Bytes held (memory pages, or bytes written into chunk files).
    stored: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    /// The volatile cache, when the drive has one.
    cache: Option<Mutex<Cache>>,
}

/// What a volatile drive holds that a power cut may lose (#172).
#[derive(Default)]
struct Cache {
    /// Writes since the last flush, in order (`None`: zeros of that length).
    log: Vec<(u64, Option<Vec<u8>>, u64)>,
    /// Pages as reads see them: the durable page with the log applied.
    view: HashMap<u64, Box<[u8]>>,
}

/// See the module documentation. Cheap to clone: one drive, many handles.
#[derive(Clone)]
pub struct EmulatedDevice {
    inner: Arc<Inner>,
}

fn registry() -> &'static Mutex<HashMap<String, EmulatedDevice>> {
    static R: OnceLock<Mutex<HashMap<String, EmulatedDevice>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The drive `spec` names: the one already open under that name, or a new
/// one. A name reopened with another size or backing is refused.
pub fn open(spec: &EmulatedSpec) -> DriveResult<EmulatedDevice> {
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = r.get(&spec.name) {
        if d.inner.spec != *spec {
            return Err(DriveError::Other(anyhow::anyhow!(
                "emulated drive {} is already open as {}",
                spec.name,
                d.inner.spec.uri()
            )));
        }
        return Ok(d.clone());
    }
    let (store, stored) = match &spec.backing {
        None => (Store::Memory((0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect()), 0),
        Some(dir) => {
            let (d, held) = DirStore::open(dir).map_err(DriveError::Io)?;
            (Store::Dir(Arc::new(d)), held)
        }
    };
    let id = DeviceId {
        uuid: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, spec.uri().as_bytes()),
        serial: format!("EMU-{}", spec.name),
        model: format!("stormblock emulated {}", human(spec.size)),
        path: spec.uri(),
        wwn: String::new(),
    };
    let dev = EmulatedDevice {
        inner: Arc::new(Inner {
            spec: spec.clone(),
            id,
            store,
            failed: AtomicBool::new(false),
            stored: AtomicU64::new(stored),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            cache: spec.volatile.then(|| Mutex::new(Cache::default())),
        }),
    };
    r.insert(spec.name.clone(), dev.clone());
    Ok(dev)
}

/// The emulated drive open under `name`, if there is one.
pub fn get(name: &str) -> Option<EmulatedDevice> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned()
}

/// Fail (or recover) the emulated drive a device path or name names: every
/// I/O answers EIO while it is failed. Whether there was such a drive.
pub fn set_failed(path_or_name: &str, failed: bool) -> bool {
    let name = match EmulatedSpec::parse(path_or_name) {
        Some(Ok(s)) => s.name,
        _ => path_or_name.to_string(),
    };
    match get(&name) {
        Some(d) => {
            d.inner.failed.store(failed, Ordering::SeqCst);
            tracing::warn!("emulated drive {name}: {}", if failed { "FAILED (every I/O answers EIO)" } else { "recovered" });
            true
        }
        None => false,
    }
}

/// Cut the power to a volatile emulated drive (#172): every flushed write
/// stays; each write still in its cache is kept with probability `keep`
/// (repeatable for one `seed`), in the order it was made; the cache is
/// emptied. The drive keeps its name, and reads what survived. Returns how
/// many cached writes there were, or `None` for no such volatile drive.
pub fn crash(path_or_name: &str, seed: u64, keep: f64) -> Option<usize> {
    use rand::{Rng, SeedableRng};
    let name = match EmulatedSpec::parse(path_or_name) {
        Some(Ok(s)) => s.name,
        _ => path_or_name.to_string(),
    };
    let d = get(&name)?;
    let Store::Memory(shards) = &d.inner.store else { return None };
    let mut c = d.inner.cache.as_ref()?.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let n = c.log.len();
    for (off, data, len) in std::mem::take(&mut c.log) {
        if rng.gen_bool(keep) {
            d.mem_write(shards, off, data.as_deref(), len);
        }
    }
    c.view.clear();
    tracing::warn!("emulated drive {name}: power cut, {n} cached write(s) kept or lost at random");
    Some(n)
}

fn human(n: u64) -> String {
    for (s, u) in [(1u64 << 50, "PiB"), (1 << 40, "TiB"), (1 << 30, "GiB"), (1 << 20, "MiB")] {
        if n >= s && n % s == 0 {
            return format!("{}{u}", n / s);
        }
    }
    format!("{n}B")
}

/// Where a directory-backed drive's time goes (#300): calls and nanoseconds
/// per kind of filesystem operation, for every drive in the process.
pub mod dir_stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub const KINDS: [&str; 8] = ["task", "open", "read", "write", "punch", "remove", "stat", "close"];
    static CALLS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    static NANOS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

    /// One call of `kind` that took `d` (for a wait, not a closure).
    pub(super) fn add(kind: usize, d: std::time::Duration) {
        CALLS[kind].fetch_add(1, Ordering::Relaxed);
        NANOS[kind].fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub(super) fn time<T>(kind: usize, f: impl FnOnce() -> T) -> T {
        let t = std::time::Instant::now();
        let r = f();
        CALLS[kind].fetch_add(1, Ordering::Relaxed);
        NANOS[kind].fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    /// `(kind, calls, seconds)` for every kind, and the counts reset.
    pub fn take() -> Vec<(&'static str, u64, f64)> {
        KINDS
            .iter()
            .enumerate()
            .map(|(i, k)| {
                (*k, CALLS[i].swap(0, Ordering::Relaxed), NANOS[i].swap(0, Ordering::Relaxed) as f64 / 1e9)
            })
            .collect()
    }
}
use dir_stats::time as dtime;
const T_TASK: usize = 0;
const T_OPEN: usize = 1;
const T_READ: usize = 2;
const T_WRITE: usize = 3;
const T_PUNCH: usize = 4;
const T_REMOVE: usize = 5;
const T_STAT: usize = 6;
const T_CLOSE: usize = 7;

fn eio(name: &str) -> DriveError {
    DriveError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("emulated drive {name} is failed (EIO)"),
    ))
}

impl EmulatedDevice {
    pub fn spec(&self) -> &EmulatedSpec {
        &self.inner.spec
    }

    pub fn is_failed(&self) -> bool {
        self.inner.failed.load(Ordering::SeqCst)
    }

    pub fn set_failed(&self, failed: bool) {
        self.inner.failed.store(failed, Ordering::SeqCst);
    }

    /// Bytes the drive actually holds (what was written and not zero).
    pub fn stored_bytes(&self) -> u64 {
        self.inner.stored.load(Ordering::Relaxed)
    }

    fn check(&self, offset: u64, len: u64) -> DriveResult<()> {
        if self.is_failed() {
            return Err(eio(&self.inner.spec.name));
        }
        if offset.checked_add(len).is_none_or(|end| end > self.inner.spec.size) {
            return Err(DriveError::OutOfRange { offset, len, capacity: self.inner.spec.size });
        }
        Ok(())
    }

    fn mem_shard(shards: &[Mutex<HashMap<u64, Box<[u8]>>>], page: u64) -> &Mutex<HashMap<u64, Box<[u8]>>> {
        &shards[(page as usize) % shards.len()]
    }

    /// Write `buf` (or zeros, when `buf` is `None`, over `len` bytes).
    fn mem_write(&self, shards: &[Mutex<HashMap<u64, Box<[u8]>>>], offset: u64, buf: Option<&[u8]>, len: u64) {
        let mut done = 0u64;
        while done < len {
            let at = offset + done;
            let page = at / PAGE;
            let off = (at % PAGE) as usize;
            let n = ((PAGE as usize - off) as u64).min(len - done) as usize;
            let src = buf.map(|b| &b[done as usize..done as usize + n]);
            let zero = src.is_none_or(|s| s.iter().all(|&b| b == 0));
            let mut m = Self::mem_shard(shards, page).lock().unwrap_or_else(|e| e.into_inner());
            if zero {
                if let Some(p) = m.get_mut(&page) {
                    p[off..off + n].fill(0);
                    if n == PAGE as usize || p.iter().all(|&b| b == 0) {
                        m.remove(&page);
                        self.inner.stored.fetch_sub(PAGE, Ordering::Relaxed);
                    }
                }
            } else {
                let p = m.entry(page).or_insert_with(|| {
                    self.inner.stored.fetch_add(PAGE, Ordering::Relaxed);
                    vec![0u8; PAGE as usize].into_boxed_slice()
                });
                p[off..off + n].copy_from_slice(src.unwrap());
            }
            done += n as u64;
        }
    }

    fn mem_read(shards: &[Mutex<HashMap<u64, Box<[u8]>>>], offset: u64, buf: &mut [u8]) {
        let len = buf.len() as u64;
        let mut done = 0u64;
        while done < len {
            let at = offset + done;
            let page = at / PAGE;
            let off = (at % PAGE) as usize;
            let n = ((PAGE as usize - off) as u64).min(len - done) as usize;
            let dst = &mut buf[done as usize..done as usize + n];
            match Self::mem_shard(shards, page).lock().unwrap_or_else(|e| e.into_inner()).get(&page) {
                Some(p) => dst.copy_from_slice(&p[off..off + n]),
                None => dst.fill(0),
            }
            done += n as u64;
        }
    }

    async fn dir_io(&self, store: Arc<DirStore>, offset: u64, op: DirOp) -> DriveResult<Vec<u8>> {
        let inner = Arc::clone(&self.inner);
        let queued = std::time::Instant::now();
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::os::unix::fs::FileExt;
            dir_stats::add(T_TASK, queued.elapsed()); // waiting for a blocking thread
            let stored = &inner.stored;
            let mut out = Vec::new();
            let (len, data) = match &op {
                DirOp::Read(n) => (*n as u64, None),
                DirOp::Write(b) => (b.len() as u64, Some(b.as_slice())),
                DirOp::Zero(n) => (*n, None),
            };
            if let DirOp::Read(n) = op {
                out = vec![0u8; n];
            }
            let mut done = 0u64;
            while done < len {
                let at = offset + done;
                let chunk = at / CHUNK;
                let off = at % CHUNK;
                let n = (CHUNK - off).min(len - done);
                match &op {
                    DirOp::Read(_) => {
                        if let Some(f) = store.file(chunk, false)? {
                            let dst = &mut out[done as usize..(done + n) as usize];
                            let mut got = 0usize;
                            while got < dst.len() {
                                let r = dtime(T_READ, || f.read_at(&mut dst[got..], off + got as u64))?;
                                if r == 0 {
                                    break; // past the file's end: zeros
                                }
                                got += r;
                            }
                        }
                    }
                    DirOp::Write(_) => {
                        let src = &data.unwrap()[done as usize..(done + n) as usize];
                        if src.iter().all(|&b| b == 0) {
                            // Zeros where nothing is stored store nothing.
                            if let Some(f) = store.file(chunk, false)? {
                                dtime(T_PUNCH, || punch_hole(&f, off, n))?;
                                store.account(chunk, &f, stored);
                            }
                        } else if let Some(f) = store.file(chunk, true)? {
                            dtime(T_WRITE, || f.write_all_at(src, off))?;
                            store.account(chunk, &f, stored);
                        }
                    }
                    DirOp::Zero(_) => {
                        if off == 0 && n == CHUNK {
                            store.remove(chunk, stored)?;
                        } else if let Some(f) = store.file(chunk, false)? {
                            dtime(T_PUNCH, || punch_hole(&f, off, n))?;
                            store.account(chunk, &f, stored);
                        }
                    }
                }
                done += n;
            }
            Ok(out)
        })
        .await
        .map_err(|e| DriveError::Other(anyhow::anyhow!("emulated I/O task: {e}")))?
        .map_err(DriveError::Io)
    }

    /// A write into the volatile cache: logged, and applied to the pages
    /// reads see. `None` writes zeros.
    fn cache_write(&self, shards: &[Mutex<HashMap<u64, Box<[u8]>>>], c: &mut Cache, offset: u64, buf: Option<&[u8]>, len: u64) {
        let mut done = 0u64;
        while done < len {
            let at = offset + done;
            let page = at / PAGE;
            let off = (at % PAGE) as usize;
            let n = ((PAGE as usize - off) as u64).min(len - done) as usize;
            let p = c.view.entry(page).or_insert_with(|| {
                let mut b = vec![0u8; PAGE as usize];
                Self::mem_read(shards, page * PAGE, &mut b);
                b.into_boxed_slice()
            });
            match buf {
                Some(b) => p[off..off + n].copy_from_slice(&b[done as usize..done as usize + n]),
                None => p[off..off + n].fill(0),
            }
            done += n as u64;
        }
        c.log.push((offset, buf.map(<[u8]>::to_vec), len));
    }

    fn cache_read(shards: &[Mutex<HashMap<u64, Box<[u8]>>>], c: &Cache, offset: u64, buf: &mut [u8]) {
        Self::mem_read(shards, offset, buf);
        let len = buf.len() as u64;
        let mut done = 0u64;
        while done < len {
            let at = offset + done;
            let page = at / PAGE;
            let off = (at % PAGE) as usize;
            let n = ((PAGE as usize - off) as u64).min(len - done) as usize;
            if let Some(p) = c.view.get(&page) {
                buf[done as usize..done as usize + n].copy_from_slice(&p[off..off + n]);
            }
            done += n as u64;
        }
    }

    async fn zero(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.check(offset, len)?;
        if let (Some(cache), Store::Memory(shards)) = (&self.inner.cache, &self.inner.store) {
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            self.cache_write(shards, &mut c, offset, None, len);
            return Ok(());
        }
        match &self.inner.store {
            Store::Memory(shards) => {
                self.mem_write(shards, offset, None, len);
                Ok(())
            }
            Store::Dir(d) => self.dir_io(Arc::clone(d), offset, DirOp::Zero(len)).await.map(|_| ()),
        }
    }
}

enum DirOp {
    Read(usize),
    Write(Vec<u8>),
    Zero(u64),
}

#[cfg(target_os = "linux")]
fn punch_hole(f: &std::fs::File, off: u64, len: u64) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe {
        libc::fallocate(
            f.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            off as libc::off_t,
            len as libc::off_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // A filesystem without hole punching: write the zeros.
        use std::os::unix::fs::FileExt;
        let z = vec![0u8; (1 << 20).min(len) as usize];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(z.len() as u64) as usize;
            f.write_all_at(&z[..n], off + done)?;
            done += n as u64;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(f: &std::fs::File, off: u64, len: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let z = vec![0u8; len as usize];
    f.write_all_at(&z, off)
}

#[async_trait]
impl BlockDevice for EmulatedDevice {
    fn id(&self) -> &DeviceId {
        &self.inner.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.inner.spec.size
    }

    fn block_size(&self) -> u32 {
        self.inner.spec.block_size
    }

    fn optimal_io_size(&self) -> u32 {
        4096
    }

    fn device_type(&self) -> DriveType {
        DriveType::Emulated
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        self.check(offset, buf.len() as u64)?;
        self.inner.reads.fetch_add(1, Ordering::Relaxed);
        if let (Some(cache), Store::Memory(shards)) = (&self.inner.cache, &self.inner.store) {
            let c = cache.lock().unwrap_or_else(|e| e.into_inner());
            Self::cache_read(shards, &c, offset, buf);
            return Ok(buf.len());
        }
        match &self.inner.store {
            Store::Memory(shards) => Self::mem_read(shards, offset, buf),
            Store::Dir(d) => {
                let out = self.dir_io(Arc::clone(d), offset, DirOp::Read(buf.len())).await?;
                buf.copy_from_slice(&out);
            }
        }
        Ok(buf.len())
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.check(offset, buf.len() as u64)?;
        self.inner.writes.fetch_add(1, Ordering::Relaxed);
        if let (Some(cache), Store::Memory(shards)) = (&self.inner.cache, &self.inner.store) {
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            self.cache_write(shards, &mut c, offset, Some(buf), buf.len() as u64);
            return Ok(buf.len());
        }
        match &self.inner.store {
            Store::Memory(shards) => self.mem_write(shards, offset, Some(buf), buf.len() as u64),
            Store::Dir(d) => {
                self.dir_io(Arc::clone(d), offset, DirOp::Write(buf.to_vec())).await?;
            }
        }
        Ok(buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        if self.is_failed() {
            return Err(eio(&self.inner.spec.name));
        }
        if let (Some(cache), Store::Memory(shards)) = (&self.inner.cache, &self.inner.store) {
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            for (off, data, len) in std::mem::take(&mut c.log) {
                self.mem_write(shards, off, data.as_deref(), len);
            }
            c.view.clear();
        }
        Ok(())
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.zero(offset, len).await
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.zero(offset, len).await
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: !self.is_failed(), ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIB: u64 = 1 << 40;
    const PIB: u64 = 1 << 50;

    fn spec(uri: &str) -> EmulatedSpec {
        EmulatedSpec::parse(uri).unwrap().unwrap()
    }

    #[test]
    fn sizes_and_uris_parse() {
        assert_eq!(parse_size("256T"), Some(256 * TIB));
        assert_eq!(parse_size("1P"), Some(PIB));
        assert_eq!(parse_size("1PiB"), Some(PIB));
        assert_eq!(parse_size("64M"), Some(64 << 20));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("1X"), None);
        let s = spec("emulated://d1?size=1P&backing=/tmp/x&lbs=512");
        assert_eq!((s.name.as_str(), s.size, s.block_size), ("d1", PIB, 512));
        assert_eq!(s.backing, Some(PathBuf::from("/tmp/x")));
        assert!(EmulatedSpec::parse("emulated://d1").unwrap().is_err(), "size is required");
        assert!(EmulatedSpec::parse("emulated://?size=1T").unwrap().is_err(), "a name is required");
        assert!(EmulatedSpec::parse("emulated://d1?size=1T&color=red").unwrap().is_err());
        assert!(EmulatedSpec::parse("/dev/sda").is_none());
        assert_eq!(spec(&s.uri()), s);
    }

    #[tokio::test]
    async fn a_petabyte_drive_stores_only_what_is_written() {
        let d = open(&spec(&format!("emulated://pb-{}?size=1P", uuid::Uuid::new_v4().simple()))).unwrap();
        assert_eq!(d.capacity_bytes(), PIB);
        assert_eq!(d.device_type(), DriveType::Emulated);
        // Far apart: the first and last MiB, and one in the middle.
        for at in [0, PIB / 2 + 12345 * PAGE, PIB - (1 << 20)] {
            let data: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8 ^ (at >> 20) as u8).collect();
            d.write(at, &data).await.unwrap();
            let mut back = vec![0u8; 1 << 20];
            d.read(at, &mut back).await.unwrap();
            assert_eq!(back, data);
        }
        assert_eq!(d.stored_bytes(), 3 << 20, "three MiB written, three held");
        let mut z = vec![1u8; 8192];
        d.read(TIB, &mut z).await.unwrap();
        assert!(z.iter().all(|&b| b == 0), "never written reads as zeros");
        // A 64 GiB zero range (a 1 PiB slab's table) costs nothing.
        d.write_zeroes(4 * TIB, 64 << 30).await.unwrap();
        assert_eq!(d.stored_bytes(), 3 << 20);
        // Discard and zeros give the space back.
        d.discard(0, 1 << 20).await.unwrap();
        d.write(PIB - (1 << 20), &vec![0u8; 1 << 20]).await.unwrap();
        assert_eq!(d.stored_bytes(), 1 << 20);
        assert!(d.read(PIB, &mut [0u8; 4096]).await.is_err(), "past the end is refused");
    }

    /// #172: a volatile drive loses what was not flushed, keeps what was.
    #[tokio::test]
    async fn a_volatile_drive_keeps_what_was_flushed_through_a_power_cut() {
        let name = format!("vol-{}", uuid::Uuid::new_v4().simple());
        let s = spec(&format!("emulated://{name}?size=1T&volatile=1"));
        assert!(s.volatile && spec(&s.uri()).volatile);
        assert!(EmulatedSpec::parse("emulated://x?size=1T&volatile=1&backing=/tmp/y").unwrap().is_err());
        let d = open(&s).unwrap();
        d.write(0, &[1u8; 8192]).await.unwrap();
        d.flush().await.unwrap();
        d.write(4096, &[2u8; 100]).await.unwrap();
        d.write(TIB / 2, &[3u8; 4096]).await.unwrap();
        d.write_zeroes(0, 4096).await.unwrap();
        // Reads see the cache.
        let mut b = vec![0u8; 8192];
        d.read(0, &mut b).await.unwrap();
        assert!(b[..4096].iter().all(|&x| x == 0) && b[4096..4196].iter().all(|&x| x == 2) && b[4196..].iter().all(|&x| x == 1));
        // A cut that keeps nothing: what was flushed, and only that.
        assert_eq!(crash(&name, 1, 0.0), Some(3));
        d.read(0, &mut b).await.unwrap();
        assert!(b.iter().all(|&x| x == 1), "the flushed write, whole");
        let mut far = vec![9u8; 4096];
        d.read(TIB / 2, &mut far).await.unwrap();
        assert!(far.iter().all(|&x| x == 0), "the unflushed write is gone");
        // A cut that keeps everything: as if flushed.
        d.write(TIB / 2, &[4u8; 4096]).await.unwrap();
        assert_eq!(crash(&name, 2, 1.0), Some(1));
        d.read(TIB / 2, &mut far).await.unwrap();
        assert!(far.iter().all(|&x| x == 4));
        assert_eq!(crash("no-such-drive", 0, 0.5), None);
    }

    #[tokio::test]
    async fn one_name_is_one_drive_and_it_fails_on_command() {
        let name = format!("same-{}", uuid::Uuid::new_v4().simple());
        let s = spec(&format!("emulated://{name}?size=256T"));
        let a = open(&s).unwrap();
        a.write(4096, b"hello world, 4096 bytes of it......").await.unwrap();
        let b = open(&s).unwrap();
        let mut buf = vec![0u8; 11];
        b.read(4096, &mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world", "the same drive, not a fresh one");
        assert!(open(&spec(&format!("emulated://{name}?size=1P"))).is_err(), "another size is refused");
        assert_eq!(a.id(), b.id());

        assert!(set_failed(&s.uri(), true));
        assert!(b.read(0, &mut [0u8; 4096]).await.is_err());
        assert!(a.write(0, &[1u8; 4096]).await.is_err());
        assert!(a.flush().await.is_err());
        assert!(!a.smart_status().unwrap().healthy);
        assert!(set_failed(&name, false));
        a.write(0, &[1u8; 4096]).await.unwrap();
        assert!(!set_failed("no-such-drive", true));
    }

    #[tokio::test]
    async fn a_directory_backing_keeps_the_data_sparse_and_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("dir-{}", uuid::Uuid::new_v4().simple());
        let uri = format!("emulated://{name}?size=256T&backing={}", dir.path().display());
        let d = open(&spec(&uri)).unwrap();
        let at = 200 * TIB + (CHUNK - 4096); // across a chunk boundary
        let data: Vec<u8> = (0..16384).map(|i| (i % 253) as u8).collect();
        d.write(at, &data).await.unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(files.len(), 2, "two chunk files, nothing else");
        // Same directory, another process's view: forget the open drive.
        registry().lock().unwrap().remove(&name);
        let d2 = open(&spec(&uri)).unwrap();
        let mut back = vec![0u8; 16384];
        d2.read(at, &mut back).await.unwrap();
        assert_eq!(back, data);
        assert!(d2.stored_bytes() > 0);
        let mut z = vec![9u8; 4096];
        d2.read(100 * TIB, &mut z).await.unwrap();
        assert!(z.iter().all(|&b| b == 0));
        // A whole chunk zeroed is a file removed.
        d2.write_zeroes((at / CHUNK + 1) * CHUNK, CHUNK).await.unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// What the directory holds, the slow way (what the drive used to rescan).
    fn held_in(dir: &std::path::Path) -> u64 {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| held_bytes(&e.metadata().unwrap())).sum()
    }

    /// #300: the running count is what the directory holds, after writes,
    /// overwrites (counted once), zero writes, punches, a whole chunk
    /// removed, more chunks than handles kept open, and a reopen; and a
    /// zero write where nothing is stored creates no file.
    #[tokio::test]
    async fn a_directory_backing_counts_what_it_holds_without_rescanning() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("cnt-{}", uuid::Uuid::new_v4().simple());
        let uri = format!("emulated://{name}?size=1P&backing={}", dir.path().display());
        let d = open(&spec(&uri)).unwrap();
        let block: Vec<u8> = (0..65536).map(|i| (i % 251) as u8 + 1).collect();
        let check = |d: &EmulatedDevice, what: &str| {
            assert_eq!(d.stored_bytes(), held_in(dir.path()), "{what}");
        };
        d.write(0, &block).await.unwrap();
        check(&d, "one write");
        for _ in 0..5 {
            d.write(0, &block).await.unwrap();
        }
        check(&d, "the same range overwritten");
        d.write(3 * CHUNK + 4096, &vec![0u8; 65536]).await.unwrap();
        assert!(!dir.path().join(format!("{:012x}.chunk", 3)).exists(), "zeros create nothing");
        d.write(1 << 20, &block).await.unwrap();
        d.write(1 << 20, &vec![0u8; 65536]).await.unwrap();
        check(&d, "a zero write over data");
        d.write_zeroes(0, 32768).await.unwrap();
        check(&d, "a punch");
        // More chunks than handles kept open: every one still written and read.
        let many = OPEN_CHUNKS as u64 + 40;
        for c in 0..many {
            d.write(c * CHUNK + 8192, &block[..4096]).await.unwrap();
        }
        check(&d, "many chunks");
        let mut back = vec![0u8; 4096];
        for c in [0, 1, many / 2, many - 1] {
            d.read(c * CHUNK + 8192, &mut back).await.unwrap();
            assert_eq!(back, block[..4096], "chunk {c}");
        }
        d.write_zeroes(5 * CHUNK, CHUNK).await.unwrap();
        assert!(!dir.path().join(format!("{:012x}.chunk", 5)).exists());
        check(&d, "a chunk removed");
        let before = d.stored_bytes();
        registry().lock().unwrap().remove(&name);
        let d2 = open(&spec(&uri)).unwrap();
        assert_eq!(d2.stored_bytes(), before, "the same count after a reopen");
        d2.write(5 * CHUNK, &block).await.unwrap();
        check(&d2, "a removed chunk written again");
    }
}
