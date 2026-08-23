#![cfg(feature = "stormfs-data")]
//! `/api/v1/stormfs` integration tests — the data-path surface StormFS
//! consumes (#49, #50).
//!
//! Driven over real HTTP against the real router, because the contract these
//! tests are pinning is a wire contract: StormFS is a separate program, and
//! what it sees is the status code and the JSON, not the engine types.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn setup_state(dir: &TempDir) -> Arc<AppState> {
    build_state(dir, StormBlockConfig::default()).await
}

/// A node with one RAID 1 array carrying one slab, registered in `arrays` so
/// tests can create volumes the ordinary way.
async fn build_state(dir: &TempDir, config: StormBlockConfig) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let member_count = 2;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let capacity_bytes = array.capacity_bytes();
    let arc_array = Arc::new(array);
    let backing: Arc<dyn BlockDevice> = arc_array.clone();

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;

    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));
    state.arrays.write().await.insert(
        array_id,
        stormblock::mgmt::ArrayInfo {
            array: arc_array,
            level: RaidLevel::Raid1,
            member_count,
            capacity_bytes,
            stripe_size: 64 * 1024,
        },
    );
    state
}

/// The one array this node has.
async fn array_id(c: &reqwest::Client, base: &str) -> String {
    let (s, v) = get(c, format!("{base}/api/v1/arrays")).await;
    assert_eq!(s, 200, "{v}");
    v["items"][0]["id"].as_str().unwrap().to_string()
}

async fn start_server(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

async fn post(c: &reqwest::Client, url: String, body: Value) -> (u16, Value) {
    let resp = c.post(url).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

async fn get(c: &reqwest::Client, url: String) -> (u16, Value) {
    let resp = c.get(url).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// Create a volume through the management API and return its id.
async fn make_volume(c: &reqwest::Client, base: &str, name: &str, size: u64) -> String {
    let array = array_id(c, base).await;
    let (s, v) = post(
        c,
        format!("{base}/api/v1/volumes"),
        json!({ "name": name, "size": size.to_string(), "array_id": array }),
    )
    .await;
    assert_eq!(s, 201, "creating {name}: {v}");
    v["id"].as_str().unwrap().to_string()
}

/// Allocate chunks, asserting success.
async fn allocate(c: &reqwest::Client, base: &str, body: Value) -> Value {
    let (s, v) = post(c, format!("{base}/api/v1/stormfs/allocate"), body).await;
    assert_eq!(s, 200, "allocate: {v}");
    v
}

// ---- #49: chunk lifecycle ----------------------------------------------

#[tokio::test]
async fn allocate_hands_back_batched_chunks_on_the_named_tier() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    // Eight chunks in one round trip — the batching the issue asks for, so a
    // 1 GiB write is not 256 round trips.
    let v = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 4, "count": 8, "hint_volume": vol, "zero": false }),
    )
    .await;

    let extents = v["extents"].as_array().unwrap();
    assert_eq!(extents.len(), 8);
    assert_eq!(v["count"], 8);
    assert_eq!(v["slot_size"], SLOT);
    for e in extents {
        assert_eq!(e["volume"], vol.as_str());
        assert_eq!(e["len"], SLOT * 4);
    }

    // Chunks do not overlap.
    let mut offsets: Vec<u64> = extents.iter().map(|e| e["offset"].as_u64().unwrap()).collect();
    offsets.sort_unstable();
    for pair in offsets.windows(2) {
        assert!(pair[1] - pair[0] >= SLOT * 4, "chunks overlap: {offsets:?}");
    }
}

/// StormFS owns which tier. Being handed a slower one silently would be worse
/// than being told there is no room.
#[tokio::test]
async fn a_tier_with_no_storage_is_507_not_a_quiet_substitution() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let (s, v) = post(
        &c,
        format!("{base}/api/v1/stormfs/allocate"),
        json!({ "tier": "cold", "len": SLOT, "count": 1, "hint_volume": vol }),
    )
    .await;
    assert_eq!(s, 507, "{v}");
    assert_eq!(v["code"], 507);
}

#[tokio::test]
async fn an_unknown_tier_is_rejected() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let (s, _) = post(
        &c,
        format!("{base}/api/v1/stormfs/allocate"),
        json!({ "tier": "tepid", "len": SLOT, "count": 1, "hint_volume": vol }),
    )
    .await;
    assert_eq!(s, 400);
}

