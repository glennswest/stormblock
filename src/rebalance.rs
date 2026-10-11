//! Rebalance the pool (#154): move extents from fuller slabs onto emptier ones
//! of the same pool, so a drive that was added is used, not only filled by
//! whatever new writes happen to land on it.
//!
//! **Planned read-only, then moved one extent at a time.** The plan reads the
//! extent map and the registry under their read locks and holds nothing
//! after. Each move then holds only the fence on its own source slot while it
//! copies (`PlacementEngine::migrate_leg_unlocked`, as the flow-over and the
//! drain do): never the map or the registry across a copy (#269). Between
//! moves it gives the disks back to live I/O, and `max_bytes_per_sec` caps it.
//!
//! **What a pool is:** slabs of one role (system or data) and one slot size
//! (#156). A dedicated slab (#150), and a quarantined one (a drain, a health
//! report) are not in it. A slab above the pool's mean fill gives extents
//! to slabs below it, until each is near the mean.
//!
//! **What moves:** a data leg of an extent, to a slab where the volume's
//! legs stay apart at its own rung (`mirror:2@shelf`: still another shelf).
//! A shared slot (a golden and its clones) moves once, for every map naming
//! it. Extents of a parity volume stay: a stripe's members and P/Q are
//! placed together, and moving one member alone could put two of them in
//! one domain.
//!
//! **Durable as the drain is:** a source slot is owed until the map that no
//! longer names it is on disk (persist, then release, every 64 moves and at
//! the end).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::PlacementEngine;
use crate::volume::gem::{GlobalExtentMap, Leg};
use crate::volume::{VolumeId, VolumeManager};

/// What `POST /api/v1/slabs/rebalance` takes.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// Plan and report, move nothing. **The default**: a rebalance is asked
    /// for with `"dry_run": false`.
    #[serde(default = "yes")]
    pub dry_run: bool,
    /// `system` or `data`: one half only. Absent: both.
    #[serde(default)]
    pub pool: Option<String>,
    /// At most this many moves (default 10 000).
    #[serde(default)]
    pub max_moves: Option<usize>,
    /// Bytes per second the moves may copy. Absent: as fast as live I/O
    /// leaves room for.
    #[serde(default)]
    pub max_bytes_per_sec: Option<u64>,
}

fn yes() -> bool {
    true
}

const DEFAULT_MAX_MOVES: usize = 10_000;
/// A slab within this much of its pool's mean fill is not a source. One above
/// it gives extents until it is at its share, to slabs below theirs.
const TOLERANCE: f64 = 0.02;
const YIELD_MAX: std::time::Duration = std::time::Duration::from_secs(2);

/// One extent to move.
#[derive(Debug, Clone, Serialize)]
pub struct Move {
    pub volume: VolumeId,
    pub extent: u64,
    pub from: SlabId,
    pub to: SlabId,
    pub bytes: u64,
}

/// A slab's fill before and after the plan.
#[derive(Debug, Clone, Serialize)]
pub struct SlabFill {
    pub id: SlabId,
    pub domain: String,
    pub total_slots: u64,
    pub used_before: u64,
    pub used_after: u64,
}

/// One pool: slabs of one role and slot size.
#[derive(Debug, Clone, Serialize)]
pub struct PoolPlan {
    pub role: String,
    pub slot_size: u64,
    /// The pool's mean fill, 0..1.
    pub mean_fill: f64,
    pub slabs: Vec<SlabFill>,
    pub moves: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub pools: Vec<PoolPlan>,
    pub moves: usize,
    pub bytes: u64,
    /// Extents on a slab above the mean that stay: no slab below the mean
    /// keeps the volume's legs apart, or the volume has parity.
    pub left: u64,
    #[serde(skip)]
    pub list: Vec<Move>,
}

