//! A held blob as the source of something else (#733): a pallet member, an
//! import, an image-spec member, read in place by digest, never copied out
//! first.
//!
//! A [`BlobSource`] holds the blob's [`Placement`] for as long as it lives,
//! so a `DELETE` of the blob meanwhile takes it out of the index at once and
//! gives its space back only when the source is dropped: the read that is
//! under way ends on the bytes it began on, as a `GET` in flight does.

use std::sync::Arc;

use async_trait::async_trait;

use super::format::{digest_string, parse_digest, Digest};
use super::{BlobError, BlobStore, Placement};

#[derive(Clone)]
pub struct BlobSource {
    store: Arc<BlobStore>,
    placement: Arc<Placement>,
    digest: Digest,
}

impl std::fmt::Debug for BlobSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobSource({}, {} bytes)", self.name(), self.len())
    }
}

impl BlobSource {
    /// The blob `digest` (`sha256:<hex>`) in `store`, held. A malformed
    /// digest is `Invalid`; one the store does not hold `NotFound`, naming it.
    pub async fn open(store: &Arc<BlobStore>, digest: &str) -> Result<BlobSource, BlobError> {
        let d = parse_digest(digest).map_err(BlobError::Invalid)?;
        let placement = store.get(&d).await.ok_or_else(|| not_held(&d))?;
        Ok(BlobSource { store: store.clone(), placement, digest: d })
    }

    /// The same, from the node's store; no store is a blob not held.
    pub async fn from_engine(state: &Arc<crate::mgmt::AppState>, digest: &str) -> Result<BlobSource, BlobError> {
        let d = parse_digest(digest).map_err(BlobError::Invalid)?;
        match super::engine::store(state, false).await? {
            Some(store) => BlobSource::open(&store, &digest_string(&d)).await,
            None => Err(not_held(&d)),
        }
    }

    pub fn len(&self) -> u64 {
        self.placement.size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// `sha256:<hex>`.
    pub fn name(&self) -> String {
        digest_string(&self.digest)
    }

    /// Exactly `buf.len()` bytes from `off`; past the end is an error.
    pub async fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> Result<(), BlobError> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut done = 0usize;
        // In I/O-sized pieces, as the store's own stream reads.
        while done < buf.len() {
            let n = (buf.len() - done).min(super::IO_UNIT);
            self.store.read(&self.placement, off + done as u64, &mut buf[done..done + n]).await?;
            done += n;
        }
        Ok(())
    }

    /// Up to `buf.len()` bytes from `off`, short at the blob's end (0 past
    /// it), as a file's read is.
    pub async fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, BlobError> {
        let len = self.len();
        if off >= len {
            return Ok(0);
        }
        let take = ((len - off) as usize).min(buf.len());
        self.read_exact_at(off, &mut buf[..take]).await?;
        Ok(take)
    }
}

fn not_held(d: &Digest) -> BlobError {
    BlobError::NotFound(format!("blob {} is not held by the blob store", digest_string(d)))
}

/// A pallet member straight out of the store.
#[async_trait]
impl crate::pallet::MemberContent for BlobSource {
    fn byte_len(&self) -> u64 {
        self.len()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::pallet::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.len()) {
            return Err(crate::pallet::PalletError::OutOfRange { offset, len: buf.len() as u64 });
        }
        self.read_exact_at(offset, buf)
            .await
            .map_err(|e| crate::pallet::PalletError::Io(std::io::Error::other(format!("blob {}: {e}", self.name()))))
    }
}
