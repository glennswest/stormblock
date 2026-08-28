//! Drain a drive over HTTP (#70 item 3): move every leg off every slab on a
//! device, one extent at a time, until nothing is left and the drive is safe
//! to pull.
//!
//! The placement engine's `evacuate_slab` holds the GEM and the registry for
//! its whole run, which is fine for a CLI and not for a node serving volumes.
//! A drain takes the two locks per extent, releases them, yields, and goes
//! again — so I/O flows between moves and a cancel lands within one extent.
//! The slabs being drained are quarantined first, so nothing new arrives
//! while the old leaves.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::PlacementEngine;
use crate::volume::gem::{GlobalExtentMap, Leg};
use crate::volume::VolumeManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainState {
    Running,
    /// Nothing of any volume remains on the device: safe to remove.
    Empty,
    /// Some legs could not be moved; they are listed. The drive is not safe
    /// to remove.
    Stuck,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrainStatus {
    pub drive: String,
    pub state: DrainState,
    pub slabs: Vec<SlabId>,
    pub moved: u64,
    pub failed: u64,
    /// Legs still on the device, data and parity.
    pub remaining: u64,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    pub errors: Vec<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A drain in progress or finished, one per drive path.
pub struct Drain {
    pub status: Arc<RwLock<DrainStatus>>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl Drain {
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

/// The drains this node has been asked for.
#[derive(Default)]
pub struct Drains {
    by_drive: HashMap<String, Drain>,
}

impl Drains {
    pub fn get(&self, drive: &str) -> Option<&Drain> {
        self.by_drive.get(drive)
    }

    pub async fn status(&self, drive: &str) -> Option<DrainStatus> {
        match self.by_drive.get(drive) {
            Some(d) => Some(d.status.read().await.clone()),
            None => None,
        }
    }

    pub async fn all(&self) -> Vec<DrainStatus> {
        let mut out = Vec::new();
        for d in self.by_drive.values() {
            out.push(d.status.read().await.clone());
        }
        out
    }

    pub async fn is_running(&self, drive: &str) -> bool {
        matches!(self.status(drive).await, Some(s) if s.state == DrainState::Running)
    }

    /// Start draining the slabs on `drive`. The caller has checked that the
    /// slabs are on that device and that none is the metadata slab.
    pub fn start(
        &mut self,
        drive: String,
        slabs: Vec<SlabId>,
        gem: Arc<RwLock<GlobalExtentMap>>,
        registry: Arc<RwLock<SlabRegistry>>,
        volumes: Arc<tokio::sync::Mutex<VolumeManager>>,
    ) -> Arc<RwLock<DrainStatus>> {
        let status = Arc::new(RwLock::new(DrainStatus {
            drive: drive.clone(),
            state: DrainState::Running,
            slabs: slabs.clone(),
            moved: 0,
            failed: 0,
            remaining: 0,
            started_at: now(),
            finished_at: None,
            errors: Vec::new(),
        }));
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let st = status.clone();
        tokio::spawn(async move {
            run(slabs, gem, registry, volumes, st, cancel_rx).await;
        });
        self.by_drive.insert(drive, Drain { status: status.clone(), cancel });
        status
    }
}

/// Legs of anything still on these slabs.
fn remaining_on(gem: &GlobalExtentMap, slabs: &[SlabId]) -> u64 {
    slabs
        .iter()
        .map(|s| gem.slab_extents(*s).len() as u64 + gem.slab_parity(*s).len() as u64)
        .sum()
}

async fn run(
    slabs: Vec<SlabId>,
    gem: Arc<RwLock<GlobalExtentMap>>,
    registry: Arc<RwLock<SlabRegistry>>,
    volumes: Arc<tokio::sync::Mutex<VolumeManager>>,
    status: Arc<RwLock<DrainStatus>>,
    cancel: tokio::sync::watch::Receiver<bool>,
) {
    let engine = PlacementEngine::new();
    // Legs that failed to move: skipped so one bad extent does not stall
    // everything behind it.
    let mut stuck: HashSet<Leg> = HashSet::new();

    {
        let mut reg = registry.write().await;
        for s in &slabs {
            reg.set_quarantined(*s, true);
        }
    }

    let mut since_persist = 0u32;
    loop {
        if *cancel.borrow() {
            let mut st = status.write().await;
            st.state = DrainState::Cancelled;
            st.finished_at = Some(now());
            let mut reg = registry.write().await;
            for s in &slabs {
                reg.set_quarantined(*s, false);
            }
            break;
        }

        // One step under the locks: pick a leg, move it.
        let step = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            let mut pick = None;
            'slabs: for s in &slabs {
                for (vol, vext, loc) in g.slab_extents(*s) {
                    if let Some(leg) = loc.leg_on(*s) {
                        if !stuck.contains(&leg) {
                            pick = Some((vol, vext, *s, false));
                            break 'slabs;
                        }
                    }
                }
                for (vol, stripe, grp) in g.slab_parity(*s) {
                    if let Some(leg) = grp.legs.iter().find(|l| l.slab_id == *s) {
                        if !stuck.contains(leg) {
                            pick = Some((vol, stripe, *s, true));
                            break 'slabs;
                        }
                    }
                }
            }
            match pick {
                None => None,
                Some((vol, idx, slab, is_parity)) => {
                    let res = if is_parity {
                        engine.migrate_parity_leg(&mut g, &mut r, vol, idx, slab, None).await
                    } else {
                        engine.migrate_leg(&mut g, &mut r, vol, idx, slab, None).await
                    };
                    let remaining = remaining_on(&g, &slabs);
                    Some((res, remaining, vol, idx, slab, is_parity))
                }
            }
        };

        match step {
            None => {
                let remaining = remaining_on(&*gem.read().await, &slabs);
                let mut st = status.write().await;
                st.remaining = remaining;
                st.finished_at = Some(now());
                st.state = if remaining == 0 { DrainState::Empty } else { DrainState::Stuck };
                if remaining == 0 {
                    volumes.lock().await.persist().await;
                }
                // A stuck drive stays quarantined: it is still leaving.
                if remaining == 0 {
                    let mut reg = registry.write().await;
                    for s in &slabs {
                        reg.set_quarantined(*s, false);
                    }
                }
                break;
            }
            Some((Ok(_), remaining, _, _, _, _)) => {
                let mut st = status.write().await;
                st.moved += 1;
                st.remaining = remaining;
                since_persist += 1;
            }
            Some((Err(e), remaining, vol, idx, slab, is_parity)) => {
                // Find the leg to skip.
                let leg = {
                    let g = gem.read().await;
                    if is_parity {
                        g.lookup_parity(vol, idx).and_then(|grp| grp.legs.iter().copied().find(|l| l.slab_id == slab))
                    } else {
                        g.lookup(vol, idx).and_then(|l| l.leg_on(slab))
                    }
                };
                if let Some(l) = leg {
                    stuck.insert(l);
                }
                let mut st = status.write().await;
                st.failed += 1;
                st.remaining = remaining;
                if st.errors.len() < 32 {
                    st.errors.push(format!(
                        "{} {} of volume {vol}: {e}",
                        if is_parity { "parity of stripe" } else { "extent" },
                        idx
                    ));
                }
            }
        }

        if since_persist >= 64 {
            volumes.lock().await.persist().await;
            since_persist = 0;
        }
        tokio::task::yield_now().await;
    }
}
