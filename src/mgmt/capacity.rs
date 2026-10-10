//! What a pool can promise, and what is promised out of it (#152).
//!
//! Thin provisioning is bounded per pool. A drive's **overcommit** (off, or
//! a ratio 1–16, set by stormdrive, stormdrive#13) scales what its slabs can
//! promise. A pool is a role (`system`, `data`); a dedicated array slab
//! (#150) is a pool of its own.
//!
//! - **promisable** = Σ over the pool's slabs of capacity × ratio (× 1 when
//!   off);
//! - **written** = its allocated slots;
//! - **committed** = written + what each unsealed volume placed in it can
//!   still take: the extents it does not hold alone (unwritten, or shared with
//!   a golden — a clone's shared extents are counted once, at the slot's
//!   owner), times its redundancy overhead. A sealed volume (golden, blank,
//!   snapshot) takes nothing more.
//!
//! **Admission** (owner, on #152: the master's A with C's switch): the
//! system half is counted and reported and never refuses (laid by the
//! install, thin by design). The data half (and dedicated array slabs) is
//! checked when a claim binds, against `[capacity] admission`: `report` (the
//! default, until real nodes publish numbers) admits it and logs one that
//! would be refused; `enforce` refuses it, with the numbers.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::drive::slab::{SlabId, SlabRole};
use crate::mgmt::AppState;
use crate::volume::VolumeId;

/// Kept in the data directory.
pub const OVERCOMMIT_FILE: &str = "overcommit.json";
/// The most a drive's slabs may promise per byte.
pub const MAX_RATIO: f64 = 16.0;

/// A drive's overcommit, as stormdrive sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DriveOvercommit {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub wwn: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub path: String,
    pub enabled: bool,
    pub ratio: f64,
}

impl DriveOvercommit {
    /// What a byte of this drive's slabs may promise.
    pub fn factor(&self) -> f64 {
        if self.enabled { self.ratio.clamp(1.0, MAX_RATIO) } else { 1.0 }
    }

    /// Whether this is the drive behind `id` (a slab's `drive_id()`): its
    /// wwn, else its serial, else its path (stormdrive#13).
    pub fn names(&self, wwn: &str, serial: &str, path: &str) -> bool {
        if !self.wwn.is_empty() && !wwn.is_empty() {
            return self.wwn.eq_ignore_ascii_case(wwn);
        }
        if !self.serial.is_empty() && !serial.is_empty() {
            return self.serial == serial;
        }
        !self.path.is_empty() && self.path == path
    }
}

fn file(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join(OVERCOMMIT_FILE))
}

/// Every drive's overcommit this node was told.
pub fn load(state: &AppState) -> Vec<DriveOvercommit> {
    file(state)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Keep `o`, replacing the entry for the same drive.
pub fn set(state: &AppState, o: DriveOvercommit) -> Result<(), String> {
    if o.enabled && !(1.0..=MAX_RATIO).contains(&o.ratio) {
        return Err(format!("ratio {} is not between 1 and {MAX_RATIO}", o.ratio));
    }
    if o.wwn.is_empty() && o.serial.is_empty() && o.path.is_empty() {
        return Err("name the drive: wwn, serial or path".into());
    }
    let mut all = load(state);
    all.retain(|x| !x.names(&o.wwn, &o.serial, &o.path));
    all.push(o);
    let p = file(state).ok_or("this node keeps no data directory")?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&all).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// What admission does in the data half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Report,
    Enforce,
}

pub fn admission(state: &AppState) -> Admission {
    let word = std::env::var("STORMBLOCK_ADMISSION")
        .ok()
        .or_else(|| state.config.capacity.admission.clone())
        .unwrap_or_default();
    if word.trim().eq_ignore_ascii_case("enforce") { Admission::Enforce } else { Admission::Report }
}

/// One pool's numbers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Pool {
    pub pool: String,
    pub role: String,
    pub promisable_bytes: u64,
    pub written_bytes: u64,
    pub committed_bytes: u64,
    pub free_bytes: u64,
    /// promisable − committed (negative: promised past what it can).
    pub headroom_bytes: i64,
    /// Whether a claim here is checked against it.
    pub enforced: bool,
}

