//! Wire-contract drift guard for `/v1` (#34, mirror of stormblock-csi#8).
//!
//! `contract/` holds the same golden JSON the CSI driver pins, copied from
//! `glennswest/stormblock-csi`. Both sides assert their own serde types
//! round-trip these files, so a field rename, a tag change or a dropped
//! `skip_serializing_if` fails a build here rather than a cluster later — and
//! one change has to update both pins to land.
//!
//! Round-trip means: parse the fixture, deserialize it into the engine's type,
//! serialize it back, and compare as JSON values. Key order does not matter;
//! every key, tag and value does.

use std::path::PathBuf;

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use stormblock::mgmt::api::v1::{
    AttachInfo, CreateVolumeRequest, DualAttachWindow, GroupSnapshot, NodeCapacity, Snapshot,
    SyncState, V1Error, Volume,
};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contract").join(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name} is not JSON: {e}"))
}

/// Deserialize the fixture into `T`, serialize it back, and require the result
/// to be the same JSON.
fn round_trip<T: Serialize + DeserializeOwned>(name: &str) -> T {
    let golden = fixture(name);
    let typed: T = serde_json::from_value(golden.clone())
        .unwrap_or_else(|e| panic!("{name} does not deserialize into {}: {e}", std::any::type_name::<T>()));
    let back = serde_json::to_value(&typed)
        .unwrap_or_else(|e| panic!("{name} does not serialize back: {e}"));
    assert_eq!(back, golden, "{name} changed shape on the way through");
    typed
}

#[test]
fn volume_round_trips() {
    let v: Volume = round_trip("volume.json");
    // Spot-check the parts a rename would quietly break rather than fail on.
    assert_eq!(v.id, "vol-0000002a");
    assert_eq!(v.epoch, 3);
    assert_eq!(v.master_node(), Some("w1"));
    assert_eq!(v.replicas.len(), 2);
    assert!(matches!(v.replicas[0].sync, SyncState::InSync));
    match &v.replicas[1].sync {
        SyncState::Resyncing { progress_pct, lag_bytes } => {
            assert_eq!(*progress_pct, 62.5);
            assert_eq!(*lag_bytes, 1 << 20);
        }
        other => panic!("expected resyncing, got {other:?}"),
    }
    assert_eq!(v.qos_class.as_deref(), Some("gold"));
    assert!(v.encrypted);
}

#[test]
fn sync_state_detached_round_trips() {
    let s: SyncState = round_trip("sync-state-detached.json");
    assert!(matches!(s, SyncState::Detached));
}

/// The nvme_tcp shape carries the shared subsystem NQN plus the nsid the volume
/// was hot-added as — a node connects once and later attaches pick a namespace
/// out of the controller it already has, so losing `nsid` on the wire would
/// silently point every volume at the same namespace.
#[test]
fn attach_info_nvme_tcp_round_trips() {
    let a: AttachInfo = round_trip("attach-info-nvme-tcp.json");
    match a {
        AttachInfo::NvmeTcp { nqn, addresses, nsid } => {
            assert_eq!(nqn, "nqn.2024.io.stormblock:default");
            assert_eq!(addresses.len(), 1);
            assert_eq!(addresses[0].traddr, "192.168.200.21");
            assert_eq!(addresses[0].trsvcid, 4420);
            assert_eq!(nsid, Some(7));
        }
        other => panic!("expected nvme_tcp, got {other:?}"),
    }
}

#[test]
fn attach_info_ublk_round_trips() {
    let a: AttachInfo = round_trip("attach-info-ublk.json");
    match a {
        AttachInfo::Ublk { device_hint } => assert_eq!(device_hint, "ublkb3"),
        other => panic!("expected ublk, got {other:?}"),
    }
}

#[test]
fn volume_sources_round_trip() {
    use stormblock::mgmt::api::v1::VolumeSource;
    let snap: VolumeSource = round_trip("volume-source-snapshot.json");
    assert_eq!(snap, VolumeSource::Snapshot("snap-00000001".to_string()));
    let vol: VolumeSource = round_trip("volume-source-volume.json");
    assert_eq!(vol, VolumeSource::Volume("vol-0000002a".to_string()));
}

#[test]
fn dual_attach_window_round_trips() {
    let w: DualAttachWindow = round_trip("dual-attach-window.json");
    assert_eq!(w.volume_id, "vol-0000002a");
    assert_eq!(w.epoch, 3);
    assert_eq!(w.target_node, "w2");
    assert_eq!(w.expires_at_ms, 1754700000000);
}

/// Every field set, so a field the engine stopped reading shows up as a
/// round-trip difference rather than as a request that quietly does less.
#[test]
fn create_volume_request_round_trips() {
    let r: CreateVolumeRequest = round_trip("create-volume-request.json");
    assert_eq!(r.name, "pvc-web-0");
    assert_eq!(r.size_bytes, 10_737_418_240);
    assert_eq!(r.master_node.as_deref(), Some("w1"));
    assert_eq!(r.excluded_nodes, vec!["w3".to_string()]);
    assert_eq!(r.replica_tier.slaves, 1);
    assert_eq!(r.qos_class.as_deref(), Some("gold"));
    assert!(!r.encrypted);
    assert!(r.source.is_some());
}

#[test]
fn snapshot_and_group_snapshot_round_trip() {
    let s: Snapshot = round_trip("snapshot.json");
    assert_eq!(s.id, "snap-00000001");
    assert_eq!(s.source_volume_id, "vol-0000002a");
    assert!(s.ready);
    assert_eq!(s.group_snapshot_id.as_deref(), Some("gsnap-00000001"));

    let g: GroupSnapshot = round_trip("group-snapshot.json");
    assert_eq!(g.id, "gsnap-00000001");
    assert_eq!(g.snapshots.len(), 1);
    assert_eq!(g.snapshots[0].group_snapshot_id.as_deref(), Some(g.id.as_str()));
}

#[test]
fn node_capacity_round_trips() {
    let n: NodeCapacity = round_trip("node-capacity.json");
    assert_eq!(n.node, "w1");
    assert_eq!(n.total_bytes, 1_099_511_627_776);
    assert_eq!(n.topology.get("topology.stormblock.io/rack").map(String::as_str), Some("r1"));
}

/// The error envelope is produced rather than parsed, so the fixture is checked
/// against what an error actually serializes to: the same keys, the same types,
/// and — for the fencing shape — `current_epoch` alongside the code the client
/// retries on. The `message` is prose and only its presence is contractual.
#[tokio::test]
async fn error_envelopes_match_the_fixtures() {
    use axum::response::IntoResponse;

    async fn body_of(e: V1Error) -> (u16, Value) {
        let resp = e.into_response();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    let (status, produced) =
        body_of(V1Error::AlreadyExists("volume pvc-web-0 exists with size 10737418240".into()))
            .await;
    let golden = fixture("error-envelope-already-exists.json");
    assert_eq!(status, 409);
    assert_eq!(keys(&produced), keys(&golden));
    assert_eq!(produced["code"], golden["code"]);
    assert!(produced["message"].is_string());

    let (status, produced) = body_of(V1Error::StaleEpoch(4)).await;
    let golden = fixture("error-envelope-stale-epoch.json");
    assert_eq!(status, 412);
    assert_eq!(keys(&produced), keys(&golden), "the fencing shape carries current_epoch");
    assert_eq!(produced["code"], golden["code"]);
    assert_eq!(produced["current_epoch"], golden["current_epoch"]);
    assert!(produced["message"].is_string());
}

fn keys(v: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = v.as_object().expect("an object").keys().map(String::as_str).collect();
    k.sort_unstable();
    k
}
