//! A live mirror of an in-use volume to a peer engine's namespace (#295,
//! stormstorage#44, stormcluster#53).
//!
//! A VM's disk moves to another node while the guest runs: this engine (S)
//! serves the volume, so only it can send every write to a second copy. The
//! caller makes a fresh thin volume on the target engine (T), attaches it for
//! S over NVMe/TCP, and asks S to mirror into it. From then on:
//!
//! - every write to the volume also goes to T, under a per-extent lock held
//!   across both, and is acknowledged once both have it; a discard that
//!   unmapped a whole extent here makes T read zeros there; a flush flushes T
//!   too;
//! - a background copy walks the extents mapped when it started, each under
//!   the same extent lock, so a write lands either before the copy reads the
//!   extent (and is copied) or after it (and is mirrored). An extent that
//!   reads as zeros is skipped: T is fresh, so it reads zeros there too, and
//!   stays thin. Foreground I/O goes first;
//! - once the copy is through, the mirror is `synced`: T has every byte;
//! - an error on T never reaches the guest: the mirror goes `failed`, says
//!   why, and the volume carries on as before.
//!
//! The switch-over (and the abort) is `stop`: in-flight writes finish, T is
//! flushed, the mirror is dropped. S's volume is never changed by any of it.
//!
//! A mirror lives in this process. It is recorded on disk by the API
//! (`mirrors.json`) so that after a restart it reports `failed` rather than
//! vanishing: it never silently stops.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::drive::{BlockDevice, DriveError};

/// Extent locks, sharded: a write takes the shards of the extents it
/// touches, in order; the copy takes one.
const SHARDS: usize = 256;

/// What `GET /api/v1/volumes/{id}` says of a mirror.
#[derive(Debug, Clone, Serialize)]
pub struct MirrorStatus {
    pub target: String,
    /// `copying`, `synced` or `failed`.
    pub state: &'static str,
    pub bytes_total: u64,
    pub bytes_remaining: u64,
    pub message: Option<String>,
    /// Seconds since the epoch.
    pub started_at: u64,
}

pub struct LiveMirror {
    target: String,
    dev: Arc<dyn BlockDevice>,
    extent: u64,
    shards: Vec<tokio::sync::Mutex<()>>,
    failed: AtomicBool,
    synced: AtomicBool,
    stopping: AtomicBool,
    bytes_total: AtomicU64,
    bytes_remaining: AtomicU64,
    message: std::sync::Mutex<Option<String>>,
    started_at: u64,
}

