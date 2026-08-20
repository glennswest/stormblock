//! Selection — read-only, over the shared policy.
//!
//! The rule itself is **not here**. It lives in
//! [`stormblock_pallet_format::select`], `no_std` and allocation-free, because
//! a boot-time consumer has to answer "which pallet do I use, and what instead
//! if it is bad" with the same rule the running system applies. Two answers to
//! that question is a node that boots the wrong image, or none.
//!
//! What this module adds is the part that needs a host: drives to scan, `Vec`
//! to collect into, and async I/O to read with. [`PalletBrowser`] is the thin
//! read-only wrapper — every method reads, and there is no method here that
//! writes, by construction rather than by discipline.

use uuid::Uuid;

use super::format::PalletKind;
use super::store::{PalletLocation, PalletStore};
use super::{Pallet, PalletError, PartitionView, Result, VerifyReport};

pub use stormblock_pallet_format::select::{fallback_after as raw_fallback_after, sort_candidates};
pub use stormblock_pallet_format::Candidate;

/// A location, as the shared policy sees it.
pub fn candidate_of(l: &PalletLocation) -> Candidate {
    Candidate {
        id: l.id.to_bytes_le(),
        kind: l.kind,
        version: l.version,
        attributes: l.attributes,
        readable: l.is_readable(),
    }
}

fn id_of(c: &Candidate) -> Uuid {
    Uuid::from_bytes_le(c.id)
}

/// Candidates of one kind, in the order a consumer should try them.
///
/// `kind` of `None` means "do not filter". A boot consumer should pass
/// `Some(PalletKind::Boot)`: priority orders pallets that compete with each
/// other, and an app pallet is not competing with the kernel.
pub fn order(candidates: &[Candidate], kind: Option<PalletKind>) -> Vec<Candidate> {
    let mut v = candidates.to_vec();
    let n = sort_candidates(&mut v, kind);
    v.truncate(n);
    v
}

/// The one a consumer would use right now.
pub fn select(candidates: &[Candidate], kind: Option<PalletKind>) -> Option<Candidate> {
    stormblock_pallet_format::select::select(candidates, kind)
}

/// What to use instead of `failed` — the next one down the ladder.
pub fn fallback_after(
    candidates: &[Candidate],
    failed: Uuid,
    kind: Option<PalletKind>,
) -> Option<Candidate> {
    raw_fallback_after(candidates, &failed.to_bytes_le(), kind)
}

/// Every candidate in order — the fallback chain a consumer will walk if each
/// in turn fails to verify.
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
        let all: Vec<Candidate> = self.list().await.iter().map(candidate_of).collect();
        order(&all, kind)
    }

    /// The pallet a consumer would boot, with where it lives.
    pub async fn select(&self, kind: Option<PalletKind>) -> Option<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(candidate_of).collect();
        let chosen = select(&cands, kind)?;
        all.into_iter().find(|l| l.id == id_of(&chosen))
    }

    /// What to boot instead, if `failed` turns out to be bad.
    pub async fn fallback_after(
        &self,
        failed: Uuid,
        kind: Option<PalletKind>,
    ) -> Option<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(candidate_of).collect();
        let next = fallback_after(&cands, failed, kind)?;
        all.into_iter().find(|l| l.id == id_of(&next))
    }

    /// The whole fallback chain, in the order it would be walked.
    pub async fn chain(&self, kind: Option<PalletKind>) -> Vec<PalletLocation> {
        let all = self.list().await;
        let cands: Vec<Candidate> = all.iter().map(candidate_of).collect();
        chain(&cands, kind)
            .into_iter()
            .filter_map(|c| all.iter().find(|l| l.id == id_of(&c)).cloned())
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// The policy has its own tests in the crate that owns it. What is worth
    /// asserting here is the seam: a `PalletLocation` becomes the candidate the
    /// shared rule expects, and an answer maps back to the location it came
    /// from.
    fn location(name: &str, version: u64, priority: u8, kind: PalletKind) -> PalletLocation {
        PalletLocation {
            id: Uuid::new_v4(),
            drive: "disk0".into(),
            drive_index: 0,
            entry_index: 0,
            partition_name: name.into(),
            name: name.into(),
            kind,
            version,
            version_label: String::new(),
            attributes: super::super::format::Attributes {
                priority,
                tries_left: 1,
                successful: false,
                sealed: true,
                read_only: true,
                required: true,
            },
            start_bytes: 0,
            size_bytes: 0,
            used_bytes: 0,
            member_count: 0,
            state: super::super::store::PalletState::Readable,
        }
    }

    #[test]
    fn a_location_carries_into_the_shared_policy_and_back() {
        let old = location("stormcos-boot", 1, 14, PalletKind::Boot);
        let new = location("stormcos-boot", 2, 15, PalletKind::Boot);
        let cands: Vec<Candidate> = [&old, &new].into_iter().map(candidate_of).collect();

        let chosen = select(&cands, Some(PalletKind::Boot)).unwrap();
        assert_eq!(id_of(&chosen), new.id);
        assert_eq!(chosen.version, 2);

        let next = fallback_after(&cands, new.id, Some(PalletKind::Boot)).unwrap();
        assert_eq!(id_of(&next), old.id);

        let ladder = chain(&cands, Some(PalletKind::Boot));
        assert_eq!(ladder.len(), 2);
        assert_eq!(id_of(&ladder[0]), new.id);
    }

    #[test]
    fn an_unreadable_pallet_never_becomes_a_candidate() {
        let mut broken = location("stormcos-boot", 3, 15, PalletKind::Boot);
        broken.state = super::super::store::PalletState::Unreadable { reason: "torn".into() };
        let good = location("stormcos-boot", 1, 5, PalletKind::Boot);
        let cands: Vec<Candidate> = [&broken, &good].into_iter().map(candidate_of).collect();

        assert!(!cands[0].readable);
        assert_eq!(id_of(&select(&cands, None).unwrap()), good.id);
    }
}