/// Plan a rebalance. `spread`: each volume's rung (the drive unless said).
pub fn plan(
    gem: &GlobalExtentMap,
    reg: &SlabRegistry,
    spread: &HashMap<VolumeId, String>,
    pool: Option<&str>,
    max_moves: usize,
) -> Plan {
    // Volumes whose stripes stay as they are.
    let parity: HashSet<VolumeId> = gem
        .resident_ids()
        .into_iter()
        .filter(|v| gem.get_volume_map(v).is_some_and(|m| !m.parity.is_empty()))
        .collect();

    let mut pools: HashMap<(String, u64), Vec<SlabId>> = HashMap::new();
    for (id, slab) in reg.iter() {
        if reg.is_dedicated(id) || reg.is_quarantined(id) {
            continue;
        }
        let role = reg.role_of(id).to_string();
        if pool.is_some_and(|p| !p.eq_ignore_ascii_case(&role)) {
            continue;
        }
        pools.entry((role, slab.slot_size())).or_default().push(*id);
    }

    let mut out = Plan { pools: Vec::new(), moves: 0, bytes: 0, left: 0, list: Vec::new() };
    let mut keys: Vec<_> = pools.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let mut ids = pools.remove(&key).unwrap_or_default();
        ids.sort_by_key(|id| id.0);
        let (role, slot_size) = key;
        let mut used: HashMap<SlabId, u64> = HashMap::new();
        let mut total: HashMap<SlabId, u64> = HashMap::new();
        for id in &ids {
            let s = reg.get(id).expect("listed");
            total.insert(*id, s.total_slots());
            used.insert(*id, s.total_slots().saturating_sub(s.free_slots()));
        }
        let before = used.clone();
        let sum_total: u64 = total.values().sum();
        let mean = if sum_total == 0 { 0.0 } else { used.values().sum::<u64>() as f64 / sum_total as f64 };
        let mut moves = 0usize;
        if ids.len() >= 2 {
            let fill = |u: u64, t: u64| if t == 0 { 1.0 } else { u as f64 / t as f64 };
            // Where each slab is going: its share of the pool's fill.
            let target: HashMap<SlabId, u64> =
                ids.iter().map(|id| (*id, (mean * total[id] as f64).ceil() as u64)).collect();
            let over: Vec<SlabId> =
                ids.iter().copied().filter(|id| fill(used[id], total[id]) > mean + TOLERANCE).collect();
            let mut seen: HashSet<Leg> = HashSet::new();
            // Where an extent's legs will be once the moves planned so far
            // are made: two legs of one extent must never be planned onto
            // one slab, or into one domain at the volume's rung.
            let mut placed: HashMap<(VolumeId, u64), Vec<SlabId>> = HashMap::new();
            for src in over {
                for (vol, vext, loc) in gem.slab_extents(src) {
                    if out.list.len() >= max_moves || used[&src] <= target[&src] {
                        break;
                    }
                    let Some(leg) = loc.leg_on(src) else { continue };
                    if !seen.insert(leg) {
                        continue;
                    }
                    if parity.contains(&vol) {
                        out.left += 1;
                        continue;
                    }
                    let rung = spread.get(&vol).map(String::as_str).unwrap_or("drive");
                    let legs = placed.entry((vol, vext)).or_insert_with(|| loc.legs().map(|l| l.slab_id).collect());
                    let others: Vec<_> = legs.iter().filter(|s| **s != src).map(|s| reg.domain_of(s)).collect();
                    // The emptiest slab below its target that keeps the legs apart.
                    let to = ids
                        .iter()
                        .copied()
                        .filter(|id| *id != src && !legs.contains(id))
                        .filter(|id| used[id] < target[id])
                        .filter(|id| !reg.collides(id, &others, rung))
                        .min_by(|a, b| fill(used[a], total[a]).total_cmp(&fill(used[b], total[b])));
                    let Some(to) = to else {
                        out.left += 1;
                        continue;
                    };
                    for s in legs.iter_mut() {
                        if *s == src {
                            *s = to;
                        }
                    }
                    *used.get_mut(&src).unwrap() -= 1;
                    *used.get_mut(&to).unwrap() += 1;
                    out.list.push(Move { volume: vol, extent: vext, from: src, to, bytes: slot_size });
                    moves += 1;
                }
            }
        }
        out.pools.push(PoolPlan {
            role,
            slot_size,
            mean_fill: mean,
            slabs: ids
                .iter()
                .map(|id| SlabFill {
                    id: *id,
                    domain: reg.domain_of(id).to_string(),
                    total_slots: total[id],
                    used_before: before[id],
                    used_after: used[id],
                })
                .collect(),
            moves,
            bytes: moves as u64 * slot_size,
        });
    }
    out.moves = out.list.len();
    out.bytes = out.list.iter().map(|m| m.bytes).sum();
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub state: State,
    pub planned: usize,
    pub moved: u64,
    /// Changed since the plan (rewritten, freed, moved by something else):
    /// left where it is.
    pub skipped: u64,
    pub failed: u64,
    pub bytes: u64,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    pub errors: Vec<String>,
}

