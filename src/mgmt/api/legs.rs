//! Where each leg of a volume's extents is on its drive (#176, #51 item 3).
//!
//! A reader that must not reconstruct RAID — firmware, an initramfs, a
//! recovery tool reading drives with the engine down — needs, for each
//! extent, every copy it could read and where that copy is on its drive, so
//! it can take one good leg and stop. A mirrored extent has several data
//! legs; a parity extent has one, and the stripe's P and Q are listed apart,
//! for a reader that does reconstruct.
//!
//! `GET /api/v1/volumes/{id}/legs[?start=<extent>&limit=<n>]`: the volume's
//! mapped extents from `start` (an extent index), at most `limit` (4096,
//! at most 65536), and `next` when there are more. Each leg names its drive
//! (path, serial, WWN, model: the disk, not the partition) and the absolute
//! byte offset of the slot on it: the slab's place on the drive (a
//! partition's start) + the slab's data offset + slot × slot size. A
//! drive's path is the engine's; a fabric drive's is its URI, and the offset
//! is on that namespace. Unmapped extents read as zeros and are not listed.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::slabs::DriveRef;
use super::ApiError;
use crate::drive::slab::SlabId;
use crate::mgmt::AppState;
use crate::volume::gem::Leg;
use crate::volume::VolumeId;

const DEFAULT_LIMIT: usize = 4096;
const MAX_LIMIT: usize = 65536;

#[derive(Debug, Deserialize)]
pub struct LegsQuery {
    #[serde(default)]
    pub start: u64,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct VolumeLegs {
    pub volume: String,
    /// Bytes per extent (the slot size); an extent's virtual byte range is
    /// `extent × extent_bytes` for `extent_bytes`.
    pub extent_bytes: u64,
    pub extents: Vec<ExtentLegs>,
    /// The parity groups of the stripes these extents are in (RAID-5/6
    /// policies only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parity: Vec<StripeLegs>,
    /// The `start` of the next page, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ExtentLegs {
    pub extent: u64,
    /// The extent's first byte in the volume.
    pub offset: u64,
    /// Shared with another volume (a clone and its golden): the same slot.
    pub shared: bool,
    /// The primary first, then the mirrors.
    pub legs: Vec<LegAt>,
}

#[derive(Debug, Serialize)]
pub struct StripeLegs {
    pub stripe: u64,
    /// The data extents of the stripe, in order.
    pub members: Vec<u64>,
    /// P, then Q (RAID-6).
    pub legs: Vec<LegAt>,
}

#[derive(Debug, Serialize)]
pub struct LegAt {
    pub slab: String,
    pub slot: u64,
    pub drive: DriveRef,
    /// The slot's first byte on the drive.
    pub drive_offset: u64,
    pub bytes: u64,
    /// `ok`; `failed` (this volume stopped trusting the slab: do not read
    /// it); `missing` (no such slab on this node).
    pub state: &'static str,
}

/// `GET /api/v1/volumes/{id}/legs`.
pub async fn volume_legs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<LegsQuery>,
) -> Response {
    let id = match super::volumes::volume_key(&state, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let handle = state.volume_manager.lock().await.get_volume_handle(&id);
    let Some(handle) = handle else {
        return ApiError::not_found(format!("volume {} not found", id.0));
    };
    // A cold map is paged in first (#158), with no manager lock held.
    if let Err(e) = handle.resident().await {
        return ApiError::internal(format!("loading volume {}'s extent map: {e}", id.0));
    }
    let failed = handle.health().await.failed_slabs;
    let extent_bytes = handle.extent_size();
    match legs_of(&state, id, extent_bytes, &failed, q.start, q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)).await {
        Some(v) => Json(v).into_response(),
        None => ApiError::not_found(format!("volume {} has no extent map", id.0)),
    }
}

/// The page of a volume's legs from extent `start`, at most `limit` extents.
/// The map then the registry, both read, nothing else held (#364).
pub async fn legs_of(
    state: &Arc<AppState>,
    id: VolumeId,
    extent_bytes: u64,
    failed: &[SlabId],
    start: u64,
    limit: usize,
) -> Option<VolumeLegs> {
    let gem = state.gem.read().await;
    let map = gem.get_volume_map(&id);
    let reg = state.slab_registry.read().await;
    let at = |leg: Leg| -> LegAt {
        match reg.get(&leg.slab_id) {
            Some(slab) => {
                let size = slab.slot_size();
                LegAt {
                    slab: leg.slab_id.0.to_string(),
                    slot: leg.slot_idx,
                    drive: DriveRef::of(slab.device()),
                    drive_offset: slab.device().drive_offset() + slab.data_offset() + leg.slot_idx * size,
                    bytes: size,
                    state: if failed.contains(&leg.slab_id) { "failed" } else { "ok" },
                }
            }
            None => LegAt {
                slab: leg.slab_id.0.to_string(),
                slot: leg.slot_idx,
                drive: DriveRef { serial: String::new(), wwn: String::new(), model: String::new(), path: String::new() },
                drive_offset: 0,
                bytes: 0,
                state: "missing",
            },
        }
    };
    let Some(map) = map else {
        // Nothing written yet: nothing mapped.
        return Some(VolumeLegs { volume: id.0.to_string(), extent_bytes, extents: Vec::new(), parity: Vec::new(), next: None });
    };
    let mut keys = map.extents.keys_in(start..u64::MAX);
    let mut extents = Vec::new();
    let mut next = None;
    for vext in keys.by_ref() {
        if extents.len() == limit {
            next = Some(vext);
            break;
        }
        let Some(loc) = map.extents.get(&vext) else { continue };
        extents.push(ExtentLegs {
            extent: vext,
            offset: vext * extent_bytes,
            shared: loc.ref_count > 1,
            legs: loc.legs().map(at).collect(),
        });
    }
    let mut parity = Vec::new();
    if let (Some(first), Some(last)) = (extents.first(), extents.last()) {
        for (stripe, group) in map.parity.iter() {
            let members = group.members(*stripe);
            if members.end <= first.extent || members.start > last.extent {
                continue;
            }
            parity.push(StripeLegs { stripe: *stripe, members: members.collect(), legs: group.legs.iter().copied().map(at).collect() });
        }
    }
    Some(VolumeLegs { volume: id.0.to_string(), extent_bytes, extents, parity, next })
}
