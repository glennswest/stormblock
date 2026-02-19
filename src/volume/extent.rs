//! Extent allocator — free-space bitmap, contiguous allocation.

use std::collections::HashMap;
use std::fmt;

use bitvec::prelude::*;
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::raid::RaidArrayId;

/// Default extent size: 4 MB.
pub const DEFAULT_EXTENT_SIZE: u64 = 4 * 1024 * 1024;

/// Unique identifier for a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub Uuid);

impl VolumeId {
    pub fn new() -> Self {
        VolumeId(Uuid::new_v4())
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A physical extent on a RAID array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub array_id: RaidArrayId,
    pub offset: u64,
    pub length: u64,
}

/// Bitmap tracking free/allocated extents on a single RAID array.
///
/// bit=1 → free, bit=0 → allocated.
#[derive(Debug)]
pub struct ExtentBitmap {
    bitmap: BitVec<u8, Lsb0>,
    total_extents: u64,
    free_extents: u64,
    /// Hint: start scanning from this position for next allocation.
    hint: u64,
}

impl ExtentBitmap {
    /// Create a new bitmap where all extents are free.
    pub fn new(total_extents: u64) -> Self {
        let mut bitmap = BitVec::repeat(true, total_extents as usize);
        // All bits set = all free
        let _ = &mut bitmap; // suppress unused warning
        ExtentBitmap {
            bitmap,
            total_extents,
            free_extents: total_extents,
            hint: 0,
        }
    }

    /// Allocate `count` extents. Returns starting indices.
    /// Tries to find contiguous extents first, falls back to scattered allocation.
    pub fn allocate(&mut self, count: u64) -> Option<Vec<u64>> {
        if count == 0 || count > self.free_extents {
            return None;
        }

        // Try contiguous allocation starting from hint
        if let Some(start) = self.find_contiguous(count) {
            let mut indices = Vec::with_capacity(count as usize);
            for i in start..start + count {
                self.bitmap.set(i as usize, false);
                indices.push(i);
            }
            self.free_extents -= count;
            self.hint = start + count;
            if self.hint >= self.total_extents {
                self.hint = 0;
            }
            return Some(indices);
        }

        // Fall back to scattered allocation
        let mut indices = Vec::with_capacity(count as usize);
        let mut pos = self.hint;
        let start_pos = pos;
        let mut wrapped = false;

        while (indices.len() as u64) < count {
            if pos >= self.total_extents {
                if wrapped {
                    return None; // Should not happen if free_extents >= count
                }
                pos = 0;
                wrapped = true;
            }
            if wrapped && pos >= start_pos {
                return None;
            }
            if self.bitmap[pos as usize] {
                self.bitmap.set(pos as usize, false);
                indices.push(pos);
            }
            pos += 1;
        }

        self.free_extents -= count;
        self.hint = pos;
        if self.hint >= self.total_extents {
            self.hint = 0;
        }
        Some(indices)
    }

    /// Find a contiguous run of `count` free extents, starting from hint with wrap-around.
    fn find_contiguous(&self, count: u64) -> Option<u64> {
        if count > self.total_extents {
            return None;
        }

        let total = self.total_extents;
        let mut run_start = self.hint;
        let mut run_len = 0u64;
        let mut checked = 0u64;

        let mut pos = self.hint;
        while checked < total {
            let idx = pos % total;
            if self.bitmap[idx as usize] {
                if run_len == 0 {
                    run_start = idx;
                }
                run_len += 1;
                if run_len >= count {
                    // Verify the run doesn't wrap around (contiguous means sequential)
                    if run_start + count <= total {
                        return Some(run_start);
                    }
                }
            } else {
                run_len = 0;
            }
            pos += 1;
            checked += 1;
        }
        None
    }

    /// Free an extent at the given index.
    pub fn free(&mut self, index: u64) {
        debug_assert!(index < self.total_extents);
        debug_assert!(!self.bitmap[index as usize], "double free of extent {index}");
        self.bitmap.set(index as usize, true);
        self.free_extents += 1;
        // Update hint if freed extent is before current hint
        if index < self.hint {
            self.hint = index;
        }
    }

    pub fn total(&self) -> u64 {
        self.total_extents
    }

    pub fn free_count(&self) -> u64 {
        self.free_extents
    }

    pub fn allocated_count(&self) -> u64 {
        self.total_extents - self.free_extents
    }
}

/// Manages extent allocation across multiple RAID arrays.
#[derive(Debug)]
pub struct ExtentAllocator {
    bitmaps: HashMap<RaidArrayId, ExtentBitmap>,
    extent_size: u64,
}

impl ExtentAllocator {
    pub fn new(extent_size: u64) -> Self {
        ExtentAllocator {
            bitmaps: HashMap::new(),
            extent_size,
        }
    }

