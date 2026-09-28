//! Per-host NVMe/TCP subsystems, allowed hosts and DH-HMAC-CHAP (#210).
//!
//! pve connected to forge's :4420 with no arrangement and saw 71 namespaces.
//! These tests are the issue's "done when": a host that was given nothing
//! sees nothing, a host sees only what was attached to it, a golden is only
//! ever read-only and only to a named host, and a host that must prove a
//! secret cannot connect without it. Everything goes through the engine's
//! own HTTP surface and over real NVMe/TCP — the in-house initiator for data,
//! the test initiator for Connect statuses, discovery and identify.

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
use stormblock::target::nvmeof::auth::DhchapKey;
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::{VolumeId, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const SHARED: &str = "nqn.2026-09.test:shared";
const H1: &str = "nqn.2014-08.org.nvmexpress:uuid:11111111-1111-1111-1111-111111111111";
const H2: &str = "nqn.2014-08.org.nvmexpress:uuid:22222222-2222-2222-2222-222222222222";
/// Connect Invalid Host: SCT 1, SC 0x84.
const INVALID_HOST: u16 = 0x184;
const DNR: u16 = 0x4000;

struct Node {
    state: Arc<AppState>,
    api: String,
    nvme: SocketAddr,
    clone_a: Uuid,
    clone_b: Uuid,
    golden: Uuid,
    _server: tokio::task::JoinHandle<()>,
}

async fn start_target(state: &Arc<AppState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: addr,
        nqn: SHARED.into(),
        advertised_addr: Some(addr),
        ..Default::default()
    }));
    *state.nvmeof_target.write().await = Some(target.clone());
    // What the daemon does at startup, in its order.
    stormblock::mgmt::nvme_hosts::apply_shared_policy(state, &target);
    stormblock::mgmt::nvme_hosts::restore(state).await;
    stormblock::mgmt::api::exports::restore_exports(state).await;
    stormblock::mgmt::api::v1::restore_nvme_nsids(state).await;
    tokio::spawn(async move {
        let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
        let _ = target.run_with_listener(listener, &reactor).await;
    });
    common::wait_for_listener(addr).await;
    addr
}

async fn node(dir: &TempDir, allow_any_host: bool) -> Node {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.nvmeof = Some(NvmeofExportConfig {
        listen_addr: "127.0.0.1:0".into(),
        nqn: SHARED.into(),
        export_drives: false,
        allow_any_host,
        allowed_hosts: Vec::new(),
        require_dhchap: false,
        boothost_host_nqn: None,
    });
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let dev = FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 256 * MIB)
        .await
        .unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let clone_a = vm.create_volume("clone-a", 16 * MIB, array).await.unwrap().0;
    let clone_b = vm.create_volume("clone-b", 16 * MIB, array).await.unwrap().0;
    let golden = vm.create_volume("golden", 16 * MIB, array).await.unwrap().0;
    vm.seal_volume(VolumeId(golden), None).await.unwrap();
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let mut st = AppState::new(config, vm, reg, gem);
    // Every boot claim releases the last one at once (no #97 grace).
    st.claim_grace = std::time::Duration::ZERO;
    let state = Arc::new(st);
    let nvme = start_target(&state).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(http).await;
    Node { state, api: format!("http://{http}"), nvme, clone_a, clone_b, golden, _server: server }
}

