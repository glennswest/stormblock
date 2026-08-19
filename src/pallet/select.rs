//! Selection — read-only, and deliberately dependency-free.
//!
//! Boot-time consumers (stormuefi in firmware, an initramfs, a recovery shell)
//! need exactly two things: *which pallets are on these disks*, and *which one
//! do I use — and if it is bad, what do I use instead*. They must not be able
//! to write, must not need the builder, and must not carry an allocator's
//! worth of code to answer it.
//!
//! So the policy lives here as **pure functions over plain data**. No I/O, no
//! device, no async, nothing borrowed from the writer. [`PalletBrowser`] is the
//! thin read-only wrapper that feeds them from real drives; a `no_std`
//! consumer feeds them from whatever it can read in firmware and gets the same
//! answers, which is the property that matters — one selection rule, not one
//! per consumer that then drift.
//!
//! The rule, from `PALLET-SPEC.md`:
//!
//! ```text
//! candidates = pallets with priority > 0, ordered by (priority desc, version desc)
//! for p in candidates:
//!     if p.successful == 0 and p.tries_left == 0: skip
//!     if not verify(p): continue          # fall back
//!     use p
//! ```

use uuid::Uuid;

use super::format::{Attributes, PalletKind};
use super::store::{PalletLocation, PalletStore};
use super::{Pallet, PalletError, PartitionView, Result, VerifyReport};

/// The minimum a consumer needs to select between pallets.
///
/// Deliberately not [`PalletLocation`]: this is what a pre-kernel reader can
/// fill in from a GPT entry and a superblock, with no notion of drive paths,
/// indexes or anything else the running system knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub id: Uuid,
    pub kind: PalletKind,
    pub version: u64,
    pub attributes: Attributes,
    /// False when the superblock or a table did not check out. Such a pallet
    /// is never selected, and never counts as a fallback.
    pub readable: bool,
}

impl Candidate {
    /// Ordering key: priority first, version second — ties in priority broken
    /// by the monotonic version, never by disk order.
    pub fn order_key(&self) -> (u8, u64) {
        (self.attributes.priority, self.version)
    }

    /// Usable at all: parses, has a priority, and either was confirmed good or
    /// still has attempts left.
    pub fn is_candidate(&self) -> bool {
        self.readable && self.attributes.is_candidate()
    }
}

impl From<&PalletLocation> for Candidate {
    fn from(l: &PalletLocation) -> Candidate {
        Candidate {
            id: l.id,
            kind: l.kind,
            version: l.version,
            attributes: l.attributes,
            readable: l.is_readable(),
        }
    }
}

/// Candidates of one kind, in the order a consumer should try them.
///
/// `kind` of `None` means "do not filter" — for a consumer that genuinely
/// wants every pallet on the disk. A boot consumer should pass
/// `Some(PalletKind::Boot)`: priority orders pallets that compete with each
/// other, and an app pallet is not competing with the kernel.
pub fn order(candidates: &[Candidate], kind: Option<PalletKind>) -> Vec<Candidate> {
    let mut v: Vec<Candidate> = candidates
        .iter()
        .copied()
        .filter(|c| c.is_candidate())
        .filter(|c| match kind {
            // A pallet written before `kind` existed says Unspecified, and a
            // boot consumer should still consider it — refusing would strand a
            // node on an older image for a field it never wrote.
            Some(k) => c.kind == k || c.kind == PalletKind::Unspecified,
            None => true,
        })
        .collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.order_key()));
    v
}

/// The one a consumer would use right now.
pub fn select(candidates: &[Candidate], kind: Option<PalletKind>) -> Option<Candidate> {
    order(candidates, kind).into_iter().next()
}

/// What to use instead of `failed` — the next one down the ladder.
///
/// This is the whole of rollback as a *decision*. Making it stick is an
/// attribute write, which a read-only consumer does not do: firmware falls
/// back by simply trying the next one, and the running system records the
/// choice with [`super::PalletManager::rollback`].
pub fn fallback_after(
    candidates: &[Candidate],
    failed: Uuid,
    kind: Option<PalletKind>,
) -> Option<Candidate> {
    let ordered = order(candidates, kind);
    match ordered.iter().position(|c| c.id == failed) {
        Some(i) => ordered.into_iter().nth(i + 1),
        // The failed pallet is not in the ladder at all (priority 0, or out of
        // tries). Then the head of the ladder *is* the answer.
        None => ordered.into_iter().next(),
    }
}

