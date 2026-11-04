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

/// A forge client certificate as stormcert#117 signs it: client auth only,
/// no subjectAltName of any kind, named by its subject (`subj`, e.g.
/// `/CN=stormcentral/O=stormcentral:checks`). Files `<file>.crt`/`.key`.
fn leaf_no_san(dir: &Path, ca: &str, file: &str, subj: &str) {
    openssl(
        dir,
        &[
            "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
            "-keyout", &format!("{file}.key"), "-out", &format!("{file}.csr"), "-subj", subj,
        ],
    );
    let ext = format!("{file}.ext");
    std::fs::write(dir.join(&ext), "basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n").unwrap();
    openssl(
        dir,
        &[
            "x509", "-req", "-in", &format!("{file}.csr"), "-CA", &format!("{ca}.crt"), "-CAkey",
            &format!("{ca}.key"), "-CAcreateserial", "-out", &format!("{file}.crt"), "-days", "365",
            "-extfile", &ext,
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
            admin_subjects: Vec::new(),
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

/// Forge's CA, stormcentral's certificate, another node's, and a revoked
/// stormcentral's, with the CRL that says so (stormcert#61).
fn forge_pki(d: &Path) {
    ca(d, "forge-ca");
    leaf_san(d, "forge-ca", "stormcentral", "clientAuth", 365, "DNS:stormcentral.g8.lo");
    leaf_san(d, "forge-ca", "server7", "clientAuth", 365, "DNS:server7.g8.lo");
    leaf_san(d, "forge-ca", "old-stormcentral", "clientAuth", 365, "DNS:stormcentral.g8.lo");
    // Forge revokes one (stormcert#61): its CRL.
    std::fs::write(d.join("index.txt"), "").unwrap();
    std::fs::write(d.join("crlnumber"), "1000\n").unwrap();
    std::fs::write(
        d.join("ca.cnf"),
        "[ ca ]\ndefault_ca = d\n[ d ]\ndir = .\ndatabase = index.txt\ncrlnumber = crlnumber\n\
         default_md = sha256\ndefault_crl_days = 30\ncertificate = forge-ca.crt\nprivate_key = forge-ca.key\n",
    )
    .unwrap();
    openssl(d, &["ca", "-config", "ca.cnf", "-revoke", "old-stormcentral.crt"]);
    openssl(d, &["ca", "-config", "ca.cnf", "-gencrl", "-out", "forge.crl"]);
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
    forge_pki(&d);

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

/// #453: stormcentral's forge-issued client certificate carries no SAN
/// (stormcert#117: `CN=stormcentral`, `O=stormcentral:checks`, client auth
/// only), so `tls_admin_names` can never match it. Listed by subject CN in
/// `tls_admin_subjects`, it is admin; another forge client (a CN not listed)
/// gets no admin verb; a `system:` subject is never admin even when listed
/// (forge's nodes are `system:node:*`); one forge revoked is nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forge_client_certificate_is_admin_by_its_subject_cn() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let d = pki.dir.clone();
    ca(&d, "forge-ca");
    leaf_no_san(&d, "forge-ca", "stormcentral", "/CN=stormcentral/O=stormcentral:checks");
    leaf_no_san(&d, "forge-ca", "someone", "/CN=someone/O=stormcentral:checks");
    leaf_no_san(&d, "forge-ca", "node7", "/CN=system:node:server7/O=system:nodes");
    leaf_no_san(&d, "forge-ca", "old-stormcentral", "/CN=stormcentral/O=stormcentral:checks");
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
    let cn = stormblock::mgmt::tls::subject_cn(&pem_der(&pki.read("stormcentral.crt"))).expect("one CN");
    assert_eq!(cn, "stormcentral");
    assert_eq!(stormblock::mgmt::tls::subject_cn(&pem_der(&pki.read("node7.crt"))).as_deref(), Some("system:node:server7"));

    let port = free_port();
    let mut cfg = config(&pki, &format!("127.0.0.1:{port}"));
    cfg.management.admin_token = Some("admin-453".into());
    cfg.management.data_dir = Some(tmp.path().join("data").display().to_string());
    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    cfg.management.tls_admin_ca = Some(pki.p("forge-ca.crt").display().to_string());
    cfg.management.tls_admin_crl = Some(pki.p("forge.crl").display().to_string());
    cfg.management.tls_admin_subjects = vec!["stormcentral".into(), "system:node:server7".into()];
    cfg.validate().expect("the config validates");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state));
    wait_listening(port).await;
    let base = format!("https://127.0.0.1:{port}");
    let trust = serde_json::json!({
        "ca": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        "bootstrap_token": "abc.def"
    });

    // stormcentral by subject: an admin verb and the ordinary ones, no token.
    let sc = pki.client(Some("stormcentral"));
    let r = sc.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap();
    assert_eq!(r.status(), 200, "admin by its CN: {}", r.text().await.unwrap_or_default());
    assert_eq!(sc.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 200);

    // Not listed, or a node's `system:` name even when listed: no admin verb.
    for who in ["someone", "node7"] {
        let c = pki.client(Some(who));
        assert_eq!(c.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401, "{who}");
        assert_eq!(c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 401, "{who}");
    }
    // Revoked by forge: nothing, though its CN is listed.
    let old = pki.client(Some("old-stormcentral"));
    assert_eq!(old.put(format!("{base}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401);
    assert_eq!(old.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status(), 401);
}

/// The first certificate of a PEM file, as DER.
fn pem_der(pem: &[u8]) -> Vec<u8> {
    rustls_pemfile::certs(&mut std::io::BufReader::new(pem)).next().unwrap().unwrap().as_ref().to_vec()
}

/// The configured files, each in `live/`, where nothing is yet.
fn config_on_live(pki: &Pki, live: &Path, port: u16, data: &Path) -> StormBlockConfig {
    let at = |f: &str| Some(live.join(f).display().to_string());
    let mut c = config(pki, &format!("127.0.0.1:{port}"));
    c.management.admin_token = Some("admin-483".into());
    c.management.data_dir = Some(data.display().to_string());
    c.management.tls_cert = at("server.crt");
    c.management.tls_key = at("server.key");
    c.management.tls_client_ca = at("node-ca.crt");
    c.management.tls_admin_ca = at("forge-ca.crt");
    c.management.tls_admin_crl = at("forge.crl");
    c.management.tls_admin_subjects = vec!["stormcentral".into()];
    c
}

/// A renamed copy: how a file appears, whole.
fn appear(pki: &Pki, live: &Path, f: &str) {
    let tmp = live.join(format!(".{f}.tmp"));
    std::fs::copy(pki.p(f), &tmp).unwrap();
    std::fs::rename(&tmp, live.join(f)).unwrap();
}

async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("never: {what}");
}

/// A GET's status; a kept connection the server just ended counts as 0.
async fn status(c: &reqwest::Client, url: &str) -> u16 {
    c.get(url).send().await.map_or(0, |r| r.status().as_u16())
}

fn plain_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap()
}

async fn missing_of(r: reqwest::Response) -> (String, Vec<String>) {
    let v: serde_json::Value = r.json().await.unwrap();
    let t = &v["tls"];
    (
        t["serving"].as_str().unwrap_or_default().to_string(),
        t["missing"].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_string()).collect(),
    )
}

/// #483, the node's first boot from its config through the real listener: every
/// file named and none there. The config validates, the API answers plain with
/// its credential checks on, and health names what is missing. Then each file
/// appears and is taken up with no restart: the pair turns the listener to TLS
/// and ends an open plain connection; a missing CRL fails closed; the pair going
/// away never turns it back to plain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tls_file_is_taken_up_as_it_appears_and_a_missing_crl_fails_closed() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    forge_pki(&pki.dir);
    let live = tmp.path().join("live");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let port = free_port();
    let cfg = config_on_live(&pki, &live, port, &data);
    cfg.validate().expect("files not there yet are no reason to refuse the config");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state.clone()));
    wait_listening(port).await;
    let http = format!("http://127.0.0.1:{port}");
    let https = format!("https://127.0.0.1:{port}");
    let asker: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let url_scheme = || stormblock::mgmt::finder::answer_url(&state, asker).unwrap();

    // Nothing there: plain, and the token is still required.
    let plain = plain_client();
    let (serving, missing) = missing_of(plain.get(format!("{http}/api/v1/health")).send().await.unwrap()).await;
    assert_eq!(serving, "http");
    assert_eq!(missing, ["tls_cert", "tls_key", "tls_client_ca", "tls_admin_ca", "tls_admin_crl"]);
    assert_eq!(plain.get(format!("{http}/api/v1/volumes")).send().await.unwrap().status(), 401);
    let r = plain.get(format!("{http}/api/v1/volumes")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(url_scheme().starts_with("http://"), "{}", url_scheme());

    // Half a pair: still plain.
    appear(&pki, &live, "server.crt");
    eventually("the cert is seen", || async {
        let (_, m) = missing_of(plain.get(format!("{http}/api/v1/health")).send().await.unwrap()).await;
        !m.iter().any(|k| k == "tls_cert")
    })
    .await;
    assert_eq!(plain.get(format!("{http}/api/v1/health")).send().await.unwrap().status(), 200);

    // A plain connection held open, as a caller's keep-alive is.
    let mut held = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    held.write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: n1\r\n\r\n").await.unwrap();
    let mut first = [0u8; 16];
    held.read_exact(&mut first).await.unwrap();
    assert!(first.starts_with(b"HTTP/1.1 200"));

    // The whole pair: HTTPS, the held connection ended, plain refused.
    appear(&pki, &live, "server.key");
    let anon = pki.client(None);
    eventually("the listener serves TLS", || async {
        anon.get(format!("{https}/api/v1/health")).send().await.is_ok()
    })
    .await;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), held.read_to_end(&mut rest))
        .await
        .expect("the plain connection ends when the pair appears")
        .unwrap();
    assert!(plain_client().get(format!("{http}/api/v1/health")).send().await.is_err(), "no plain once TLS is up");
    let (serving, missing) = missing_of(anon.get(format!("{https}/api/v1/health")).send().await.unwrap()).await;
    assert_eq!(serving, "https");
    assert_eq!(missing, ["tls_client_ca", "tls_admin_ca", "tls_admin_crl"]);
    assert!(url_scheme().starts_with("https://"), "{}", url_scheme());

    // The node CA: its client certificate becomes a credential, for this very
    // client too, whose kept connection was asked for no certificate.
    let kubelet = pki.client(Some("kubelet"));
    let volumes = format!("{https}/api/v1/volumes");
    assert_eq!(kubelet.get(&volumes).send().await.unwrap().status(), 401);
    appear(&pki, &live, "node-ca.crt");
    eventually("a node-CA certificate is a credential", || async {
        status(&kubelet, &volumes).await == 200
    })
    .await;

    // Forge's CA with no CRL: fails closed, for admin and a forge node alike.
    let trust = serde_json::json!({
        "ca": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        "bootstrap_token": "abc.def"
    });
    appear(&pki, &live, "forge-ca.crt");
    eventually("forge's CA is seen", || async {
        let (_, m) = missing_of(anon.get(format!("{https}/api/v1/health")).send().await.unwrap()).await;
        m == ["tls_admin_crl"]
    })
    .await;
    let manifest = format!("{https}/api/v1/releases/nosuch/manifest");
    let sc = pki.client(Some("stormcentral"));
    let node = pki.client(Some("server7"));
    assert_eq!(sc.get(&volumes).send().await.unwrap().status(), 401, "no CRL: not admin");
    assert_eq!(sc.put(format!("{https}/api/v1/forge/trust")).json(&trust).send().await.unwrap().status(), 401);
    assert_eq!(node.get(&manifest).send().await.unwrap().status(), 401, "no CRL: not a forge node");

    // The CRL: admin by certificate, a forge node's read, the revoked one not.
    appear(&pki, &live, "forge.crl");
    eventually("forge's certificate is admin", || async {
        sc.put(format!("{https}/api/v1/forge/trust"))
            .json(&trust)
            .send()
            .await
            .is_ok_and(|r| r.status() == 200)
    })
    .await;
    eventually("a forge node reads; there is no such release", || async {
        status(&node, &manifest).await == 404
    })
    .await;
    let old = pki.client(Some("old-stormcentral"));
    assert_eq!(old.get(&volumes).send().await.unwrap().status(), 401);
    assert!(missing_of(anon.get(format!("{https}/api/v1/health")).send().await.unwrap()).await.1.is_empty());

    // The CRL gone again: closed again. The pair gone: never plain again.
    std::fs::remove_file(live.join("forge.crl")).unwrap();
    std::fs::remove_file(live.join("server.crt")).unwrap();
    std::fs::remove_file(live.join("server.key")).unwrap();
    eventually("the files going is seen", || async {
        let (s, m) = missing_of(anon.get(format!("{https}/api/v1/health")).send().await.unwrap()).await;
        s == "https" && m == ["tls_cert", "tls_key", "tls_admin_crl"]
    })
    .await;
    eventually("the CRL gone: closed again, on the kept connection too", || async {
        status(&sc, &volumes).await == 401
    })
    .await;
    eventually("the node CA's certificate still works", || async {
        status(&kubelet, &volumes).await == 200
    })
    .await;
    assert!(plain_client().get(format!("{http}/api/v1/health")).send().await.is_err(), "the pair held is still served");
}

