//! Live migration of a disk (#83): NVMe ANA, and the epoch enforced at the
//! target (#6's contract, settled on stormstorage#33).
//!
//! Through the engine's own HTTP surface and over real NVMe/TCP: a volume's
//! ANA state is what Identify and the ANA log page report, a change reaches a
//! connected host as a notice, and I/O on a path that does not serve is
//! refused with the path status a multipath host fails over on. And a fence
//! takes a fenced head's leg away before it answers: its next write fails,
//! stale attaches are refused, and the new head's attach at the new epoch
//! works — across a restart too.

use crate::common;
use crate::common::nvmeof_initiator::NvmeofInitiator;
use std::net::SocketAddr;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::{NvmeofExportConfig, StormBlockConfig};
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const SHARED: &str = "nqn.2026-10.test:shared";
const H1: &str = "nqn.2014-08.org.nvmexpress:uuid:11111111-1111-1111-1111-111111111111";
const H2: &str = "nqn.2014-08.org.nvmexpress:uuid:22222222-2222-2222-2222-222222222222";

struct Node {
    state: Arc<AppState>,
    api: String,
    nvme: SocketAddr,
    _server: tokio::task::JoinHandle<()>,
}

async fn node(dir: &TempDir) -> Node {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.management.node_name = Some("sno".into());
    config.nvmeof = Some(NvmeofExportConfig {
        listen_addr: "127.0.0.1:0".into(),
        nqn: SHARED.into(),
        export_drives: false,
        allow_any_host: false,
        allowed_hosts: Vec::new(),
        require_dhchap: false,
        boothost_host_nqn: None,
    });
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let dev = FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 128 * MIB)
        .await
        .unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nvme = listener.local_addr().unwrap();
    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: nvme,
        nqn: SHARED.into(),
        advertised_addr: Some(nvme),
        ..Default::default()
    }));
    *state.nvmeof_target.write().await = Some(target.clone());
    stormblock::mgmt::nvme_hosts::apply_shared_policy(&state, &target);
    stormblock::mgmt::nvme_hosts::restore(&state).await;
    stormblock::mgmt::api::v1::restore_nvme_nsids(&state).await;
    tokio::spawn(async move {
        let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
        let _ = target.run_with_listener(listener, &reactor).await;
    });
    common::wait_for_listener(nvme).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(http).await;
    Node { state, api: format!("http://{http}"), nvme, _server: server }
}

async fn call(method: reqwest::Method, url: String, body: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new().request(method, url).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null))
}

fn spec(addr: SocketAddr, nqn: &str, nsid: u32, host: &str) -> NvmeTcpSpec {
    NvmeTcpSpec { addr: addr.to_string(), nqn: nqn.into(), nsid, host_nqn: Some(host.into()), dhchap: None }
}

async fn admin_as(addr: SocketAddr, subnqn: &str, host: &str) -> NvmeofInitiator {
    let mut i = NvmeofInitiator::connect(addr).await.unwrap();
    i.ic_handshake().await.unwrap();
    let (_, status) = i.fabric_connect_raw(subnqn, host, 0).await.unwrap();
    assert_eq!(status, 0, "connect {subnqn} as {host}");
    i
}