impl LiveMirror {
    /// A mirror into `dev` (named `target`), for a volume of `extent`-sized
    /// extents. Copying until `copy` says otherwise.
    pub fn new(target: String, dev: Arc<dyn BlockDevice>, extent: u64) -> Arc<Self> {
        Arc::new(LiveMirror {
            target,
            dev,
            extent: extent.max(4096),
            shards: (0..SHARDS).map(|_| tokio::sync::Mutex::new(())).collect(),
            failed: AtomicBool::new(false),
            synced: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            bytes_total: AtomicU64::new(0),
            bytes_remaining: AtomicU64::new(0),
            message: std::sync::Mutex::new(None),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn status(&self) -> MirrorStatus {
        let state = if self.failed.load(Ordering::SeqCst) {
            "failed"
        } else if self.synced.load(Ordering::SeqCst) {
            "synced"
        } else {
            "copying"
        };
        MirrorStatus {
            target: self.target.clone(),
            state,
            bytes_total: self.bytes_total.load(Ordering::SeqCst),
            bytes_remaining: self.bytes_remaining.load(Ordering::SeqCst),
            message: self.message.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            started_at: self.started_at,
        }
    }

    /// Still taking writes: not failed, not stopped.
    pub fn is_live(&self) -> bool {
        !self.failed.load(Ordering::SeqCst) && !self.stopping.load(Ordering::SeqCst)
    }

    /// Give up on T, saying why. The volume goes on without it.
    pub fn fail(&self, why: impl Into<String>) {
        let why = why.into();
        if !self.failed.swap(true, Ordering::SeqCst) {
            tracing::error!("live mirror to {}: failed: {why}; the volume carries on without it", self.target);
            *self.message.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
        }
    }

    /// The extent locks for `[offset, offset + len)`, in shard order.
    pub async fn lock_range(&self, offset: u64, len: u64) -> Vec<tokio::sync::MutexGuard<'_, ()>> {
        let first = offset / self.extent;
        let last = (offset + len.max(1) - 1) / self.extent;
        let mut idx: Vec<usize> = (first..=last.min(first + SHARDS as u64)).map(|x| (x % SHARDS as u64) as usize).collect();
        idx.sort_unstable();
        idx.dedup();
        let mut out = Vec::with_capacity(idx.len());
        for i in idx {
            out.push(self.shards[i].lock().await);
        }
        out
    }

    /// Send a write that landed here to T.
    pub async fn write(&self, offset: u64, buf: &[u8]) {
        if !self.is_live() || buf.is_empty() {
            return;
        }
        if let Err(e) = self.dev.write(offset, buf).await {
            self.fail(format!("write of {} bytes at {offset}: {e}", buf.len()));
        }
    }

    /// Make `[offset, offset + len)` read zeros on T (a discard here
    /// unmapped it).
    pub async fn zero(&self, offset: u64, len: u64) {
        if !self.is_live() || len == 0 {
            return;
        }
        if let Err(e) = self.dev.write_zeroes(offset, len).await {
            self.fail(format!("zeroing {len} bytes at {offset}: {e}"));
        }
    }

    /// Flush T after a flush here.
    pub async fn flush(&self) {
        if !self.is_live() {
            return;
        }
        if let Err(e) = self.dev.flush().await {
            self.fail(format!("flush: {e}"));
        }
    }

    /// Copy the extents mapped on `vol` when the mirror began, then say
    /// `synced`. `mapped` is that list; `capacity` the volume's size.
    pub async fn copy(self: Arc<Self>, vol: Arc<dyn BlockDevice>, mapped: Vec<u64>, capacity: u64) {
        let ext = self.extent;
        self.bytes_total.store(mapped.len() as u64 * ext, Ordering::SeqCst);
        self.bytes_remaining.store(mapped.len() as u64 * ext, Ordering::SeqCst);
        let mut buf = vec![0u8; ext as usize];
        let mut last_fg = super::thin::FOREGROUND_IO.load(Ordering::Relaxed);
        let mut last_took = Duration::ZERO;
        let (mut copied, mut skipped) = (0u64, 0u64);
        let began = Instant::now();
        for x in mapped {
            if !self.is_live() {
                return;
            }
            // Foreground first: after volume I/O since the last extent, wait
            // a quarter of what that extent took (at most 50 ms).
            let fg = super::thin::FOREGROUND_IO.load(Ordering::Relaxed);
            if fg != last_fg {
                tokio::time::sleep((last_took / 4).min(Duration::from_millis(50))).await;
            }
            let t = Instant::now();
            let at = x * ext;
            if at >= capacity {
                self.bytes_remaining.fetch_sub(ext, Ordering::SeqCst);
                continue;
            }
            let n = ext.min(capacity - at) as usize;
            {
                let _g = self.lock_range(at, n as u64).await;
                if let Err(e) = vol.read(at, &mut buf[..n]).await {
                    self.fail(format!("reading extent {x} here to copy it: {e}"));
                    return;
                }
                if buf[..n].iter().all(|&b| b == 0) {
                    skipped += 1;
                } else if let Err(e) = self.dev.write(at, &buf[..n]).await {
                    self.fail(format!("copying extent {x}: {e}"));
                    return;
                } else {
                    copied += 1;
                }
            }
            self.bytes_remaining.fetch_sub(ext, Ordering::SeqCst);
            last_took = t.elapsed();
            last_fg = super::thin::FOREGROUND_IO.load(Ordering::Relaxed);
        }
        if !self.is_live() {
            return;
        }
        if let Err(e) = self.dev.flush().await {
            self.fail(format!("flush after the copy: {e}"));
            return;
        }
        self.synced.store(true, Ordering::SeqCst);
        tracing::info!(
            "live mirror to {}: synced ({copied} extent(s) copied, {skipped} all-zero skipped, {:.1} s); writes mirrored",
            self.target,
            began.elapsed().as_secs_f64()
        );
    }

    /// Stop: no more writes go to T. The caller has made in-flight writes
    /// finish first. T is flushed when the mirror is still good; the error,
    /// if the flush fails, is returned (and the mirror says failed).
    pub async fn stop(&self) -> Result<(), DriveError> {
        let live = self.is_live();
        self.stopping.store(true, Ordering::SeqCst);
        if live {
            if let Err(e) = self.dev.flush().await {
                self.fail(format!("flush at the stop: {e}"));
                return Err(e);
            }
        }
        Ok(())
    }
}
