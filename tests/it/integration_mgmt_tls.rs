//! The management API over TLS, with a node-CA client certificate as a
//! credential (#203).
//!
//! Real sockets and real handshakes: the engine's own listener
//! (`start_management_server`, `serve_tls`) and reqwest as the caller. The
//! certificates are made here with the `openssl` CLI, as stormcert would make
//! them: a node CA, the node's serving pair (127.0.0.1), a client pair from
//! the node CA, and a client pair from a CA the node does not know.

use crate::common;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::tls::{Reloader, TlsFiles};
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;

const TOKEN: &str = "node-token-203";

fn openssl(dir: &Path, args: &[&str]) {
    let out = Command::new("openssl").args(args).current_dir(dir).output().expect("openssl runs");
    assert!(out.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn have_openssl() -> bool {
    Command::new("openssl").arg("version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A CA (`<name>.crt`, `<name>.key`).
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

/// A leaf `<name>.crt`/`<name>.key` signed by `ca`, for `usage`
/// (serverAuth|clientAuth), with `days` of validity.
fn leaf(dir: &Path, ca: &str, name: &str, usage: &str, days: u32) {
    leaf_san(dir, ca, name, usage, days, "IP:127.0.0.1,DNS:localhost")
}

fn leaf_san(dir: &Path, ca: &str, name: &str, usage: &str, days: u32, san: &str) {
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
        format!(
            "basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={usage}\nsubjectAltName={san}\n"
        ),
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509", "-req", "-in", &format!("{name}.csr"), "-CA", &format!("{ca}.crt"), "-CAkey",
            &format!("{ca}.key"), "-CAcreateserial", "-out", &format!("{name}.crt"), "-days",
            &days.to_string(), "-extfile", &ext,
        ],
    );
}

struct Pki {
    dir: PathBuf,
}

impl Pki {
    fn new(tmp: &TempDir) -> Self {
        let dir = tmp.path().join("pki");
        std::fs::create_dir_all(&dir).unwrap();
        ca(&dir, "node-ca");
        ca(&dir, "rogue-ca");
        leaf(&dir, "node-ca", "server", "serverAuth", 365);
        leaf(&dir, "node-ca", "kubelet", "clientAuth", 365);
        leaf(&dir, "rogue-ca", "intruder", "clientAuth", 365);
        Pki { dir }
    }
    fn p(&self, f: &str) -> PathBuf {
        self.dir.join(f)
    }
    fn read(&self, f: &str) -> Vec<u8> {
        std::fs::read(self.p(f)).unwrap()
    }
    fn files(&self) -> TlsFiles {
        TlsFiles {
            cert: self.p("server.crt"),
            key: self.p("server.key"),
            client_ca: Some(self.p("node-ca.crt")),
            admin_ca: None,
            admin_crl: None,
            admin_names: Vec::new(),
        }
    }
    /// A caller trusting the node CA, presenting `identity` (a leaf name).
    fn client(&self, identity: Option<&str>) -> reqwest::Client {
        let mut b = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(&self.read("node-ca.crt")).unwrap())
            .tls_info(true)
            .timeout(Duration::from_secs(10));
        if let Some(name) = identity {
            let mut pem = self.read(&format!("{name}.crt"));
            pem.extend(self.read(&format!("{name}.key")));
            b = b.identity(reqwest::Identity::from_pem(&pem).unwrap());
        }
        b.build().unwrap()
    }
}

async fn state_with(dir: &TempDir, config: StormBlockConfig) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 16 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(4096);
    vm.add_backing_device(array_id, backing).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    Arc::new(AppState::new(config, vm, reg, gem))
}

