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
//! One lock per slot that is held, made when first taken and dropped with
//! the last guard, so it costs memory only for slots in use (#331). It was
//! 4096 hashed shards: two slots in one shard waited for each other, and with
//! the flow-over moving several extents at once, a move holding a shard could
//! wait on its own read when an engine in the same process served that read
//! from a slot in the same shard (a test's in-process appliance).
//!
//! A parity volume fences a whole stripe — every member and parity leg —
//! for a read-modify-write, a discard, a verify or a resync (#240,
//! `ThinVolumeHandle::fenced_stripe`), and never takes a slot twice in one
//! operation: a second shared hold queues behind a waiting move.
//!
//! Lock order: a volume's extent shards and its volume lock, then the fence,
//! then the map and the registry. I/O never waits for the fence while
//! holding the map or the registry. A move that already holds them (the
//! whole-run rebalance and evacuation) can only *try* the fence
//! ([`try_exclusive`]), and reports the slot busy when an I/O has it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::volume::gem::Leg;

/// The table of held slots is split so I/O on different slots rarely meets
/// on one mutex; each is held only to find or drop an entry.
const TABLES: usize = 64;

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

type Table = Mutex<HashMap<Leg, Arc<RwLock<()>>>>;

static FENCE: LazyLock<Vec<Table>> = LazyLock::new(|| (0..TABLES).map(|_| Mutex::new(HashMap::new())).collect());

fn table(leg: Leg) -> &'static Table {
    let (hi, lo) = leg.slab_id.0.as_u64_pair();
    let h = (hi ^ lo ^ leg.slot_idx).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &FENCE[(h >> 32) as usize % TABLES]
}

/// The lock of `leg`, made when nobody holds one.
fn lock_of(leg: Leg) -> Arc<RwLock<()>> {
    table(leg).lock().unwrap().entry(leg).or_default().clone()
}

/// Drops `leg`'s lock from the table once nothing else refers to it: the
/// table's copy and `lock` are the only two (a new holder clones it under
/// the table's mutex, so the count cannot rise while it is held).
fn release(leg: Leg, lock: Arc<RwLock<()>>) {
    let mut t = table(leg).lock().unwrap();
    if Arc::strong_count(&lock) == 2 {
        t.remove(&leg);
    }
}

/// One slot's guard, either kind, that takes its lock out of the table when
/// it is the last.
struct Guard<G> {
    leg: Leg,
    guard: Option<G>,
    lock: Option<Arc<RwLock<()>>>,
}

impl<G> Drop for Guard<G> {
    fn drop(&mut self) {
        self.guard.take();
        if let Some(lock) = self.lock.take() {
            release(self.leg, lock);
        }
    }
}

/// Shared guards on the slots an I/O is using.
pub struct Held {
    _guards: Vec<Guard<OwnedRwLockReadGuard<()>>>,
}

/// The exclusive guard a move holds on the slot it moves.
pub struct Exclusive {
    leg: Leg,
    _guard: Option<Guard<OwnedRwLockWriteGuard<()>>>,
}

/// The order slots are taken in, the same for every holder.
fn order(leg: &Leg) -> (u128, u64) {
    (leg.slab_id.0.as_u128(), leg.slot_idx)
}

/// Hold every leg shared. Taken in one order, so two holders can never wait
/// for each other.
pub async fn hold(legs: impl IntoIterator<Item = Leg>) -> Held {
    if off() {
        return Held { _guards: Vec::new() };
    }
    let mut legs: Vec<Leg> = legs.into_iter().collect();
    legs.sort_unstable_by_key(order);
    legs.dedup();
    let mut guards = Vec::with_capacity(legs.len());
    for leg in legs {
        let lock = lock_of(leg);
        // Made before the wait, so a cancelled wait still releases the entry.
        let mut g = Guard { leg, guard: None, lock: Some(lock.clone()) };
        g.guard = Some(lock.read_owned().await);
        guards.push(g);
    }
    Held { _guards: guards }
}

impl Exclusive {
    /// Whether this guard covers `leg` — the one the move is about to copy.
    pub fn covers(&self, leg: Leg) -> bool {
        self.leg == leg
    }
}

/// Wait for every I/O on `leg` to finish and keep new ones out. Never while
/// holding the map or the registry.
pub async fn exclusive(leg: Leg) -> Exclusive {
    if off() {
        return Exclusive { leg, _guard: None };
    }
    let lock = lock_of(leg);
    let mut g = Guard { leg, guard: None, lock: Some(lock.clone()) };
    g.guard = Some(lock.write_owned().await);
    Exclusive { leg, _guard: Some(g) }
}

/// The exclusive guard if nothing holds `leg` now.
pub fn try_exclusive(leg: Leg) -> Option<Exclusive> {
    if off() {
        return Some(Exclusive { leg, _guard: None });
    }
    let lock = lock_of(leg);
    let mut g = Guard { leg, guard: None, lock: Some(lock.clone()) };
    g.guard = Some(lock.try_write_owned().ok()?);
    Some(Exclusive { leg, _guard: Some(g) })
}

/// Slots with a lock in the table now (tests).
#[cfg(test)]
fn held_now() -> usize {
    FENCE.iter().map(|t| t.lock().unwrap().len()).sum()
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

    #[tokio::test]
    async fn two_slots_never_wait_for_each_other_and_nothing_is_kept() {
        // Every pair of slots is independent: the old shards made some wait.
        let ex = exclusive(leg(10)).await;
        for s in 11..5000 {
            assert!(try_exclusive(leg(s)).is_some(), "slot {s} waits for slot 10");
            let h = tokio::time::timeout(std::time::Duration::from_secs(1), hold([leg(s)])).await;
            assert!(h.is_ok(), "an I/O on slot {s} waits for a move of slot 10");
        }
        assert!(!ex.covers(leg(11)));
        drop(ex);
        // A failed try and a dropped wait leave nothing behind.
        let ex = exclusive(leg(20)).await;
        assert!(try_exclusive(leg(20)).is_none());
        let w = tokio::spawn(async { hold([leg(20)]).await; });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        w.abort();
        let _ = w.await;
        drop(ex);
        assert_eq!(held_now(), 0, "every lock dropped with its last guard");
    }
}
