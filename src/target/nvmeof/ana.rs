//! Asymmetric Namespace Access (NVMe 1.4 §8.20) — #83.
//!
//! A volume served from two nodes (the same subsystem NQN, the same NSID and
//! NGUID: the per-volume serve subsystems are named by the volume) is one
//! multipath namespace to a Linux initiator. ANA is how each node tells the
//! host which path to use: the node the volume is moving to answers
//! `optimized`, the node it is leaving `inaccessible`, and an ANA change
//! notice makes the host re-read the log page and follow without an unmount.
//!
//! The state belongs to the *volume* on this node, not to one subsystem: a
//! volume may be served from the shared subsystem, a host's own and its
//! per-volume portal at once, and every one of them must say the same. So it
//! is kept here, process-wide, keyed by the device UUID (the NGUID), and
//! every subsystem asks.
//!
//! Groups are by state, five of them, each always reported: 1 optimized,
//! 2 non-optimized, 3 inaccessible, 4 persistent loss, 5 change. A namespace
//! moves between groups when its state changes (ANACAP bit 6 clear), which
//! the Linux host follows from the NSID lists of the log page. That keeps
//! the log page a fixed size whatever NSIDs a subsystem uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Log page identifier: Asymmetric Namespace Access.
pub const LID_ANA: u8 = 0x0C;

/// Completion DW0 for an ANA change notice: type 0x2 (Notice), info 0x03
/// (Asymmetric Namespace Access Change), log page 0x0C.
pub const AEN_ANA_CHANGE: u32 = 0x2 | (0x03 << 8) | ((LID_ANA as u32) << 16);

/// Number of ANA groups (NANAGRPID) and the largest group ID (ANAGRPMAX).
pub const GROUPS: u32 = 5;

/// ANA transition time (ANATT), seconds: how long a host waits in `change`.
pub const ANATT_SECS: u8 = 10;

/// ANACAP: optimized, non-optimized, inaccessible, persistent loss and
/// change are all reported; bit 6 clear (a namespace's group may change).
pub const ANACAP: u8 = 0x1F;

/// Maximum NSID and Maximum Number of Allowed Namespaces (NN, MNAN). Linux
/// sizes its ANA log buffer from MNAN and refuses a controller whose MNAN is
/// zero or above NN.
pub const MAX_NAMESPACES: u32 = 1024;

/// One namespace's access state, as the log page encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnaState {
    #[default]
    Optimized,
    NonOptimized,
    Inaccessible,
    PersistentLoss,
    Change,
}

impl AnaState {
    pub const ALL: [AnaState; 5] = [
        AnaState::Optimized,
        AnaState::NonOptimized,
        AnaState::Inaccessible,
        AnaState::PersistentLoss,
        AnaState::Change,
    ];

    /// The state's code in the log page.
    pub fn code(self) -> u8 {
        match self {
            AnaState::Optimized => 0x01,
            AnaState::NonOptimized => 0x02,
            AnaState::Inaccessible => 0x03,
            AnaState::PersistentLoss => 0x04,
            AnaState::Change => 0x0F,
        }
    }

    /// The group a namespace in this state is reported in (1..=5).
    pub fn group(self) -> u32 {
        match self {
            AnaState::Optimized => 1,
            AnaState::NonOptimized => 2,
            AnaState::Inaccessible => 3,
            AnaState::PersistentLoss => 4,
            AnaState::Change => 5,
        }
    }

    /// The path-related status (SCT 3) an I/O command gets in this state, or
    /// `None` when the namespace serves I/O here. The host retries such a
    /// command on another path and re-reads the log page.
    pub fn io_refusal(self) -> Option<u8> {
        match self {
            AnaState::Optimized | AnaState::NonOptimized => None,
            AnaState::PersistentLoss => Some(0x01),
            AnaState::Inaccessible => Some(0x02),
            AnaState::Change => Some(0x03),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AnaState::Optimized => "optimized",
            AnaState::NonOptimized => "non_optimized",
            AnaState::Inaccessible => "inaccessible",
            AnaState::PersistentLoss => "persistent_loss",
            AnaState::Change => "change",
        }
    }