async fn attach(n: &Node, vol: Uuid, body: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/volumes/{vol}/attach", n.api))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v = resp.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// A controller as `host` on `subnqn`: the Connect's (dw0, status).
async fn connect_as(addr: SocketAddr, subnqn: &str, host: &str) -> (u32, u16, NvmeofInitiator) {
    let mut i = NvmeofInitiator::connect(addr).await.unwrap();
    i.ic_handshake().await.unwrap();
    let (dw0, status) = i.fabric_connect_raw(subnqn, host, 0).await.unwrap();
    (dw0, status, i)
}

/// The subsystem NQNs the discovery log page shows `host`.
async fn discover(addr: SocketAddr, host: &str) -> Vec<String> {
    let (_, status, mut i) = connect_as(addr, "nqn.2014-08.org.nvmexpress.discovery", host).await;
    assert_eq!(status, 0, "discovery is open to every host; what it lists is not");
    let page = i.get_log_page(0x70, 4096).await.unwrap();
    let numrec = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
    (0..numrec)
        .map(|k| {
            let e = &page[1024 + k * 1024..1024 + (k + 1) * 1024];
            let nqn = &e[256..512];
            String::from_utf8_lossy(&nqn[..nqn.iter().position(|b| *b == 0).unwrap_or(256)]).to_string()
        })
        .collect()
}

fn spec(addr: SocketAddr, nqn: &str, nsid: u32, host: &str, key: Option<DhchapKey>) -> NvmeTcpSpec {
    NvmeTcpSpec { addr: addr.to_string(), nqn: nqn.into(), nsid, host_nqn: Some(host.into()), dhchap: key }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_given_nothing_sees_nothing() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    // Something is served — to H1.
    let (s, _) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(s, 200);

    // A stranger's discovery lists nothing, and the shared subsystem refuses it.
    assert!(discover(n.nvme, H2).await.is_empty());
    let (_, status, _) = connect_as(n.nvme, SHARED, H2).await;
    assert_eq!(status & !DNR, INVALID_HOST, "status {status:#x}");
    assert_ne!(status & DNR, 0, "a refusal is not worth retrying");
    // So does a subsystem it merely names.
    let (_, status, _) = connect_as(n.nvme, "nqn.2026-09.test:nothing", H2).await;
    assert_ne!(status, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_sees_only_what_was_attached_to_it() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    let (s, a) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(s, 200, "{a}");
    let nqn = a["nqn"].as_str().unwrap().to_string();
    assert!(nqn.starts_with(&format!("{SHARED}:host:")), "{nqn}");
    assert_ne!(nqn, SHARED);
    assert_eq!(a["host_nqn"], H1);
    let nsid_a = a["nsid"].as_u64().unwrap() as u32;
    // Again: the same address (idempotent).
    let (_, again) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!((again["nqn"].as_str().unwrap(), again["nsid"].as_u64().unwrap() as u32), (nqn.as_str(), nsid_a));
    // A second volume for the same host lands beside it.
    let (_, b) = attach(&n, n.clone_b, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(b["nqn"].as_str().unwrap(), nqn);
    assert_ne!(b["nsid"].as_u64().unwrap() as u32, nsid_a);

    // H1 finds exactly its own subsystem, and exactly those two namespaces.
    assert_eq!(discover(n.nvme, H1).await, vec![nqn.clone()]);
    let (_, status, mut i) = connect_as(n.nvme, &nqn, H1).await;
    assert_eq!(status, 0);
    let mut ns = i.active_namespaces().await.unwrap();
    ns.sort_unstable();
    assert_eq!(ns.len(), 2, "{ns:?}");

    // And reads back what it writes, through the engine's own initiator.
    let dev = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid_a, H1, None)).await.unwrap();
    let block = vec![0xA5u8; 4096];
    dev.write(0, &block).await.unwrap();
    dev.flush().await.unwrap();
    let mut back = vec![0u8; 4096];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, block);

    // H2 can neither reach H1's subsystem nor see it.
    let (_, status, _) = connect_as(n.nvme, &nqn, H2).await;
    assert_eq!(status & !DNR, INVALID_HOST);
    assert!(NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid_a, H2, None)).await.is_err());
    assert!(discover(n.nvme, H2).await.is_empty());

    // Detached from H1, the subsystem goes with its last namespace.
    let c = reqwest::Client::new();
    for v in [n.clone_a, n.clone_b] {
        let r = c.delete(format!("{}/api/v1/volumes/{v}/attach?host_nqn={H1}", n.api)).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    assert!(discover(n.nvme, H1).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_that_names_no_host_is_refused_on_a_closed_node() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    let (s, v) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp"})).await;
    assert_eq!(s, 400, "{v}");
    assert!(v.to_string().contains("host_nqn"), "the refusal says what to send: {v}");
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/exports", n.api))
        .json(&serde_json::json!({"volume_id": n.clone_a, "protocol": "nvmeof"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // Not a host NQN at all.
    let (s, _) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": "pve"})).await;
    assert_eq!(s, 400);
    // Nothing got onto the shared subsystem on the way.
    let t = n.state.nvmeof_target.read().await.clone().unwrap();
    assert_eq!(t.namespace_count().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_golden_is_never_shared_and_only_ever_read_only() {
    let dir = TempDir::new().unwrap();
    // Even on a node that opens its shared subsystem to every host.
    let n = node(&dir, true).await;
    let (s, v) = attach(&n, n.golden, serde_json::json!({"transport": "nvme-tcp", "mode": "ro"})).await;
    assert_eq!(s, 400, "{v}");
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/exports", n.api))
        .json(&serde_json::json!({"volume_id": n.golden, "protocol": "nvmeof"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // To a named host, read-only: the helper that reads goldens.
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/exports", n.api))
        .json(&serde_json::json!({"volume_id": n.golden, "protocol": "nvmeof", "host_nqn": H1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let e: serde_json::Value = r.json().await.unwrap();
    let nqn = e["nqn"].as_str().unwrap().to_string();
    let nsid = e["nsid"].as_u64().unwrap() as u32;
    assert_eq!(e["port"].as_u64().unwrap() as u16, n.nvme.port());

    let (_, status, mut i) = connect_as(n.nvme, &nqn, H1).await;
    assert_eq!(status, 0);
    let id = i.identify_namespace(nsid).await.unwrap();
    assert_ne!(id[99] & 1, 0, "NSATTR says write-protected, so the host's block device is read-only");
    let dev = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1, None)).await.unwrap();
    let mut buf = vec![0u8; 4096];
    dev.read(0, &mut buf).await.unwrap();
    assert!(dev.write(0, &vec![1u8; 4096]).await.is_err(), "a golden takes no write over the wire");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_with_a_secret_must_prove_it() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    let (s, a) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1, "dhchap": true})).await;
    assert_eq!(s, 200, "{a}");
    let secret = a["dhchap_secret"].as_str().expect("the attach hands the host its secret").to_string();
    assert!(secret.starts_with("DHHC-1:"));
    let nqn = a["nqn"].as_str().unwrap().to_string();
    let nsid = a["nsid"].as_u64().unwrap() as u32;
    let key = DhchapKey::parse(&secret).unwrap();

    // The Connect asks for authentication, and nothing else runs first.
    let (dw0, status, mut i) = connect_as(n.nvme, &nqn, H1).await;
    assert_eq!(status, 0);
    assert_ne!(dw0 & stormblock::target::nvmeof::CONNECT_AUTHREQ_ATR, 0);
    assert!(i.identify_controller().await.map(|d| d.is_empty()).unwrap_or(true), "no identify before auth");

    // With the secret: in, and data moves.
    let dev = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1, Some(key.clone()))).await.unwrap();
    dev.write(0, &vec![7u8; 4096]).await.unwrap();
    let mut back = vec![0u8; 4096];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, vec![7u8; 4096]);

    // Without one, or with another: out. Saying H1's NQN is not enough.
    let e = NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1, None)).await.err().expect("no secret");
    assert!(e.to_string().contains("DH-HMAC-CHAP"), "{e}");
    assert!(NvmeofDevice::connect(&spec(n.nvme, &nqn, nsid, H1, Some(DhchapKey::generate()))).await.is_err());

    // A later attach without `dhchap` does not take the secret away.
    let (_, b) = attach(&n, n.clone_b, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(b["dhchap_secret"].as_str(), Some(secret.as_str()));
    assert!(NvmeofDevice::connect(&spec(n.nvme, &nqn, b["nsid"].as_u64().unwrap() as u32, H1, None)).await.is_err());
}

/// An NQN and an NSID are an address a machine has written down: a restart
/// puts every host subsystem back, with the same numbers and the same hosts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_every_address_and_every_door() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    let (_, a) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H1})).await;
    let (_, b) = attach(&n, n.clone_b, serde_json::json!({"transport": "nvme-tcp", "host_nqn": H2, "dhchap": true})).await;
    let secret = DhchapKey::parse(b["dhchap_secret"].as_str().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("nvme_hosts.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "it holds secrets: {mode:o}");
    }
    let dev = NvmeofDevice::connect(&spec(n.nvme, a["nqn"].as_str().unwrap(), a["nsid"].as_u64().unwrap() as u32, H1, None))
        .await
        .unwrap();
    dev.write(4096, &vec![9u8; 4096]).await.unwrap();
    dev.flush().await.unwrap();
    drop(dev);

    // A new listener over the same engine state: what the daemon rebuilds.
    n.state.nvmeof_target.write().await.take();
    n.state.nvme_hosts.lock().await.subsystems.clear();
    let addr = start_target(&n.state).await;

    let dev = NvmeofDevice::connect(&spec(addr, a["nqn"].as_str().unwrap(), a["nsid"].as_u64().unwrap() as u32, H1, None))
        .await
        .unwrap();
    let mut back = vec![0u8; 4096];
    dev.read(4096, &mut back).await.unwrap();
    assert_eq!(back, vec![9u8; 4096]);
    let bn = b["nqn"].as_str().unwrap();
    let bs = b["nsid"].as_u64().unwrap() as u32;
    assert!(NvmeofDevice::connect(&spec(addr, bn, bs, H2, None)).await.is_err(), "the secret came back too");
    NvmeofDevice::connect(&spec(addr, bn, bs, H2, Some(secret))).await.unwrap();
    // And H2 still cannot reach H1's.
    let (_, status, _) = connect_as(addr, a["nqn"].as_str().unwrap(), H2).await;
    assert_eq!(status & !DNR, INVALID_HOST);
}

/// One volume is never two namespaces of one subsystem — the kernel's
/// "duplicate IDs in subsystem for nsid 3" — and an attach record survives a
/// restart at its NSID instead of being handed to another volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_volume_is_one_namespace_across_exports_attaches_and_restarts() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, true).await;
    let c = reqwest::Client::new();
    let e: serde_json::Value = c
        .post(format!("{}/api/v1/exports", n.api))
        .json(&serde_json::json!({"volume_id": n.clone_a, "protocol": "nvmeof"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exported = e["nsid"].as_u64().unwrap();
    let (s, a) = attach(&n, n.clone_a, serde_json::json!({"transport": "nvme-tcp"})).await;
    assert_eq!(s, 200, "{a}");
    assert_eq!(a["nsid"].as_u64().unwrap(), exported, "the same volume, the same namespace");
    let (_, b) = attach(&n, n.clone_b, serde_json::json!({"transport": "nvme-tcp"})).await;
    let b_nsid = b["nsid"].as_u64().unwrap();
    let t = n.state.nvmeof_target.read().await.clone().unwrap();
    assert_eq!(t.namespace_count().await, 2);

    // Restart: both come back where they were, and nothing is doubled.
    n.state.nvmeof_target.write().await.take();
    let addr = start_target(&n.state).await;
    let t = n.state.nvmeof_target.read().await.clone().unwrap();
    assert_eq!(t.namespace_count().await, 2);
    assert_eq!(t.default_subsystem().nsid_of(n.clone_b).await, Some(b_nsid as u32));
    assert_eq!(t.default_subsystem().nsid_of(n.clone_a).await, Some(exported as u32));
    let (_, status, _) = connect_as(addr, SHARED, H2).await;
    assert_eq!(status, 0, "this node opened its shared subsystem on purpose");
}

/// The duplicate-ID guard at the target itself.
#[tokio::test]
async fn a_subsystem_never_holds_one_device_twice() {
    let dir = TempDir::new().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(dir.path().join("d.bin").to_str().unwrap(), 4 * MIB).await.unwrap(),
    );
    let t = NvmeofTarget::new(NvmeofConfig::default());
    let sub = t.default_subsystem();
    assert_eq!(sub.add_namespace_next(dev.clone(), false).await, 1);
    assert_eq!(sub.add_namespace_next(dev.clone(), false).await, 1);
    assert!(!sub.add_namespace_at(3, dev.clone(), false).await, "not a second NSID for it");
    assert!(sub.add_namespace_at(1, dev, false).await, "its own NSID again is fine");
    assert_eq!(sub.namespace_count().await, 1);
}

/// A boot claim serves the machine's clone to that machine alone, from its
/// own subsystem, under the host NQN its firmware composes from the name the
/// reply gives (`nqn.2026-09.lo.storm:host-<name>`) — no firmware change —
/// and never the golden it was cloned from. The next boot's clone replaces
/// the last one there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_boot_claim_is_served_to_that_machine_alone() {
    let dir = TempDir::new().unwrap();
    let n = node(&dir, false).await;
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{}/api/v1/synonyms", n.api))
        .json(&serde_json::json!({"namespace": "boothost", "name": "server1", "volume": n.golden.to_string()}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let claim = |c: reqwest::Client, api: String| async move {
        let r = c.post(format!("{api}/api/v1/synonyms/boothost/server1/claim")).json(&serde_json::json!({})).send().await.unwrap();
        assert_eq!(r.status(), 201);
        r.json::<serde_json::Value>().await.unwrap()
    };
    let first = claim(c.clone(), n.api.clone()).await;
    let at = &first["attach"];
    let nqn = at["nqn"].as_str().expect("a claim answers where to attach").to_string();
    assert_eq!(nqn, format!("{SHARED}:host:server1"));
    assert_eq!(at["port"].as_u64().unwrap() as u16, n.nvme.port());
    let me = "nqn.2026-09.lo.storm:host-server1";
    assert!(at["host_nqns"].as_array().unwrap().iter().any(|h| h == me), "{at}");

    // The machine sees its clone, and only its clone.
    assert_eq!(discover(n.nvme, me).await, vec![nqn.clone()]);
    let (_, status, mut i) = connect_as(n.nvme, &nqn, me).await;
    assert_eq!(status, 0);
    assert_eq!(i.active_namespaces().await.unwrap(), vec![at["nsid"].as_u64().unwrap() as u32]);
    let dev = NvmeofDevice::connect(&spec(n.nvme, &nqn, at["nsid"].as_u64().unwrap() as u32, me, None)).await.unwrap();
    assert_eq!(dev.capacity_bytes(), 16 * MIB);
    drop(dev);
    // Another machine does not.
    let (_, status, _) = connect_as(n.nvme, &nqn, "nqn.2026-09.lo.storm:host-server2").await;
    assert_eq!(status & !DNR, INVALID_HOST);
    assert!(discover(n.nvme, "nqn.2026-09.lo.storm:host-server2").await.is_empty());

    // Next boot: a fresh clone in the same place, the old one gone from it.
    let second = claim(c, n.api.clone()).await;
    assert_eq!(second["attach"]["nqn"].as_str().unwrap(), nqn);
    let (_, _, mut i) = connect_as(n.nvme, &nqn, me).await;
    assert_eq!(i.active_namespaces().await.unwrap(), vec![second["attach"]["nsid"].as_u64().unwrap() as u32]);
    let t = n.state.nvmeof_target.read().await.clone().unwrap();
    assert_eq!(t.namespace_count().await, 0, "nothing on the shared subsystem");
}
