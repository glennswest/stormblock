//! Destructive verbs need the admin token or a storage-admin
//! SubjectAccessReview; the node token keeps the ordinary ones; every
//! destructive call is audited (#274, owner's B, stormcos#250).

use std::sync::{Arc, Mutex};

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use stormblock::mgmt::config::{KubeAuthConfig, StormBlockConfig};
use stormblock::mgmt::AppState;
use stormblock::volume::VolumeManager;
use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1 << 20;
const NODE: &str = "node-token";
const ADMIN: &str = "admin-token";

/// A stand-in apiserver: `alice` (group storage-admins) may do anything to
/// storage.storm.io; `bob` may not; any other token is not a token. It keeps
/// what it was asked.
async fn apiserver(asked: Arc<Mutex<Vec<Value>>>) -> String {
    let a1 = asked.clone();
    let a2 = asked.clone();
    let app = Router::new()
        .route(
            "/apis/authentication.k8s.io/v1/tokenreviews",
            post(move |Json(b): Json<Value>| {
                let a = a1.clone();
                async move {
                    a.lock().unwrap().push(b.clone());
                    let user = match b["spec"]["token"].as_str() {
                        Some("alice-k8s") => Some(("alice", vec!["storage-admins", "system:authenticated"])),
                        Some("bob-k8s") => Some(("bob", vec!["system:authenticated"])),
                        Some("stormcert-k8s") => Some(("system:serviceaccount:stormcert:stormcert", vec!["system:serviceaccounts"])),
                        _ => None,
                    };
                    Json(match user {
                        Some((u, g)) => json!({ "status": { "authenticated": true, "user": { "username": u, "uid": u, "groups": g } } }),
                        None => json!({ "status": { "authenticated": false, "error": "invalid bearer token" } }),
                    })
                }
            }),
        )
        .route(
            "/apis/authorization.k8s.io/v1/subjectaccessreviews",
            post(move |Json(b): Json<Value>| {
                let a = a2.clone();
                async move {
                    a.lock().unwrap().push(b.clone());
                    let spec = &b["spec"];
                    let ra = &spec["resourceAttributes"];
                    // stormcert: get boothost, resourceNames [server1] only (#216).
                    let stormcert = spec["user"] == "system:serviceaccount:stormcert:stormcert"
                        && ra["resource"] == "boothost"
                        && ra["verb"] == "get"
                        && ra["name"] == "server1";
                    let ok = (spec["groups"].as_array().is_some_and(|g| g.iter().any(|x| x == "storage-admins")) || stormcert)
                        && ra["group"] == "storage.storm.io";
                    Json(json!({ "status": { "allowed": ok, "reason": if ok { "storage-admin" } else { "no RBAC policy matched" } } }))
                }
            }),
        );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

struct Node {
    base: String,
    state: Arc<AppState>,
    dir: TempDir,
}

async fn node(audit_only: bool, kube: Option<String>) -> Node {
    let dir = TempDir::new().unwrap();
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.management.api_token = Some(NODE.into());
    config.management.admin_token = Some(ADMIN.into());
    config.management.kubernetes = kube.map(|u| KubeAuthConfig { api_url: u, ..Default::default() });
    let mut vm = VolumeManager::new(MIB);
    let dev = stormblock::drive::filedev::FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 64 * MIB)
        .await
        .unwrap();
    vm.add_backing_device(stormblock::raid::RaidArrayId(uuid::Uuid::new_v4()), Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    state.set_auth(stormblock::serve::api::AuthConfig {
        api_token: Some(NODE.into()),
        admin_token: Some(ADMIN.into()),
        audit_only,
    });
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Node { base: format!("http://{addr}/api/v1"), state, dir }
}

async fn call(n: &Node, m: reqwest::Method, path: &str, tok: Option<&str>, body: Option<Value>) -> u16 {
    let mut r = reqwest::Client::new().request(m, format!("{}{path}", n.base));
    if let Some(t) = tok {
        r = r.bearer_auth(t);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    r.send().await.unwrap().status().as_u16()
}

async fn volume(n: &Node, name: &str, sealed: bool) -> String {
    let mut vm = n.state.volume_manager.lock().await;
    let id = vm.create_volume_any(name, 4 * MIB).await.unwrap();
    if sealed {
        vm.seal_volume(id, None).await.unwrap();
    }
    id.0.to_string()
}

fn audit(n: &Node) -> Vec<Value> {
    std::fs::read_to_string(n.dir.path().join("audit.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The node token: the ordinary verbs, an unsealed volume's delete among
/// them; never a slab, an array, forge, or a sealed golden.
#[tokio::test]
async fn the_node_token_keeps_the_ordinary_verbs_and_only_those() {
    use reqwest::Method as M;
    let n = node(false, None).await;
    let plain = volume(&n, "plain", false).await;
    let golden = volume(&n, "golden", true).await;

    assert_eq!(call(&n, M::GET, "/volumes", Some(NODE), None).await, 200);
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{plain}"), Some(NODE), None).await, 204, "an unsealed volume is the node token's");
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{golden}"), Some(NODE), None).await, 401, "a sealed golden is not");
    let id = uuid::Uuid::new_v4();
    assert_ne!(call(&n, M::DELETE, &format!("/volumes/{id}/attach"), Some(NODE), None).await, 401, "a detach is ordinary");
    assert_ne!(call(&n, M::DELETE, &format!("/exports/{id}"), Some(NODE), None).await, 401, "an export withdrawn is ordinary");
    for (m, p, b) in [
        (M::POST, "/slabs".to_string(), Some(json!({ "device_path": "/nonexistent" }))),
        (M::DELETE, format!("/slabs/{id}"), None),
        (M::POST, "/arrays".to_string(), Some(json!({}))),
        (M::POST, format!("/arrays/{id}/members/0/fail"), None),
        (M::PUT, "/forge".to_string(), Some(json!({}))),
        (M::DELETE, "/forge".to_string(), None),
        (M::POST, "/spares".to_string(), Some(json!({}))),
        (M::POST, "/pallets/gpt".to_string(), Some(json!({}))),
        (M::POST, format!("/volumes/{golden}/seal"), None),
    ] {
        assert_eq!(call(&n, m.clone(), &p, Some(NODE), b.clone()).await, 401, "{m} {p} with the node token");
        assert_ne!(call(&n, m.clone(), &p, Some(ADMIN), b).await, 401, "{m} {p} with the admin token");
    }
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{golden}"), Some(ADMIN), None).await, 204);

    // Every destructive call is in the audit log, refusals too.
    let log = audit(&n);
    assert!(log.iter().any(|r| r["who"] == "node-token" && r["decision"] == "refused" && r["path"] == format!("/api/v1/volumes/{golden}")));
    let del = log.iter().find(|r| r["who"] == "admin-token" && r["path"] == format!("/api/v1/volumes/{golden}")).unwrap();
    assert_eq!((del["decision"].as_str(), del["status"].as_u64(), del["resource"].as_str(), del["verb"].as_str()), (Some("allowed"), Some(204), Some("volumes"), Some("delete")));
    assert_eq!(del["target"], golden.as_str());
    assert!(!log.iter().any(|r| r["path"] == format!("/api/v1/volumes/{plain}")), "an ordinary delete is not audited");
}

/// `admin_gate = audit`: the node token gets through, and the log says each
/// call enforce would refuse.
#[tokio::test]
async fn audit_mode_lets_the_node_token_through_and_says_so() {
    let n = node(true, None).await;
    let golden = volume(&n, "g", true).await;
    assert_eq!(call(&n, reqwest::Method::DELETE, &format!("/volumes/{golden}"), Some(NODE), None).await, 204);
    let log = audit(&n);
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0]["decision"], "allowed-audit-only");
    assert_eq!(log[0]["who"], "node-token");
}

/// A Kubernetes bearer: TokenReview names the user, a SubjectAccessReview
/// for storage.storm.io decides. alice (storage-admins) may; bob may not
/// (403); a bad token is 401; with no apiserver configured, 401.
#[tokio::test]
async fn a_kubernetes_bearer_is_reviewed_and_named_in_the_audit_log() {
    use reqwest::Method as M;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;
    let g1 = volume(&n, "g1", true).await;
    let g2 = volume(&n, "g2", true).await;

    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{g1}"), Some("bob-k8s"), None).await, 403);
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{g1}"), Some("nobody"), None).await, 401);
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{g1}"), Some("alice-k8s"), None).await, 204);
    // A bearer is no ordinary credential: reads stay the node token's.
    assert_eq!(call(&n, M::GET, "/volumes", Some("alice-k8s"), None).await, 401);

    let sar: Vec<Value> = asked.lock().unwrap().iter().filter(|b| b["kind"] == "SubjectAccessReview").cloned().collect();
    let attrs = &sar.last().unwrap()["spec"]["resourceAttributes"];
    assert_eq!((attrs["group"].as_str(), attrs["resource"].as_str(), attrs["verb"].as_str()), (Some("storage.storm.io"), Some("volumes"), Some("delete")));
    assert_eq!(attrs["name"], g1.as_str());

    let log = audit(&n);
    assert!(log.iter().any(|r| r["who"] == "kubernetes:bob" && r["decision"] == "refused"));
    assert!(log.iter().any(|r| r["who"] == "kubernetes:alice" && r["decision"] == "allowed" && r["status"] == 204));

    // A review is of one name (#216): g2 is asked about afresh.
    let before = asked.lock().unwrap().len();
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{g2}"), Some("alice-k8s"), None).await, 204);
    assert!(asked.lock().unwrap().len() > before, "another volume, another review");
    // Asked again within the minute, of the same name: answered from the cache.
    let tpm = Some(json!({"tpm": "none"}));
    assert_eq!(call(&n, M::PUT, "/boothost/server1/tpm", Some("alice-k8s"), tpm.clone()).await, 200);
    let before = asked.lock().unwrap().len();
    assert_eq!(call(&n, M::PUT, "/boothost/server1/tpm", Some("alice-k8s"), tpm).await, 200);
    assert_eq!(asked.lock().unwrap().len(), before, "the review was cached");

    // No apiserver named: a bearer that is not the admin token is refused.
    let m = node(false, None).await;
    let g3 = volume(&m, "g3", true).await;
    assert_eq!(call(&m, M::DELETE, &format!("/volumes/{g3}"), Some("alice-k8s"), None).await, 401);
}

/// stormcert reads a machine's attestation with its own ServiceAccount: a
/// bearer a SubjectAccessReview allows `get` on `boothost` of that machine,
/// and nothing else (#216). The TPM mark is the admin's.
#[tokio::test]
async fn stormcert_reads_an_attestation_with_its_own_bearer_and_only_that() {
    use reqwest::Method as M;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;

    // The mark: not the node token (it would let a node downgrade itself).
    let tpm = json!({"tpm": "required"});
    assert_eq!(call(&n, M::PUT, "/boothost/server1/tpm", Some(NODE), Some(tpm.clone())).await, 401);
    assert_eq!(call(&n, M::PUT, "/boothost/server1/tpm", Some("stormcert-k8s"), Some(tpm.clone())).await, 403);
    assert_eq!(call(&n, M::PUT, "/boothost/server1/tpm", Some(ADMIN), Some(tpm.clone())).await, 200);
    assert_eq!(call(&n, M::PUT, "/boothost/server2/tpm", Some("alice-k8s"), Some(json!({"tpm": "none"}))).await, 200);
    assert_eq!(call(&n, M::DELETE, "/boothost/server2/tpm", Some(NODE), None).await, 401);
    assert!(audit(&n).iter().any(|r| r["path"] == "/api/v1/boothost/server1/tpm" && r["who"] == "admin-token"));

    // The read: stormcert's bearer for server1, not server2, and nothing else.
    assert_eq!(call(&n, M::GET, "/boothost/server1/attestation", Some("stormcert-k8s"), None).await, 200);
    assert_eq!(call(&n, M::GET, "/boothost/server2/attestation", Some("stormcert-k8s"), None).await, 403);
    assert_eq!(call(&n, M::GET, "/boothost/server1", Some("stormcert-k8s"), None).await, 401);
    assert_eq!(call(&n, M::GET, "/boothost/server1/attestation", Some("nobody"), None).await, 401);
    assert_eq!(call(&n, M::GET, "/boothost/server1/attestation", None, None).await, 401);
    assert_eq!(call(&n, M::GET, "/boothost/server2/attestation", Some(NODE), None).await, 200);
    let sar: Vec<Value> = asked.lock().unwrap().iter().filter(|b| b["kind"] == "SubjectAccessReview").cloned().collect();
    let attrs = &sar.last().unwrap()["spec"]["resourceAttributes"];
    assert_eq!((attrs["resource"].as_str(), attrs["verb"].as_str(), attrs["name"].as_str()), (Some("boothost"), Some("get"), Some("server2")));

    let v: Value = reqwest::Client::new()
        .get(format!("{}/boothost/server1/attestation", n.base))
        .bearer_auth("stormcert-k8s")
        .send().await.unwrap().json().await.unwrap();
    assert_eq!((v["tpm"].as_str(), v["requires"].as_str(), v["claimed"].as_bool()), (Some("required"), Some("tpm_quote"), Some(false)));
}

/// An admin token is minted into its own file when none is configured, 0600,
/// apart from the node token.
#[test]
fn an_admin_token_is_minted_apart_from_the_node_token() {
    let dir = TempDir::new().unwrap();
    let mut m = stormblock::mgmt::config::ManagementConfig::default();
    m.data_dir = Some(dir.path().join("engine").to_string_lossy().to_string());
    m.admin_token_file = Some(dir.path().join("admin/admin_token").to_string_lossy().to_string());
    let r = stormblock::mgmt::auth::resolve(&m).unwrap();
    let admin = r.auth.admin_token.clone().unwrap();
    assert_ne!(Some(admin.clone()), r.auth.api_token);
    let f = dir.path().join("admin/admin_token");
    assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), admin);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(dir.path().join("admin")).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(stormblock::mgmt::auth::resolve(&m).unwrap().auth.admin_token, Some(admin), "kept, not re-minted");
    assert!(!r.auth.audit_only);
    m.admin_gate = Some("audit".into());
    assert!(stormblock::mgmt::auth::resolve(&m).unwrap().auth.audit_only);
}
