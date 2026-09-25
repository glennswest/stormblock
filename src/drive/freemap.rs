//! A slab's free-slot bitmap, with a summary so finding the first free slot
//! does not scan the whole map (#145).
//!
//! `BitVec::first_one()` walks from bit 0 on every allocation, so a slab's
//! allocation cost grew with how full it was: measured on dev, 25 µs a slot
//! empty and 168 µs at 90% of 4 M slots — and a 256 TB drive at 1 MiB has
//! 244 M of them. Here the bitmap is split into chunks of [`CHUNK`] slots with
//! a free count each; the first free slot is the first chunk with a non-zero
//! count, then the first set bit inside it. Still first-fit, same answer as
//! before, in O(slots / CHUNK + CHUNK / 64) instead of O(slots).

use bitvec::prelude::*;

/// Slots per summary chunk.
pub const CHUNK: usize = 1 << 16;

#[derive(Debug, Clone)]
pub struct FreeMap {
    bits: BitVec<u8, Lsb0>,
    /// Free slots in each chunk of `CHUNK`.
    free_in: Vec<u32>,
}

impl FreeMap {
    /// `n` slots, all free or all used.
    pub fn new(n: usize, free: bool) -> Self {
        let bits = BitVec::repeat(free, n);
        let mut m = FreeMap { bits, free_in: Vec::new() };
        m.recount();
        m
    }

    fn recount(&mut self) {
        let chunks = self.bits.len().div_ceil(CHUNK);
        self.free_in = (0..chunks)
            .map(|c| {
                let end = ((c + 1) * CHUNK).min(self.bits.len());
                self.bits[c * CHUNK..end].count_ones() as u32
            })
            .collect();
    }

    pub fn len(&self) -> usize {
        self.bits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    pub fn is_free(&self, i: usize) -> bool {
        self.bits.get(i).map(|b| *b).unwrap_or(false)
    }

    /// Mark slot `i` free or used. A no-op when it already is.
    pub fn set(&mut self, i: usize, free: bool) {
        if i >= self.bits.len() || self.bits[i] == free {
            return;
        }
        self.bits.set(i, free);
        let c = &mut self.free_in[i / CHUNK];
        if free {
            *c += 1;
        } else {
            *c -= 1;
        }
    }

    /// The first free slot — first-fit, as `first_one()` answered.
    pub fn first_free(&self) -> Option<usize> {
        let c = self.free_in.iter().position(|n| *n > 0)?;
        let start = c * CHUNK;
        let end = (start + CHUNK).min(self.bits.len());
        self.bits[start..end].first_one().map(|i| start + i)
    }

    /// Grow (or shrink) to `n` slots; new slots take `free`.
    pub fn resize(&mut self, n: usize, free: bool) {
        self.bits.resize(n, free);
        self.recount();
    }

    pub fn count_free(&self) -> usize {
        self.free_in.iter().map(|n| *n as usize).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same answer as a scan, whatever was allocated and freed.
    #[test]
    fn first_free_is_first_fit() {
        let n = 3 * CHUNK + 123;
        let mut m = FreeMap::new(n, true);
        let mut naive = vec![true; n];
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for step in 0..200_000usize {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if step % 3 == 0 {
                let i = (x as usize) % n;
                m.set(i, true);
                naive[i] = true;
            } else if let Some(i) = m.first_free() {
                assert_eq!(Some(i), naive.iter().position(|b| *b), "step {step}");
                m.set(i, false);
                naive[i] = false;
            }
        }
        assert_eq!(m.count_free(), naive.iter().filter(|b| **b).count());
        assert_eq!(m.first_free(), naive.iter().position(|b| *b));
    }

    #[test]
    fn a_full_map_has_nothing_free_and_growing_adds_free_slots_at_the_end() {
        let mut m = FreeMap::new(CHUNK + 5, false);
        assert_eq!(m.first_free(), None);
        m.resize(CHUNK + 10, true);
        assert_eq!(m.first_free(), Some(CHUNK + 5));
        assert_eq!(m.count_free(), 5);
        m.set(3, true);
        m.set(3, true);
        assert_eq!(m.count_free(), 6, "setting a free slot free again changes nothing");
        assert_eq!(m.first_free(), Some(3));
    }
}
