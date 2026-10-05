//! The slot fence: I/O and a move of the same slot never overlap (#239).
//!
//! A volume's I/O looks its extent up in the map, lets the map go, and then
//! reads or writes the slot it found — the unreplicated steady state takes no
//! lock at all between the two. A move (the flow-over, a drain, a rebalance)
//! copies a slot, points every map at the copy and later frees the source,
//! and freeing discards it. Landing in that gap is lost data:
//!
//! - a write to the source after the copy is not in the copy;
//! - a read of the source after it was freed reads a discarded slot — zeros
//!   on a thin device;
//! - a copy-on-write that reads the source after it was freed fills the
//!   clone's new slot with those zeros around the bytes it was writing.
//!
//! The last is what a fresh install did to `cni-bin` on 11.56: the system
//! half flowed onto the local disk in the background while Cilium copied its
//! plugins in, and the root directory block came back without its checksum
//! tail (`No space for directory leaf checksum`).
//!
//! So I/O holds a **shared** guard on every slot it looked up, and checks the
//! map again once it has it; a move holds the **exclusive** guard on the slot
//! it moves for the copy and the rewrite of the maps. Whichever comes second
//! sees the other's result: the I/O finds the new slot and uses it, or the
//! move copies what the I/O wrote. A freed source is never touched again,
//! because every I/O that found it has finished before the move could start.
//!
//! Hashed into shards, so it costs a fixed amount of memory whatever the
//! pool's size; two slots in one shard only wait for each other.
//!
//! Lock order: a volume's extent shards and its volume lock, then the fence,
//! then the map and the registry. I/O never waits for the fence while
//! holding the map or the registry. A move that already holds them (the
//! whole-run rebalance and evacuation) can only *try* the fence
//! ([`try_exclusive`]), and reports the slot busy when an I/O has it.

use std::sync::LazyLock;

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::volume::gem::Leg;

const SHARDS: usize = 4096;

/// Off switch, for a test to show what the fence prevents. Tests only.
#[cfg(test)]
pub static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Off switch for relocate-on-write (writes to a slab being emptied land in
/// place again), for a test to show what it prevents. Tests only.
#[cfg(test)]
pub static RELOCATE_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether relocate-on-write is switched off (only ever by a test).
pub fn relocate_off() -> bool {
    #[cfg(test)]
    {
        RELOCATE_OFF.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
fn off() -> bool {
    OFF.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(not(test))]
fn off() -> bool {
    false
}

static FENCE: LazyLock<Vec<RwLock<()>>> =
    LazyLock::new(|| (0..SHARDS).map(|_| RwLock::new(())).collect());

fn shard(leg: Leg) -> usize {
    let (hi, lo) = leg.slab_id.0.as_u64_pair();
    let h = (hi ^ lo ^ u64::from(leg.slot_idx)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 32) as usize % SHARDS
}

/// Shared guards on the slots an I/O is using.
pub struct Held {
    _guards: Vec<RwLockReadGuard<'static, ()>>,
}

/// The exclusive guard a move holds on the slot it moves.
pub struct Exclusive {
    leg: Leg,
    _guard: Option<RwLockWriteGuard<'static, ()>>,
}

/// Hold every leg shared. Taken in shard order, so two holders can never
/// wait for each other.
pub async fn hold(legs: impl IntoIterator<Item = Leg>) -> Held {
    if off() {
        return Held { _guards: Vec::new() };
    }
    let mut idx: Vec<usize> = legs.into_iter().map(shard).collect();
    idx.sort_unstable();
    idx.dedup();
    let mut guards = Vec::with_capacity(idx.len());
    for i in idx {
        guards.push(FENCE[i].read().await);
    }
    Held { _guards: guards }
}

impl Exclusive {
    /// Whether this guard covers `leg` — the one the move is about to copy.
    pub fn covers(&self, leg: Leg) -> bool {
        self.leg == leg || shard(self.leg) == shard(leg)
    }
}

/// Wait for every I/O on `leg` to finish and keep new ones out. Never while
/// holding the map or the registry.
pub async fn exclusive(leg: Leg) -> Exclusive {
    if off() {
        return Exclusive { leg, _guard: None };
    }
    Exclusive { leg, _guard: Some(FENCE[shard(leg)].write().await) }
}

/// The exclusive guard if nothing holds `leg` now.
pub fn try_exclusive(leg: Leg) -> Option<Exclusive> {
    if off() {
        return Some(Exclusive { leg, _guard: None });
    }
    FENCE[shard(leg)].try_write().ok().map(|g| Exclusive { leg, _guard: Some(g) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::slab::SlabId;

    fn leg(slot: u64) -> Leg {
        Leg { slab_id: SlabId(uuid::Uuid::from_u128(0x239)), slot_idx: slot }
    }

    #[tokio::test]
    async fn a_move_waits_for_io_and_io_waits_for_a_move() {
        let held = hold([leg(1), leg(2), leg(1)]).await;
        assert!(try_exclusive(leg(1)).is_none(), "an I/O holds slot 1");
        drop(held);
        let ex = try_exclusive(leg(1)).expect("nothing holds slot 1");
        assert!(ex.covers(leg(1)));
        let waiting = tokio::spawn(async { hold([leg(1)]).await; });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiting.is_finished(), "I/O waits for the move");
        drop(ex);
        waiting.await.unwrap();
    }
}
