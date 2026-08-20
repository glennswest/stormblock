//! Which pallet to boot, and what to boot instead.
//!
//! Policy, as pure functions over plain data: no I/O, no allocation, no
//! ordering imposed on the caller's array unless it asks. It lives beside the
//! reader for the same reason the reader is here at all — "which pallet is
//! next" answered two ways in two repos is a node that boots the wrong image,
//! or none.
//!
//! The rule, from `docs/pallets.md`:
//!
//! ```text
//! candidates = pallets with priority > 0, ordered by (priority desc, version desc)
//! for p in candidates:
//!     if p.successful == 0 and p.tries_left == 0: skip
//!     if not verify(p): continue          # fall back
//!     use p
//! ```

use crate::{Attributes, PalletKind};

/// The minimum needed to choose between pallets.
///
/// What a pre-kernel reader can fill in from a partition's type GUID, its
/// attribute bits and its superblock — with no notion of drive paths, indexes
/// or anything else a running system knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The GPT `UniquePartitionGUID`, as stored. Opaque here; it exists so a
    /// caller can map an answer back to the partition it came from.
    pub id: [u8; 16],
    pub kind: PalletKind,
    pub version: u64,
    pub attributes: Attributes,
    /// False when the superblock or a table did not check out. Such a pallet
    /// is never selected and never counts as a fallback.
    pub readable: bool,
}

impl Candidate {
    /// Total order, highest first: priority, then version, then the partition
    /// GUID as a tie-break.
    ///
    /// The GUID is there so the order is *total* rather than merely
    /// deterministic-in-practice. Two pallets that agree on priority and
    /// version would otherwise be ordered by whatever sequence the caller
    /// happened to collect them in — and a fallback chain that depends on scan
    /// order is one that differs between firmware and the running system.
    pub fn order_key(&self) -> (u8, u64, [u8; 16]) {
        (self.attributes.priority, self.version, self.id)
    }

    /// Usable at all: parses, has a priority, and either was confirmed good or
    /// still has attempts left.
    pub fn is_eligible(&self) -> bool {
        self.readable && self.attributes.is_candidate()
    }

    /// Does this pallet compete in the given kind's ladder?
    ///
    /// `None` means "do not filter". A pallet written before the `kind` field
    /// existed says `Unspecified`, and a boot consumer should still consider
    /// it — refusing would strand a node on an older image over a field it
    /// never wrote.
    pub fn in_ladder(&self, kind: Option<PalletKind>) -> bool {
        match kind {
            Some(k) => self.kind == k || self.kind == PalletKind::Unspecified,
            None => true,
        }
    }

    fn competes(&self, kind: Option<PalletKind>) -> bool {
        self.is_eligible() && self.in_ladder(kind)
    }
}

/// The pallet a consumer would use right now.
pub fn select(candidates: &[Candidate], kind: Option<PalletKind>) -> Option<Candidate> {
    candidates
        .iter()
        .filter(|c| c.competes(kind))
        .max_by_key(|c| c.order_key())
        .copied()
}

/// What to use instead of `failed` — the next one down the ladder.
///
/// This is the whole of rollback as a *decision*. Making it stick is an
/// attribute write, which a read-only consumer does not do: firmware falls
/// back by simply trying the next one.
pub fn fallback_after(
    candidates: &[Candidate],
    failed: &[u8; 16],
    kind: Option<PalletKind>,
) -> Option<Candidate> {
    match candidates.iter().find(|c| c.id == *failed && c.competes(kind)) {
        // Strictly below the failed one in the total order.
        Some(f) => {
            let key = f.order_key();
            candidates
                .iter()
                .filter(|c| c.competes(kind) && c.order_key() < key)
                .max_by_key(|c| c.order_key())
                .copied()
        }
        // The failed pallet is not in the ladder at all — priority 0, out of
        // tries, or unreadable. Then the head of the ladder *is* the answer.
        None => select(candidates, kind),
    }
}

