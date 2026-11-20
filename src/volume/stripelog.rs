//! Dirty-stripe log — the parity write hole, bounded.
//!
//! A parity write is two writes, data then parity, and a crash between them
//! leaves the stripe's parity stale. md without a journal has the same hole
//! and closes it with a write-intent bitmap; this is the same idea at stripe
//! granularity. A stripe is marked before its read-modify-write, the mark is
//! persisted (one fsync the *first* time a stripe goes dirty since the last
//! flush), and `flush` — the moment a consumer asked for durability — clears
//! the set. On restart, the stripes left in the log are the only ones whose
//! parity has to be recomputed; without a data directory there is no log and
//! a whole-volume `resync?verify=true` remains the recovery.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

/// Where a volume's dirty stripes are kept between a crash and its restart.
///
/// `dirty` holds only what is durable (#708, finding 07322d3af07a): a stripe
/// joins it after the file that names it is written, synced and renamed into
/// place, never before. A mark that fails leaves nothing behind, so a retry
/// writes the file again rather than finding the stripe "already marked".
/// The file is written under `dirty`'s lock, so two marks never write the
/// same temporary file at once.
pub struct StripeLog {
    path: Option<PathBuf>,
    dirty: std::sync::Mutex<BTreeSet<u64>>,
}

const MAGIC: &[u8; 8] = b"STRMDRT\0";

impl StripeLog {
    /// A log that records nothing — no data directory.
    pub fn none() -> Self {
        StripeLog { path: None, dirty: std::sync::Mutex::new(BTreeSet::new()) }
    }

    /// The log for a volume under `dir`.
    pub fn at(dir: &std::path::Path, volume: uuid::Uuid) -> Self {
        StripeLog {
            path: Some(dir.join(format!("stripes-{}.log", volume.simple()))),
            dirty: std::sync::Mutex::new(BTreeSet::new()),
        }
    }

    pub fn is_persistent(&self) -> bool {
        self.path.is_some()
    }

    /// Mark a stripe dirty, durably: `Ok` only once the log on disk names it
    /// (one write the first time a stripe goes dirty since the last clear).
    /// On an error the stripe is not marked, and the caller must not touch
    /// the stripe (#708).
    pub fn mark(&self, stripe: u64) -> std::io::Result<()> {
        let mut dirty = self.dirty.lock().unwrap();
        if dirty.contains(&stripe) {
            return Ok(());
        }
        let mut next = dirty.clone();
        next.insert(stripe);
        self.write(&next)?;
        *dirty = next;
        Ok(())
    }

    /// Everything that is dirty right now.
    pub fn dirty(&self) -> Vec<u64> {
        self.dirty.lock().unwrap().iter().copied().collect()
    }

    /// Forget every mark — after a flush, or after the stripes were verified.
    /// When the file cannot be removed the marks are kept: a restart then
    /// verifies stripes that are fine, never misses one that is not.
    pub fn clear(&self) -> std::io::Result<()> {
        let mut dirty = self.dirty.lock().unwrap();
        self.write(&BTreeSet::new())?;
        dirty.clear();
        Ok(())
    }

    /// What a previous run left behind, if anything. Loads it into the set
    /// so a `clear` after verification removes the file's claim too.
    pub fn load(&self) -> Vec<u64> {
        let Some(path) = &self.path else { return Vec::new() };
        let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
        if bytes.len() < 8 || &bytes[..8] != MAGIC {
            return Vec::new();
        }
        let mut out = Vec::new();
        for chunk in bytes[8..].chunks_exact(8) {
            out.push(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        self.dirty.lock().unwrap().extend(out.iter().copied());
        out
    }

    /// The log on disk made to name exactly `set`, durably. Called with
    /// `dirty`'s lock held.
    fn write(&self, set: &BTreeSet<u64>) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        #[cfg(test)]
        if let Some(e) = tests::injected_failure() {
            return Err(e);
        }
        if set.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Ok(());
        }
        let tmp = path.with_extension("log.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(MAGIC)?;
            for s in set {
                f.write_all(&s.to_le_bytes())?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        static FAIL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// Fail the next `n` log writes on this thread (ENOSPC), as a full or
    /// failing data directory would.
    pub(crate) fn fail_writes(n: u32) {
        FAIL.with(|f| f.set(n));
    }

    pub(super) fn injected_failure() -> Option<std::io::Error> {
        FAIL.with(|f| {
            let n = f.get();
            (n > 0).then(|| {
                f.set(n - 1);
                std::io::Error::from_raw_os_error(28)
            })
        })
    }

    /// #708 (finding 07322d3af07a): a mark whose write fails is no mark: it
    /// says so, leaves no entry behind, and the retry writes the file, which
    /// a restart then reads.
    #[test]
    fn a_mark_that_cannot_be_written_is_not_a_mark() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        let log = StripeLog::at(dir.path(), id);
        log.mark(1).unwrap();
        fail_writes(1);
        assert!(log.mark(5).is_err(), "the write failed");
        assert_eq!(log.dirty(), vec![1], "no entry for a stripe whose mark is not on disk");
        assert_eq!(StripeLog::at(dir.path(), id).load(), vec![1], "the file is the last good one");
        log.mark(5).unwrap();
        assert_eq!(StripeLog::at(dir.path(), id).load(), vec![1, 5], "the retry is durable");
        // A clear that cannot remove the file keeps the marks.
        fail_writes(1);
        assert!(log.clear().is_err());
        assert_eq!(log.dirty(), vec![1, 5]);
        log.clear().unwrap();
        assert!(StripeLog::at(dir.path(), id).load().is_empty());
    }

    /// Marks from many threads at once each land in the file (#708: the
    /// temporary file was written by any of them at once).
    #[test]
    fn marks_from_many_threads_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        let log = std::sync::Arc::new(StripeLog::at(dir.path(), id));
        let hs: Vec<_> = (0..8u64)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for i in 0..16 {
                        log.mark(t * 100 + i).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(StripeLog::at(dir.path(), id).load().len(), 128);
    }

    #[test]
    fn marks_survive_and_clear_removes_the_file() {
        let dir = std::env::temp_dir().join(format!("stormblock-stripelog-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4();
        let log = StripeLog::at(&dir, id);
        log.mark(7).unwrap();
        log.mark(3).unwrap();
        log.mark(7).unwrap();
        assert_eq!(log.dirty(), vec![3, 7]);

        let again = StripeLog::at(&dir, id);
        assert_eq!(again.load(), vec![3, 7], "a restart sees what was dirty");
        again.clear().unwrap();
        assert!(again.dirty().is_empty());
        let fresh = StripeLog::at(&dir, id);
        assert!(fresh.load().is_empty(), "cleared log leaves nothing behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_with_no_home_records_nothing_but_still_tracks() {
        let log = StripeLog::none();
        log.mark(1).unwrap();
        assert_eq!(log.dirty(), vec![1]);
        assert!(!log.is_persistent());
        assert!(log.load().is_empty());
    }
}
