//! `/serve/v1` exports bound to one host (#212, the #210 follow-up).
//!
//! Each serve export is a subsystem of its own on its own portal, and they
//! admitted any host that reached the port: discovery there named the
//! volume, and Connect let anyone in. An export that names `host_nqn` now
//! admits that host alone; one that names none admits any host only while
//! `[serve] allow_any_host` is left on, and with it off is refused (and a row
//! from before host binding admits nobody).

use std::net::SocketAddr;
use std::sync::Arc;

use stormblock::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::serve::ctx::ServeContext;
use stormblock::serve::status::MkStatus;
use stormblock::serve::wiring::{WireProto, WireState, WiringTable};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::VolumeManager;
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::common;
use crate::common::nvmeof_initiator::NvmeofInitiator;

const SLOT: u64 = 4096;
const H1: &str = "nqn.2014-08.org.nvmexpress:uuid:11111111-1111-1111-1111-111111111111";
const H2: &str = "nqn.2014-08.org.nvmexpress:uuid:22222222-2222-2222-2222-222222222222";
const DISCOVERY: &str = "nqn.2014-08.org.nvmexpress.discovery";

fn free_ports(span: u16) -> u16 {
    loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = l.local_addr().unwrap().port();
        drop(l);
        if base < 65535 - span && (0..span).all(|i| std::net::TcpListener::bind(("127.0.0.1", base + i)).is_ok()) {
            return base;
        }
    }
}

struct Serve {
    ctx: Arc<ServeContext>,
    api: String,
    vols: Vec<Uuid>,
    data_dir: String,
    _server: tokio::task::JoinHandle<()>,
}

async fn serve(dir: &TempDir, allow_any_host: Option<bool>, legacy_row: bool) -> Serve {
    let devices = common::create_file_devices(dir, 2, 32 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let mut vols = Vec::new();
    for n in ["a", "b", "c"] {
        vols.push(vm.create_volume_any(n, 1 << 20).await.unwrap().0);
    }
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.serve.portal_base = Some(free_ports(8));
    config.serve.portal_span = Some(8);
    config.serve.advertise_addr = Some("127.0.0.1".into());
    config.serve.allow_any_host = allow_any_host;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config.clone(), vm, reg, gem));
    let cfg = config.serve_config("127.0.0.1:3260", "127.0.0.1:4420").unwrap();
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let data_dir = cfg.data_dir.clone();
    let mut wiring = WiringTable::load(&cfg.data_dir);
    if legacy_row {
        // A row from before host binding: no host_nqn, and an export entry
        // serve made.
        let id = Uuid::new_v4();
        wiring
            .insert(id, vols[2], WireProto::Nvmeof, None, &cfg.iqn_prefix, &cfg.nqn_prefix, cfg.portal_base, cfg.portal_span, false)
            .unwrap();
        let nqn = wiring.get(&id).unwrap().nqn.clone().unwrap();
        state.exports.write().await.push(stormblock::mgmt::ExportEntry {
            id,
            volume_id: vols[2],
            protocol: stormblock::mgmt::ExportProtocol::Nvmeof,
            target_id: nqn,
            status: stormblock::mgmt::ExportStatus::PendingRestart,
            lun_id: None,
            nsid: Some(1),
            host_nqn: None,
            subsystem: None,
            serve: true,
        });
    }
    let reactor = Arc::new(ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false }));
    let ctx = Arc::new(ServeContext::new(cfg, state.clone(), Arc::new(MkStatus::new()), None, reactor, wiring));
    stormblock::serve::reconcile::pass(&ctx).await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let router = stormblock::serve::api::router(ctx.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(http).await;
    Serve { ctx, api: format!("http://{http}"), vols, data_dir, _server: server }
}

async fn export(s: &Serve, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new().post(format!("{}/serve/v1/exports", s.api)).json(&body).send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(serde_json::Value::Null))
}

fn spec(port: u16, nqn: &str, host: &str) -> NvmeTcpSpec {
    NvmeTcpSpec { addr: format!("127.0.0.1:{port}"), nqn: nqn.into(), nsid: 1, host_nqn: Some(host.into()), dhchap: None }
}

