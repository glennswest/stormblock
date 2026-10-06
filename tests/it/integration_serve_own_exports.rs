//! `/serve/v1` serves its own exports and nobody else's (#217).
//!
//! Its reconciler used to wire every entry of the engine's export table: an
//! export made with `POST /api/v1/exports {host_nqn}`, served to that host
//! alone on the engine's listener (#210), was served again on a per-volume
//! portal that admits any host, and its recorded NSID was rewritten to 1.
//! And at every start the engine put serve's own exports (NSID 1, no
//! subsystem) on its shared subsystem as well.

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::serve::ctx::ServeContext;
use stormblock::serve::status::MkStatus;
use stormblock::serve::wiring::{WireProto, WireState, WiringTable};
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::VolumeManager;
use tempfile::TempDir;
use uuid::Uuid;

use crate::common;

const SLOT: u64 = 4096;

/// A port range nothing else is using, for the per-volume portals.
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

async fn node(dir: &TempDir) -> (Arc<AppState>, StormBlockConfig, [Uuid; 4]) {
    let devices = common::create_file_devices(dir, 2, 32 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let mut ids = [Uuid::nil(); 4];
    for (i, n) in ["host-bound", "shared", "serve-legacy", "serve-marked"].iter().enumerate() {
        ids[i] = vm.create_volume_any(n, 1 << 20).await.unwrap().0;
    }
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.serve.portal_base = Some(free_ports(8));
    config.serve.portal_span = Some(8);
    config.serve.advertise_addr = Some("127.0.0.1".into());
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config.clone(), vm, reg, gem));
    (state, config, ids)
}

fn entry(volume_id: Uuid, target_id: String, nsid: u32) -> ExportEntry {
    ExportEntry {
        id: Uuid::new_v4(),
        volume_id,
        protocol: ExportProtocol::Nvmeof,
        target_id,
        status: ExportStatus::Active,
        lun_id: None,
        nsid: Some(nsid),
        host_nqn: None,
        subsystem: None,
        serve: false,
    }
}