/// `(group id, state, nsids)` of every descriptor in an ANA log page.
fn ana_groups(page: &[u8]) -> Vec<(u32, u8, Vec<u32>)> {
    let n = u16::from_le_bytes([page[8], page[9]]) as usize;
    let mut out = Vec::new();
    let mut off = 16;
    for _ in 0..n {
        let d = &page[off..off + 32];
        let grpid = u32::from_le_bytes(d[0..4].try_into().unwrap());
        let nnsids = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
        let nsids = (0..nnsids)
            .map(|k| u32::from_le_bytes(page[off + 32 + k * 4..off + 36 + k * 4].try_into().unwrap()))
            .collect();
        out.push((grpid, d[16], nsids));
        off += 32 + nnsids * 4;
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volumes_ana_state_is_reported_announced_and_enforced() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir).await;
    let vol = n.state.volume_manager.lock().await.create_volume_any("vm-disk", 16 * MIB).await.unwrap().0;
    let (s, a) = call(
        reqwest::Method::POST,
        format!("{}/api/v1/volumes/{vol}/attach", n.api),
        serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1}),
    )
    .await;
    assert_eq!(s, 200, "{a}");
    let nqn = a["nqn"].as_str().unwrap().to_string();
    let nsid = a["nsid"].as_u64().unwrap() as u32;

    // Identify says ANA, as Linux's multipath reads it.
    let mut admin = admin_as(n.nvme, &nqn, H1).await;
    let ctrl = admin.identify_controller().await.unwrap();
    assert_eq!(ctrl[76] & 0b1010, 0b1010, "CMIC: several controllers, ANA reporting");
    assert_ne!(u32::from_le_bytes(ctrl[92..96].try_into().unwrap()) & (1 << 11), 0, "OAES: ANA change");
    let ns = admin.identify_namespace(nsid).await.unwrap();
    assert_eq!(ns[30] & 1, 1, "NMIC: shared");
    assert_eq!(u32::from_le_bytes(ns[92..96].try_into().unwrap()), 1, "group 1: optimized");
    let groups = ana_groups(&admin.get_log_page(0x0C, 4096).await.unwrap());
    assert_eq!(groups.len(), 4, "no empty change group");
    assert_eq!(groups[0], (1, 0x01, vec![nsid]));
    assert!(groups.iter().all(|g| g.1 != 0), "every descriptor has a state");

    let dev = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1)).await.unwrap();
    dev.write(0, &vec![0x11u8; 4096]).await.unwrap();

    // The host waits on an AER; the node is told the volume moved away.
    admin.post_async_event().await.unwrap();
    let (s, v) = call(
        reqwest::Method::PUT,
        format!("{}/api/v1/volumes/{vol}/ana", n.api),
        serde_json::json!({"state": "inaccessible"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "inaccessible");
    assert_eq!(v["served"][0]["nqn"], nqn.as_str());
    let dw0 = tokio::time::timeout(std::time::Duration::from_secs(5), admin.next_completion_dw0())
        .await
        .expect("an ANA change notice")
        .unwrap();
    assert_eq!(dw0, 0x2 | (0x03 << 8) | (0x0C << 16), "notice: ANA change, log page 0x0C");
    let groups = ana_groups(&admin.get_log_page(0x0C, 4096).await.unwrap());
    assert_eq!(groups[2], (3, 0x03, vec![nsid]), "now in the inaccessible group");
    assert!(groups[0].2.is_empty());

    // I/O on this path is refused with the path status (SCT 3, SC 2), which
    // a multipath host fails over on rather than surfacing.
    let mut io = NvmeofInitiator::connect(n.nvme).await.unwrap();
    io.ic_handshake().await.unwrap();
    io.fabric_connect(&nqn, H1, 1).await.unwrap();
    let e = io.write(nsid, 0, &vec![0x22u8; 4096]).await.unwrap_err().to_string();
    assert!(e.contains("0x604"), "path status inaccessible: {e}");
    assert!(dev.write(0, &vec![0x22u8; 4096]).await.is_err());

    // Kept across a restart: the node that was left stays left.
    let kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("ana.json")).unwrap()).unwrap();
    assert_eq!(kept[vol.to_string()], "inaccessible");
    stormblock::target::nvmeof::ana::load(Default::default());
    stormblock::mgmt::ana::load(&n.state.config);
    let (_, v) = call(reqwest::Method::GET, format!("{}/api/v1/volumes/{vol}/ana", n.api), serde_json::json!({})).await;
    assert_eq!(v["state"], "inaccessible", "reloaded from ana.json");

    // Back to optimized: I/O flows again, and nothing was written meanwhile.
    let (s, _) = call(
        reqwest::Method::PUT,
        format!("{}/api/v1/volumes/{vol}/ana", n.api),
        serde_json::json!({"state": "optimized"}),
    )
    .await;
    assert_eq!(s, 200);
    let back = io.read(nsid, 0, 1).await.unwrap();
    assert_eq!(back, vec![0x11u8; 4096]);
    let (s, _) = call(
        reqwest::Method::PUT,
        format!("{}/api/v1/volumes/{vol}/ana", n.api),
        serde_json::json!({"state": "sideways"}),
    )
    .await;
    assert_eq!(s, 400);
}