/// #483: "not there" is the only leniency. A file that is there and does not
/// parse refuses at start, whichever it is, pair or not; half a pair in the
/// config is still refused; and at run time a broken file is retried, with the
/// listener staying as it was until a good one arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tls_file_that_is_there_and_broken_is_still_an_error() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let live = tmp.path().join("live");
    std::fs::create_dir_all(&live).unwrap();
    let files = TlsFiles {
        cert: live.join("server.crt"),
        key: live.join("server.key"),
        client_ca: Some(live.join("node-ca.crt")),
        admin_ca: Some(live.join("forge-ca.crt")),
        admin_crl: Some(live.join("forge.crl")),
        admin_names: Vec::new(),
        admin_subjects: Vec::new(),
    };
    Reloader::new(files.clone()).expect("nothing there: no error");
    for f in ["server.crt", "server.key", "node-ca.crt", "forge-ca.crt", "forge.crl"] {
        std::fs::write(live.join(f), b"not what it should be").unwrap();
        assert!(Reloader::new(files.clone()).is_err(), "{f} broken, the rest not there");
        std::fs::remove_file(live.join(f)).unwrap();
    }

    let mut c = StormBlockConfig::default();
    c.management.tls_cert = Some("/nonexistent/server.crt".into());
    assert!(c.validate().unwrap_err().to_string().contains("tls_cert requires tls_key"));

    // At run time: a broken pair leaves it plain, a good one turns it to TLS.
    let state = state_with(&tmp, config(&pki, "127.0.0.1:0")).await;
    let reloader = Arc::new(Reloader::with_interval(files, Duration::from_millis(50)).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = stormblock::mgmt::api::router(state);
    tokio::spawn(stormblock::mgmt::serve_tls(listener, router, reloader.clone()));
    wait_listening(port).await;
    let plain = plain_client();
    std::fs::write(live.join("server.crt"), b"half written").unwrap();
    std::fs::write(live.join("server.key"), b"half written").unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!reloader.serving_tls());
    assert_eq!(plain.get(format!("http://127.0.0.1:{port}/api/v1/health")).send().await.unwrap().status(), 200);
    appear(&pki, &live, "server.crt");
    appear(&pki, &live, "server.key");
    let anon = pki.client(None);
    eventually("the good pair is served", || async {
        anon.get(format!("https://127.0.0.1:{port}/api/v1/health")).send().await.is_ok()
    })
    .await;
    assert!(reloader.serving_tls());
}