/// What the discovery log page on `port` shows `host`.
async fn discover(port: u16, host: &str) -> Vec<String> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut i = NvmeofInitiator::connect(addr).await.unwrap();
    i.ic_handshake().await.unwrap();
    let (_, status) = i.fabric_connect_raw(DISCOVERY, host, 0).await.unwrap();
    assert_eq!(status, 0, "discovery itself is open");
    let page = i.get_log_page(0x70, 4096).await.unwrap();
    let n = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
    (0..n)
        .map(|k| {
            let e = &page[1024 + k * 1024..1024 + (k + 1) * 1024];
            let nqn = &e[256..512];
            String::from_utf8_lossy(&nqn[..nqn.iter().position(|b| *b == 0).unwrap_or(256)]).to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_for_one_host_admits_that_host_alone() {
    let dir = TempDir::new().unwrap();
    let s = serve(&dir, None, false).await;

    let (st, a) = export(&s, serde_json::json!({"volume_id": s.vols[0], "protocol": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(st, 201, "{a}");
    assert_eq!(a["host_nqn"], H1, "{a}");
    let port = a["attach"]["port"].as_u64().unwrap() as u16;
    let nqn = a["attach"]["nqn"].as_str().unwrap().to_string();

    // The host it was made for: in, and data moves.
    let dev = NvmeofDevice::connect(&spec(port, &nqn, H1)).await.expect("H1 connects");
    dev.write(0, &vec![5u8; 4096]).await.unwrap();
    let mut back = vec![0u8; 4096];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, vec![5u8; 4096]);
    // Any other: out, and discovery on the port does not name it.
    assert!(NvmeofDevice::connect(&spec(port, &nqn, H2)).await.is_err(), "H2 must not connect");
    assert_eq!(discover(port, H1).await, vec![nqn.clone()]);
    assert!(discover(port, H2).await.is_empty(), "discovery shows H2 nothing");

    // Kept across a restart: the wiring table carries the host.
    let reloaded = WiringTable::load(&s.data_dir);
    let row = reloaded.exports.iter().find(|w| w.volume_id == s.vols[0]).unwrap();
    assert_eq!(row.host_nqn.as_deref(), Some(H1));

    // Naming no host, on a node that still allows it: any host, as before.
    let (st, b) = export(&s, serde_json::json!({"volume_id": s.vols[1], "protocol": "nvme-tcp"})).await;
    assert_eq!(st, 201, "{b}");
    assert!(b.get("host_nqn").is_none());
    let (port_b, nqn_b) = (b["attach"]["port"].as_u64().unwrap() as u16, b["attach"]["nqn"].as_str().unwrap().to_string());
    assert!(NvmeofDevice::connect(&spec(port_b, &nqn_b, H2)).await.is_ok(), "allow_any_host: any host");

    // Not an NQN: refused, nothing made.
    let before = s.ctx.wiring.lock().await.exports.len();
    let (st, e) = export(&s, serde_json::json!({"volume_id": s.vols[2], "protocol": "nvme-tcp", "host_nqn": "pve"})).await;
    assert_eq!(st, 400, "{e}");
    assert_eq!(s.ctx.wiring.lock().await.exports.len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_allows_no_host_refuses_an_unbound_export_and_closes_old_ones() {
    let dir = TempDir::new().unwrap();
    let s = serve(&dir, Some(false), true).await;

    // The row from before host binding is wired, and admits nobody.
    let legacy = {
        let w = s.ctx.wiring.lock().await;
        w.exports.iter().find(|r| r.volume_id == s.vols[2]).cloned().unwrap()
    };
    assert_eq!(legacy.state, WireState::Active);
    let nqn = legacy.nqn.clone().unwrap();
    assert!(NvmeofDevice::connect(&spec(legacy.portal_port, &nqn, H1)).await.is_err(), "nobody gets in");
    assert!(discover(legacy.portal_port, H1).await.is_empty());

    // An export naming no host: refused, nothing made.
    let before = s.ctx.wiring.lock().await.exports.len();
    let (st, e) = export(&s, serde_json::json!({"volume_id": s.vols[0], "protocol": "nvme-tcp"})).await;
    assert_eq!(st, 400, "{e}");
    assert!(e.to_string().contains("allow_any_host"), "{e}");
    assert_eq!(s.ctx.wiring.lock().await.exports.len(), before);

    // Naming one: served to it.
    let (st, a) = export(&s, serde_json::json!({"volume_id": s.vols[0], "protocol": "nvme-tcp", "host_nqn": H2})).await;
    assert_eq!(st, 201, "{a}");
    let (port, nqn) = (a["attach"]["port"].as_u64().unwrap() as u16, a["attach"]["nqn"].as_str().unwrap().to_string());
    assert!(NvmeofDevice::connect(&spec(port, &nqn, H2)).await.is_ok());
    assert!(NvmeofDevice::connect(&spec(port, &nqn, H1)).await.is_err());

    // A volume created with its export, for a host.
    let r = reqwest::Client::new()
        .post(format!("{}/serve/v1/volumes", s.api))
        .json(&serde_json::json!({"name": "d", "size_bytes": 1 << 20, "export": true, "host_nqn": H1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["export"]["host_nqn"], H1, "{v}");
    // And one without a host: refused before any volume is made.
    let count = s.ctx.state.volume_manager.lock().await.list_volumes().await.len();
    let r = reqwest::Client::new()
        .post(format!("{}/serve/v1/volumes", s.api))
        .json(&serde_json::json!({"name": "e", "size_bytes": 1 << 20, "export": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(s.ctx.state.volume_manager.lock().await.list_volumes().await.len(), count, "no orphan volume");
}

/// #188: every serve NVMe export is a subsystem of one listener, so a node
/// serves far more than its port span (8 here) — 300 exports, each its own
/// NQN on one port, each with its own volume. And a subsystem drains on its
/// own: withdrawn while a host is attached, it serves that host, refuses a
/// new Connect, and goes once the host lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_hundred_exports_share_one_listener_and_drain_one_by_one() {
    let dir = TempDir::new().unwrap();
    let s = serve(&dir, None, false).await;
    let client = reqwest::Client::new();
    let mut made: Vec<(Uuid, u16, String)> = Vec::new();
    for i in 0..300u32 {
        let id = s.ctx.state.volume_manager.lock().await.create_volume_any(&format!("v{i}"), 1 << 20).await.unwrap().0;
        let (st, a) = export(&s, serde_json::json!({"volume_id": id, "protocol": "nvme-tcp"})).await;
        assert_eq!(st, 201, "export {i}: {a}");
        made.push((id, a["attach"]["port"].as_u64().unwrap() as u16, a["attach"]["nqn"].as_str().unwrap().to_string()));
    }
    let port = made[0].1;
    assert!(made.iter().all(|m| m.1 == port), "every export on the one listener");
    let nqns: std::collections::HashSet<&String> = made.iter().map(|m| &m.2).collect();
    assert_eq!(nqns.len(), 300, "each its own subsystem");
    // The first, a middle and the last each serve their own volume.
    for k in [0usize, 150, 299] {
        let (id, _, nqn) = &made[k];
        let vol = s.ctx.state.volume_manager.lock().await.get_volume(&stormblock::volume::VolumeId(*id)).unwrap();
        vol.write(0, &vec![k as u8 + 1; 4096]).await.unwrap();
        let dev = NvmeofDevice::connect(&spec(port, nqn, H1)).await.expect("connect");
        let mut back = vec![0u8; 4096];
        dev.read(0, &mut back).await.unwrap();
        assert_eq!(back, vec![k as u8 + 1; 4096], "export {k} serves its own volume");
    }

    // Drain one with a host attached.
    let (_, _, nqn) = made[7].clone();
    // One controller, held open as a host holds its queues.
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut held = NvmeofInitiator::connect(addr).await.unwrap();
    held.ic_handshake().await.unwrap();
    let (_, st) = held.fabric_connect_raw(&nqn, H1, 0).await.unwrap();
    assert_eq!(st, 0, "connected");
    let eid = {
        let w = s.ctx.wiring.lock().await;
        w.exports.iter().find(|r| r.nqn.as_deref() == Some(nqn.as_str())).unwrap().export_id
    };
    let r = client.delete(format!("{}/serve/v1/exports/{eid}", s.api)).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    stormblock::serve::reconcile::pass(&s.ctx).await.unwrap();
    let state = |s: &Serve| {
        let ctx = s.ctx.clone();
        async move { ctx.wiring.lock().await.exports.iter().find(|r| r.export_id == eid).map(|r| r.state) }
    };
    assert_eq!(state(&s).await, Some(stormblock::serve::wiring::WireState::Draining), "held: still draining");
    let id = held.identify_controller().await.expect("the attached host is still served");
    assert!(!id.is_empty(), "the attached host is still served");
    let mut late = NvmeofInitiator::connect(addr).await.unwrap();
    late.ic_handshake().await.unwrap();
    let (_, st) = late.fabric_connect_raw(&nqn, H1, 0).await.unwrap();
    assert_ne!(st, 0, "a draining subsystem takes no one new");
    drop(late);
    drop(held);
    for _ in 0..50 {
        stormblock::serve::reconcile::pass(&s.ctx).await.unwrap();
        if !matches!(state(&s).await, Some(stormblock::serve::wiring::WireState::Draining)) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_ne!(state(&s).await, Some(stormblock::serve::wiring::WireState::Draining), "let go: withdrawn");
    assert!(NvmeofDevice::connect(&spec(port, &nqn, H1)).await.is_err(), "withdrawn: gone");
    // Its neighbours are untouched.
    assert!(NvmeofDevice::connect(&spec(port, &made[8].2, H1)).await.is_ok());

    // All of them go, and the listener holds only its own subsystem again.
    let ids: Vec<Uuid> = s.ctx.wiring.lock().await.exports.iter().map(|r| r.export_id).collect();
    for id in ids {
        client.delete(format!("{}/serve/v1/exports/{id}", s.api)).send().await.unwrap();
    }
    for _ in 0..3 {
        stormblock::serve::reconcile::pass(&s.ctx).await.unwrap();
    }
    let left = s.ctx.nvme_listener.lock().await.as_ref().unwrap().target.subsystems().len();
    assert_eq!(left, 1, "only the listener's own subsystem is left");
}

/// #98: a claim's own subsystem is a subsystem of the serve listener, as
/// every `/serve/v1` export is (#188), not a listener of its own. It had a
/// port each, from the range the serve listener binds at; a node claims twice
/// per boot, so thousands of listeners at fleet scale. Each subsystem still
/// admits only its own host, and the volume is namespace 1 of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claims_subsystem_is_served_on_the_one_serve_listener() {
    use stormblock::target::nvmeof::HostAccess;
    let dir = TempDir::new().unwrap();
    let s = serve(&dir, None, false).await;
    let state = s.ctx.state.clone();
    *state.per_volume.write().await = Some(stormblock::mgmt::PerVolumeServing {
        nqn_prefix: s.ctx.cfg.nqn_prefix.clone(),
        portal_base: s.ctx.cfg.portal_base,
        portal_span: s.ctx.cfg.portal_span,
        reactor: s.ctx.reactor.clone(),
        serve: Arc::downgrade(&s.ctx),
    });

    // A /serve/v1 export first: it binds the serve listener.
    let (st, a) = export(&s, serde_json::json!({"volume_id": s.vols[0], "protocol": "nvme-tcp", "host_nqn": H1})).await;
    assert_eq!(st, 201, "{a}");
    let port = a["attach"]["port"].as_u64().unwrap() as u16;

    let one = |h: &str| HostAccess::Hosts([(h.to_string(), None)].into_iter().collect());
    let (nqn_b, port_b) = stormblock::mgmt::api::v1::ensure_volume_subsystem(&state, s.vols[1], one(H1)).await.expect("served");
    let (nqn_c, port_c) = stormblock::mgmt::api::v1::ensure_volume_subsystem(&state, s.vols[2], one(H2)).await.expect("served");
    assert_eq!((port_b, port_c), (port, port), "every subsystem on the one serve listener");
    assert!(nqn_b.ends_with(&format!(":vol-{}", s.vols[1])) && nqn_c.ends_with(&format!(":vol-{}", s.vols[2])));
    // Idempotent: the same address again.
    assert_eq!(
        stormblock::mgmt::api::v1::ensure_volume_subsystem(&state, s.vols[1], one(H1)).await,
        Some((nqn_b.clone(), port))
    );
    // Nothing else bound in the range: no listener of its own.
    for p in port + 1..port + s.ctx.cfg.portal_span {
        assert!(std::net::TcpListener::bind(("0.0.0.0", p)).is_ok(), "port {p} is free");
    }

    // Each admits its own host, and the volume is namespace 1.
    let dev = NvmeofDevice::connect(&spec(port, &nqn_b, H1)).await.expect("H1 reaches its claim");
    dev.write(0, &vec![7u8; 4096]).await.unwrap();
    let mut back = vec![0u8; 4096];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, vec![7u8; 4096]);
    assert!(NvmeofDevice::connect(&spec(port, &nqn_b, H2)).await.is_err(), "H2 does not reach H1's");
    assert!(NvmeofDevice::connect(&spec(port, &nqn_c, H2)).await.is_ok(), "H2 reaches its own");
    let mut seen = discover(port, H1).await;
    seen.sort();
    let mut want = vec![a["attach"]["nqn"].as_str().unwrap().to_string(), nqn_b.clone()];
    want.sort();
    assert_eq!(seen, want, "discovery shows H1 what it may connect to");
}
