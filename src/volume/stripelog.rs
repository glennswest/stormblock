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

    /// Mark a stripe dirty. Persists only when the stripe was clean.
    pub fn mark(&self, stripe: u64) -> std::io::Result<()> {
        let newly = self.dirty.lock().unwrap().insert(stripe);
        if newly {
            self.persist()
        } else {
            Ok(())
        }
    }

    /// Everything that is dirty right now.
    pub fn dirty(&self) -> Vec<u64> {
        self.dirty.lock().unwrap().iter().copied().collect()
    }

    /// Forget every mark — after a flush, or after the stripes were verified.
    pub fn clear(&self) -> std::io::Result<()> {
        self.dirty.lock().unwrap().clear();
        self.persist()
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

    fn persist(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let set: Vec<u64> = self.dirty.lock().unwrap().iter().copied().collect();
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
            for s in &set {
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
mod tests {
    use super::*;

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