/// Order a caller's array in place, highest first, with the pallets that do not
/// compete moved to the back.
///
/// Returns how many of them do compete: `&candidates[..n]` is the fallback
/// chain, in the order it would be walked. In place because a `no_std`
/// consumer has nowhere to put a second array.
pub fn sort_candidates(candidates: &mut [Candidate], kind: Option<PalletKind>) -> usize {
    // Insertion sort: the arrays here are a handful of pallets, and it keeps
    // this allocation-free without pulling in a sort that is not in `core`.
    let n = candidates.len();
    let mut competing = 0;
    for i in 0..n {
        if candidates[i].competes(kind) {
            candidates.swap(i, competing);
            competing += 1;
        }
    }
    for i in 1..competing {
        let mut j = i;
        while j > 0 && candidates[j - 1].order_key() < candidates[j].order_key() {
            candidates.swap(j - 1, j);
            j -= 1;
        }
    }
    competing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u8, version: u64, priority: u8, tries: u8, good: bool, kind: PalletKind) -> Candidate {
        let mut raw = [0u8; 16];
        raw[0] = id;
        Candidate {
            id: raw,
            kind,
            version,
            attributes: Attributes {
                priority,
                tries_left: tries,
                successful: good,
                sealed: true,
                read_only: true,
                required: true,
            },
            readable: true,
        }
    }

    #[test]
    fn priority_wins_and_version_breaks_the_tie() {
        let low = cand(1, 9, 1, 1, true, PalletKind::Boot);
        let old = cand(2, 1, 5, 1, true, PalletKind::Boot);
        let new = cand(3, 2, 5, 1, true, PalletKind::Boot);
        let mut all = [low, old, new];
        assert_eq!(select(&all, Some(PalletKind::Boot)).unwrap().id, new.id);
        let n = sort_candidates(&mut all, Some(PalletKind::Boot));
        assert_eq!(n, 3);
        assert_eq!([all[0].id, all[1].id, all[2].id], [new.id, old.id, low.id]);
    }

    #[test]
    fn a_pallet_that_cannot_boot_is_not_in_the_ladder_at_all() {
        let disabled = cand(1, 3, 0, 3, true, PalletKind::Boot);
        let spent = cand(2, 2, 5, 0, false, PalletKind::Boot);
        let good = cand(3, 1, 5, 1, false, PalletKind::Boot);
        let mut broken = cand(4, 4, 9, 3, true, PalletKind::Boot);
        broken.readable = false;

        let mut all = [disabled, spent, good, broken];
        assert_eq!(sort_candidates(&mut all, Some(PalletKind::Boot)), 1);
        assert_eq!(all[0].id, good.id);
        assert_eq!(select(&all, Some(PalletKind::Boot)).unwrap().id, good.id);
    }

    #[test]
    fn kinds_do_not_compete_but_an_unlabelled_pallet_still_counts() {
        let boot = cand(1, 1, 5, 1, true, PalletKind::Boot);
        let kube = cand(2, 9, 15, 1, true, PalletKind::Kube);
        let legacy = cand(3, 2, 6, 1, true, PalletKind::Unspecified);
        let all = [boot, kube, legacy];

        assert_eq!(
            select(&all, Some(PalletKind::Boot)).unwrap().id,
            legacy.id,
            "a pallet written before `kind` existed is still a boot candidate"
        );
        let mut ladder = all;
        let n = sort_candidates(&mut ladder, Some(PalletKind::Boot));
        assert!(
            !ladder[..n].iter().any(|c| c.id == kube.id),
            "a kube pallet does not outrank boot by carrying a bigger number"
        );
        assert_eq!(select(&all, Some(PalletKind::Kube)).unwrap().id, kube.id);
    }

    #[test]
    fn falling_back_takes_the_next_one_down() {
        let newest = cand(1, 3, 15, 1, false, PalletKind::Boot);
        let previous = cand(2, 2, 14, 1, true, PalletKind::Boot);
        let oldest = cand(3, 1, 13, 1, true, PalletKind::Boot);
        let all = [newest, previous, oldest];

        assert_eq!(fallback_after(&all, &newest.id, None).unwrap().id, previous.id);
        assert_eq!(fallback_after(&all, &previous.id, None).unwrap().id, oldest.id);
        assert!(fallback_after(&all, &oldest.id, None).is_none(), "the chain ends");
    }

    #[test]
    fn falling_back_from_something_not_in_the_ladder_starts_at_the_top() {
        let disabled = cand(1, 3, 0, 1, true, PalletKind::Boot);
        let good = cand(2, 2, 5, 1, true, PalletKind::Boot);
        let all = [disabled, good];
        assert_eq!(fallback_after(&all, &disabled.id, None).unwrap().id, good.id);
        assert_eq!(fallback_after(&all, &[0xFF; 16], None).unwrap().id, good.id);
    }

    /// Two pallets agreeing on priority *and* version must still order the same
    /// way everywhere, or firmware and the running system disagree about which
    /// one is the fallback.
    #[test]
    fn the_order_is_total_even_when_priority_and_version_tie() {
        let a = cand(1, 4, 5, 1, true, PalletKind::Boot);
        let b = cand(2, 4, 5, 1, true, PalletKind::Boot);
        assert_eq!(select(&[a, b], None).unwrap().id, b.id);
        assert_eq!(select(&[b, a], None).unwrap().id, b.id, "not scan order");
        assert_eq!(fallback_after(&[a, b], &b.id, None).unwrap().id, a.id);
    }
}