/// The engine's exports keep their NSIDs and get no portal; a row an earlier
/// engine made for one drains; serve's own — marked, or from before the mark
/// and named as serve names them — are wired.
#[tokio::test]
async fn the_reconciler_wires_only_its_own_exports() {
    let dir = TempDir::new().unwrap();
    let (state, config, [host_vol, shared_vol, legacy_vol, marked_vol]) = node(&dir).await;
    let (nqn_prefix, _) = config.serve_prefixes();
    let mut host = entry(host_vol, format!("nqn.2024.io.stormblock:{host_vol}"), 3);
    host.host_nqn = Some("nqn.2026-09.lo.storm:host-other".into());
    host.subsystem = Some("nqn.2026-08.lo.storm:node:host:other".into());
    let shared = entry(shared_vol, format!("nqn.2024.io.stormblock:{shared_vol}"), 7);
    let legacy = entry(legacy_vol, format!("{nqn_prefix}:vol-{legacy_vol}"), 1);
    let mut marked = entry(marked_vol, format!("{nqn_prefix}:vol-{marked_vol}"), 1);
    marked.serve = true;
    *state.exports.write().await = vec![host.clone(), shared.clone(), legacy.clone(), marked.clone()];

    let cfg = config.serve_config("127.0.0.1:3260", "127.0.0.1:4420").unwrap();
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    // What an engine before #217 left: the shared export wired on a portal.
    let mut wiring = WiringTable::load(&cfg.data_dir);
    wiring
        .insert(shared.id, shared_vol, WireProto::Nvmeof, None, &cfg.iqn_prefix, &cfg.nqn_prefix, cfg.portal_base, cfg.portal_span, false)
        .unwrap();
    wiring.get_mut(&shared.id).unwrap().state = WireState::Active;
    let (base, span) = (cfg.portal_base, cfg.portal_span);
    let reactor = Arc::new(ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false }));
    let ctx = Arc::new(ServeContext::new(cfg, state.clone(), Arc::new(MkStatus::new()), None, reactor, wiring));

    stormblock::serve::reconcile::pass(&ctx).await.unwrap();
    stormblock::serve::reconcile::pass(&ctx).await.unwrap();

    {
        let w = ctx.wiring.lock().await;
        assert!(w.get(&host.id).is_none(), "a host-bound export is never wired");
        assert!(
            w.get(&shared.id).is_none_or(|r| matches!(r.state, WireState::Draining | WireState::Withdrawn)),
            "the row an earlier engine made for a shared export drains: {:?}",
            w.get(&shared.id).map(|r| r.state)
        );
        for e in [&legacy, &marked] {
            let r = w.get(&e.id).expect("serve's own export is wired");
            assert_eq!(r.state, WireState::Active, "serve's export {} is active", e.volume_id);
        }
    }
    let ex = state.exports.read().await.clone();
    let get = |id: Uuid| ex.iter().find(|e| e.id == id).unwrap().clone();
    assert_eq!(get(host.id).nsid, Some(3), "the host-bound export keeps its NSID");
    assert_eq!(get(shared.id).nsid, Some(7), "the shared export keeps its NSID");
    assert!(get(legacy.id).serve, "a pre-mark serve export is marked");
    assert!(!get(host.id).serve && !get(shared.id).serve);
    let portals = state.nvme_portals.read().await.clone();
    assert!(portals.contains_key(&legacy_vol) && portals.contains_key(&marked_vol));
    assert!(!portals.contains_key(&host_vol) && !portals.contains_key(&shared_vol));

    // As another host: no portal in the range serves either engine export.
    for port in base..base + span {
        for vol in [host_vol, shared_vol] {
            let uri = format!("nvme-tcp://127.0.0.1:{port}/{nqn_prefix}:vol-{vol}?nsid=1");
            assert!(
                stormblock::drive::open_one_drive(&uri).await.is_err(),
                "volume {vol} must not be reachable on portal {port}"
            );
        }
    }
    // And serve's own is, where the wiring says.
    let port = ctx.wiring.lock().await.get(&marked.id).unwrap().portal_port;
    let uri = format!("nvme-tcp://127.0.0.1:{port}/{nqn_prefix}:vol-{marked_vol}?nsid=1");
    stormblock::drive::open_one_drive(&uri).await.expect("serve's own export is served");
}

/// At start, the engine restores its exports onto the shared subsystem at
/// their NSIDs, and leaves serve's (NSID 1, its own subsystem) to serve.
#[tokio::test]
async fn the_engine_does_not_put_serve_exports_on_its_shared_subsystem() {
    let dir = TempDir::new().unwrap();
    let (state, config, [_, shared_vol, legacy_vol, _]) = node(&dir).await;
    let (nqn_prefix, _) = config.serve_prefixes();
    let shared = entry(shared_vol, format!("nqn.2024.io.stormblock:{shared_vol}"), 5);
    let legacy = entry(legacy_vol, format!("{nqn_prefix}:vol-{legacy_vol}"), 1);
    std::fs::write(dir.path().join("exports.json"), serde_json::to_vec(&vec![shared.clone(), legacy.clone()]).unwrap()).unwrap();

    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        nqn: "nqn.2026-08.lo.storm:test".into(),
        ..Default::default()
    }));
    *state.nvmeof_target.write().await = Some(target.clone());
    stormblock::mgmt::api::exports::restore_exports(&state).await;

    let sub = target.default_subsystem();
    assert_eq!(sub.nsid_of(shared_vol).await, Some(5), "the engine's export is back at its NSID");
    assert_eq!(sub.nsid_of(legacy_vol).await, None, "serve's export is not on the shared subsystem");
    let ex = state.exports.read().await.clone();
    assert!(ex.iter().find(|e| e.id == legacy.id).unwrap().serve, "kept, and marked as serve's");
}