/// Every candidate below the selected one, in order — the fallback chain a
/// consumer will walk if each in turn fails to verify.
pub fn chain(candidates: &[Candidate], kind: Option<PalletKind>) -> Vec<Candidate> {
    order(candidates, kind)
}

/// Read-only view of the pallets on a set of drives.
///
/// Every method reads. There is no method here that writes, by construction
/// rather than by discipline — this is the type a boot-time or recovery
/// consumer holds, and the type the `stormuefi` reader mirrors.
pub struct PalletBrowser {
    store: PalletStore,
}

impl PalletBrowser {
    pub fn new(store: PalletStore) -> Self {
        PalletBrowser { store }
    }

    pub fn store(&self) -> &PalletStore {
        &self.store
    }

    /// Every pallet on every drive, wherever it is.
    pub async fn list(&self) -> Vec<PalletLocation> {
        self.store.scan().await
    }

    /// Everything on one drive — several pallets per drive is the normal case.
    pub async fn list_drive(&self, drive_index: usize) -> Result<Vec<PalletLocation>> {
        self.store.scan_drive(drive_index).await
    }

    pub async fn candidates(&self, kind: Option<PalletKind>) -> Vec<Candidate> {
        self.list().await.iter().map(Candidate::from).collect::<Vec<_>>().pipe_order(kind)
    }

    /// The pallet a consumer would boot, with where it lives.
    pub async fn select(&self, kind: Option<PalletKind>) -> Option<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(Candidate::from).collect();
        let chosen = select(&cands, kind)?;
        all.into_iter().find(|l| l.id == chosen.id)
    }

    /// What to boot instead, if `failed` turns out to be bad.
    pub async fn fallback_after(
        &self,
        failed: Uuid,
        kind: Option<PalletKind>,
    ) -> Option<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(Candidate::from).collect();
        let next = fallback_after(&cands, failed, kind)?;
        all.into_iter().find(|l| l.id == next.id)
    }

    /// The whole fallback chain, in the order it would be walked.
    pub async fn chain(&self, kind: Option<PalletKind>) -> Vec<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(Candidate::from).collect();
        let ordered = chain(&cands, kind);
        ordered
            .into_iter()
            .filter_map(|c| all.iter().find(|l| l.id == c.id).cloned())
            .collect()
    }

    /// Parse a pallet's manifest without reading any content.
    pub async fn open(&self, loc: &PalletLocation) -> Result<Pallet> {
        self.store.open(loc).await
    }

    pub fn view(&self, loc: &PalletLocation) -> Result<PartitionView> {
        self.store.view(loc)
    }

    /// Check a pallet the way a pre-kernel consumer does: manifest first, then
    /// every member's content against the digest **the manifest** records.
    pub async fn verify(&self, id: Uuid) -> Result<VerifyReport> {
        let loc = self
            .store
            .find(id)
            .await
            .map_err(|_| PalletError::NotFound(format!("pallet {id}")))?;
        super::PalletManager::new(self.store.clone()).verify(loc.id).await
    }

    /// Select, verify, and fall back until one passes — the read-only half of
    /// boot policy, run in full.
    ///
    /// Returns the first pallet that verifies, and the ones rejected on the way
    /// with the reason each was rejected. A caller that wants the decision
    /// recorded hands the result to [`super::PalletManager::activate`]; one in
    /// firmware simply uses it.
    pub async fn select_verified(
        &self,
        kind: Option<PalletKind>,
    ) -> (Option<PalletLocation>, Vec<(PalletLocation, String)>) {
        let mut rejected = Vec::new();
        for loc in self.chain(kind).await {
            match self.verify(loc.id).await {
                Ok(r) if r.ok => return (Some(loc), rejected),
                Ok(r) => {
                    rejected.push((loc, r.reason.unwrap_or_else(|| "failed verification".into())))
                }
                Err(e) => rejected.push((loc, e.to_string())),
            }
        }
        (None, rejected)
    }
}

/// Small helper so `candidates()` reads as one expression.
trait PipeOrder {
    fn pipe_order(self, kind: Option<PalletKind>) -> Vec<Candidate>;
}

impl PipeOrder for Vec<Candidate> {
    fn pipe_order(self, kind: Option<PalletKind>) -> Vec<Candidate> {
        order(&self, kind)
    }
}
