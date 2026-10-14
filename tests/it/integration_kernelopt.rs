//! An optional kernel image pulled from forge on day 2 (#383).
//!
//! A stand-in forge: an engine serving its management API over TLS, with
//! forge's CA as `tls_admin_ca`, a sealed `golden-gpu-…` volume and a
//! published release whose manifest lists it as `optional` for one kernel.
//! A node: an engine with `[kernel]` naming that forge and holding a
//! certificate forge's CA issued it (what stormcert enrolment leaves). The
//! certificates are made with the `openssl` CLI, as in integration_mgmt_tls.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;

const KERNEL: &str = "6.17.1-300.fc43.x86_64";
const GOLDEN: &str = "golden-gpu-0123456789ab";
const MIB: usize = 1 << 20;

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

async fn engine(dir: &Path, sub: &str, config: StormBlockConfig) -> Arc<AppState> {
    let tmp = dir.join(sub);
    std::fs::create_dir_all(&tmp).unwrap();
    let mut devices = Vec::new();
    for i in 0..2 {
        let p = tmp.join(format!("d{i}.img"));
        devices.push(Arc::new(
            stormblock::drive::filedev::FileDevice::open_with_capacity(p.to_str().unwrap(), 64 * MIB as u64).await.unwrap(),
        ) as Arc<dyn BlockDevice>);
    }
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let mut vm = VolumeManager::new(4096);
    vm.add_backing_device(array_id, Arc::new(array) as Arc<dyn BlockDevice>).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    Arc::new(AppState::new(config, vm, reg, gem))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_listening(port: u16) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing listening on {port}");
}

