//! A volume pulled from a peer engine over NVMe/TCP, by id (#656): the
//! transport a registry sync stands on.
//!
//! The peer holds a sealed golden and a sealed clone of it, and serves
//! NVMe/TCP closed to unnamed hosts (#210). The puller asks for the golden,
//! then the clone by name. The golden arrives with only the extents it maps;
//! the clone arrives as a copy-on-write clone of the golden's copy, given
//! only what it holds of its own. The bytes are the peer's. The peer's
//! namespaces are let go of after each copy, and nothing is served to the
//! puller once it is done.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use stormblock::mgmt::config::{NvmeofExportConfig, StormBlockConfig};
use stormblock::mgmt::AppState;
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::{VolumeId, VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;

pub const HERE_NQN: &str = "nqn.2026-09.lo.storm:host-here";

/// An engine; `peer` gives it NVMe/TCP closed to unnamed hosts.
pub async fn engine(dir: &TempDir, peer: bool) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(uuid::Uuid::new_v4()), devices[0].clone()).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    if peer {
        config.management.node_name = Some("peer".into());
        config.nvmeof = Some(NvmeofExportConfig {
            listen_addr: "127.0.0.1:0".into(),
            nqn: "nqn.2026-09.test:peer".into(),
            export_drives: false,
            allow_any_host: false,
            allowed_hosts: Vec::new(),
            require_dhchap: false,
            boothost_host_nqn: None,
        });
    } else {
        config.management.node_name = Some("here".into());
        config.fabric.host_nqn = Some(HERE_NQN.into());
    }
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));
    if peer {
        start_target(&state).await;
    }
    state
}

/// The peer's NVMe/TCP target, as the daemon starts it.
async fn start_target(state: &Arc<AppState>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: addr,
        nqn: "nqn.2026-09.test:peer".into(),
        advertised_addr: Some(addr),
        ..Default::default()
    }));
    *state.nvmeof_target.write().await = Some(target.clone());
    stormblock::mgmt::nvme_hosts::apply_shared_policy(state, &target);
    stormblock::mgmt::nvme_hosts::restore(state).await;
    tokio::spawn(async move {
        let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
        let _ = target.run_with_listener(listener, &reactor).await;
    });
    common::wait_for_listener(addr).await;
}

pub async fn serve(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    let h = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (format!("http://{addr}"), h)
}

pub async fn put(state: &AppState, id: VolumeId, at: u64, byte: u8, len: usize) {
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    dev.write(at, &vec![byte; len]).await.unwrap();
    dev.flush().await.unwrap();
}

pub async fn all_of(state: &AppState, id: VolumeId) -> Vec<u8> {
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    let mut b = vec![0u8; dev.capacity_bytes() as usize];
    dev.read(0, &mut b).await.unwrap();
    b
}

pub async fn by_name(state: &AppState, name: &str) -> Option<VolumeId> {
    state.volume_manager.lock().await.find_volume(name).await
}

/// The namespaces the peer serves to the puller's host NQN.
pub async fn served_here(peer: &AppState) -> usize {
    let h = peer.nvme_hosts.lock().await;
    h.subsystems.values().filter(|s| s.hosts.iter().any(|e| e.nqn == HERE_NQN)).map(|s| s.namespaces.len()).sum()
}

