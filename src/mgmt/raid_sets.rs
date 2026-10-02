//! RAID sets in the engine: what assembles them at startup, registers them,
//! gives each its failure domain, and finds the slab on it (#252).
//!
//! The arrays themselves (`crate::raid`) know nothing of slabs, volumes or
//! the API; this is where the two meet. The same calls serve the daemon's
//! startup (every configured drive scanned) and the API (`POST
//! /api/v1/arrays/assemble`, for drives registered after startup — an
//! `nvme-tcp://` leg, say).

use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::{AppState, ArrayInfo};
use crate::placement::domain::FailureDomain;
use crate::raid::{RaidArray, RaidArrayId};

/// The failure domain of a set's slab: `shelf=<pool>/set=<name>` above the
/// array's own identity. A set is one domain — every slab on it fails with
/// it — and the shelf groups the sets that share an enclosure.
pub fn set_domain(pool: &str, name: &str) -> FailureDomain {
    let mut labels = Vec::new();
    if !pool.is_empty() {
        labels.push(("shelf".to_string(), pool.to_string()));
    }
    if !name.is_empty() {
        labels.push(("set".to_string(), name.to_string()));
    }
    FailureDomain::from_labels(labels)
}

/// Take an array on: its domain labels, its supervisor (spares, rebuild),
/// and its place in `state.arrays`. Does not touch its slab.
pub async fn register(state: &Arc<AppState>, array: Arc<RaidArray>) {
    let id = array.array_id();
    let domain = set_domain(&array.pool(), &array.name());
    if !domain.is_empty() {
        state.slab_registry.write().await.label_device(&array.id().path, domain);
    }
    array.start(Some(state.spares.clone()));
    let info = ArrayInfo {
        level: array.level(),
        member_count: array.member_count(),
        capacity_bytes: array.capacity_bytes(),
        stripe_size: array.stripe_size(),
        array,
    };
    let n = {
        let mut arrays = state.arrays.write().await;
        arrays.insert(id, info);
        arrays.len()
    };
    metrics::gauge!("stormblock_arrays_total").set(n as f64);
}

/// The array a drive is a member of, and its slot.
pub async fn member_of(state: &AppState, dev: &Arc<dyn BlockDevice>) -> Option<(RaidArrayId, usize)> {
    let arrays = state.arrays.read().await;
    arrays.iter().find_map(|(id, info)| info.array.slot_of(dev.id()).map(|slot| (*id, slot)))
}

/// What assembly found and did.
#[derive(Debug, Default, Serialize)]
pub struct AssembleReport {
    pub arrays: Vec<AssembledArray>,
    /// Spares found and taken into the pool.
    pub spares: Vec<Uuid>,
    /// Arrays that could not be put back together, and why.
    pub refused: Vec<Refused>,
    /// Drives whose superblock is damaged.
    pub damaged: Vec<Refused>,
    /// Drives that were members of an assembled array and are no longer —
    /// replaced after they failed. Left alone; `force` on a spare or create
    /// request reuses one.
    pub stale: Vec<String>,
    /// Slabs found on the arrays and adopted, and the volumes they held.
    pub slabs_adopted: usize,
    pub volumes_adopted: usize,
}

#[derive(Debug, Serialize)]
pub struct AssembledArray {
    pub id: Uuid,
    pub name: String,
    pub level: String,
    pub state: String,
    /// Already held by this engine; nothing was done.
    pub already: bool,
}

#[derive(Debug, Serialize)]
pub struct Refused {
    pub what: String,
    pub error: String,
}

/// Read the drives' superblocks; put every array they describe back
/// together, register it and adopt the slab on it; take spares into the
/// pool. An array already held is left alone. Returns, with the report, the
/// drives that turned out to be members or spares — those carry no slab of
/// their own and must not be scanned or formatted as plain drives.
pub async fn assemble_and_adopt(
    state: &Arc<AppState>,
    devs: &[Arc<dyn BlockDevice>],
) -> (AssembleReport, Vec<Arc<dyn BlockDevice>>) {
    let scan = crate::raid::scan(devs).await;
    let mut report = AssembleReport::default();
    let claimed: Vec<Arc<dyn BlockDevice>> = devs.iter().filter(|d| scan.claims(d)).cloned().collect();
    for (path, e) in &scan.damaged {
        tracing::error!("drive {path}: damaged RAID superblock ({e}); left alone");
        report.damaged.push(Refused { what: path.clone(), error: e.clone() });
    }

    let mut found_slabs = Vec::new();
    for group in scan.arrays {
        let uuid = group[0].1.array_uuid;
        let name = group[0].1.name.clone();
        if state.arrays.read().await.contains_key(&RaidArrayId(uuid)) {
            report.arrays.push(AssembledArray {
                id: uuid,
                name,
                level: group[0].1.level.map(|l| l.to_string()).unwrap_or_default(),
                state: "held".into(),
                already: true,
            });
            continue;
        }
        let members_of_group = group.clone();
        match RaidArray::assemble(group).await {
            Ok(array) => {
                let array = Arc::new(array);
                for (d, _) in &members_of_group {
                    if array.slot_of(d.id()).is_none() {
                        tracing::warn!(
                            "drive {} carries an old superblock of array {uuid} ('{name}'): it was replaced; left alone",
                            d.id().path
                        );
                        report.stale.push(d.id().path.clone());
                    }
                }
                let st = array.status();
                tracing::info!(
                    "assembled {} {} '{}' from its drives: {} ({} of {} members failed)",
                    array.level(),
                    array.array_id(),
                    array.name(),
                    st.state,
                    st.failed,
                    array.member_count()
                );
                let dev: Arc<dyn BlockDevice> = array.clone();
                found_slabs.extend(crate::drive::discover::slabs_in_partitions(&dev).await);
                report.arrays.push(AssembledArray {
                    id: uuid,
                    name: array.name(),
                    level: array.level().to_string(),
                    state: st.state.to_string(),
                    already: false,
                });
                register(state, array).await;
            }
            Err(e) => {
                tracing::error!("array {uuid} ('{name}') not assembled: {e}; its drives are left alone");
                report.refused.push(Refused { what: format!("array {uuid} ({name})"), error: e.to_string() });
            }
        }
    }
    for (dev, sb) in &scan.spares {
        if !state.spares.holds(dev) {
            state.spares.adopt(dev.clone(), sb);
            report.spares.push(sb.member_uuid);
            tracing::info!(
                "hot spare {} on {} (pool '{}')",
                sb.member_uuid,
                dev.id().path,
                if sb.pool.is_empty() { "global" } else { &sb.pool }
            );
        }
    }
    if !found_slabs.is_empty() {
        let mut vm = state.volume_manager.lock().await;
        match vm.adopt_slabs(found_slabs).await {
            Ok(r) => {
                report.slabs_adopted = r.slabs.len();
                report.volumes_adopted = r.volumes.len();
            }
            Err(e) => tracing::warn!("adopting the slabs on the assembled arrays: {e}"),
        }
    }
    (report, claimed)
}

/// Whether a drive is held by an array or is a spare — what a create, an
/// add or a spare request must refuse (#215).
pub async fn drive_in_use(state: &AppState, dev: &Arc<dyn BlockDevice>) -> Option<String> {
    if let Some((id, slot)) = member_of(state, dev).await {
        return Some(format!("drive {} is slot {slot} of array {id}", dev.id().uuid));
    }
    if state.spares.holds(dev) {
        return Some(format!("drive {} is a hot spare", dev.id().uuid));
    }
    None
}