fn config(pki: &Pki, listen: &str) -> StormBlockConfig {
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("n1".into());
    c.management.api_token = Some(TOKEN.into());
    c.management.listen_addr = listen.into();
    c.management.tls_cert = Some(pki.p("server.crt").display().to_string());
    c.management.tls_key = Some(pki.p("server.key").display().to_string());
    c.management.tls_client_ca = Some(pki.p("node-ca.crt").display().to_string());
    c
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

/// The node's own listener, from its config: HTTPS with the stormcert pair;
/// a node-CA client certificate is a credential at the node token's tier;
/// one from another CA is refused at the handshake; the token still works
/// without a certificate; health answers anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_ca_client_certificate_is_a_credential_and_no_other_is() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let port = free_port();
    let cfg = config(&pki, &format!("127.0.0.1:{port}"));
    cfg.validate().expect("the config validates");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state));
    wait_listening(port).await;
    let base = format!("https://127.0.0.1:{port}");

    // Plain HTTP on the port is not an API.
    let plain = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/v1/health")).send().await;
    assert!(plain.map(|r| !r.status().is_success()).unwrap_or(true), "plaintext answered");

    // Health: anyone with the node CA, no credential.
    let anon = pki.client(None);
    let r = anon.get(format!("{base}/api/v1/health")).send().await.unwrap();
    assert_eq!(r.status(), 200, "health is public");

    // No certificate, no token: refused. The token: allowed.
    let r = anon.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = anon.get(format!("{base}/api/v1/volumes")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200, "the token still works over TLS");

    // A node-CA client certificate and no token: the ordinary verbs.
    let kubelet = pki.client(Some("kubelet"));
    let r = kubelet.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 200, "a node-CA certificate reads");
    let r = kubelet
        .post(format!("{base}/api/v1/volumes"))
        .json(&serde_json::json!({"name": "by-cert", "size": "4M"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "a node-CA certificate creates a volume: {}", r.status());

    // …but not a destructive one: that is the admin token's (#274).
    let r = kubelet
        .delete(format!("{base}/api/v1/slabs/{}", uuid::Uuid::new_v4()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a certificate is not the admin token");
    let body = r.text().await.unwrap();
    assert!(body.contains("admin token required"), "{body}");

    // A certificate from a CA the node does not know: no connection at all.
    let intruder = pki.client(Some("intruder"));
    let e = intruder.get(format!("{base}/api/v1/volumes")).send().await;
    assert!(e.is_err(), "another CA's certificate got an answer: {:?}", e.map(|r| r.status()));
}

/// stormcert renews the pair: a renewed one is served to the next
/// connection with no restart, and a broken one leaves the old one served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_pair_is_served_without_a_restart() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let state = state_with(&tmp, config(&pki, "127.0.0.1:0")).await;
    let reloader = Arc::new(Reloader::with_interval(pki.files(), Duration::ZERO).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = stormblock::mgmt::api::router(state);
    tokio::spawn(stormblock::mgmt::serve_tls(listener, router, reloader));
    wait_listening(port).await;
    let base = format!("https://127.0.0.1:{port}");

    let served = |client: reqwest::Client| {
        let base = base.clone();
        async move {
            let r = client.get(format!("{base}/api/v1/health")).send().await.unwrap();
            assert_eq!(r.status(), 200);
            r.extensions()
                .get::<reqwest::tls::TlsInfo>()
                .and_then(|i| i.peer_certificate().map(<[u8]>::to_vec))
                .expect("the server's certificate")
        }
    };
    let der = |pem: &[u8]| -> Vec<u8> {
        rustls_pemfile::certs(&mut &pem[..]).next().unwrap().unwrap().as_ref().to_vec()
    };

    let first = served(pki.client(None)).await;
    assert_eq!(first, der(&pki.read("server.crt")));

    // Renewed in place, as stormcert does.
    leaf(&pki.dir, "node-ca", "server", "serverAuth", 30);
    let renewed = der(&pki.read("server.crt"));
    assert_ne!(renewed, first);
    let now = served(pki.client(None)).await;
    assert_eq!(now, renewed, "the renewed pair is served without a restart");

    // And the client CA with it: a client certificate still verifies.
    let r = pki.client(Some("kubelet")).get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // A half-written renewal does not take the API down.
    std::fs::write(pki.p("server.crt"), b"-----BEGIN CERTIFICATE-----\ngarbage\n").unwrap();
    let still = served(pki.client(None)).await;
    assert_eq!(still, renewed, "the previous pair is kept while the new one does not load");
}

/// A client CA without a serving pair is a configuration error, said at
/// startup rather than served as plain HTTP.
#[test]
fn a_client_ca_needs_tls() {
    let mut c = StormBlockConfig::default();
    c.management.tls_client_ca = Some("/nonexistent/ca.crt".into());
    let e = c.validate().unwrap_err().to_string();
    assert!(e.contains("tls_client_ca needs tls_cert"), "{e}");
}

/// #379 (stormcentral#416): a client certificate forge's CA issued to a
/// listed admin identity is the admin token's tier, with nothing handed over
/// by hand; one forge issued to another node is no credential here at all;
/// one forge revoked (its CRL) is refused; a node-CA certificate is still not
/// admin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forge_issued_certificate_for_a_listed_identity_is_admin_and_revocable() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let d = pki.dir.clone();
    ca(&d, "forge-ca");
    leaf_san(&d, "forge-ca", "stormcentral", "clientAuth", 365, "DNS:stormcentral.g8.lo");
    leaf_san(&d, "forge-ca", "server7", "clientAuth", 365, "DNS:server7.g8.lo");
    leaf_san(&d, "forge-ca", "old-stormcentral", "clientAuth", 365, "DNS:stormcentral.g8.lo");
    // Forge revokes one (stormcert#61): its CRL.
    std::fs::write(d.join("index.txt"), "").unwrap();
    std::fs::write(d.join("crlnumber"), "1000\n").unwrap();
    std::fs::write(
        d.join("ca.cnf"),
        "[ ca ]\ndefault_ca = d\n[ d ]\ndir = .\ndatabase = index.txt\ncrlnumber = crlnumber\n\
         default_md = sha256\ndefault_crl_days = 30\ncertificate = forge-ca.crt\nprivate_key = forge-ca.key\n",
    )
    .unwrap();
    openssl(&d, &["ca", "-config", "ca.cnf", "-revoke", "old-stormcentral.crt"]);
    openssl(&d, &["ca", "-config", "ca.cnf", "-gencrl", "-out", "forge.crl"]);

    let port = free_port();
    let mut cfg = config(&pki, &format!("127.0.0.1:{port}"));
    cfg.management.admin_token = Some("admin-379".into());
    cfg.management.data_dir = Some(tmp.path().join("data").display().to_string());
    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    cfg.management.tls_admin_ca = Some(pki.p("forge-ca.crt").display().to_string());
    cfg.management.tls_admin_crl = Some(pki.p("forge.crl").display().to_string());
    cfg.management.tls_admin_names = vec!["stormcentral.g8.lo".into()];
    cfg.validate().expect("the config validates");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state));
    wait_listening(port).await;
    let base = format!("https://127.0.0.1:{port}");
    let trust = serde_json::json!({
        "ca": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        "bootstrap_token": "abc.def"
    });

    // stormcentral, by its forge-issued certificate: admin, no token.
    let sc = pki.client(Some("stormcentral"));
    let r = sc.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap();
    assert_eq!(r.status(), 200, "an admin verb by forge's certificate: {}", r.text().await.unwrap_or_default());
    let r = sc.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 200, "and the ordinary ones");

    // Another node forge enrolled: no credential here at all.
    let other = pki.client(Some("server7"));
    assert_eq!(other.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 401);
    assert_eq!(other.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401);

    // Revoked by forge: refused, though it names stormcentral.
    let old = pki.client(Some("old-stormcentral"));
    assert_eq!(old.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401);
    assert_eq!(old.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 401);

    // A node-CA certificate: ordinary verbs, never admin.
    let kubelet = pki.client(Some("kubelet"));
    assert_eq!(kubelet.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 200);
    assert_eq!(kubelet.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401);
}