fn content() -> Vec<u8> {
    // 6 MiB: a pattern, a zero MiB in the middle (a sparse image), a pattern.
    let mut v: Vec<u8> = (0..6 * MIB).map(|i| (i % 251) as u8 + 1).collect();
    v[2 * MIB..3 * MIB].fill(0);
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_pulls_its_kernels_gpu_image_from_forge_with_its_enrolled_certificate() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki: PathBuf = tmp.path().join("pki");
    std::fs::create_dir_all(&pki).unwrap();
    ca(&pki, "forge-ca");
    ca(&pki, "node-ca");
    leaf(&pki, "forge-ca", "forge", "serverAuth", "IP:127.0.0.1,DNS:localhost");
    leaf(&pki, "forge-ca", "server8", "clientAuth", "DNS:server8.g16.lo");
    let p = |f: &str| pki.join(f).display().to_string();

    // The stand-in forge.
    let port = free_port();
    let forge_data = tmp.path().join("forge-data");
    std::fs::create_dir_all(&forge_data).unwrap();
    let mut fc = StormBlockConfig::default();
    fc.management.node_name = Some("forge".into());
    fc.management.api_token = Some("forge-token".into());
    fc.management.listen_addr = format!("127.0.0.1:{port}");
    fc.management.data_dir = Some(forge_data.display().to_string());
    fc.management.tls_cert = Some(p("forge.crt"));
    fc.management.tls_key = Some(p("forge.key"));
    fc.management.tls_client_ca = Some(p("node-ca.crt"));
    fc.management.tls_admin_ca = Some(p("forge-ca.crt"));
    fc.validate().expect("forge's config validates");
    let forge = engine(tmp.path(), "forge", fc).await;
    let data = content();
    let digest = {
        let mut vm = forge.volume_manager.lock().await;
        let id = vm.create_volume_any(GOLDEN, data.len() as u64).await.unwrap();
        let dev = vm.get_volume(&id).unwrap();
        dev.write(0, &data).await.unwrap();
        dev.flush().await.unwrap();
        vm.seal_volume(id, None).await.unwrap();
        let open = vm.create_volume_any("scratch", 4 * MIB as u64).await.unwrap();
        let _ = open;
        drop(vm);
        stormblock::mgmt::goldens::digest(dev).await.unwrap()
    };
    // Release 12.30, whose manifest lists the kernel set's gpu as optional.
    std::fs::write(
        forge_data.join("releases.json"),
        serde_json::to_vec(&serde_json::json!([{
            "version": "12.30", "volume": "scratch", "volume_id": uuid::Uuid::new_v4(), "created_unix": 1,
            "manifest": [
                {"kind": "golden", "name": "golden-modules-aa", "digest": "x"},
                {"kind": "golden", "name": GOLDEN, "digest": format!("sha256:{digest}"),
                 "size_bytes": data.len(), "optional": true, "image": "gpu", "kernel": "6.17.1-300.fc43"},
                {"kind": "golden", "name": "golden-gpu-other", "digest": "y", "optional": true,
                 "image": "gpu", "kernel": "6.16.9-200.fc43"}
            ]
        }]))
        .unwrap(),
    )
    .unwrap();
    tokio::spawn(stormblock::mgmt::start_management_server(forge.clone()));
    wait_listening(port).await;
    let forge_url = format!("https://127.0.0.1:{port}");

    // What forge lets the enrolled certificate do: read sealed content and
    // manifests, nothing else.
    let as_node = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(p("forge-ca.crt")).unwrap()).unwrap())
        .identity(
            reqwest::Identity::from_pem(&[std::fs::read(p("server8.crt")).unwrap(), std::fs::read(p("server8.key")).unwrap()].concat())
                .unwrap(),
        )
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let r = as_node.get(format!("{forge_url}/api/v1/volumes/{GOLDEN}/content")).header("Range", "bytes=0-9").send().await.unwrap();
    assert_eq!(r.status(), 206, "a sealed golden's content, by Range");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[..10]);
    assert_eq!(as_node.get(format!("{forge_url}/api/v1/releases/12.30/manifest")).send().await.unwrap().status(), 200);
    assert_eq!(as_node.get(format!("{forge_url}/api/v1/volumes/scratch/content")).send().await.unwrap().status(), 409, "unsealed: no");
    assert_eq!(as_node.get(format!("{forge_url}/api/v1/volumes")).send().await.unwrap().status(), 401, "nothing else");
    assert_eq!(as_node.get(format!("{forge_url}/api/v1/releases")).send().await.unwrap().status(), 401);
    assert_eq!(
        as_node.delete(format!("{forge_url}/api/v1/volumes/scratch")).send().await.unwrap().status(),
        401,
        "never a write"
    );

    // The node.
    let node_data = tmp.path().join("node-data");
    std::fs::create_dir_all(&node_data).unwrap();
    let mut nc = StormBlockConfig::default();
    nc.management.data_dir = Some(node_data.display().to_string());
    nc.kernel.forge_url = Some(forge_url.clone());
    nc.kernel.client_cert = Some(p("server8.crt"));
    nc.kernel.client_key = Some(p("server8.key"));
    nc.kernel.forge_ca = Some(p("forge-ca.crt"));
    let node = engine(tmp.path(), "node", nc).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/api/v1/kernel/optional", listener.local_addr().unwrap());
    let router = stormblock::mgmt::api::router(node.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let c = reqwest::Client::new();
    let ask = |body: serde_json::Value| {
        let c = c.clone();
        let api = api.clone();
        async move {
            let r = c.post(&api).json(&body).send().await.unwrap();
            let code = r.status().as_u16();
            (code, r.json::<serde_json::Value>().await.unwrap())
        }
    };

    // Pulled and cloned.
    let (code, v) = ask(serde_json::json!({"images": ["gpu"], "release": "12.30", "kernel": KERNEL})).await;
    assert_eq!(code, 202, "{v}");
    assert_eq!(v["images"][0]["state"], "pulling", "{v}");
    assert_eq!(v["images"][0]["golden"], GOLDEN, "the gpu for this kernel, not the other");
    let mut last = serde_json::Value::Null;
    for _ in 0..300 {
        last = c.get(format!("{api}/gpu")).send().await.unwrap().json().await.unwrap();
        if last["state"] == "ready" || last["state"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(last["state"], "ready", "{last}");
    {
        let vm = node.volume_manager.lock().await;
        let g = vm.find_volume(GOLDEN).await.expect("the golden, pulled");
        assert!(vm.is_sealed(&g), "sealed");
        let clone = vm.find_volume("gpu").await.expect("its clone, as the image's name");
        let dev = vm.get_volume(&clone).unwrap();
        drop(vm);
        let mut back = vec![0u8; data.len()];
        dev.read(0, &mut back).await.unwrap();
        assert!(back == data, "the clone reads what forge holds");
    }
    let listed: serde_json::Value = c.get(&api).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["installed"][0]["golden"], GOLDEN, "{listed}");
    assert_eq!(listed["installed"][0]["kernel"], KERNEL);

    // Asked again: nothing to do.
    let before = node.volume_manager.lock().await.volume_ids().len();
    let (code, v) = ask(serde_json::json!({"images": ["gpu"], "release": "12.30", "kernel": KERNEL})).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["images"][0]["state"], "present");
    assert_eq!(node.volume_manager.lock().await.volume_ids().len(), before, "a no-op");

    // No such image for the kernel asked: said, nothing made.
    let (code, v) = ask(serde_json::json!({"images": ["misc"], "release": "12.30", "kernel": KERNEL})).await;
    assert_eq!(code, 422, "{v}");
    assert!(v["images"][0]["error"].as_str().unwrap().contains("no optional misc"), "{v}");

    // No forge reachable: refused, nothing made.
    let (code, v) = ask(serde_json::json!({
        "images": ["gpu"], "release": "12.30", "kernel": "6.16.9-200.fc43",
        "forge": format!("https://127.0.0.1:{}", free_port())
    }))
    .await;
    assert_eq!(code, 502, "{v}");
    assert!(v["error"].as_str().unwrap().contains("not reachable"), "{v}");
    assert_eq!(node.volume_manager.lock().await.volume_ids().len(), before, "nothing half-made");
}
