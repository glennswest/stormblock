//! The write-intent bitmap: one bit per chunk of member data, kept on every
//! member between the superblock and the data (#168).
//!
//! A write sets its chunks' bits **on disk** before it touches the data, and
//! the bits are cleared lazily once the chunk has been idle and flushed. After
//! a crash, the chunks whose bits are set are the only places a stripe can be
//! half written (data without its parity, one mirror leg without the other),
//! so reassembly re-checks those and nothing else.
//!
//! The rule the state below keeps: **`durable` is never ahead of the disk.**
//! A write proceeds only when every bit it needs is durable; a writer that
//! finds one that is not takes the I/O lock and writes the pages. Clearing
//! lowers `durable` *before* it writes the zeros, so a writer arriving in
//! between waits for the next write instead of trusting a bit on its way out.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::time::Instant;

use bitvec::prelude::*;

/// Where the bitmap starts on a member.
pub const BITMAP_OFFSET: u64 = 64 * 1024;
/// Room for it: up to the data.
pub const BITMAP_MAX_BYTES: u64 = super::DATA_OFFSET - BITMAP_OFFSET;
/// The smallest chunk a bit covers. A new chunk costs one synchronous
/// bitmap write, so small chunks make writes slow; large ones make the
/// resync after a crash longer.
pub const MIN_CHUNK: u64 = 64 * 1024 * 1024;
pub const PAGE: usize = 4096;
const BITS_PER_PAGE: u64 = (PAGE * 8) as u64;

/// Chunk size, bit count and on-disk bytes for a member data size.
pub fn geometry(data_size: u64, unit: u64) -> (u64, u64, u64) {
    let max_bits = BITMAP_MAX_BYTES * 8;
    let mut chunk = MIN_CHUNK.max(data_size.div_ceil(max_bits));
    // A whole number of stripe units, so a stripe never straddles two bits.
    let unit = unit.max(1);
    chunk = chunk.div_ceil(unit) * unit;
    let bits = data_size.div_ceil(chunk).max(1);
    let bytes = bits.div_ceil(8).div_ceil(PAGE as u64) * PAGE as u64;
    (chunk, bits, bytes)
}

struct State {
    wanted: BitVec<u8, Lsb0>,
    durable: BitVec<u8, Lsb0>,
    active: HashMap<u64, u32>,
    last_end: HashMap<u64, Instant>,
}

pub struct IntentBitmap {
    pub chunk: u64,
    pub bits: u64,
    pub bytes: u64,
    state: std::sync::Mutex<State>,
    /// Held while pages are written, so writes of the bitmap never overlap.
    pub io: tokio::sync::Mutex<()>,
}

/// Pages to write, and what they say.
pub struct PageWrite {
    pub pages: Vec<(u64, Vec<u8>)>,
}

impl IntentBitmap {
    pub fn new(chunk: u64, bits: u64, bytes: u64) -> Self {
        let len = (bytes * 8) as usize;
        IntentBitmap {
            chunk,
            bits,
            bytes,
            state: std::sync::Mutex::new(State {
                wanted: bitvec![u8, Lsb0; 0; len],
                durable: bitvec![u8, Lsb0; 0; len],
                active: HashMap::new(),
                last_end: HashMap::new(),
            }),
            io: tokio::sync::Mutex::new(()),
        }
    }

    /// The chunks a member-data range touches.
    pub fn chunks(&self, member_offset: u64, len: u64) -> RangeInclusive<u64> {
        let first = member_offset / self.chunk;
        let last = (member_offset + len.max(1) - 1) / self.chunk;
        first.min(self.bits - 1)..=last.min(self.bits - 1)
    }

    /// Count a write in. `true` when a bit it needs is not yet on disk: the
    /// caller must then write the pending pages (under `io`) before the data.
    pub fn begin(&self, r: RangeInclusive<u64>) -> bool {
        let mut st = self.state.lock().unwrap();
        let mut need = false;
        for c in r {
            *st.active.entry(c).or_insert(0) += 1;
            st.wanted.set(c as usize, true);
            if !st.durable[c as usize] {
                need = true;
            }
        }
        need
    }

    /// Whether every bit of a range is on disk.
    pub fn is_durable(&self, r: RangeInclusive<u64>) -> bool {
        let st = self.state.lock().unwrap();
        r.into_iter().all(|c| st.durable[c as usize])
    }

    /// Count a write out.
    pub fn end(&self, r: RangeInclusive<u64>) {
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        for c in r {
            if let Some(n) = st.active.get_mut(&c) {
                *n -= 1;
                if *n == 0 {
                    st.active.remove(&c);
                }
            }
            st.last_end.insert(c, now);
        }
    }

