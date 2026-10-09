//! Forge mode turned on per node through the API and kept by the engine
//! (#272): one stormcos image, the forge role chosen at install; and on by
//! default on a node, the API the day-2 switch (#287).

use std::net::SocketAddr;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::volume::{VolumeId, VolumeManager};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;

async fn engine(dir: &TempDir, image: Option<&[u8]>) -> (Arc<AppState>, String, Option<VolumeId>) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("engine").to_string_lossy().to_string());
    std::fs::create_dir_all(dir.path().join("engine")).unwrap();
    config.management.advertised_addr = Some("127.0.0.1".into());
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let pool = dir.path().join(format!("pool-{}.bin", Uuid::new_v4()));
    let dev = FileDevice::open_with_capacity(pool.to_str().unwrap(), 64 * MIB).await.unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let mut golden = None;
    if let Some(bytes) = image {
        let g = vm.create_volume("release", 4 * MIB, array).await.unwrap();
        let h = vm.get_volume(&g).unwrap();
        h.write(0, bytes).await.unwrap();
        h.flush().await.unwrap();
        vm.seal_volume(g, None).await.unwrap();
        golden = Some(g);
    }
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (state, format!("http://{api}"), golden)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn listening(addr: SocketAddr) -> bool {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_becomes_a_forge_keeps_it_and_stops_being_one() {
    let dir = TempDir::new().unwrap();
    let image: Vec<u8> = (0..MIB as usize).map(|i| (i % 241) as u8).collect();
    let (state, base, golden) = engine(&dir, Some(&image)).await;
    let c = reqwest::Client::new();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let before: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!(before["enabled"], false, "an ordinary node: {before}");

    // A listen address that is not one is said, and nothing changes.
    let r = c.put(format!("{base}/api/v1/forge")).json(&serde_json::json!({"listen_addr": "nowhere"})).send().await.unwrap();
    assert_eq!(r.status(), 400);

    // The install-config apply makes this call (stormcos#82).
    let r = c
        .put(format!("{base}/api/v1/forge"))
        .json(&serde_json::json!({"listen_addr": format!("127.0.0.1:{port}"), "nqn": "nqn.2026-10.test:forge"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "PUT: {}", r.status());
    let on: serde_json::Value = r.json().await.unwrap();
    assert_eq!(on["enabled"], true);
    assert_eq!(on["source"], "api");
    assert!(listening(addr).await, "the target is serving, live");
    assert!(dir.path().join("engine/forge.json").exists(), "kept in the data dir");

    // A booting machine's claim gets something to attach, and reads it.
    c.post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "boothost", "name": "node2", "volume": golden.unwrap().0.to_string()}))
        .send()
        .await
        .unwrap();
    let claim: serde_json::Value = c
        .post(format!("{base}/api/v1/synonyms/boothost/node2/claim"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let a = &claim["attach"];
    assert_eq!(a["protocol"], "nvme-tcp", "{claim}");
    assert_eq!(a["port"], port);
    let spec = NvmeTcpSpec {
        addr: addr.to_string(),
        nqn: a["nqn"].as_str().unwrap().into(),
        nsid: a["nsid"].as_u64().unwrap() as u32,
        host_nqn: Some(a["host_nqns"][0].as_str().unwrap().into()),
        dhchap: None,
    };
    let dev = NvmeofDevice::connect(&spec).await.expect("the machine connects");
    let mut back = vec![0u8; MIB as usize];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, image);
    drop(dev);

    // Off: no new connections, and kept off (#287).
    let off: serde_json::Value = c.delete(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!(off["enabled"], false, "{off}");
    assert_eq!(off["state"], "off");
    assert_eq!(off["from"], "persisted");
    let kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("engine/forge.json")).unwrap()).unwrap();
    assert_eq!(kept, serde_json::json!({"enabled": false}));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err(), "no longer accepting");
    // Twice is fine.
    assert!(c.delete(format!("{base}/api/v1/forge")).send().await.unwrap().status().is_success());
    drop(state);
}