async fn wait_job(c: &reqwest::Client, url: &str) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..300 {
        last = c.get(url).send().await.unwrap().json().await.unwrap();
        if last["state"] != "copying" {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the pull did not end: {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_golden_and_its_clone_pulled_over_nvme_tcp() {
    let (da, db) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (peer, here) = (engine(&da, true).await, engine(&db, false).await);
    let x = DEFAULT_EXTENT_SIZE;
    // A golden of 8 extents with data in 0 and 2; a clone of it that
    // rewrote 2 and wrote 3; both sealed. And a writable volume.
    let golden = peer.volume_manager.lock().await.create_volume_any("golden-base-aaaa", 8 * x).await.unwrap();
    put(&peer, golden, 0, 0x11, 4096).await;
    put(&peer, golden, 2 * x + 8192, 0x22, 4096).await;
    peer.volume_manager.lock().await.seal_volume(golden, None).await.unwrap();
    let clone = peer.volume_manager.lock().await.create_snapshot(golden, "golden-app-bbbb").await.unwrap();
    put(&peer, clone, 2 * x, 0x33, 4096).await;
    put(&peer, clone, 3 * x, 0x44, 4096).await;
    peer.volume_manager.lock().await.seal_volume(clone, None).await.unwrap();
    let scratch = peer.volume_manager.lock().await.create_volume_any("scratch", x).await.unwrap();
    // A golden whose digest on the peer is not its content: what a corrupt
    // copy looks like from the other side.
    let bad = peer.volume_manager.lock().await.create_volume_any("golden-bad-cccc", 2 * x).await.unwrap();
    put(&peer, bad, 0, 0x55, 4096).await;
    peer.volume_manager.lock().await.seal_volume(bad, None).await.unwrap();
    // The peer's digester, as the daemon runs it after a seal.
    assert_eq!(stormblock::volume::digest::digest_sealed(&peer.volume_manager).await, 3);
    let sha_of = |id: VolumeId| {
        let peer = peer.clone();
        async move { peer.volume_manager.lock().await.seal_digest(&id).unwrap().hex() }
    };
    let (golden_sha, clone_sha) = (sha_of(golden).await, sha_of(clone).await);
    {
        let mut vm = peer.volume_manager.lock().await;
        vm.set_seal_digest(bad, Some(stormblock::volume::metadata::SealDigest::new([7u8; 32])));
        vm.publish_catalog();
    }

    let (purl, ph) = serve(peer.clone()).await;
    let (hurl, hh) = serve(here.clone()).await;
    let c = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let pull = format!("{hurl}/api/v1/volumes/pull");

    // The golden, by id: only the two extents it maps are read.
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": golden.0 })).send().await.unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let job: serde_json::Value = r.json().await.unwrap();
    let job = wait_job(&c, &format!("{pull}/{}", job["id"].as_str().unwrap())).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["virtual_bytes"], 8 * x, "{job}");
    assert_eq!(job["extents_copied"], 2, "{job}");
    assert_eq!(job["bytes_total"], 2 * x, "only the mapped extents: {job}");
    assert_eq!(job["bytes_done"], 2 * x, "{job}");
    assert_eq!(job["allocated_bytes"], 2 * x, "as thin here as on the peer: {job}");
    assert_eq!(job["peer_sha256"], golden_sha.as_str(), "the peer's seal digest: {job}");
    assert_eq!(job["sha256"], golden_sha.as_str(), "the copy read back here: {job}");
    assert_eq!(job["verified"], true, "{job}");
    let g = by_name(&here, "golden-base-aaaa").await.expect("the golden is here");
    assert_eq!(job["volume"], g.0.to_string());
    assert!(here.volume_manager.lock().await.is_sealed(&g));
    assert!(all_of(&here, g).await == all_of(&peer, golden).await, "the golden's bytes");
    assert_eq!(served_here(&peer).await, 0, "the peer's namespace let go of after the copy");

    // The clone, by name: a clone of the golden's copy here, only its own
    // extents read (2 rewritten, 3 new), 0 shared.
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": "golden-app-bbbb" })).send().await.unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let job: serde_json::Value = r.json().await.unwrap();
    assert_eq!(job["parent"], g.0.to_string(), "found by its parent's name: {job}");
    let job = wait_job(&c, &format!("{pull}/{}", job["id"].as_str().unwrap())).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!((job["extents_copied"].as_u64(), job["extents_shared"].as_u64(), job["extents_zeroed"].as_u64()), (Some(2), Some(1), Some(0)), "{job}");
    assert_eq!(job["bytes_done"], 2 * x, "never the whole golden again: {job}");
    assert_eq!((job["sha256"].as_str(), job["verified"].as_bool()), (Some(clone_sha.as_str()), Some(true)), "{job}");
    let k = by_name(&here, "golden-app-bbbb").await.expect("the clone is here");
    {
        let vm = here.volume_manager.lock().await;
        let d = vm.seal_digest(&k).expect("the copy keeps the digest it was read back with");
        assert_eq!((d.hex(), d.verdict.as_str()), (clone_sha.clone(), "intact"));
        assert!(vm.is_sealed(&k));
        assert_eq!(vm.parent(&k), Some(g), "a clone of the golden's copy, not a flattened copy");
    }
    assert!(all_of(&here, k).await == all_of(&peer, clone).await, "the clone's bytes");
    assert_eq!(served_here(&peer).await, 0);

    // The same again: here already.
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": golden.0 })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["volume"], g.0.to_string(), "{v}");
    // A writable volume is the live mirror's, a missing one a 404, a
    // different volume of the same name a 409, a bad peer a 422.
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": scratch.0 })).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("live mirror"));
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": uuid::Uuid::new_v4() })).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": clone.0, "name": "golden-base-aaaa" })).send().await.unwrap();
    assert_eq!(r.status(), 409, "a different volume of that name here is never overwritten");
    let r = c.post(&pull).json(&serde_json::json!({ "from": "forge:9090", "volume": golden.0 })).send().await.unwrap();
    assert_eq!(r.status(), 422);
    let list: serde_json::Value = c.get(&pull).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["count"], 2, "{list}");

    // A copy whose sha is not the peer's: deleted, the job failed with both.
    let r = c.post(&pull).json(&serde_json::json!({ "from": purl, "volume": "golden-bad-cccc" })).send().await.unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let job: serde_json::Value = r.json().await.unwrap();
    let job = wait_job(&c, &format!("{pull}/{}", job["id"].as_str().unwrap())).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["verified"], false, "{job}");
    let m = job["message"].as_str().unwrap_or_default();
    assert!(m.contains(&"07".repeat(32)) && m.contains("not the peer's volume"), "both shas named: {job}");
    assert!(by_name(&here, "golden-bad-cccc").await.is_none(), "never kept");
    assert_eq!(served_here(&peer).await, 0);
    ph.abort();
    hh.abort();
}

/// A copy a restart cut short: its half-made volume deleted, the job failed.
#[tokio::test]
async fn a_pull_cut_short_by_a_restart_leaves_nothing_half_made() {
    let d = TempDir::new().unwrap();
    let here = engine(&d, false).await;
    let half = here.volume_manager.lock().await.create_volume_any("golden-half", DEFAULT_EXTENT_SIZE).await.unwrap();
    let job = uuid::Uuid::new_v4();
    let rec = serde_json::json!({ job.to_string(): {
        "id": job, "state": "copying", "from": "http://peer:9090", "peer_volume": "p", "name": "golden-half", "namespace": "default",
        "volume": half.0, "virtual_bytes": DEFAULT_EXTENT_SIZE, "peer_allocated_bytes": 0, "bytes_total": 0, "bytes_done": 0,
        "extents_copied": 0, "extents_shared": 0, "extents_zeroed": 0, "created_at": 1 } });
    std::fs::write(d.path().join("pull-jobs.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
    stormblock::mgmt::pull::resume(&here).await;
    let j = stormblock::mgmt::pull::get(&here, job).unwrap();
    assert_eq!(j.state, "failed");
    assert!(j.message.as_deref().unwrap().contains("half-made copy was deleted"), "{j:?}");
    assert!(here.volume_manager.lock().await.get_volume(&half).is_none());
}