    /// The pages whose bits differ from what is on disk, with their new
    /// contents. Call under `io`; then write them and `commit`.
    pub fn pending(&self) -> PageWrite {
        let st = self.state.lock().unwrap();
        let pages = (self.bytes / PAGE as u64) as usize;
        let mut out = Vec::new();
        for p in 0..pages {
            let lo = p * PAGE * 8;
            let hi = lo + PAGE * 8;
            if st.wanted[lo..hi] != st.durable[lo..hi] {
                let bytes = st.wanted.as_raw_slice()[p * PAGE..(p + 1) * PAGE].to_vec();
                out.push((p as u64, bytes));
            }
        }
        PageWrite { pages: out }
    }

    /// The pages of a `pending` are on disk.
    pub fn commit(&self, w: &PageWrite) {
        let mut st = self.state.lock().unwrap();
        for (p, bytes) in &w.pages {
            let lo = *p as usize * PAGE;
            st.durable.as_raw_mut_slice()[lo..lo + PAGE].copy_from_slice(bytes);
        }
    }

    /// Stop wanting the bits of chunks that have no write in flight and
    /// whose last write ended before `before` (and so before the flush that
    /// made it durable). Lowers `durable` for them at once — see the module
    /// docs. Returns how many were cleared.
    pub fn clear_idle(&self, before: Instant) -> usize {
        let mut st = self.state.lock().unwrap();
        let ones: Vec<usize> = st.wanted.iter_ones().collect();
        let mut n = 0;
        for c in ones {
            let c64 = c as u64;
            if st.active.contains_key(&c64) {
                continue;
            }
            let idle = st.last_end.get(&c64).map(|t| *t < before).unwrap_or(true);
            if idle {
                st.wanted.set(c, false);
                st.durable.set(c, false);
                st.last_end.remove(&c64);
                n += 1;
            }
        }
        n
    }

    /// Bits set in memory.
    pub fn dirty_count(&self) -> usize {
        self.state.lock().unwrap().wanted.count_ones()
    }
}

/// The chunks a bitmap read from disk marks, OR'd over several members.
pub fn dirty_chunks(images: &[Vec<u8>], bits: u64) -> Vec<u64> {
    let mut out = Vec::new();
    for c in 0..bits {
        let byte = (c / 8) as usize;
        let bit = (c % 8) as u8;
        if images.iter().any(|img| img.get(byte).map(|b| b & (1 << bit) != 0).unwrap_or(false)) {
            out.push(c);
        }
    }
    out
}

/// The page a chunk's bit is in.
pub fn page_of(chunk: u64) -> u64 {
    chunk / BITS_PER_PAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_fits_and_aligns() {
        let (chunk, bits, bytes) = geometry(1 << 40, 65536);
        assert_eq!(chunk, MIN_CHUNK);
        assert_eq!(bits, (1u64 << 40) / MIN_CHUNK);
        assert!(bytes <= BITMAP_MAX_BYTES);
        assert_eq!(bytes % PAGE as u64, 0);
        // A huge drive: chunk grows so the map still fits.
        let (chunk, bits, bytes) = geometry(1 << 50, 65536 * 3);
        assert!(bytes <= BITMAP_MAX_BYTES, "{bytes}");
        assert_eq!(chunk % (65536 * 3), 0);
        assert!(bits * chunk >= 1 << 50);
    }

    #[test]
    fn a_write_waits_for_its_bit_and_clearing_lowers_it_first() {
        let (chunk, bits, bytes) = geometry(10 * MIN_CHUNK, 65536);
        let b = IntentBitmap::new(chunk, bits, bytes);
        let r = b.chunks(MIN_CHUNK + 5, 10);
        assert_eq!(r, 1..=1);
        assert!(b.begin(r.clone()), "a fresh bit must be written first");
        let w = b.pending();
        assert_eq!(w.pages.len(), 1);
        assert_eq!(w.pages[0].1[0], 0b10);
        b.commit(&w);
        assert!(b.pending().pages.is_empty());
        // A second writer of the same chunk need not wait.
        assert!(!b.begin(r.clone()));
        b.end(r.clone());
        // Still one write in flight: nothing clears.
        assert_eq!(b.clear_idle(Instant::now() + std::time::Duration::from_secs(1)), 0);
        b.end(r.clone());
        assert_eq!(b.clear_idle(Instant::now() + std::time::Duration::from_secs(1)), 1);
        // The bit is no longer trusted before the zero is written.
        assert!(!b.is_durable(r.clone()));
        assert!(b.begin(r.clone()), "a writer arriving mid-clear must write again");
        let w = b.pending();
        // Wanted again, durable 0: the page goes out with the bit set.
        assert_eq!(w.pages[0].1[0], 0b10);
    }

    #[test]
    fn dirty_chunks_ors_the_members() {
        let a = vec![0b0000_0001u8, 0];
        let b = vec![0b0000_0100u8, 0b1];
        assert_eq!(dirty_chunks(&[a, b], 16), vec![0, 2, 8]);
    }
}