    pub fn parse(s: &str) -> Option<AnaState> {
        AnaState::ALL.into_iter().find(|a| a.as_str() == s || a.as_str().replace('_', "-") == s)
    }
}

struct Registry {
    states: RwLock<HashMap<Uuid, AnaState>>,
    /// Bumped on every change: the log page's change count.
    chgcnt: AtomicU64,
    /// The volume whose state changed, to every admin connection.
    changes: tokio::sync::broadcast::Sender<Uuid>,
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry {
        states: RwLock::new(HashMap::new()),
        chgcnt: AtomicU64::new(0),
        changes: tokio::sync::broadcast::channel(256).0,
    })
}

/// The state of the volume whose device UUID this is (optimized unless set).
pub fn state_of(volume: Uuid) -> AnaState {
    registry().states.read().unwrap_or_else(|e| e.into_inner()).get(&volume).copied().unwrap_or_default()
}

/// Every volume whose state is not the default.
pub fn all() -> HashMap<Uuid, AnaState> {
    registry().states.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Set a volume's state. Connected hosts of every subsystem serving it are
/// told (an ANA change notice). True when it changed.
pub fn set(volume: Uuid, state: AnaState) -> bool {
    let r = registry();
    let changed = {
        let mut m = r.states.write().unwrap_or_else(|e| e.into_inner());
        let old = m.get(&volume).copied().unwrap_or_default();
        if state == AnaState::default() {
            m.remove(&volume);
        } else {
            m.insert(volume, state);
        }
        old != state
    };
    if changed {
        r.chgcnt.fetch_add(1, Ordering::SeqCst);
        let _ = r.changes.send(volume);
    }
    changed
}

/// Put back states kept across a restart, before anything is served. No
/// notices: nobody is connected yet.
pub fn load(states: HashMap<Uuid, AnaState>) {
    let mut m = registry().states.write().unwrap_or_else(|e| e.into_inner());
    m.clear();
    m.extend(states.into_iter().filter(|(_, s)| *s != AnaState::default()));
}

pub fn subscribe() -> tokio::sync::broadcast::Receiver<Uuid> {
    registry().changes.subscribe()
}

pub fn change_count() -> u64 {
    registry().chgcnt.load(Ordering::SeqCst)
}

/// The ANA log page for namespaces `(nsid, state)`, `len` bytes from `offset`.
///
/// Header (16 bytes): change count, group count. Then one 32-byte
/// descriptor per group — ID, NSID count, change count, state — each
/// followed by its NSIDs in ascending order (Linux walks them against its
/// sorted namespace list). `rgo` (Return Groups Only) leaves the NSIDs out.
pub fn log_page(namespaces: &[(u32, AnaState)], rgo: bool, offset: usize, len: usize) -> Vec<u8> {
    let chgcnt = change_count();
    let mut page = Vec::with_capacity(16 + GROUPS as usize * 32 + namespaces.len() * 4);
    page.extend_from_slice(&chgcnt.to_le_bytes());
    page.extend_from_slice(&(GROUPS as u16).to_le_bytes());
    page.extend_from_slice(&[0u8; 6]);
    for state in AnaState::ALL {
        let mut nsids: Vec<u32> = namespaces.iter().filter(|(_, s)| *s == state).map(|(n, _)| *n).collect();
        nsids.sort_unstable();
        let mut desc = [0u8; 32];
        desc[0..4].copy_from_slice(&state.group().to_le_bytes());
        let listed = if rgo { 0 } else { nsids.len() as u32 };
        desc[4..8].copy_from_slice(&listed.to_le_bytes());
        desc[8..16].copy_from_slice(&chgcnt.to_le_bytes());
        desc[16] = state.code();
        page.extend_from_slice(&desc);
        if !rgo {
            for n in nsids {
                page.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    let start = offset.min(page.len());
    let mut out = page[start..].to_vec();
    out.resize(len, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_their_names() {
        for s in AnaState::ALL {
            assert_eq!(AnaState::parse(s.as_str()), Some(s));
        }
        assert_eq!(AnaState::parse("non-optimized"), Some(AnaState::NonOptimized));
        assert_eq!(AnaState::parse("bogus"), None);
    }

    #[test]
    fn aen_dw0_encodes_an_ana_change() {
        assert_eq!(AEN_ANA_CHANGE & 0x7, 0x2);
        assert_eq!((AEN_ANA_CHANGE >> 8) & 0xFF, 0x03);
        assert_eq!((AEN_ANA_CHANGE >> 16) & 0xFF, 0x0C);
    }

    #[test]
    fn log_page_groups_namespaces_by_state_in_nsid_order() {
        let page = log_page(
            &[(7, AnaState::Inaccessible), (3, AnaState::Optimized), (1, AnaState::Optimized)],
            false,
            0,
            4096,
        );
        assert_eq!(u16::from_le_bytes([page[8], page[9]]), 5);
        // Group 1: optimized, NSIDs 1 and 3.
        let g1 = &page[16..48];
        assert_eq!(u32::from_le_bytes(g1[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(g1[4..8].try_into().unwrap()), 2);
        assert_eq!(g1[16], 0x01);
        assert_eq!(u32::from_le_bytes(page[48..52].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(page[52..56].try_into().unwrap()), 3);
        // Group 2: non-optimized, empty.
        let g2 = &page[56..88];
        assert_eq!(u32::from_le_bytes(g2[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(g2[4..8].try_into().unwrap()), 0);
        assert_eq!(g2[16], 0x02);
        // Group 3: inaccessible, NSID 7.
        let g3 = &page[88..120];
        assert_eq!(u32::from_le_bytes(g3[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(g3[4..8].try_into().unwrap()), 1);
        assert_eq!(g3[16], 0x03);
        assert_eq!(u32::from_le_bytes(page[120..124].try_into().unwrap()), 7);
        // Groups 4 and 5 follow, every state nonzero (Linux refuses a zero).
        assert_eq!(page[124 + 16], 0x04);
        assert_eq!(page[156 + 16], 0x0F);
        assert_eq!(page.len(), 4096);
    }

    #[test]
    fn return_groups_only_lists_no_nsids() {
        let page = log_page(&[(1, AnaState::Optimized)], true, 0, 16 + 5 * 32);
        assert_eq!(u32::from_le_bytes(page[20..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(page[48..52].try_into().unwrap()), 2, "next descriptor follows directly");
    }

    #[test]
    fn a_set_state_is_announced_and_counted() {
        let v = Uuid::new_v4();
        let mut rx = subscribe();
        let before = change_count();
        assert_eq!(state_of(v), AnaState::Optimized);
        assert!(set(v, AnaState::Inaccessible));
        assert!(!set(v, AnaState::Inaccessible), "no change, no notice");
        assert_eq!(state_of(v), AnaState::Inaccessible);
        assert!(change_count() > before);
        assert_eq!(rx.try_recv().unwrap(), v);
        assert!(set(v, AnaState::Optimized));
        assert!(!all().contains_key(&v), "the default is not kept");
    }

    #[test]
    fn io_is_refused_with_the_path_status_only_where_not_served() {
        assert_eq!(AnaState::Optimized.io_refusal(), None);
        assert_eq!(AnaState::NonOptimized.io_refusal(), None);
        assert_eq!(AnaState::Inaccessible.io_refusal(), Some(0x02));
        assert_eq!(AnaState::PersistentLoss.io_refusal(), Some(0x01));
        assert_eq!(AnaState::Change.io_refusal(), Some(0x03));
    }
}
