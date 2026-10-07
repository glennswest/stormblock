//! A block device that loses what a power cut would (#171).
//!
//! **Tests and verification only.** It keeps the whole device in memory.
//! Every write lands in a *volatile cache*: reads see it at once, but it
//! reaches the durable image only at `flush`, the way a drive with a
//! write-back cache behaves. [`CrashDevice::crash`] then returns the durable
//! image with a **random subset** of the unflushed writes applied, in their
//! original order, because a drive may persist any part of its cache, in any
//! order, before the power goes. Discard keeps the old data, as most SSDs may
//! and every HDD does.
//!
//! What it proves: code that is correct against this device survives a power
//! cut on any drive that honours FLUSH. [`CrashDevice::crash_with`] also tears
//! writes (#191): a kept write lands only in part — a prefix of its blocks, or
//! any subset of them — the way a drive cut mid-write may leave it. Writes
//! tear at the drive's atomic unit: its 4096-byte block (a 4Kn drive), or 512
//! bytes with [`CrashDevice::with_atomic_unit`] (a drive with 512-byte
//! sectors, where even one 4 KiB write can land in part); never inside one.
//! What it cannot prove: firmware that lies about FLUSH.

use std::sync::Mutex;

use async_trait::async_trait;
use rand::{Rng, SeedableRng};
use uuid::Uuid;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

/// How a power cut tears the writes it keeps (#191).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tear {
    /// Every kept write lands whole.
    None,
    /// With this probability a kept write of more than one atomic unit lands
    /// only as its first n units (1 ≤ n < its units): a drive writing in order.
    Prefix(f64),
    /// With this probability a kept write of more than one atomic unit lands
    /// as a random subset of its units: a drive completing out of order.
    Scatter(f64),
}

pub struct CrashDevice {
    id: DeviceId,
    block: u32,
    /// What a write tears at (#191): the device's block unless set — a 4Kn
    /// drive's atomic unit; 512 for a drive with 512-byte sectors.
    atomic: usize,
    /// Writes the crash that made this device tore.
    torn: usize,
    state: Mutex<State>,
}

struct State {
    /// What survives a power cut for certain.
    durable: Vec<u8>,
    /// What reads see: `durable` with every cached write applied.
    view: Vec<u8>,
    /// Writes since the last flush, in order.
    cached: Vec<(u64, Vec<u8>)>,
    flushes: u64,
    /// Writes taken so far.
    writes: u64,
    /// The power goes as this write arrives (#191): what survived then is
    /// kept in `cut`, and a crash uses it.
    cut_at: Option<u64>,
    cut: Option<(Vec<u8>, Vec<(u64, Vec<u8>)>)>,
}

impl CrashDevice {
    /// A zeroed device of `bytes`.
    pub fn new(bytes: u64) -> Self {
        Self::from_image(vec![0u8; bytes as usize])
    }

    fn from_image(image: Vec<u8>) -> Self {
        CrashDevice {
            id: DeviceId {
                uuid: Uuid::new_v4(),
                serial: "CRASH".into(),
                model: "crash-sim".into(),
                path: "crash://".into(),
                wwn: String::new(),
            },
            block: 4096,
            atomic: 4096,
            torn: 0,
            state: Mutex::new(State {
                view: image.clone(),
                durable: image,
                cached: Vec::new(),
                flushes: 0,
                writes: 0,
                cut_at: None,
                cut: None,
            }),
        }
    }

    /// Cut the power: a new device holding the durable image plus a random
    /// subset of the writes still in the cache (each kept with probability
    /// `keep`), every one whole. `seed` makes it repeatable.
    pub fn crash(&self, seed: u64, keep: f64) -> CrashDevice {
        self.crash_with(seed, keep, Tear::None)
    }

    /// [`crash`](Self::crash), with the kept writes torn as `tear` says
    /// (#191).
    pub fn crash_with(&self, seed: u64, keep: f64, tear: Tear) -> CrashDevice {
        let st = self.state.lock().unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        // A cut armed with `cut_at` that fired: the power went then.
        let (durable, cached) = match &st.cut {
            Some((d, c)) => (d, c),
            None => (&st.durable, &st.cached),
        };
        let mut image = durable.clone();
        let block = self.atomic;
        let mut torn = 0;
        for (off, data) in cached {
            if !rng.gen_bool(keep) {
                continue;
            }
            let off = *off as usize;
            // The write's pieces, at the device's block boundaries.
            let mut pieces = Vec::new();
            let mut at = off;
            while at < off + data.len() {
                let end = ((at / block + 1) * block).min(off + data.len());
                pieces.push((at, end));
                at = end;
            }
            let kept: Vec<bool> = match tear {
                Tear::Prefix(p) if pieces.len() > 1 && rng.gen_bool(p) => {
                    let n = rng.gen_range(1..pieces.len());
                    (0..pieces.len()).map(|i| i < n).collect()
                }
                Tear::Scatter(p) if pieces.len() > 1 && rng.gen_bool(p) => {
                    (0..pieces.len()).map(|_| rng.gen_bool(0.5)).collect()
                }
                _ => vec![true; pieces.len()],
            };
            if kept.iter().any(|k| !k) {
                torn += 1;
            }
            for ((a, b), k) in pieces.into_iter().zip(kept) {
                if k {
                    image[a..b].copy_from_slice(&data[a - off..b - off]);
                }
            }
        }
        let mut after = CrashDevice::from_image(image);
        after.atomic = self.atomic;
        after.torn = torn;
        after
    }