/// The node's one rebalance (the last one, finished or not).
#[derive(Default)]
pub struct Rebalance {
    job: Option<(Arc<RwLock<Status>>, tokio::sync::watch::Sender<bool>)>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Rebalance {
    pub async fn status(&self) -> Option<Status> {
        match &self.job {
            Some((s, _)) => Some(s.read().await.clone()),
            None => None,
        }
    }

    pub async fn running(&self) -> bool {
        match &self.job {
            Some((s, _)) => s.read().await.state == State::Running,
            None => false,
        }
    }

    pub fn cancel(&self) -> bool {
        match &self.job {
            Some((_, c)) => c.send(true).is_ok(),
            None => false,
        }
    }

    /// Run `plan`'s moves in the background.
    pub fn start(
        &mut self,
        moves: Vec<Move>,
        max_bytes_per_sec: Option<u64>,
        gem: Arc<crate::lockwatch::TrackedRwLock<GlobalExtentMap>>,
        registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
        volumes: Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    ) -> Arc<RwLock<Status>> {
        let status = Arc::new(RwLock::new(Status {
            state: State::Running,
            planned: moves.len(),
            moved: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
            started_at: now(),
            finished_at: None,
            errors: Vec::new(),
        }));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let st = status.clone();
        tokio::spawn(crate::lockwatch::named("the pool's rebalance".to_string(), async move {
            run(moves, max_bytes_per_sec, gem, registry, volumes, st, rx).await;
        }));
        self.job = Some((status.clone(), tx));
        status
    }
}

async fn persist_then_release(
    volumes: &Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    registry: &Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    engine: &PlacementEngine,
) {
    VolumeManager::persist_detached(volumes).await;
    let mut reg = registry.write().await;
    engine.release_owed(&mut reg).await;
}

async fn run(
    moves: Vec<Move>,
    max_bytes_per_sec: Option<u64>,
    gem: Arc<crate::lockwatch::TrackedRwLock<GlobalExtentMap>>,
    registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    volumes: Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    status: Arc<RwLock<Status>>,
    cancel: tokio::sync::watch::Receiver<bool>,
) {
    let _pin = match crate::volume::gem::pin_resident(&gem).await {
        Ok(p) => p,
        Err(e) => {
            let mut st = status.write().await;
            st.errors.push(format!("loading extent maps: {e}"));
            st.state = State::Done;
            st.finished_at = Some(now());
            return;
        }
    };
    let engine = PlacementEngine::new();
    let began = std::time::Instant::now();
    let mut foreground = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
    let mut last_move = std::time::Duration::ZERO;
    let mut copied = 0u64;
    let mut since_persist = 0u32;
    let mut state = State::Done;
    for m in moves {
        if *cancel.borrow() {
            state = State::Cancelled;
            break;
        }
        // Live I/O first, as the flow-over and the drain do.
        if crate::volume::thin::FOREGROUND_IO.load(Relaxed) != foreground {
            tokio::time::sleep((last_move / 4).min(YIELD_MAX)).await;
        }
        foreground = crate::volume::thin::FOREGROUND_IO.load(Relaxed);
        if let Some(rate) = max_bytes_per_sec.filter(|r| *r > 0) {
            let due = std::time::Duration::from_secs_f64(copied as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(began.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
        let started = std::time::Instant::now();
        // Still as planned: the extent's leg on `from`, and `to` still a
        // place for it (not quarantined since, the legs still apart).
        let leg = {
            let g = gem.read().await;
            let r = registry.read().await;
            g.lookup(m.volume, m.extent).and_then(|loc| {
                let leg = loc.leg_on(m.from)?;
                let ok = !r.is_quarantined(&m.to) && !loc.legs().any(|l| l.slab_id == m.to);
                ok.then_some(leg)
            })
        };
        let Some(leg) = leg else {
            status.write().await.skipped += 1;
            continue;
        };
        let fence = crate::volume::fence::exclusive(leg).await;
        let res = engine.migrate_leg_unlocked(&gem, &registry, m.volume, m.extent, leg, m.to, &fence).await;
        drop(fence);
        last_move = started.elapsed();
        let mut st = status.write().await;
        match res {
            Ok(_) => {
                st.moved += 1;
                st.bytes += m.bytes;
                copied += m.bytes;
                since_persist += 1;
            }
            Err(crate::placement::PlacementError::Busy { .. } | crate::placement::PlacementError::ExtentNotFound { .. }) => {
                st.skipped += 1;
            }
            Err(e) => {
                st.failed += 1;
                if st.errors.len() < 32 {
                    st.errors.push(format!("extent {} of volume {}: {e}", m.extent, m.volume));
                }
            }
        }
        drop(st);
        if since_persist >= 64 {
            persist_then_release(&volumes, &registry, &engine).await;
            since_persist = 0;
        }
    }
    persist_then_release(&volumes, &registry, &engine).await;
    let mut st = status.write().await;
    st.state = state;
    st.finished_at = Some(now());
    tracing::info!(
        "rebalance {:?}: {} moved ({} bytes), {} skipped, {} failed",
        st.state,
        st.moved,
        st.bytes,
        st.skipped,
        st.failed
    );
}