/// The engine will not pick a volume out of the air: handing StormFS the
/// address space of something else's volume is not a recoverable mistake.
#[tokio::test]
async fn allocate_without_a_hint_refuses_before_any_chunk_exists() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    make_volume(&c, &base, "somebody-elses-volume", 8 * 1024 * 1024).await;

    let (s, v) = post(
        &c,
        format!("{base}/api/v1/stormfs/allocate"),
        json!({ "tier": "hot", "len": SLOT, "count": 1 }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("hint_volume"),
        "the error must say how to fix it: {v}"
    );
}

/// The sweeper crashes between freeing an extent and dropping its queue
/// entry, so it re-frees. That has to be a success or one crash wedges the
/// queue forever.
#[tokio::test]
async fn deallocate_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let v = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await;
    let offset = v["extents"][0]["offset"].as_u64().unwrap();
    let body = json!({ "extents": [{ "volume": vol, "offset": offset, "len": SLOT * 2 }] });

    let (s, first) = post(&c, format!("{base}/api/v1/stormfs/deallocate"), body.clone()).await;
    assert_eq!(s, 200, "{first}");
    assert_eq!(first["freed"], 1, "one extent, as the caller counted them");
    assert_eq!(first["slots_freed"], 2);
    assert_eq!(first["bytes_freed"], SLOT * 2);
    assert_eq!(first["already_free"], 0);

    let (s, again) = post(&c, format!("{base}/api/v1/stormfs/deallocate"), body).await;
    assert_eq!(s, 200, "re-freeing must succeed: {again}");
    assert_eq!(again["freed"], 0);
    assert_eq!(again["already_free"], 1);
    assert_eq!(again["bytes_freed"], 0);
}

/// Trim gives back the space and keeps the address range, so the same offsets
/// cannot be handed to a second caller while StormFS still maps them.
#[tokio::test]
async fn trim_returns_space_without_returning_the_address() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let v = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 4, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await;
    let offset = v["extents"][0]["offset"].as_u64().unwrap();

    let (s, t) = post(
        &c,
        format!("{base}/api/v1/stormfs/trim"),
        json!({ "extents": [{ "volume": vol, "offset": offset, "len": SLOT * 4 }] }),
    )
    .await;
    assert_eq!(s, 200, "{t}");
    assert_eq!(t["bytes_freed"], SLOT * 4);

    // The range is still owned, so the next chunk lands after it.
    let next = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await;
    assert_eq!(next["extents"][0]["offset"].as_u64().unwrap(), offset + SLOT * 4);

    // And the map says so: owned, with nothing under it.
    let (s, m) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{vol}")).await;
    assert_eq!(s, 200, "{m}");
    assert_eq!(m["unmapped_owned_bytes"], SLOT * 4);
}

#[tokio::test]
async fn extent_map_reports_what_the_block_layer_holds() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 2, "hint_volume": vol, "zero": false }),
    )
    .await;

    let (s, m) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{vol}")).await;
    assert_eq!(s, 200, "{m}");
    assert_eq!(m["volume"], vol.as_str());
    assert_eq!(m["slot_size"], SLOT);
    assert_eq!(m["allocated_bytes"], SLOT * 4);
    assert_eq!(m["owned_bytes"], SLOT * 4);
    assert_eq!(m["volume_version"], 0, "nothing has been committed yet");

    let extents = m["extents"].as_array().unwrap();
    assert_eq!(extents.len(), 4);
    for e in extents {
        assert_eq!(e["owned"], true, "fsck must see these as accounted for");
        assert_eq!(e["version"], 0);
        assert!(e["slab"].is_string());
    }
}

#[tokio::test]
async fn extent_map_for_an_unknown_volume_is_404() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();

    let (s, _) = get(
        &c,
        format!("{base}/api/v1/stormfs/extent-map/{}", uuid::Uuid::new_v4()),
    )
    .await;
    assert_eq!(s, 404);
}

// ---- #50: fence-free commits -------------------------------------------