    /// Tear at `bytes` rather than the block (#191): 512 is a drive whose
    /// sectors are 512 bytes, where even one 4 KiB write can land in part.
    pub fn with_atomic_unit(mut self, bytes: usize) -> Self {
        assert!(bytes > 0 && self.block as usize % bytes == 0);
        self.atomic = bytes;
        self
    }

    /// How many writes the crash that made this device tore.
    pub fn torn_writes(&self) -> usize {
        self.torn
    }

    /// Cut the power as the `n`th write from now arrives (#191), whatever is
    /// running then: a persist or a slot-table sync part-way, its pieces
    /// cached and not flushed. The device goes on working; `crash_with` uses
    /// what survived at the cut.
    pub fn cut_at(&self, n: u64) {
        let mut st = self.state.lock().unwrap();
        st.cut_at = Some(st.writes + n);
        st.cut = None;
    }

    /// Whether an armed cut has happened.
    pub fn cut_taken(&self) -> bool {
        self.state.lock().unwrap().cut.is_some()
    }

    /// Writes waiting in the cache.
    pub fn cached_writes(&self) -> usize {
        self.state.lock().unwrap().cached.len()
    }

    pub fn flushes(&self) -> u64 {
        self.state.lock().unwrap().flushes
    }
}

#[async_trait]
impl BlockDevice for CrashDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.state.lock().unwrap().view.len() as u64
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn optimal_io_size(&self) -> u32 {
        self.block
    }

    fn device_type(&self) -> DriveType {
        DriveType::File
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let st = self.state.lock().unwrap();
        let end = offset as usize + buf.len();
        if end > st.view.len() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: st.view.len() as u64 });
        }
        buf.copy_from_slice(&st.view[offset as usize..end]);
        Ok(buf.len())
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let mut st = self.state.lock().unwrap();
        let end = offset as usize + buf.len();
        if end > st.view.len() {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: st.view.len() as u64 });
        }
        if st.cut.is_none() && st.cut_at == Some(st.writes) {
            // The power goes as this write arrives: it is not in the cache.
            st.cut = Some((st.durable.clone(), st.cached.clone()));
        }
        st.writes += 1;
        st.view[offset as usize..end].copy_from_slice(buf);
        st.cached.push((offset, buf.to_vec()));
        Ok(buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        let mut st = self.state.lock().unwrap();
        let cached = std::mem::take(&mut st.cached);
        for (off, data) in cached {
            st.durable[off as usize..off as usize + data.len()].copy_from_slice(&data);
        }
        st.flushes += 1;
        Ok(())
    }

    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        // The old data stays: a discard is a hint, not a zeroing.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_flushed_writes_are_certain_to_survive() {
        let d = CrashDevice::new(1 << 20);
        d.write(0, &[1u8; 4096]).await.unwrap();
        d.flush().await.unwrap();
        d.write(4096, &[2u8; 4096]).await.unwrap();
        // Keep none of the cache: only the flushed write survives.
        let after = d.crash(1, 0.0);
        let mut b = vec![0u8; 8192];
        after.read(0, &mut b).await.unwrap();
        assert!(b[..4096].iter().all(|&x| x == 1));
        assert!(b[4096..].iter().all(|&x| x == 0));
        // Keep all of it: both survive.
        let after = d.crash(1, 1.0);
        after.read(0, &mut b).await.unwrap();
        assert!(b[4096..].iter().all(|&x| x == 2));
    }

    /// #191: a kept multi-block write may land in part, at block boundaries
    /// only: a prefix, or any subset; never inside a block.
    #[tokio::test]
    async fn a_kept_write_may_land_torn_at_block_boundaries() {
        let d = CrashDevice::new(1 << 20);
        let data: Vec<u8> = (0..8u8).flat_map(|i| vec![i + 1; 4096]).collect();
        d.write(4096, &data).await.unwrap();
        let blocks = |dev: &CrashDevice| {
            let st = dev.state.lock().unwrap();
            (1..9).map(|b| st.view[b * 4096..(b + 1) * 4096].to_vec()).collect::<Vec<_>>()
        };
        let (mut prefixes, mut scatters) = (0, 0);
        for seed in 0..64 {
            let p = blocks(&d.crash_with(seed, 1.0, Tear::Prefix(1.0)));
            let n = p.iter().take_while(|b| b[0] != 0).count();
            assert!((1..8).contains(&n), "a torn prefix keeps 1..8 of 8 blocks, kept {n}");
            for (i, b) in p.iter().enumerate() {
                let want = if i < n { (i + 1) as u8 } else { 0 };
                assert!(b.iter().all(|&x| x == want), "block {i} torn inside");
            }
            prefixes += 1;
            let s = blocks(&d.crash_with(seed, 1.0, Tear::Scatter(1.0)));
            for (i, b) in s.iter().enumerate() {
                assert!(b.iter().all(|&x| x == 0) || b.iter().all(|&x| x == (i + 1) as u8), "block {i} torn inside");
            }
            if s.iter().any(|b| b[0] == 0) && s.iter().skip_while(|b| b[0] != 0).any(|b| b[0] != 0) {
                scatters += 1; // a hole before a kept block: not a prefix
            }
        }
        assert_eq!(prefixes, 64);
        assert!(scatters > 0, "scatter tears out of order");
        // Untorn, the write is whole.
        assert!(blocks(&d.crash_with(0, 1.0, Tear::None)).iter().enumerate().all(|(i, b)| b[0] == (i + 1) as u8));
    }
}