/// The pools, and each slab's share of what is committed.
#[derive(Debug, Clone, Default)]
pub struct Accounting {
    pub pools: Vec<Pool>,
    pub slab_committed: HashMap<SlabId, u64>,
    pub slab_pool: HashMap<SlabId, String>,
}

fn pool_key(role: SlabRole, dedicated: Option<SlabId>) -> String {
    match dedicated {
        Some(s) => format!("array:{}", s.0),
        None => role.to_string(),
    }
}

/// Count it all, now.
pub async fn account(state: &AppState) -> Accounting {
    let oc = load(state);
    // Slabs: promisable and written, per pool.
    struct S {
        pool: String,
        promisable: f64,
        written: u64,
        free: u64,
    }
    let mut slabs: HashMap<SlabId, S> = HashMap::new();
    let mut roles: HashMap<String, SlabRole> = HashMap::new();
    {
        let reg = state.slab_registry.read().await;
        for (id, s) in reg.iter() {
            let d = s.device().drive_id();
            let factor = oc.iter().find(|o| o.names(&d.wwn, &d.serial, &d.path)).map(|o| o.factor()).unwrap_or(1.0);
            let cap = (s.total_slots() * s.slot_size()) as f64;
            let pool = pool_key(s.role(), s.is_dedicated().then_some(*id));
            roles.insert(pool.clone(), s.role());
            slabs.insert(*id, S {
                pool,
                promisable: cap * factor,
                written: s.allocated_slots() * s.slot_size(),
                free: s.free_slots() * s.slot_size(),
            });
        }
    }
    // Volumes: what each unsealed one can still take, per pool.
    let vols: Vec<(VolumeId, u64, bool, std::sync::Arc<crate::volume::ThinVolumeHandle>)> = {
        let vm = state.volume_manager.lock().await;
        vm.list_volumes()
            .await
            .into_iter()
            .filter_map(|(id, _, virt, _)| vm.get_volume_handle(&id).map(|h| (id, virt, vm.is_sealed(&id), h)))
            .collect()
    };
    let mut promise: HashMap<String, f64> = HashMap::new();
    {
        let gem = state.gem.read().await;
        for (id, virt, sealed, h) in &vols {
            if *sealed {
                continue;
            }
            let ext = h.extent_size().max(1);
            let extents = virt.div_ceil(ext);
            let held = if gem.is_cold(id) {
                gem.cold(id).map(|c| c.exclusive as u64).unwrap_or(0)
            } else {
                gem.get_volume_map(id).map(|m| m.exclusive() as u64).unwrap_or(0)
            };
            let more = extents.saturating_sub(held) as f64 * ext as f64 * h.redundancy().scheme.overhead();
            *promise.entry(pool_key(h.placement_role(), h.pinned_slab())).or_default() += more;
        }
    }
    // Pools.
    let mut acc = Accounting::default();
    let mut by_pool: HashMap<String, (f64, u64, u64)> = HashMap::new();
    for s in slabs.values() {
        let e = by_pool.entry(s.pool.clone()).or_default();
        e.0 += s.promisable;
        e.1 += s.written;
        e.2 += s.free;
    }
    for (pool, (promisable, written, free)) in &by_pool {
        let p = promise.get(pool).copied().unwrap_or(0.0);
        let committed = *written + p as u64;
        let role = roles.get(pool).copied().unwrap_or(SlabRole::Data);
        acc.pools.push(Pool {
            pool: pool.clone(),
            role: role.to_string(),
            promisable_bytes: *promisable as u64,
            written_bytes: *written,
            committed_bytes: committed,
            free_bytes: *free,
            headroom_bytes: *promisable as i64 - committed as i64,
            enforced: role == SlabRole::Data,
        });
    }
    acc.pools.sort_by(|a, b| a.pool.cmp(&b.pool));
    // Each slab: what is written on it, and its share of the pool's promise.
    for (id, s) in &slabs {
        let pool_promisable = by_pool[&s.pool].0;
        let share = if pool_promisable > 0.0 { s.promisable / pool_promisable } else { 0.0 };
        let p = promise.get(&s.pool).copied().unwrap_or(0.0);
        acc.slab_committed.insert(*id, s.written + (p * share) as u64);
        acc.slab_pool.insert(*id, s.pool.clone());
    }
    acc
}

