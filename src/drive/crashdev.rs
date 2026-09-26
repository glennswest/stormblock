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
//! cut on any drive that honours FLUSH. What it cannot prove: torn sectors
//! within one write (every write here is atomic), and firmware that lies
//! about FLUSH.

use std::sync::Mutex;

use async_trait::async_trait;
use rand::{Rng, SeedableRng};
use uuid::Uuid;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

pub struct CrashDevice {
    id: DeviceId,
    block: u32,
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
            state: Mutex::new(State { view: image.clone(), durable: image, cached: Vec::new(), flushes: 0 }),
        }
    }

    /// Cut the power: a new device holding the durable image plus a random
    /// subset of the writes still in the cache (each kept with probability
    /// `keep`). `seed` makes it repeatable.
    pub fn crash(&self, seed: u64, keep: f64) -> CrashDevice {
        let st = self.state.lock().unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut image = st.durable.clone();
        for (off, data) in &st.cached {
            if rng.gen_bool(keep) {
                image[*off as usize..*off as usize + data.len()].copy_from_slice(data);
            }
        }
        CrashDevice::from_image(image)
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
}