/// The commit primitive end to end: stage somewhere else, swap it in, and the
/// version moves.
#[tokio::test]
async fn commit_swaps_a_range_and_bumps_its_version() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let live = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();
    let staged = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    let (s, out) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({
            "volume": vol, "offset": live, "len": SLOT * 2,
            "expected_version": 0,
            "staged": { "offset": staged },
        }),
    )
    .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["version"], 1);
    assert_eq!(out["extents_swapped"], 2);
    assert_eq!(out["extents_released"], 2);

    let (_, m) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{vol}")).await;
    assert_eq!(m["volume_version"], 1);
}

/// The whole point of the CAS. A writer that lost its lease is refused, is
/// told what version to retry at, and has corrupted nothing.
#[tokio::test]
async fn a_stale_commit_is_409_and_carries_the_current_version() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let mut offsets = Vec::new();
    for _ in 0..3 {
        offsets.push(
            allocate(
                &c,
                &base,
                json!({ "tier": "hot", "len": SLOT, "count": 1, "hint_volume": vol, "zero": false }),
            )
            .await["extents"][0]["offset"]
                .as_u64()
                .unwrap(),
        );
    }
    let (live, quick, slow) = (offsets[0], offsets[1], offsets[2]);

    let (s, _) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT, "expected_version": 0,
                "staged": { "offset": quick } }),
    )
    .await;
    assert_eq!(s, 200);

    // The slow writer still believes the range is at 0.
    let (s, err) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT, "expected_version": 0,
                "staged": { "offset": slow } }),
    )
    .await;
    assert_eq!(s, 409, "{err}");
    assert_eq!(err["current_version"], 1, "must say what to retry at: {err}");

    // Retrying at the version it was handed works, with no fencing round trip.
    let (s, ok) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT, "expected_version": 1,
                "staged": { "offset": slow } }),
    )
    .await;
    assert_eq!(s, 200, "{ok}");
    assert_eq!(ok["version"], 2);
}

/// All-or-nothing: a staged range with a gap is refused rather than replacing
/// live data with zeros half-way through.
#[tokio::test]
async fn a_staged_range_with_a_gap_is_refused() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let live = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 4, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();
    // Two slots staged, four claimed.
    let staged = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    let (s, err) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT * 4, "expected_version": 0,
                "staged": { "offset": staged } }),
    )
    .await;
    assert_eq!(s, 400, "{err}");

    // Nothing moved: the range is still at version 0 with its extents intact.
    let (_, m) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{vol}")).await;
    assert_eq!(m["volume_version"], 0);
    assert_eq!(m["allocated_bytes"], SLOT * 6);
}

#[tokio::test]
async fn an_unaligned_commit_is_refused() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let (s, _) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": SLOT / 2, "len": SLOT, "expected_version": 0 }),
    )
    .await;
    assert_eq!(s, 400);
}

/// A commit with nothing staged is an atomic truncate.
#[tokio::test]
async fn a_commit_with_no_staged_range_punches_a_hole() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let live = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 3, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    let (s, out) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT * 3, "expected_version": 0 }),
    )
    .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["extents_swapped"], 0);
    assert_eq!(out["extents_released"], 3);

    let (_, m) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{vol}")).await;
    assert_eq!(m["allocated_bytes"], 0);
    assert_eq!(m["volume_version"], 1, "a punched range must not read as untouched");
}

// ---- #50: pins ----------------------------------------------------------