    /// Register a RAID array with the allocator.
    pub fn add_array(&mut self, array_id: RaidArrayId, capacity: u64) {
        let total_extents = capacity / self.extent_size;
        self.bitmaps.insert(array_id, ExtentBitmap::new(total_extents));
    }

    /// Allocate `count` extents from a specific array.
    pub fn allocate(&mut self, array_id: RaidArrayId, count: u64) -> Option<Vec<Extent>> {
        let bitmap = self.bitmaps.get_mut(&array_id)?;
        let indices = bitmap.allocate(count)?;
        Some(indices.into_iter().map(|idx| Extent {
            array_id,
            offset: idx * self.extent_size,
            length: self.extent_size,
        }).collect())
    }

    /// Free an extent.
    pub fn free(&mut self, extent: &Extent) {
        if let Some(bitmap) = self.bitmaps.get_mut(&extent.array_id) {
            let index = extent.offset / self.extent_size;
            bitmap.free(index);
        }
    }

    /// Free space on an array in extents.
    pub fn free_count(&self, array_id: &RaidArrayId) -> u64 {
        self.bitmaps.get(array_id).map(|b| b.free_count()).unwrap_or(0)
    }

    /// Total space on an array in extents.
    pub fn total_count(&self, array_id: &RaidArrayId) -> u64 {
        self.bitmaps.get(array_id).map(|b| b.total()).unwrap_or(0)
    }

    /// Extent size in bytes.
    pub fn extent_size(&self) -> u64 {
        self.extent_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_array_id() -> RaidArrayId {
        RaidArrayId(Uuid::new_v4())
    }

    #[test]
    fn bitmap_allocate_and_free() {
        let mut bm = ExtentBitmap::new(10);
        assert_eq!(bm.free_count(), 10);
        assert_eq!(bm.total(), 10);

        let indices = bm.allocate(3).unwrap();
        assert_eq!(indices.len(), 3);
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(bm.free_count(), 7);

        // Free middle extent
        bm.free(1);
        assert_eq!(bm.free_count(), 8);

        // Allocate 1 — should reuse freed extent
        let indices2 = bm.allocate(1).unwrap();
        assert_eq!(indices2.len(), 1);
        assert_eq!(bm.free_count(), 7);
    }

    #[test]
    fn bitmap_exhaustion() {
        let mut bm = ExtentBitmap::new(4);
        let _ = bm.allocate(4).unwrap();
        assert_eq!(bm.free_count(), 0);
        assert!(bm.allocate(1).is_none());
    }

    #[test]
    fn bitmap_scattered_allocation() {
        let mut bm = ExtentBitmap::new(8);
        // Allocate all
        let _ = bm.allocate(8).unwrap();
        // Free alternating extents: 0, 2, 4, 6
        bm.free(0);
        bm.free(2);
        bm.free(4);
        bm.free(6);
        assert_eq!(bm.free_count(), 4);

        // Request 4 — can't be contiguous, should scatter
        let indices = bm.allocate(4).unwrap();
        assert_eq!(indices.len(), 4);
        assert_eq!(bm.free_count(), 0);
    }

    #[test]
    fn allocator_multi_array() {
        let a1 = test_array_id();
        let a2 = test_array_id();
        let mut alloc = ExtentAllocator::new(DEFAULT_EXTENT_SIZE);
        alloc.add_array(a1, 100 * DEFAULT_EXTENT_SIZE);
        alloc.add_array(a2, 50 * DEFAULT_EXTENT_SIZE);

        assert_eq!(alloc.total_count(&a1), 100);
        assert_eq!(alloc.total_count(&a2), 50);

        let extents = alloc.allocate(a1, 10).unwrap();
        assert_eq!(extents.len(), 10);
        assert_eq!(alloc.free_count(&a1), 90);

        // Free one
        alloc.free(&extents[5]);
        assert_eq!(alloc.free_count(&a1), 91);
    }

    #[test]
    fn allocator_unknown_array() {
        let mut alloc = ExtentAllocator::new(DEFAULT_EXTENT_SIZE);
        let bogus = test_array_id();
        assert!(alloc.allocate(bogus, 1).is_none());
        assert_eq!(alloc.free_count(&bogus), 0);
    }

    #[test]
    fn extent_offsets_correct() {
        let a = test_array_id();
        let mut alloc = ExtentAllocator::new(DEFAULT_EXTENT_SIZE);
        alloc.add_array(a, 10 * DEFAULT_EXTENT_SIZE);

        let extents = alloc.allocate(a, 3).unwrap();
        assert_eq!(extents[0].offset, 0);
        assert_eq!(extents[1].offset, DEFAULT_EXTENT_SIZE);
        assert_eq!(extents[2].offset, 2 * DEFAULT_EXTENT_SIZE);
        assert!(extents.iter().all(|e| e.length == DEFAULT_EXTENT_SIZE));
        assert!(extents.iter().all(|e| e.array_id == a));
    }
}