/// What the next start of the engine does: the node's own setting, with no
/// --config and no argv change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_start_serves_the_forge_this_node_keeps() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    {
        let (_state, base, _) = engine(&dir, None).await;
        let r = reqwest::Client::new()
            .put(format!("{base}/api/v1/forge"))
            .json(&serde_json::json!({"listen_addr": format!("127.0.0.1:{port}"), "nqn": "nqn.2026-10.test:forge", "allow_any_host": true}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        let off = reqwest::Client::new().get(format!("{base}/api/v1/forge")).send().await.unwrap();
        assert!(off.status().is_success());
        // This engine goes away; its listener with it.
        _state.nvmeof_target.write().await.take().unwrap().stop_accepting();
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (state, base, _) = engine(&dir, None).await;
    stormblock::mgmt::forge::restore(&state, None).await;
    let s: serde_json::Value = reqwest::Client::new().get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["enabled"], true, "{s}");
    assert_eq!(s["source"], "api");
    assert_eq!(s["allow_any_host"], true, "the host policy came back with it");
    assert!(listening(addr).await);
}

/// A target the configuration set up is the configuration's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_target_is_not_the_apis_to_change() {
    let dir = TempDir::new().unwrap();
    let (state, base, _) = engine(&dir, None).await;
    let settings = stormblock::mgmt::config::NvmeofExportConfig {
        listen_addr: format!("127.0.0.1:{}", free_port()),
        nqn: "nqn.2026-10.test:configured".into(),
        export_drives: false,
        allow_any_host: false,
        allowed_hosts: Vec::new(),
        require_dhchap: false,
        boothost_host_nqn: None,
    };
    let target = stormblock::mgmt::forge::target_from(&settings, &state.config.management).unwrap();
    let reactor = Arc::new(stormblock::target::reactor::ReactorPool::new(
        &stormblock::target::reactor::ReactorConfig { core_count: 1, pin_cores: false },
    ));
    stormblock::mgmt::forge::serve(&state, &reactor, Arc::new(target)).await.unwrap();
    stormblock::mgmt::forge::mark_configured(&state).await;

    let c = reqwest::Client::new();
    let s: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["source"], "config");
    let r = c.put(format!("{base}/api/v1/forge")).json(&serde_json::json!({"listen_addr": "127.0.0.1:4420"})).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(c.delete(format!("{base}/api/v1/forge")).send().await.unwrap().status(), 409);
    assert!(state.nvmeof_target.read().await.is_some(), "still serving");
}