#[tokio::test]
async fn a_pin_hands_back_a_snapshot_to_read_through() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await;

    let (s, pin) = post(&c, format!("{base}/api/v1/stormfs/pins"), json!({ "volume": vol })).await;
    assert_eq!(s, 201, "{pin}");
    let pin_id = pin["pin"].as_str().unwrap().to_string();
    let snapshot = pin["snapshot"].as_str().unwrap().to_string();
    assert_ne!(snapshot, vol, "a pin reads through its own volume");
    assert_eq!(pin["version"], 0);

    let (s, list) = get(&c, format!("{base}/api/v1/stormfs/pins")).await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["count"], 1);

    // The snapshot is a real volume, listed like any other.
    let (_, vols) = get(&c, format!("{base}/api/v1/volumes")).await;
    let names: Vec<&str> = vols["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert!(names.contains(&snapshot.as_str()));

    let resp = c
        .delete(format!("{base}/api/v1/stormfs/pins/{pin_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["released"], true);

    let (_, list) = get(&c, format!("{base}/api/v1/stormfs/pins")).await;
    assert_eq!(list["count"], 0);
}

/// Deleting a pin's snapshot out from under its reader is exactly what the
/// pin exists to prevent, so the volume API refuses.
#[tokio::test]
async fn a_pinned_snapshot_cannot_be_deleted_behind_the_readers_back() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let (_, pin) = post(&c, format!("{base}/api/v1/stormfs/pins"), json!({ "volume": vol })).await;
    let snapshot = pin["snapshot"].as_str().unwrap().to_string();

    let resp = c
        .delete(format!("{base}/api/v1/volumes/{snapshot}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409, "a pinned snapshot must not be deletable");
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("pin"),
        "the refusal must name the pin: {body}"
    );

    // Released, it can go.
    let pin_id = pin["pin"].as_str().unwrap();
    c.delete(format!("{base}/api/v1/stormfs/pins/{pin_id}"))
        .send()
        .await
        .unwrap();
    let (_, vols) = get(&c, format!("{base}/api/v1/volumes")).await;
    let ids: Vec<&str> = vols["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&snapshot.as_str()), "release must remove the snapshot");
}

/// Releasing a pin twice is a success, for the same reason freeing an extent
/// twice is: the reader may have crashed mid-release.
#[tokio::test]
async fn releasing_a_pin_twice_is_a_success() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let (_, pin) = post(&c, format!("{base}/api/v1/stormfs/pins"), json!({ "volume": vol })).await;
    let pin_id = pin["pin"].as_str().unwrap();

    for _ in 0..2 {
        let resp = c
            .delete(format!("{base}/api/v1/stormfs/pins/{pin_id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }
}

/// A pinned reader keeps the bytes it started with: the commit that
/// supersedes an extent decrements its reference rather than freeing it, so
/// the snapshot still has somewhere to point.
#[tokio::test]
async fn a_commit_under_a_pin_does_not_take_the_pinned_extents_away() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let live = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    let (_, pin) = post(&c, format!("{base}/api/v1/stormfs/pins"), json!({ "volume": vol })).await;
    let snapshot = pin["snapshot"].as_str().unwrap().to_string();

    let staged = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    let (s, out) = post(
        &c,
        format!("{base}/api/v1/stormfs/commit"),
        json!({ "volume": vol, "offset": live, "len": SLOT * 2, "expected_version": 0,
                "staged": { "offset": staged } }),
    )
    .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["extents_released"], 2);
    assert!(
        out["failures"].as_array().unwrap().is_empty(),
        "releasing a shared extent must not fail: {out}"
    );

    // The snapshot still maps the extents the commit displaced.
    let (s, snap_map) = get(&c, format!("{base}/api/v1/stormfs/extent-map/{snapshot}")).await;
    assert_eq!(s, 200, "{snap_map}");
    assert_eq!(
        snap_map["allocated_bytes"], SLOT * 2,
        "the pinned image must still hold its extents: {snap_map}"
    );
}

/// Chunk ownership has to outlive a restart, or the next allocate hands out
/// addresses StormFS is still using.
#[tokio::test]
async fn chunk_ownership_survives_a_restart() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(data_dir.to_string_lossy().to_string());
    let state = build_state(&dir, config).await;

    let (base, _srv) = start_server(state).await;
    let c = reqwest::Client::new();
    let vol = make_volume(&c, &base, "fs-data", 8 * 1024 * 1024).await;

    let first = allocate(
        &c,
        &base,
        json!({ "tier": "hot", "len": SLOT * 2, "count": 1, "hint_volume": vol, "zero": false }),
    )
    .await["extents"][0]["offset"]
        .as_u64()
        .unwrap();

    // A fresh node reading the same data dir must know that range is spoken
    // for, even though its volume manager is new.
    let reloaded = stormblock::mgmt::api::stormfs::StormFsState::load(&data_dir);
    assert!(
        reloaded.chunks.is_owned(stormblock::volume::VolumeId(vol.parse().unwrap()), first),
        "the chunk map must be on disk before the allocate returns"
    );
    assert_eq!(
        reloaded
            .chunks
            .owned_bytes(stormblock::volume::VolumeId(vol.parse().unwrap())),
        SLOT * 2
    );
}