async fn v1_attach(n: &Node, id: &str, host: &str, epoch: Option<u64>) -> (u16, serde_json::Value) {
    let mut body = serde_json::json!({"node": "sno", "mode": "read_write", "transport": "nvme_tcp", "host_nqn": host});
    if let Some(e) = epoch {
        body["epoch"] = e.into();
    }
    call(reqwest::Method::POST, format!("{}/v1/volumes/{id}/attach", n.api), body).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fence_takes_the_fenced_heads_leg_away_before_it_answers() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir).await;
    let (s, v) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes", n.api),
        serde_json::json!({"name": "leg-0", "size_bytes": 16 * MIB, "replica_tier": {"slaves": 0}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["epoch"], 1);

    // Head 1 attaches its leg (no epoch at epoch 1: accepted, as before).
    let (s, a) = v1_attach(&n, &id, H1, None).await;
    assert_eq!(s, 200, "{a}");
    let (nqn1, nsid1) = (a["nqn"].as_str().unwrap().to_string(), a["nsid"].as_u64().unwrap() as u32);
    let head1 = NvmeofDevice::connect(&spec(n.nvme, &nqn1, nsid1, H1)).await.unwrap();
    head1.write(0, &vec![0x01u8; 4096]).await.unwrap();
    let (_, v) = call(reqwest::Method::GET, format!("{}/v1/volumes/{id}", n.api), serde_json::json!({})).await;
    assert_eq!(v["attachments"][0]["host_nqn"], H1);
    assert_eq!(v["attachments"][0]["epoch"], 1);
    assert_eq!(v["attachments"][0]["transport"], "nvme_tcp");

    // A stale fence is refused and changes nothing.
    let (s, e) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes/{id}/fence", n.api),
        serde_json::json!({"expected_epoch": 7}),
    )
    .await;
    assert_eq!((s, e["code"].as_str()), (412, Some("stale_epoch")));
    head1.write(4096, &vec![0x01u8; 4096]).await.unwrap();

    // The fence: by the time it answers, head 1 cannot write.
    let (s, f) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes/{id}/fence", n.api),
        serde_json::json!({"expected_epoch": 1}),
    )
    .await;
    assert_eq!(s, 200, "{f}");
    assert_eq!((f["epoch"].as_u64(), f["revoked"].as_u64()), (Some(2), Some(1)));
    assert!(head1.write(0, &vec![0xEEu8; 4096]).await.is_err(), "the fenced head's write is refused");
    let mut zombie = NvmeofInitiator::connect(n.nvme).await.unwrap();
    zombie.ic_handshake().await.unwrap();
    assert!(
        zombie.fabric_connect_raw(&nqn1, H1, 1).await.map(|(_, st)| st != 0).unwrap_or(true),
        "and it cannot connect back"
    );

    // Stale attaches: at the old epoch, and with none at all once fenced.
    for asked in [Some(1), None] {
        let (s, e) = v1_attach(&n, &id, H1, asked).await;
        assert_eq!(s, 412, "{asked:?}: {e}");
        assert_eq!(e["code"], "stale_epoch");
        assert_eq!(e["current_epoch"], 2);
    }

    // Head 2 attaches at the new epoch and finds head 1's acknowledged
    // writes and not its refused one.
    let (s, a) = v1_attach(&n, &id, H2, Some(2)).await;
    assert_eq!(s, 200, "{a}");
    let (nqn2, nsid2) = (a["nqn"].as_str().unwrap().to_string(), a["nsid"].as_u64().unwrap() as u32);
    assert_ne!(nqn2, nqn1);
    let head2 = NvmeofDevice::connect(&spec(n.nvme, &nqn2, nsid2, H2)).await.unwrap();
    let mut back = vec![0u8; 8192];
    head2.read(0, &mut back).await.unwrap();
    assert_eq!(back, vec![0x01u8; 8192]);
    head2.write(0, &vec![0x02u8; 4096]).await.unwrap();
    let (_, v) = call(reqwest::Method::GET, format!("{}/v1/volumes/{id}", n.api), serde_json::json!({})).await;
    let atts = v["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1, "{v}");
    assert_eq!((atts[0]["host_nqn"].as_str(), atts[0]["epoch"].as_u64()), (Some(H2), Some(2)));

    // Persisted: a restarted engine keeps the epoch, the attachment's epoch,
    // and still refuses the zombie.
    drop(n);
    let n = node(&dir).await;
    let (_, v) = call(reqwest::Method::GET, format!("{}/v1/volumes/{id}", n.api), serde_json::json!({})).await;
    assert_eq!(v["epoch"], 2, "{v}");
    assert_eq!(v["attachments"][0]["epoch"], 2);
    let (s, _) = v1_attach(&n, &id, H1, Some(1)).await;
    assert_eq!(s, 412);
}

