//! Page-aligned buffers for O_DIRECT I/O (4096-byte aligned).
//!
//! Drives are opened by the kernel and read with O_DIRECT and io_uring
//! (#140). The hugepage allocator and IOVA lookup a userspace VFIO NVMe
//! driver would have needed went with that driver's stub (#167, decision B).

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Default alignment for DMA buffers (4096 = one page, required for O_DIRECT).
pub const DMA_ALIGNMENT: usize = 4096;

/// A buffer with guaranteed page alignment, for O_DIRECT I/O.
pub struct DmaBuf {
    ptr: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// Safety: DmaBuf is a unique owner of its allocation, safe to send/share.
unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

impl DmaBuf {
    /// Allocate a page-aligned buffer of exactly `size` bytes.
    /// Size is rounded up to a multiple of `DMA_ALIGNMENT`.
    pub fn alloc(size: usize) -> Self {
        // Never zero: a zero-size layout is undefined behaviour to allocate.
        let capacity = align_up(size.max(1), DMA_ALIGNMENT);
        let layout = Layout::from_size_align(capacity, DMA_ALIGNMENT)
            .expect("invalid DmaBuf layout");
        // Safety: layout has non-zero size (align_up guarantees >= DMA_ALIGNMENT).
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).expect("DmaBuf allocation failed (OOM)");
        DmaBuf {
            ptr,
            len: size,
            capacity,
        }
    }

    /// Allocate a zeroed buffer that can hold at least `size` bytes.
    pub fn zeroed(size: usize) -> Self {
        Self::alloc(size)
    }

    /// The usable length of this buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The allocated capacity (always >= len, aligned to DMA_ALIGNMENT).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Raw pointer to the buffer data.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Mutable raw pointer to the buffer data.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Deref for DmaBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // Safety: ptr is valid for `len` bytes, allocated and zeroed.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for DmaBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // Safety: ptr is valid for `len` bytes, we have unique ownership.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, DMA_ALIGNMENT).expect("invalid layout in DmaBuf drop");
        // Safety: ptr was allocated with this layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) }
    }
}

impl std::fmt::Debug for DmaBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmaBuf").field("len", &self.len).field("capacity", &self.capacity).finish()
    }
}

/// Round `val` up to the next multiple of `align`.
fn align_up(val: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (val + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_and_write() {
        let mut buf = DmaBuf::alloc(4096);
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.capacity(), 4096);
        assert!(buf.as_ptr() as usize % DMA_ALIGNMENT == 0);

        // Write a pattern and read it back
        buf[0] = 0xAB;
        buf[4095] = 0xCD;
        assert_eq!(buf[0], 0xAB);
        assert_eq!(buf[4095], 0xCD);
    }

    #[test]
    fn alloc_rounds_up() {
        let buf = DmaBuf::alloc(100);
        assert_eq!(buf.len(), 100);
        assert_eq!(buf.capacity(), 4096);
        assert!(buf.as_ptr() as usize % DMA_ALIGNMENT == 0);
    }

    #[test]
    fn zeroed_buffer() {
        let buf = DmaBuf::zeroed(8192);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn align_up_works() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }
}