/// After a claim bound `vol` (#152): its pool past what it can promise? In
/// the data half under `enforce`, `Err(why)` — the caller takes the claim
/// back and refuses it. Otherwise `Ok` (a `report` refusal is logged).
pub async fn admit(state: &AppState, vol: VolumeId) -> Result<(), String> {
    let pool = {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume_handle(&vol) {
            Some(h) => (pool_key(h.placement_role(), h.pinned_slab()), h.placement_role()),
            None => return Ok(()),
        }
    };
    if pool.1 != SlabRole::Data {
        return Ok(());
    }
    let acc = account(state).await;
    let Some(p) = acc.pools.iter().find(|p| p.pool == pool.0) else { return Ok(()) };
    if p.committed_bytes <= p.promisable_bytes {
        return Ok(());
    }
    let why = format!(
        "pool {} would promise {} of {} it can (written {}; raise the drives' overcommit in stormdrive, \
         or free space) (#152)",
        p.pool,
        crate::mgmt::config::human_size(p.committed_bytes),
        crate::mgmt::config::human_size(p.promisable_bytes),
        crate::mgmt::config::human_size(p.written_bytes),
    );
    match admission(state) {
        Admission::Enforce => Err(why),
        Admission::Report => {
            tracing::warn!("capacity: admitted, though [capacity] admission = enforce would refuse it: {why}");
            Ok(())
        }
    }
}

/// [`admit`] for an API that has just made `vol`: when it is refused, the
/// volume is deleted again and the answer is a 507 naming the pool's
/// numbers. Call it with no volume manager lock held.
pub async fn admit_or_undo(state: &AppState, vol: VolumeId) -> Option<axum::response::Response> {
    use axum::response::IntoResponse;
    let why = admit(state, vol).await.err()?;
    if let Err(e) = state.volume_manager.lock().await.delete_volume(vol).await {
        tracing::warn!("capacity: refused claim {} could not be taken back: {e}", vol.0);
    }
    tracing::warn!("capacity: refused a claim: {why}");
    Some(
        (
            axum::http::StatusCode::INSUFFICIENT_STORAGE,
            axum::Json(json!({ "code": "insufficient_capacity", "message": why, "error": why })),
        )
            .into_response(),
    )
}

/// What `/api/v1/slabs/pool` adds.
pub async fn report(state: &AppState) -> Value {
    let acc = account(state).await;
    json!({
        "admission": match admission(state) { Admission::Enforce => "enforce", Admission::Report => "report" },
        "pools": acc.pools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drive_is_matched_by_wwn_then_serial_then_path() {
        let o = DriveOvercommit { wwn: "naa.5000c500".into(), serial: "S1".into(), path: "/dev/sda".into(), enabled: true, ratio: 2.0, ..Default::default() };
        assert!(o.names("NAA.5000C500", "other", "/dev/sdz"), "wwn wins, any case");
        assert!(!o.names("naa.ffff", "S1", "/dev/sda"), "a different wwn is a different drive");
        let s = DriveOvercommit { serial: "S1".into(), path: "/dev/sda".into(), enabled: true, ratio: 2.0, ..Default::default() };
        assert!(s.names("", "S1", "/dev/sdb"));
        let p = DriveOvercommit { path: "/dev/sda".into(), enabled: true, ratio: 2.0, ..Default::default() };
        assert!(p.names("", "", "/dev/sda") && !p.names("", "", "/dev/sdb"));
        assert_eq!(DriveOvercommit { enabled: false, ratio: 8.0, ..Default::default() }.factor(), 1.0);
        assert_eq!(DriveOvercommit { enabled: true, ratio: 99.0, ..Default::default() }.factor(), MAX_RATIO);
    }
}