/// #195: a promote and an expired dual-attach window drop attachment
/// records; the data path behind each goes with them, and the window expires
/// on time, with no call to notice it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promote_and_an_expired_window_take_the_data_path_with_the_record() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir).await;
    let mk = |name: &'static str| {
        let api = n.api.clone();
        async move {
            let (s, v) = call(
                reqwest::Method::POST,
                format!("{api}/v1/volumes"),
                serde_json::json!({"name": name, "size_bytes": 16 * MIB, "replica_tier": {"slaves": 0}}),
            )
            .await;
            assert_eq!(s, 200, "{v}");
            let id = v["id"].as_str().unwrap().to_string();
            // A slave on another node, so it can be promoted and migrated to.
            let (s, v) = call(
                reqwest::Method::POST,
                format!("{api}/v1/volumes/{id}/placement"),
                serde_json::json!({"master_node": "sno", "slave_node": "peer"}),
            )
            .await;
            assert_eq!(s, 200, "{v}");
            id
        }
    };

    // Promote: the attachment at the current epoch is dropped, and its host
    // can no longer write or connect.
    let id = mk("promoted").await;
    let (s, a) = v1_attach(&n, &id, H1, None).await;
    assert_eq!(s, 200, "{a}");
    let (nqn, nsid) = (a["nqn"].as_str().unwrap().to_string(), a["nsid"].as_u64().unwrap() as u32);
    let host = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1)).await.unwrap();
    host.write(0, &vec![1u8; 4096]).await.unwrap();
    let (s, f) = call(reqwest::Method::POST, format!("{}/v1/volumes/{id}/fence", n.api), serde_json::json!({"expected_epoch": 1})).await;
    assert_eq!(s, 200, "{f}");
    // Reattach at the new epoch (the fence took the first one away).
    let (s, a) = v1_attach(&n, &id, H1, Some(2)).await;
    assert_eq!(s, 200, "{a}");
    let (nqn, nsid) = (a["nqn"].as_str().unwrap().to_string(), a["nsid"].as_u64().unwrap() as u32);
    let host = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1)).await.unwrap();
    host.write(0, &vec![2u8; 4096]).await.unwrap();
    let (s, p) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes/{id}/promote", n.api),
        serde_json::json!({"target_node": "peer", "fenced_epoch": 2}),
    )
    .await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["attachments"].as_array().map_or(0, |a| a.len()), 0, "{p}");
    assert!(host.write(0, &vec![3u8; 4096]).await.is_err(), "the promote took the host's path away");
    assert!(NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1)).await.is_err(), "and it cannot connect back");

    // An expired window: the migration target's attachment goes on time.
    let id = mk("migrating").await;
    let (s, w) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes/{id}/dual-attach", n.api),
        serde_json::json!({"target_node": "peer", "ttl_secs": 1}),
    )
    .await;
    assert_eq!(s, 200, "{w}");
    let (s, a) = call(
        reqwest::Method::POST,
        format!("{}/v1/volumes/{id}/attach", n.api),
        serde_json::json!({"node": "peer", "mode": "migration_target", "transport": "nvme_tcp", "host_nqn": H2}),
    )
    .await;
    assert_eq!(s, 200, "{a}");
    let (nqn, nsid) = (a["nqn"].as_str().unwrap().to_string(), a["nsid"].as_u64().unwrap() as u32);
    let target = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H2)).await.unwrap();
    target.write(0, &vec![4u8; 4096]).await.unwrap();
    // No call to the API: the timer has to do it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while target.write(0, &vec![5u8; 4096]).await.is_ok() {
        assert!(std::time::Instant::now() < deadline, "the expired window still lets the target write");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let (_, v) = call(reqwest::Method::GET, format!("{}/v1/volumes/{id}", n.api), serde_json::json!({})).await;
    assert!(
        v["attachments"].as_array().is_none_or(|a| a.iter().all(|a| a["node"] != "peer")),
        "the record went too: {v}"
    );
    assert!(n.state.v1.lock().await.dual_attach.get(&id).is_none());
}
