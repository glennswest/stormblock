//! Write-intent bitmap journal for crash recovery.
//!
//! Tracks which stripes have in-flight writes. On unclean shutdown,
//! dirty stripes have their parity re-verified/recomputed on startup.

use bitvec::prelude::*;
use std::path::{Path, PathBuf};

/// Write-intent journal: a persistent bitmap tracking dirty stripes.
///
/// Before writing a stripe, mark it dirty. After the write (including parity)
/// is fully committed, mark it clean. On crash recovery, any stripes still
/// marked dirty need parity verification.
pub struct WriteIntentJournal {
    /// One bit per stripe: 1 = dirty (in-flight write), 0 = clean.
    bitmap: BitVec<u8, Lsb0>,
    /// Total number of stripes tracked.
    stripe_count: u64,
    /// Path to persist the bitmap.
    path: PathBuf,
    /// Number of dirty stripes (cached counter).
    dirty_count: u64,
}

impl WriteIntentJournal {
    /// Create a new journal for `stripe_count` stripes, persisted at `path`.
    ///
    /// If the file exists, loads it (for recovery). Otherwise creates a clean bitmap.
    pub fn open(path: &Path, stripe_count: u64) -> std::io::Result<Self> {
        let bit_count = stripe_count as usize;

        if path.exists() {
            // Load existing bitmap for recovery
            let data = std::fs::read(path)?;
            let expected_bytes = bit_count.div_ceil(8);
            if data.len() >= expected_bytes {
                let mut bitmap = BitVec::<u8, Lsb0>::from_vec(data);
                bitmap.truncate(bit_count);
                let dirty_count = bitmap.count_ones() as u64;
                return Ok(WriteIntentJournal {
                    bitmap,
                    stripe_count,
                    path: path.to_path_buf(),
                    dirty_count,
                });
            }
            // File too small/corrupt — start fresh
        }

        let bitmap = bitvec![u8, Lsb0; 0; bit_count];
        Ok(WriteIntentJournal {
            bitmap,
            stripe_count,
            path: path.to_path_buf(),
            dirty_count: 0,
        })
    }

    /// Create an in-memory-only journal (no persistence). For testing.
    pub fn in_memory(stripe_count: u64) -> Self {
        let bit_count = stripe_count as usize;
        WriteIntentJournal {
            bitmap: bitvec![u8, Lsb0; 0; bit_count],
            stripe_count,
            path: PathBuf::new(),
            dirty_count: 0,
        }
    }

    /// Mark a stripe as dirty (write in progress).
    pub fn mark_dirty(&mut self, stripe: u64) {
        let idx = stripe as usize;
        if idx < self.bitmap.len() && !self.bitmap[idx] {
            self.bitmap.set(idx, true);
            self.dirty_count += 1;
        }
    }

    /// Mark a stripe as clean (write completed).
    pub fn mark_clean(&mut self, stripe: u64) {
        let idx = stripe as usize;
        if idx < self.bitmap.len() && self.bitmap[idx] {
            self.bitmap.set(idx, false);
            self.dirty_count -= 1;
        }
    }

    /// Return list of all dirty stripe indices (for recovery).
    pub fn dirty_stripes(&self) -> Vec<u64> {
        self.bitmap
            .iter_ones()
            .map(|i| i as u64)
            .collect()
    }

    /// Number of currently dirty stripes.
    pub fn dirty_count(&self) -> u64 {
        self.dirty_count
    }

    /// Total stripes tracked.
    pub fn stripe_count(&self) -> u64 {
        self.stripe_count
    }

    /// Is a specific stripe dirty?
    pub fn is_dirty(&self, stripe: u64) -> bool {
        let idx = stripe as usize;
        idx < self.bitmap.len() && self.bitmap[idx]
    }

    /// Flush the bitmap to disk.
    pub fn flush(&self) -> std::io::Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(()); // In-memory only
        }
        let raw = self.bitmap.as_raw_slice();
        std::fs::write(&self.path, raw)?;
        Ok(())
    }

    /// Mark all stripes clean and flush.
    pub fn clear_all(&mut self) -> std::io::Result<()> {
        self.bitmap.fill(false);
        self.dirty_count = 0;
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_dirty_and_clean() {
        let mut j = WriteIntentJournal::in_memory(1000);
        assert_eq!(j.dirty_count(), 0);
        assert!(j.dirty_stripes().is_empty());

        j.mark_dirty(0);
        j.mark_dirty(500);
        j.mark_dirty(999);
        assert_eq!(j.dirty_count(), 3);
        assert!(j.is_dirty(0));
        assert!(j.is_dirty(500));
        assert!(!j.is_dirty(1));

        let dirty = j.dirty_stripes();
        assert_eq!(dirty, vec![0, 500, 999]);

        j.mark_clean(500);
        assert_eq!(j.dirty_count(), 2);
        assert!(!j.is_dirty(500));
    }

    #[test]
    fn double_dirty_no_double_count() {
        let mut j = WriteIntentJournal::in_memory(100);
        j.mark_dirty(42);
        j.mark_dirty(42); // second mark should be no-op
        assert_eq!(j.dirty_count(), 1);
    }

    #[test]
    fn double_clean_no_underflow() {
        let mut j = WriteIntentJournal::in_memory(100);
        j.mark_dirty(42);
        j.mark_clean(42);
        j.mark_clean(42); // second clean should be no-op
        assert_eq!(j.dirty_count(), 0);
    }

    #[test]
    fn persist_and_reload() {
        let dir = std::env::temp_dir().join("stormblock-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-journal.bin");
        let _ = std::fs::remove_file(&path);

        // Create journal, mark some dirty, flush
        {
            let mut j = WriteIntentJournal::open(&path, 256).unwrap();
            j.mark_dirty(10);
            j.mark_dirty(100);
            j.mark_dirty(200);
            j.flush().unwrap();
        }

        // Reload and verify dirty stripes survived
        {
            let j = WriteIntentJournal::open(&path, 256).unwrap();
            assert_eq!(j.dirty_count(), 3);
            assert!(j.is_dirty(10));
            assert!(j.is_dirty(100));
            assert!(j.is_dirty(200));
            assert!(!j.is_dirty(50));
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_all_resets() {
        let mut j = WriteIntentJournal::in_memory(100);
        j.mark_dirty(1);
        j.mark_dirty(50);
        j.mark_dirty(99);
        assert_eq!(j.dirty_count(), 3);

        j.clear_all().unwrap();
        assert_eq!(j.dirty_count(), 0);
        assert!(j.dirty_stripes().is_empty());
    }
}
