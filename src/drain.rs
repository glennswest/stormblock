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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::placement::topology::StorageTier;
    use crate::volume::{CreateOptions, RedundancyPolicy};

    async fn slab(dir: &std::path::Path, name: &str, slot: u64) -> Slab {
        let path = dir.join(name);
        let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), 8 * 1024 * 1024).await.unwrap();
        Slab::format(std::sync::Arc::new(dev), slot, StorageTier::Hot).await.unwrap()
    }

    /// A drive is drained one extent at a time; when nothing of any volume is
    /// left on it the drain says `empty`, the data still reads, and the slab
    /// takes allocations again only if it was not the one being retired.
    #[tokio::test]
    async fn a_drain_empties_a_slab_and_keeps_the_data_readable() {
        let dir = std::env::temp_dir().join(format!("stormblock-drain-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let slot = 4096u64;
        let mut vm = VolumeManager::new(slot);
        let a = slab(&dir, "a.bin", slot).await;
        let b = slab(&dir, "b.bin", slot).await;
        let c = slab(&dir, "c.bin", slot).await;
        let (ida, _idb, _idc) = (a.slab_id(), b.slab_id(), c.slab_id());
        vm.add_slab(a).await;
        vm.add_slab(b).await;
        vm.add_slab(c).await;

        // A plain volume and a mirrored one, both with legs on `a`.
        let plain = vm.create_volume_any("plain", 1 << 20).await.unwrap();
        let mirror = vm
            .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
            .await
            .unwrap();
        let pv = vm.get_volume(&plain).unwrap();
        let mv = vm.get_volume(&mirror).unwrap();
        for i in 0..6u64 {
            pv.write(i * slot, &vec![0x10 + i as u8; slot as usize]).await.unwrap();
            mv.write(i * slot, &vec![0x40 + i as u8; slot as usize]).await.unwrap();
        }
        let gem = vm.gem().clone();
        let registry = vm.registry().clone();
        let legs_on_a_before = gem.read().await.slab_extents(ida).len();
        assert!(legs_on_a_before > 0, "something landed on a");

        let volumes = Arc::new(tokio::sync::Mutex::new(vm));
        let mut drains = Drains::default();
        let status = drains.start("a.bin".into(), vec![ida], gem.clone(), registry.clone(), volumes.clone());
        for _ in 0..500 {
            if status.read().await.state != DrainState::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let st = status.read().await.clone();
        assert_eq!(st.state, DrainState::Empty, "{st:?}");
        assert_eq!(st.moved as usize, legs_on_a_before);
        assert_eq!(st.remaining, 0);
        assert!(gem.read().await.slab_extents(ida).is_empty());
        assert_eq!(registry.read().await.get(&ida).unwrap().allocated_slots(), 0, "the slab is empty");

        for i in 0..6u64 {
            let mut buf = vec![0u8; slot as usize];
            pv.read(i * slot, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&x| x == 0x10 + i as u8));
            mv.read(i * slot, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&x| x == 0x40 + i as u8));
        }
        // The mirror is still two legs on two different slabs, neither `a`.
        let g = gem.read().await;
        for i in 0..6u64 {
            let l = g.lookup(mirror, i).unwrap();
            assert_eq!(l.leg_count(), 2);
            assert!(l.legs().all(|leg| leg.slab_id != ida));
            let legs: Vec<_> = l.legs().collect();
            assert_ne!(legs[0].slab_id, legs[1].slab_id);
        }
        drop(g);
        assert!(!registry.read().await.is_quarantined(&ida), "an empty drive is released");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