/// The value of one line of `/metrics`: the plain count of a caller and route.
async fn plain_count(pki: &Pki, base: &str, route: &str) -> u64 {
    let body = pki.client(None).get(format!("{base}/metrics")).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    let want = format!("route=\"{route}\"");
    body.lines()
        .filter(|l| l.starts_with("stormblock_mgmt_plain_requests_total") && l.contains(&want))
        .map(|l| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
        .sum()
}

/// #525: with a pair and `tls_mode = "optional"`, one port answers the same
/// request over both; only the plain one is counted, once per request, by
/// caller and route; its first request from a caller and route is in the audit
/// log once. A plain connection carries no credential but a token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_optional_listener_answers_both_and_counts_only_the_plain_one() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    stormblock::mgmt::metrics::init_metrics();
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let port = free_port();
    let audit = tmp.path().join("audit.log");
    let mut cfg = config(&pki, &format!("127.0.0.1:{port}"));
    cfg.management.tls_mode = Some("optional".into());
    cfg.management.audit_log = Some(audit.display().to_string());
    cfg.validate().expect("the config validates");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state.clone()));
    wait_listening(port).await;
    let tls = format!("https://127.0.0.1:{port}");
    let plain = format!("http://127.0.0.1:{port}");

    // A caller saying nothing keeps nobody else waiting.
    let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // The same request, both ways, the same answer.
    let over_tls = pki.client(None).get(format!("{tls}/api/v1/volumes")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(over_tls.status(), 200);
    let over_tls = over_tls.text().await.unwrap();
    assert_eq!(plain_count(&pki, &tls, "GET /api/v1/volumes").await, 0, "TLS is not counted");

    let client = reqwest::Client::new();
    let over_plain = client.get(format!("{plain}/api/v1/volumes")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(over_plain.status(), 200);
    assert_eq!(over_plain.text().await.unwrap(), over_tls);
    assert_eq!(plain_count(&pki, &tls, "GET /api/v1/volumes").await, 1, "the plain one is");

    // Each request, also on a connection kept; a refused one too (its token
    // would have crossed in the clear all the same).
    let r = client.get(format!("{plain}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 401, "plain changes nothing about who may call");
    assert_eq!(plain_count(&pki, &tls, "GET /api/v1/volumes").await, 2);

    // A body comes through the peek whole.
    let r = client
        .post(format!("{plain}/api/v1/volumes"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({"name": "by-plain", "size": "4M"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "a plain create: {}", r.status());
    assert_eq!(plain_count(&pki, &tls, "POST /api/v1/volumes").await, 1);

    // No id in a route, and the caller by address alone.
    let _ = client.get(format!("{plain}/api/v1/volumes/some-name-525")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(plain_count(&pki, &tls, "GET /api/v1/volumes/…").await, 1);
    let body = pki.client(None).get(format!("{tls}/metrics")).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    assert!(!body.contains("some-name-525"), "an id in a label");
    assert!(
        body.contains(r#"stormblock_mgmt_plain_requests_total{caller="127.0.0.1",route="GET /api/v1/volumes"} 2"#),
        "{body}"
    );

    // One audit line per caller + route, however many requests.
    let mut lines = Vec::new();
    for _ in 0..100 {
        lines = std::fs::read_to_string(&audit).unwrap_or_default().lines().map(str::to_string).collect();
        if lines.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lines.len(), 3, "{lines:?}");
    let first: serde_json::Value = lines.iter().map(|l| serde_json::from_str(l).unwrap()).find(|v: &serde_json::Value| v["method"] == "GET" && v["path"] == "/api/v1/volumes").unwrap();
    assert_eq!(first["who"], "127.0.0.1");
    assert_eq!(first["verb"], "plain-request");

    // A handed-out URL works for a caller who has not moved.
    assert_eq!(state.api_scheme(), "http");
    let health: serde_json::Value = client.get(format!("{plain}/api/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["tls"]["serving"], "https");
    assert_eq!(health["tls"]["mode"], "optional");

    // A certificate still means what it meant, over TLS.
    let r = pki.client(Some("kubelet")).get(format!("{tls}/api/v1/volumes")).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

/// #525 with #483: `optional` with no pair yet is plain; when the pair appears
/// it is both, with no restart. Plain with no pair is counted too: a caller
/// that has moved fails its handshake then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_optional_listener_with_no_pair_yet_is_plain_then_both() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    stormblock::mgmt::metrics::init_metrics();
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let mut files = pki.files();
    files.cert = pki.p("later.crt");
    files.key = pki.p("later.key");
    let mut cfg = config(&pki, "127.0.0.1:0");
    cfg.management.tls_mode = Some("optional".into());
    let state = state_with(&tmp, cfg).await;
    let reloader = Arc::new(Reloader::with_interval(files, Duration::ZERO).unwrap().with_mode(true, None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(stormblock::mgmt::serve_tls(listener, stormblock::mgmt::api::router(state), reloader));
    wait_listening(port).await;
    let tls = format!("https://127.0.0.1:{port}");
    let plain = format!("http://127.0.0.1:{port}");

    let r = reqwest::Client::new().get(format!("{plain}/api/v1/slabs")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200, "no pair yet: plain");
    assert!(pki.client(None).get(format!("{tls}/api/v1/health")).send().await.is_err(), "no pair yet: no TLS");

    std::fs::copy(pki.p("server.key"), pki.p("later.key")).unwrap();
    std::fs::copy(pki.p("server.crt"), pki.p("later.crt")).unwrap();
    let mut up = false;
    for _ in 0..100 {
        if pki.client(None).get(format!("{tls}/api/v1/health")).send().await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the pair was never taken up");

    let r = reqwest::Client::new().get(format!("{plain}/api/v1/slabs")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200, "a pair, and plain still answers");
    assert_eq!(plain_count(&pki, &tls, "GET /api/v1/slabs").await, 2, "both plain requests, either side of the pair");
}

/// #525: `required`, also when it is not written down, refuses plain once there
/// is a pair, and nothing is counted. An unknown mode is refused at validate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_required_listener_refuses_plain_and_counts_nothing() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    stormblock::mgmt::metrics::init_metrics();
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let port = free_port();
    let mut bad = config(&pki, &format!("127.0.0.1:{port}"));
    bad.management.tls_mode = Some("sometimes".into());
    assert!(bad.validate().unwrap_err().to_string().contains("tls_mode"));

    for mode in [None, Some("required")] {
        let port = free_port();
        let mut cfg = config(&pki, &format!("127.0.0.1:{port}"));
        cfg.management.tls_mode = mode.map(Into::into);
        cfg.validate().expect("the config validates");
        let state = state_with(&tmp, cfg).await;
        tokio::spawn(stormblock::mgmt::start_management_server(state.clone()));
        wait_listening(port).await;
        let tls = format!("https://127.0.0.1:{port}");

        let plain = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/v1/arrays")).bearer_auth(TOKEN).send().await;
        assert!(plain.map(|r| !r.status().is_success()).unwrap_or(true), "plaintext answered under {mode:?}");
        assert_eq!(plain_count(&pki, &tls, "GET /api/v1/arrays").await, 0);
        assert_eq!(state.api_scheme(), "https");
        let health: serde_json::Value = pki.client(None).get(format!("{tls}/api/v1/health")).send().await.unwrap().json().await.unwrap();
        assert_eq!(health["tls"]["mode"], "required");
    }
}

/// #524: with TLS on the API, the boot port answers the boot verbs in plain,
/// with no credential, and a full claim works through it; anything else there
/// is 404 whatever is presented; the reply names the management API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_boot_port_answers_the_boot_verbs_beside_a_tls_api() {
    if !have_openssl() {
        eprintln!("SKIP: needs the openssl CLI to make the certificates");
        return;
    }
    stormblock::mgmt::metrics::init_metrics();
    let tmp = TempDir::new().unwrap();
    let pki = Pki::new(&tmp);
    let (api, boot) = (free_port(), free_port());
    let mut cfg = config(&pki, &format!("127.0.0.1:{api}"));
    cfg.management.boot_listen = Some(format!("127.0.0.1:{boot}"));
    cfg.management.admin_token = Some("admin-524".into());
    cfg.validate().expect("the config validates");
    let mut same = cfg.clone();
    same.management.boot_listen = Some(format!("127.0.0.1:{api}"));
    assert!(same.validate().unwrap_err().to_string().contains("boot_listen"), "a port of its own");
    let state = state_with(&tmp, cfg).await;
    tokio::spawn(stormblock::mgmt::start_management_server(state.clone()));
    wait_listening(api).await;
    wait_listening(boot).await;
    let tls = format!("https://127.0.0.1:{api}");
    let plain = format!("http://127.0.0.1:{boot}");
    let c = reqwest::Client::new();
    let admin = pki.client(None);
    let host = format!("{plain}/api/v1/synonyms/boothost/server8");

    // The API is TLS, and this port is not.
    assert!(c.get(format!("http://127.0.0.1:{api}/api/v1/health")).send().await.map(|r| !r.status().is_success()).unwrap_or(true));
    let health: serde_json::Value = c.get(format!("{plain}/api/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["boot_listen"]["listening"], true, "{health}");
    assert_eq!(health["tls"]["serving"], "https");

    // A sealed image, assigned over TLS.
    let r: serde_json::Value = admin.post(format!("{tls}/api/v1/volumes")).bearer_auth(TOKEN)
        .json(&serde_json::json!({"name": "release", "size": "4M"})).send().await.unwrap().json().await.unwrap();
    let id = r["id"].as_str().expect("a volume id").to_string();
    let r = admin.post(format!("{tls}/api/v1/volumes/{id}/seal")).bearer_auth("admin-524").json(&serde_json::json!({"force": true})).send().await.unwrap();
    assert!(r.status().is_success(), "seal: {}", r.status());
    let r = admin.post(format!("{tls}/api/v1/synonyms")).bearer_auth(TOKEN)
        .json(&serde_json::json!({"namespace": "boothost", "name": "server8", "volume": id})).send().await.unwrap();
    assert!(r.status().is_success(), "synonym: {}", r.status());

    // Everything a machine asks, with nothing presented.
    let r = c.get(format!("{host}/intent")).send().await.unwrap();
    assert_eq!(r.status(), 200, "intent");
    let r = c.post(format!("{host}/claim")).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status(), 201, "claim");
    let claim: serde_json::Value = r.json().await.unwrap();
    let clone = claim["volume"]["id"].as_str().expect("a clone").to_string();
    assert_ne!(clone, id, "a clone, not the image");
    assert!(claim["host_secret"].is_string(), "{claim}");
    // The secret's verbs are not here: the reply says where they are.
    assert_eq!(claim["management_url"], format!("https://127.0.0.1:{api}"));

    let r = c.post(format!("{plain}/api/v1/synonyms/boothost/default/claim"))
        .json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status(), 400, "the default claim is reached, and wants a MAC");
    let r = c.get(format!("{host}/command")).send().await.unwrap();
    assert!(r.status().is_success(), "command: {}", r.status());
    let not_filtered = |b: &str| !b.contains("not on the boot port");
    let r = c.post(format!("{host}/command/done")).json(&serde_json::json!({"id": 1, "result": "ok"})).send().await.unwrap();
    assert!(not_filtered(&r.text().await.unwrap()), "command/done is on the list");
    let r = c.post(format!("{host}/installed")).json(&serde_json::json!({"volume": clone})).send().await.unwrap();
    assert!(not_filtered(&r.text().await.unwrap()), "installed is on the list");

    // Anything else: 404, the token making no difference.
    for path in ["/api/v1/volumes", "/metrics", "/api/v1/version", "/debug/stalls", "/api/v1/synonyms/boothost/server8", "/nothing"] {
        for token in [None, Some(TOKEN)] {
            let mut q = c.get(format!("{plain}{path}"));
            if let Some(t) = token {
                q = q.bearer_auth(t);
            }
            let r = q.send().await.unwrap();
            assert_eq!(r.status(), 404, "{path} with {token:?}");
            assert!(!not_filtered(&r.text().await.unwrap()), "{path}");
        }
    }
    let r = c.put(format!("{host}/intent")).bearer_auth(TOKEN).json(&serde_json::json!({"intent": "install"})).send().await.unwrap();
    assert_eq!(r.status(), 404, "setting an intent is not a machine's");
    let r = c.delete(format!("{plain}/api/v1/volumes/{clone}")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 404, "a delete");
    let r = admin.get(format!("{tls}/api/v1/volumes/{clone}")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200, "and the clone is still there");

    // This port is not who is left on the API (#525).
    let body = admin.get(format!("{tls}/metrics")).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    assert!(!body.contains("/api/v1/synonyms/…\"} "), "the boot port counted as plain on the API: {body}");

    // The finder sends a machine to this port.
    let url = stormblock::mgmt::finder::answer_url(&state, "127.0.0.1:1".parse().unwrap());
    assert_eq!(url, Some(format!("http://127.0.0.1:{boot}")));
}
