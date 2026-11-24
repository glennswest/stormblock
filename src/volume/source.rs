//! A sealed volume as the source of something else (#737): an import, an
//! image build's member or ESP, read in place, never copied out first — as a
//! pallet publish's `source: volume` member is read.
//!
//! The registry keeps each blob as a raw sealed volume holding its exact
//! bytes, with the length recorded beside it; `byte_len` says how much of
//! the volume is the content (the rest is the slot padding up to its size).
//!
//! A [`VolumeSource`] is opened only on a **sealed** volume: what it reads
//! must be what it began on. It takes a serve hold (#267) for as long as it
//! lives, so a `DELETE` of the volume meanwhile is refused (`InUse`, naming
//! the operation), and it checks the seal on every read, so an unseal
//! meanwhile fails the read rather than letting it finish on other bytes.
//! The volume itself is never written.

use std::sync::Arc;

use super::holds::ServeHolds;
use super::thin::ThinVolumeHandle;
use super::{VolumeId, VolumeManager};
use crate::drive::BlockDevice;

/// Why a volume cannot be a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// No such volume (`404`).
    NotFound(String),
    /// Not sealed, or secret (`409`).
    Conflict(String),
    /// A `byte_len` the volume does not have (`400`).
    Invalid(String),
    /// A read failed, or the volume was unsealed under it.
    Io(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NotFound(m) | SourceError::Conflict(m) | SourceError::Invalid(m) | SourceError::Io(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for SourceError {}

struct Held {
    id: VolumeId,
    name: String,
    handle: Arc<ThinVolumeHandle>,
    len: u64,
    epoch: u64,
    holds: ServeHolds,
    what: String,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.holds.release(self.id.0, &self.what);
    }
}

/// A sealed volume, held, read as the first `len` bytes of it. Clones share
/// the one hold; the last one dropped releases it.
#[derive(Clone)]
pub struct VolumeSource(Arc<Held>);

impl std::fmt::Debug for VolumeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VolumeSource({}, {} bytes)", self.describe(), self.len())
    }
}

impl VolumeSource {
    /// Volume `id` in `vm`, sealed and not secret, its first `byte_len`
    /// bytes (all of it when `None`), held by `what` (said to a `DELETE`
    /// refused meanwhile) until the last clone is dropped.
    pub async fn open(
        vm: &VolumeManager,
        id: VolumeId,
        byte_len: Option<u64>,
        what: &str,
    ) -> Result<VolumeSource, SourceError> {
        let handle = vm
            .get_volume_handle(&id)
            .ok_or_else(|| SourceError::NotFound(format!("volume {} not found", id.0)))?;
        if !handle.is_sealed() {
            return Err(SourceError::Conflict(format!(
                "volume {} is not sealed: a source is read in place and must not change under the read",
                id.0
            )));
        }
        vm.refuse_secret(&id, what).map_err(|e| SourceError::Conflict(e.to_string()))?;
        let cap = handle.capacity_bytes();
        let len = match byte_len {
            None => cap,
            Some(n) if n <= cap => n,
            Some(n) => {
                return Err(SourceError::Invalid(format!(
                    "byte_len {n} is past the end of volume {} ({cap} bytes)",
                    id.0
                )))
            }
        };
        let epoch = handle.seal_epoch();
        let name = handle.name().await;
        let holds = vm.holds();
        let what = what.to_string();
        holds.hold(id.0, what.clone());
        let held = Held { id, name, handle, len, epoch, holds, what };
        // Sealed when the hold was taken: an unseal between the check above
        // and the hold would otherwise go unseen until the first read.
        held.check()?;
        Ok(VolumeSource(Arc::new(held)))
    }

    pub fn id(&self) -> VolumeId {
        self.0.id
    }

    pub fn len(&self) -> u64 {
        self.0.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `volume <id> (<name>)`, for messages.
    pub fn describe(&self) -> String {
        if self.0.name.is_empty() {
            format!("volume {}", self.0.id.0)
        } else {
            format!("volume {} ({})", self.0.id.0, self.0.name)
        }
    }

    /// Exactly `buf.len()` bytes from `off`; past `len` is an error.
    pub async fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> Result<(), SourceError> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = off.checked_add(buf.len() as u64).filter(|&e| e <= self.len());
        if end.is_none() {
            return Err(SourceError::Io(format!(
                "{}: read of {} byte(s) at {off} is past its {} byte(s)",
                self.describe(),
                buf.len(),
                self.len()
            )));
        }
        self.0.check()?;
        // In pieces of at most CHUNK, each widened to whole blocks.
        const CHUNK: usize = 1 << 20;
        let dev: &dyn BlockDevice = &*self.0.handle;
        let bs = dev.block_size().max(1) as u64;
        let mut done = 0usize;
        let mut tmp = Vec::new();
        while done < buf.len() {
            let n = (buf.len() - done).min(CHUNK);
            let abs = off + done as u64;
            let aligned = abs / bs * bs;
            let skew = (abs - aligned) as usize;
            let span = ((skew as u64 + n as u64).div_ceil(bs) * bs) as usize;
            tmp.resize(span, 0);
            dev.read(aligned, &mut tmp[..span])
                .await
                .map_err(|e| SourceError::Io(format!("{}: read at {abs}: {e}", self.describe())))?;
            buf[done..done + n].copy_from_slice(&tmp[skew..skew + n]);
            done += n;
        }
        // Unsealed while it was read: what it read may be a mix.
        self.0.check()
    }

    /// Up to `buf.len()` bytes from `off`, short at `len` (0 past it), as a
    /// file's read is.
    pub async fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        let len = self.len();
        if off >= len {
            return Ok(0);
        }
        let take = ((len - off) as usize).min(buf.len());
        self.read_exact_at(off, &mut buf[..take]).await?;
        Ok(take)
    }
}

impl Held {
    fn check(&self) -> Result<(), SourceError> {
        if !self.handle.is_sealed() || self.handle.seal_epoch() != self.epoch {
            return Err(SourceError::Io(format!(
                "volume {} was unsealed while it was being read as a source",
                self.id.0
            )));
        }
        Ok(())
    }
}

/// A pallet member straight out of the volume.
#[async_trait::async_trait]
impl crate::pallet::MemberContent for VolumeSource {
    fn byte_len(&self) -> u64 {
        self.len()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::pallet::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.len()) {
            return Err(crate::pallet::PalletError::OutOfRange { offset, len: buf.len() as u64 });
        }
        self.read_exact_at(offset, buf)
            .await
            .map_err(|e| crate::pallet::PalletError::Io(std::io::Error::other(e.to_string())))
    }
}