/// A node (#287): with nothing kept it serves its default forge and answers
/// boot claims; told off, it stays off across a restart; told on again, it
/// serves what it was told. The daemon's default stays off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_is_a_forge_by_default_until_told_off_and_stays_off() {
    use stormblock::mgmt::forge;
    let dir = TempDir::new().unwrap();
    let image: Vec<u8> = (0..MIB as usize).map(|i| (i % 239) as u8).collect();
    let c = reqwest::Client::new();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let (state, base, golden) = engine(&dir, Some(&image)).await;
    // The daemon: nothing kept, no default → off, and says why.
    forge::restore(&state, None).await;
    let s: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!((s["state"].as_str(), s["from"].as_str()), (Some("off"), Some("default")), "{s}");

    // adopt-ublk: the node's default.
    let mut d = forge::default_settings(&state);
    assert_eq!(d.listen_addr, "0.0.0.0:4420");
    assert!(d.nqn.starts_with("nqn.2026-08.lo.storm:") && d.nqn.len() > "nqn.2026-08.lo.storm:".len(), "{}", d.nqn);
    assert!(!d.allow_any_host && d.allowed_hosts.is_empty(), "#210's closed policy");
    d.listen_addr = addr.to_string();
    forge::restore(&state, Some(d.clone())).await;
    let s: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!((s["state"].as_str(), s["from"].as_str()), (Some("on"), Some("default")), "{s}");
    assert_eq!(s["nqn"], d.nqn.as_str());
    assert!(listening(addr).await);
    assert!(!dir.path().join("engine/forge.json").exists(), "a default is not written down");

    // A boot claim is answered with something to attach, out of the box.
    c.post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "boothost", "name": "server3", "volume": golden.unwrap().0.to_string()}))
        .send()
        .await
        .unwrap();
    let claim: serde_json::Value = c
        .post(format!("{base}/api/v1/synonyms/boothost/server3/claim"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let a = &claim["attach"];
    assert_eq!(a["protocol"], "nvme-tcp", "{claim}");
    let spec = NvmeTcpSpec {
        addr: addr.to_string(),
        nqn: a["nqn"].as_str().unwrap().into(),
        nsid: a["nsid"].as_u64().unwrap() as u32,
        host_nqn: Some(a["host_nqns"][0].as_str().unwrap().into()),
        dhchap: None,
    };
    let dev = NvmeofDevice::connect(&spec).await.expect("the booting machine connects");
    let mut back = vec![0u8; MIB as usize];
    dev.read(0, &mut back).await.unwrap();
    assert_eq!(back, image);
    drop(dev);
    // An anonymous host is not let in to the shared subsystem.
    let anon = NvmeTcpSpec { addr: addr.to_string(), nqn: d.nqn.clone(), nsid: 1, host_nqn: None, dhchap: None };
    assert!(NvmeofDevice::connect(&anon).await.is_err(), "the shared subsystem admits nobody");

    // Day 2: stormcluster turns a plain worker off.
    let off: serde_json::Value = c.delete(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!((off["state"].as_str(), off["from"].as_str()), (Some("off"), Some("persisted")), "{off}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err(), "no longer accepting");
    drop(state);

    // The next start: the default does not turn it back on.
    let (state, base, _) = engine(&dir, None).await;
    forge::restore(&state, Some(d.clone())).await;
    let s: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!((s["state"].as_str(), s["from"].as_str()), (Some("off"), Some("persisted")), "{s}");
    assert!(state.nvmeof_target.read().await.is_none());

    // Told on again, with settings: served now, and at the next start.
    let r = c
        .put(format!("{base}/api/v1/forge"))
        .json(&serde_json::json!({"listen_addr": addr.to_string(), "nqn": "nqn.2026-10.test:again"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let on: serde_json::Value = r.json().await.unwrap();
    assert_eq!((on["state"].as_str(), on["from"].as_str()), (Some("on"), Some("persisted")), "{on}");
    state.nvmeof_target.write().await.take().unwrap().stop_accepting();
    drop(state);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (state, base, _) = engine(&dir, None).await;
    forge::restore(&state, Some(d)).await;
    let s: serde_json::Value = c.get(format!("{base}/api/v1/forge")).send().await.unwrap().json().await.unwrap();
    assert_eq!((s["state"].as_str(), s["from"].as_str(), s["nqn"].as_str()), (Some("on"), Some("persisted"), Some("nqn.2026-10.test:again")), "{s}");
    assert!(listening(addr).await);
}

/// #381 (stormcos#486): forge's trust reaches every node that boots from it.
/// Forge's stormcert sets it; a boot claim hands it out; `boot-claim` writes
/// `/run/stormblock/forge/` with its modes; a node that is its own forge
/// writes its own, on loopback, and never over what a claim wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_booting_from_a_forge_is_handed_its_trust_and_the_first_node_writes_its_own() {
    use std::os::unix::fs::PermissionsExt;
    use stormblock::mgmt::forge_trust;
    const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBforge\n-----END CERTIFICATE-----\n";
    let dir = TempDir::new().unwrap();
    let node_dir = dir.path().join("run/stormblock/forge");
    std::env::set_var("STORMBLOCK_FORGE_TRUST_DIR", &node_dir);
    let (state, base, golden) = engine(&dir, Some(&vec![7u8; MIB as usize])).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "boothost", "name": "server1", "volume": golden.unwrap().0.to_string()}))
        .send().await.unwrap();
    let claim = || async {
        let r = c.post(format!("{base}/api/v1/synonyms/boothost/server1/claim")).json(&serde_json::json!({})).send().await.unwrap();
        assert_eq!(r.status(), 201);
        r.json::<serde_json::Value>().await.unwrap()
    };

    // No trust set: a claim hands out none, and boot-claim leaves no directory.
    let v = claim().await;
    assert!(v["forge_trust"].is_null(), "{v}");
    assert_eq!(forge_trust::from_claim(&node_dir, &base, &v), Ok("none"));
    assert!(!node_dir.exists(), "no forge trust: the directory is absent");

    // Set by forge's stormcert: a malformed one refused.
    let r = c.put(format!("{base}/api/v1/forge/trust")).json(&serde_json::json!({"ca": "nope", "bootstrap_token": "t"})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = c.put(format!("{base}/api/v1/forge/trust"))
        .json(&serde_json::json!({"ca": CA, "bootstrap_token": "abcdef.0123456789abcdef"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let st: serde_json::Value = r.json().await.unwrap();
    assert_eq!((st["set"].as_bool(), st["url"].as_str()), (Some(true), Some("https://127.0.0.1:6443")), "{st}");
    assert!(!st.to_string().contains("abcdef.0123"), "the token is never shown: {st}");
    // Kept 0600.
    let kept = dir.path().join("engine").join(forge_trust::TRUST_FILE);
    assert_eq!(std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o600);
    // Forge mode off: this node is no forge, so it writes no trust of its own.
    assert!(!node_dir.exists());

    // A booting node's claim: handed the trust; boot-claim writes it.
    let v = claim().await;
    assert_eq!(v["forge_trust"]["ca"], CA);
    assert_eq!(v["forge_trust"]["url"], "https://127.0.0.1:6443");
    assert_eq!(v["forge_trust"]["bootstrap_token"], "abcdef.0123456789abcdef");
    let booted = dir.path().join("booted/run/stormblock/forge");
    assert_eq!(forge_trust::from_claim(&booted, &base, &v), Ok("written"));
    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(std::fs::read_to_string(booted.join("ca.crt")).unwrap(), CA);
    assert_eq!(std::fs::read_to_string(booted.join("url")).unwrap(), "https://127.0.0.1:6443\n");
    assert_eq!(std::fs::read_to_string(booted.join("bootstrap.token")).unwrap(), "abcdef.0123456789abcdef\n");
    assert_eq!((mode(booted.join("ca.crt")), mode(booted.join("url")), mode(booted.join("bootstrap.token"))), (0o644, 0o644, 0o600));

    // The first node: forge mode on, its trust set — its own, on loopback.
    let port = free_port();
    let r = c.put(format!("{base}/api/v1/forge")).json(&serde_json::json!({"listen_addr": format!("127.0.0.1:{port}")})).send().await.unwrap();
    assert!(r.status().is_success());
    let r = c.put(format!("{base}/api/v1/forge/trust"))
        .json(&serde_json::json!({"ca": CA, "url": "https://forge.g8.lo:6443", "bootstrap_token": "abcdef.0123456789abcdef"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(forge_trust::read_from(&node_dir).as_deref(), Some("self"));
    assert_eq!(std::fs::read_to_string(node_dir.join("url")).unwrap(), "https://127.0.0.1:6443\n", "its own apiserver, on loopback");
    assert_eq!(mode(node_dir.join("bootstrap.token")), 0o600);
    // Others are told the URL stormcert gave.
    assert_eq!(claim().await["forge_trust"]["url"], "https://forge.g8.lo:6443");

    // What a claim wrote this boot is never overwritten by the node's own.
    forge_trust::write_dir(&node_dir, CA, "https://other-forge:6443", "x.y", "claim http://other-forge:9090").unwrap();
    assert!(!forge_trust::write_self(&state).await);
    assert_eq!(std::fs::read_to_string(node_dir.join("url")).unwrap(), "https://other-forge:6443\n");

    // Forgotten: the claim hands out none again.
    let r = c.delete(format!("{base}/api/v1/forge/trust")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(claim().await["forge_trust"].is_null());
}
