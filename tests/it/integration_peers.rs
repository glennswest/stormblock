//! A volume on another node attached through this node's engine (#328,
//! stormvm#46).
//!
//! Node A holds a sealed golden and serves its management API over TLS, its
//! `tls_client_ca` the node CA, and NVMe/TCP closed to unnamed hosts. Node B
//! has `[fabric] peers = [A]` and a certificate from the node CA. A caller of
//! B's API (stormvm's shape: its own node, by id) clones A's golden and
//! attaches the clone; B finds it on A, has A clone it and serve it over
//! NVMe/TCP to B's host NQN, and connects with its own initiator. The bytes
//! B reads and writes are A's. The ublk half needs a kernel device and is
//! `ci-peer-attach-verify.sh`'s; here, with no ublk, the attach says so and
//! A's namespace is let go of again.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::common;
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::{NvmeofExportConfig, StormBlockConfig};
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::{VolumeId, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const B_NQN: &str = "nqn.2026-09.lo.storm:host-nodeb";

fn openssl(dir: &Path, args: &[&str]) {
    let out = Command::new("openssl").args(args).current_dir(dir).output().expect("openssl runs");
    assert!(out.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn have_openssl() -> bool {
    Command::new("openssl").arg("version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ca(dir: &Path, name: &str) {
    openssl(
        dir,
        &[
            "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
            "-keyout", &format!("{name}.key"), "-out", &format!("{name}.crt"), "-days", "3650",
            "-subj", &format!("/CN={name}"),
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        ],
    );
}

fn leaf(dir: &Path, ca: &str, name: &str, usage: &str, san: &str) {
    openssl(
        dir,
        &[
            "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
            "-keyout", &format!("{name}.key"), "-out", &format!("{name}.csr"), "-subj", &format!("/CN={name}"),
        ],
    );
    let ext = format!("{name}.ext");
    std::fs::write(
        dir.join(&ext),
        format!("basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={usage}\nsubjectAltName={san}\n"),
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509", "-req", "-in", &format!("{name}.csr"), "-CA", &format!("{ca}.crt"), "-CAkey", &format!("{ca}.key"),
            "-CAcreateserial", "-out", &format!("{name}.crt"), "-days", "365", "-extfile", &ext,
        ],
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn engine(dir: &Path, config: StormBlockConfig) -> (Arc<AppState>, RaidArrayId) {
    std::fs::create_dir_all(dir).unwrap();
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let dev = FileDevice::open_with_capacity(dir.join("pool.bin").to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    (Arc::new(AppState::new(config, vm, reg, gem)), array)
}

/// A's NVMe/TCP target, as the daemon starts it.
async fn start_target(state: &Arc<AppState>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: addr,
        nqn: "nqn.2026-09.test:node-a".into(),
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

/// B, with `[fabric]` naming A; its own API over plain HTTP, open (the
/// caller is stormvm on B; how B admits it is not what is tested here).
async fn node_b(dir: &Path, peer: &str, pki: Option<&Path>) -> (Arc<AppState>, String) {
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("nodeb".into());
    c.management.data_dir = Some(dir.display().to_string());
    c.management.require_auth = Some(false);
    c.fabric.peers = vec![peer.to_string()];
    c.fabric.host_nqn = Some(B_NQN.into());
    let p = |f: &str| pki.unwrap().join(f).display().to_string();
    if pki.is_some() {
        c.fabric.client_cert = Some(p("nodeb.crt"));
        c.fabric.client_key = Some(p("nodeb.key"));
    }
    c.fabric.ca = pki.map(|_| p("node-ca.crt")).or_else(|| Some(p_ca(dir)));
    let (b, _) = engine(dir, c).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(b.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(http).await;
    (b, format!("http://{http}/api/v1/volumes"))
}

/// The CA a node with no certificate of its own still checks A's with.
fn p_ca(dir: &Path) -> String {
    dir.parent().unwrap().join("pki").join("node-ca.crt").display().to_string()
}

/// The namespaces A serves to B's host NQN.
async fn served_to_b(a: &AppState) -> usize {
    let h = a.nvme_hosts.lock().await;
    h.subsystems
        .values()
        .filter(|s| s.hosts.iter().any(|e| e.nqn == B_NQN))
        .map(|s| s.namespaces.len())
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_volume_is_cloned_and_attached_through_this_nodes_engine() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = tmp.path().join("pki");
    std::fs::create_dir_all(&pki).unwrap();
    ca(&pki, "node-ca");
    leaf(&pki, "node-ca", "nodea", "serverAuth", "IP:127.0.0.1,DNS:localhost");
    leaf(&pki, "node-ca", "nodeb", "clientAuth", "DNS:nodeb.g16.lo");
    let p = |f: &str| pki.join(f).display().to_string();

    // Node A: a sealed golden; the management API over TLS, closed; NVMe/TCP
    // closed to unnamed hosts (#210).
    let port = free_port();
    let mut ac = StormBlockConfig::default();
    ac.management.node_name = Some("nodea".into());
    ac.management.api_token = Some("node-a-token".into());
    ac.management.listen_addr = format!("127.0.0.1:{port}");
    ac.management.data_dir = Some(tmp.path().join("a").display().to_string());
    ac.management.tls_cert = Some(p("nodea.crt"));
    ac.management.tls_key = Some(p("nodea.key"));
    ac.management.tls_client_ca = Some(p("node-ca.crt"));
    ac.nvmeof = Some(NvmeofExportConfig {
        listen_addr: "127.0.0.1:0".into(),
        nqn: "nqn.2026-09.test:node-a".into(),
        export_drives: false,
        allow_any_host: false,
        allowed_hosts: Vec::new(),
        require_dhchap: false,
        boothost_host_nqn: None,
    });
    ac.validate().expect("A's config validates");
    let (a, array) = engine(&tmp.path().join("a"), ac).await;
    let pattern: Vec<u8> = (0..4 * MIB as usize).map(|i| (i % 253) as u8 + 1).collect();
    let golden = {
        let mut vm = a.volume_manager.lock().await;
        let g = vm.create_volume("fedora-golden", 16 * MIB, array).await.unwrap();
        let dev = vm.get_volume(&g).unwrap();
        dev.write(0, &pattern).await.unwrap();
        dev.flush().await.unwrap();
        vm.seal_volume(g, None).await.unwrap();
        g.0
    };
    start_target(&a).await;
    tokio::spawn(stormblock::mgmt::start_management_server(a.clone()));
    common::wait_for_listener(format!("127.0.0.1:{port}").parse().unwrap()).await;
    let peer = format!("https://127.0.0.1:{port}");

    let (b, api) = node_b(&tmp.path().join("b"), &peer, Some(&pki)).await;
    let c = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();

    // Found on A, by id and by name.
    let (at, v) = stormblock::mgmt::peers::find(&b, &golden.to_string()).await.unwrap().expect("A holds the golden");
    assert_eq!(at, peer);
    assert_eq!(v["id"], golden.to_string());
    assert!(stormblock::mgmt::peers::find(&b, "fedora-golden").await.unwrap().is_some(), "by name too");
    assert!(stormblock::mgmt::peers::find(&b, &Uuid::new_v4().to_string()).await.unwrap().is_none());

    // A clone asked of B is made on A, beside its source.
    let r = c.post(format!("{api}/{golden}/clone")).json(&serde_json::json!({"name": "vm1-root"})).send().await.unwrap();
    let code = r.status().as_u16();
    let v: serde_json::Value = r.json().await.unwrap();
    assert!((200..300).contains(&code), "{code} {v}");
    assert_eq!(v["peer"], peer, "{v}");
    let clone: Uuid = v["id"].as_str().unwrap().parse().unwrap();
    assert!(a.volume_manager.lock().await.get_volume(&VolumeId(clone)).is_some(), "the clone is on A");
    assert!(b.volume_manager.lock().await.get_volume(&VolumeId(clone)).is_none(), "and not on B");

    // B's initiator, connected as B's host NQN to A's per-host subsystem:
    // it reads the golden's bytes, and what it writes is on A.
    let (dev, rec) = stormblock::mgmt::peers::connect(&b, &clone.to_string(), false).await.unwrap().expect("on A");
    assert_eq!(rec.volume, clone);
    assert_eq!(rec.host_nqn, B_NQN);
    assert!(rec.uri.contains("hostnqn=nqn.2026-09.lo.storm:host-nodeb"), "{}", rec.uri);
    assert_eq!(served_to_b(&a).await, 1, "A serves the clone to B's host only");
    let mut back = vec![0u8; pattern.len()];
    dev.read(0, &mut back).await.unwrap();
    assert!(back == pattern, "B reads the golden's bytes through the clone");
    let mine = vec![0x5Au8; MIB as usize];
    dev.write(8 * MIB, &mine).await.unwrap();
    dev.flush().await.unwrap();
    {
        let on_a = a.volume_manager.lock().await.get_volume(&VolumeId(clone)).unwrap();
        let mut got = vec![0u8; MIB as usize];
        on_a.read(8 * MIB, &mut got).await.unwrap();
        assert!(got == mine, "what B wrote is on A");
        let g = a.volume_manager.lock().await.get_volume(&VolumeId(golden)).unwrap();
        g.read(8 * MIB, &mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0), "and not on the golden");
    }
    drop(dev);

    // An engine started where an earlier run left this attach recorded
    // (its ublk device gone with that run) lets go of A's namespace.
    let b3_dir = tmp.path().join("b3");
    std::fs::create_dir_all(&b3_dir).unwrap();
    std::fs::write(b3_dir.join("peer_attaches.json"), serde_json::to_vec(&vec![rec.clone()]).unwrap()).unwrap();
    let (b3, _) = node_b(&b3_dir, &peer, Some(&pki)).await;
    assert_eq!(b3.peer_attaches.lock().await.all().len(), 1, "the record, read at start");
    stormblock::mgmt::peers::start(&b3);
    for _ in 0..100 {
        if served_to_b(&a).await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(served_to_b(&a).await, 0, "released at A by the next start");
    assert!(b3.peer_attaches.lock().await.all().is_empty());
    assert!(!b3_dir.join("peer_attaches.json").exists() || std::fs::read_to_string(b3_dir.join("peer_attaches.json")).unwrap().trim() == "[]");

    // The golden takes no writes: A's refusal is the answer.
    let r = c.post(format!("{api}/{golden}/attach")).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409, "{}", r.text().await.unwrap());

    // Attached through B's API. With ublk this is `device_hint`
    // (ci-peer-attach-verify.sh); without it, said, and A's namespace let go.
    let r = c.post(format!("{api}/{clone}/attach")).json(&serde_json::json!({})).send().await.unwrap();
    let code = r.status().as_u16();
    let v: serde_json::Value = r.json().await.unwrap();
    if code == 200 {
        assert_eq!(v["transport"], "ublk", "{v}");
        assert!(v["device_hint"].as_str().unwrap().starts_with("/dev/ublkb"), "{v}");
        let r = c.delete(format!("{api}/{clone}/attach")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
    } else {
        assert_eq!(code, 409, "{v}");
        assert!(v.to_string().contains("ublk is not available"), "{v}");
    }
    assert_eq!(served_to_b(&a).await, 0, "A no longer serves it to B");
    assert!(b.peer_attaches.lock().await.all().is_empty());

    // A volume no node holds: 404, as before.
    let r = c.post(format!("{api}/{}/attach", Uuid::new_v4())).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);

    // A node without the node CA's certificate is not let in by A: the
    // volume is not found through it.
    let (b2, api2) = node_b(&tmp.path().join("b2"), &peer, None).await;
    assert!(stormblock::mgmt::peers::find(&b2, &golden.to_string()).await.unwrap().is_none());
    let r = c.post(format!("{api2}/{clone}/attach")).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
}
