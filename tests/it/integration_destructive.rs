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
                        Some("user-k8s") => Some(("system:serviceaccount:vmimages:vmimages", vec!["storage-users"])),
                        Some("viewer-k8s") => Some(("carol", vec!["storage-viewers"])),
                        // storage-user once stormcos#557 adds `delete` (#422).
                        Some("tester-k8s") => Some(("system:serviceaccount:stormbootx-test:runner", vec!["storage-users-delete"])),
                        Some("other-tester-k8s") => Some(("system:serviceaccount:stormbootx-test:other", vec!["storage-users-delete"])),
                        // A family's builder: its own namespace only (#408).
                        Some("nana-k8s") => Some(("system:serviceaccount:nana:builder", vec!["nana-users"])),
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
                    let in_group = |n: &str| spec["groups"].as_array().is_some_and(|g| g.iter().any(|x| x == n));
                    let verb = ra["verb"].as_str().unwrap_or("");
                    // stormcos's roles (#382): storage-admin `*`;
                    // storage-user get/list/watch/create/update;
                    // storage-viewer get/list/watch.
                    let user = in_group("storage-users") && matches!(verb, "get" | "list" | "watch" | "create" | "update");
                    let viewer = in_group("storage-viewers") && matches!(verb, "get" | "list" | "watch");
                    let deleter = in_group("storage-users-delete")
                        && matches!(verb, "get" | "list" | "watch" | "create" | "update" | "delete");
                    // A RoleBinding in `nana` (#408): the volumes of that
                    // namespace and no other's.
                    let nana = in_group("nana-users")
                        && ra["namespace"] == "nana"
                        && matches!(verb, "get" | "list" | "watch" | "create" | "update" | "delete");
                    let ok = (in_group("storage-admins") || stormcert || user || viewer || deleter || nana) && ra["group"] == "storage.storm.io";
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
        (M::POST, format!("/arrays/{id}/release"), None),
        (M::POST, format!("/arrays/{id}/grow"), None),
        (M::PUT, "/forge".to_string(), Some(json!({}))),
        (M::DELETE, "/forge".to_string(), None),
        (M::POST, "/forge/default".to_string(), None),
        (M::POST, "/spares".to_string(), Some(json!({}))),
        (M::POST, "/pallets/gpt".to_string(), Some(json!({}))),
        (M::POST, "/images/build".to_string(), Some(json!({ "out": "/nonexistent/x.img" }))),
        (M::POST, "/images/convert".to_string(), Some(json!({ "input": "/nonexistent", "out": "/nonexistent/x.img" }))),
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
    assert_eq!((del["decision"].as_str(), del["status"].as_u64(), del["resource"].as_str(), del["verb"].as_str()), (Some("allowed"), Some(204), Some("volumes"), Some("destroy")));
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
    // A reviewed bearer reads too (#382): `list` on volumes.
    assert_eq!(call(&n, M::GET, "/volumes", Some("alice-k8s"), None).await, 200);

    let sar: Vec<Value> = asked.lock().unwrap().iter().filter(|b| b["kind"] == "SubjectAccessReview").cloned().collect();
    let attrs = &sar.iter().rev().find(|b| b["spec"]["resourceAttributes"]["verb"] == "destroy").unwrap()["spec"]["resourceAttributes"];
    assert_eq!((attrs["group"].as_str(), attrs["resource"].as_str(), attrs["verb"].as_str()), (Some("storage.storm.io"), Some("volumes"), Some("destroy")));
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
    // Since #382 its bearer reads what its role allows (`get` boothost
    // server1) and nothing else.
    assert_eq!(call(&n, M::GET, "/boothost/server1", Some("stormcert-k8s"), None).await, 200);
    assert_eq!(call(&n, M::GET, "/volumes", Some("stormcert-k8s"), None).await, 403);
    assert_eq!(call(&n, M::GET, "/boothost/server1/attestation", Some("nobody"), None).await, 401);
    assert_eq!(call(&n, M::GET, "/boothost/server1/attestation", None, None).await, 401);
    assert_eq!(call(&n, M::GET, "/boothost/server2/attestation", Some(NODE), None).await, 200);
    let sar: Vec<Value> = asked.lock().unwrap().iter().filter(|b| b["kind"] == "SubjectAccessReview").cloned().collect();
    let attrs = &sar.iter().rev().find(|b| b["spec"]["resourceAttributes"]["name"] == "server2").unwrap()["spec"]["resourceAttributes"];
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

/// #143: a builder off the node (a Kubernetes bearer allowed `create` on
/// `goldens`) makes a named golden from a stopped VM's disk; forge pulls it
/// with the reply's ticket and import body, checks the digest and records it.
#[tokio::test]
async fn a_stopped_vms_disk_becomes_a_named_golden_that_forge_pulls_with_a_ticket() {
    use sha2::{Digest, Sha256};
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;
    let forge = node(false, None).await;
    let c = reqwest::Client::new();

    // The VM's root: a clone of a base golden, written by the guest.
    let (root, base) = {
        let mut vm = n.state.volume_manager.lock().await;
        let base = vm.create_volume_any("golden-base", 4 * MIB).await.unwrap();
        vm.seal_volume(base, None).await.unwrap();
        let root = vm.create_snapshot(base, "default.vm1-root").await.unwrap();
        (root, base)
    };
    let dev = n.state.volume_manager.lock().await.get_volume(&root).unwrap();
    dev.write(MIB, &vec![0x5au8; 4096]).await.unwrap();
    dev.flush().await.unwrap();
    let mut all = vec![0u8; (4 * MIB) as usize];
    dev.read(0, &mut all).await.unwrap();
    let want = hex::encode(Sha256::digest(&all));
    drop(dev);

    let body = json!({ "volume": "default.vm1-root", "name": "bb", "provenance": { "commit": "abc123", "builder": "make-golden.sh" } });
    assert_eq!(call(&n, reqwest::Method::POST, "/goldens", Some(NODE), Some(body.clone())).await, 401, "the node token does not make goldens");
    assert_eq!(call(&n, reqwest::Method::POST, "/goldens", Some("bob-k8s"), Some(body.clone())).await, 403);
    let r = c.post(format!("{}/goldens", n.base)).bearer_auth("alice-k8s").json(&body).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let made: Value = r.json().await.unwrap();
    let g = &made["golden"];
    let name = format!("golden-bb-{}", &want[..12]);
    assert_eq!(g["name"], name.as_str(), "{made}");
    assert_eq!(g["sha256"], want.as_str(), "the digest is every byte of the disk");
    assert_eq!(g["parent"], "golden-base");
    assert_eq!(g["source"]["name"], "default.vm1-root");
    assert_eq!(g["provenance"]["commit"], "abc123");
    assert_eq!(g["made_by"], "kubernetes:alice");
    let sar: Vec<Value> = asked.lock().unwrap().iter().filter(|b| b["kind"] == "SubjectAccessReview").cloned().collect();
    assert!(sar.iter().any(|b| b["spec"]["resourceAttributes"]["resource"] == "goldens" && b["spec"]["resourceAttributes"]["verb"] == "create"), "{sar:?}");
    {
        let vm = n.state.volume_manager.lock().await;
        let gid = stormblock::volume::VolumeId(g["volume_id"].as_str().unwrap().parse().unwrap());
        assert!(vm.is_sealed(&gid), "the golden is sealed");
        assert!(!vm.is_sealed(&root), "the VM's own volume is left as it was");
        assert_eq!(vm.parent(&gid), Some(root));
        let _ = base;
    }

    // The same content again is the golden already here; nothing new.
    let again: Value = c.post(format!("{}/goldens", n.base)).bearer_auth("alice-k8s").json(&body).send().await.unwrap().json().await.unwrap();
    assert_eq!(again["existing"], true);
    assert_eq!(again["golden"]["volume_id"], g["volume_id"]);
    let goldens = n.state.volume_manager.lock().await.list_volumes().await.into_iter().filter(|v| v.1.starts_with("golden-bb-")).count();
    assert_eq!(goldens, 1, "no pending snapshot left behind");

    // The ticket: the golden's bytes, by range, to the ticket alone.
    let url = made["ticket"]["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://"), "{url}");
    let r = c.get(&url).header("Range", format!("bytes={}-{}", MIB, MIB + 9)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().await.unwrap().to_vec(), vec![0x5au8; 10]);
    let bare = url.split('?').next().unwrap();
    assert_eq!(c.get(bare).send().await.unwrap().status().as_u16(), 401, "no ticket, no content");
    assert_eq!(c.get(format!("{bare}?ticket=nope")).send().await.unwrap().status().as_u16(), 401);
    let other = url.replace(&format!("/goldens/{name}/"), "/goldens/golden-base/");
    assert_eq!(c.get(other).send().await.unwrap().status().as_u16(), 401, "a ticket reads its own golden only");

    // Forge pulls with the reply's import body, as is.
    let import = |b: Value| {
        let (c, base) = (c.clone(), forge.base.clone());
        async move {
            let st: Value = c.post(format!("{base}/volumes/import")).bearer_auth(ADMIN).json(&b).send().await.unwrap().json().await.unwrap();
            let id = st["id"].as_str().unwrap().to_string();
            for _ in 0..400 {
                let s: Value = c.get(format!("{base}/volumes/import/{id}")).bearer_auth(ADMIN).send().await.unwrap().json().await.unwrap();
                if s["state"] == "done" || s["state"] == "failed" {
                    return s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("import {id} never finished");
        }
    };
    let mut ib = made["import"].clone();
    ib["verify"] = json!(false);
    let done = import(ib.clone()).await;
    assert_eq!(done["state"], "done", "{done}");
    let rec: Value = c.get(format!("{}/goldens/{name}", forge.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!(rec["how"], "imported");
    assert_eq!(rec["sha256"], want.as_str());
    assert_eq!(rec["parent"], "golden-base");
    assert_eq!(rec["provenance"]["commit"], "abc123");
    assert!(!rec["from"].as_str().unwrap().contains("ticket"), "the ticket is not kept: {rec}");

    // Content that is not the digest it claims is refused.
    ib["name"] = json!("golden-bb-wrong");
    ib["sha256"] = json!("0".repeat(64));
    let bad = import(ib).await;
    assert_eq!(bad["state"], "failed", "{bad}");
    assert_eq!(bad["phase"], "verify", "{bad}");
}

/// #66: a drive that carries a slab `POST /api/v1/slabs` formatted (which
/// opens a device of its own) is not closed unless forced.
#[tokio::test]
async fn a_drive_carrying_an_api_formatted_slab_is_not_closed() {
    let n = node(false, None).await;
    let c = reqwest::Client::new();
    let path = n.dir.path().join("disk.bin").to_string_lossy().to_string();
    let r = c.post(format!("{}/drives", n.base)).bearer_auth(NODE).json(&json!({ "path": path, "size_bytes": 64 * MIB })).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = c.post(format!("{}/slabs", n.base)).bearer_auth(ADMIN).json(&json!({ "device_path": path, "slot_size": MIB })).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    for _ in 0..200 {
        let on = n.state.slab_registry.read().await.iter().any(|(_, s)| s.device().id().path == path);
        if on {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let enc: String = path.bytes().map(|b| format!("%{b:02X}")).collect();
    let r = c.delete(format!("{}/drives/{enc}", n.base)).bearer_auth(ADMIN).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409, "{}", r.text().await.unwrap());
    let still: Value = c.get(format!("{}/drives", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert!(still.to_string().contains(&path), "the drive is still listed: {still}");
    let r = c.delete(format!("{}/drives/{enc}?force=true", n.base)).bearer_auth(ADMIN).send().await.unwrap();
    assert!(r.status().is_success(), "force closes it: {}", r.text().await.unwrap());
}

/// #65: a drive's id is who it is, not minted per open: the same after a
/// close and reopen, and on another engine (a restart), and addressable by it.
#[tokio::test]
async fn a_drive_keeps_its_id_across_opens_and_restarts() {
    let n = node(false, None).await;
    let c = reqwest::Client::new();
    let path = n.dir.path().join("disk.bin").to_string_lossy().to_string();
    let open = |base: String| {
        let (c, path) = (c.clone(), path.clone());
        async move {
            let r: Value = c.post(format!("{base}/drives")).bearer_auth(NODE).json(&json!({ "path": path, "size_bytes": 16 * MIB })).send().await.unwrap().json().await.unwrap();
            r["uuid"].as_str().unwrap_or_else(|| panic!("{r}")).to_string()
        }
    };
    let first = open(n.base.clone()).await;
    let r = c.delete(format!("{}/drives/{first}", n.base)).bearer_auth(ADMIN).send().await.unwrap();
    assert!(r.status().is_success(), "closed by its id: {}", r.text().await.unwrap());
    let again = open(n.base.clone()).await;
    assert_eq!(again, first, "the same drive, the same id");

    let other = node(false, None).await;
    assert_eq!(open(other.base.clone()).await, first, "and on another engine: a restart keeps it");
    let got = c.get(format!("{}/drives/{first}", other.base)).bearer_auth(NODE).send().await.unwrap();
    assert_eq!(got.status().as_u16(), 200);

    let elsewhere = n.dir.path().join("other.bin").to_string_lossy().to_string();
    let r: Value = c.post(format!("{}/drives", n.base)).bearer_auth(NODE).json(&json!({ "path": elsewhere, "size_bytes": 16 * MIB })).send().await.unwrap().json().await.unwrap();
    assert_ne!(r["uuid"].as_str().unwrap(), first, "another drive, another id");
}


/// #382 (owner's (A) on stormcos#241): a Kubernetes bearer is reviewed for
/// ordinary verbs too; destructive verbs are reviewed as `destroy`, which
/// only storage-admin holds. storage-user clones and attaches and is refused
/// a destructive verb; storage-viewer only reads; no bearer is the node
/// token's rules.
#[tokio::test]
async fn a_storage_user_bearer_clones_and_attaches_and_never_destroys() {
    use reqwest::Method as M;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;
    let golden = volume(&n, "base.golden", true).await;

    // storage-user: reads, creates, clones, attaches.
    assert_eq!(call(&n, M::GET, "/volumes", Some("user-k8s"), None).await, 200);
    assert_eq!(call(&n, M::GET, &format!("/volumes/{golden}"), Some("user-k8s"), None).await, 200);
    assert_eq!(call(&n, M::POST, "/volumes", Some("user-k8s"), Some(json!({"name": "scratch", "size": "4M"}))).await, 201);
    let clone = reqwest::Client::new()
        .post(format!("{}/volumes/{golden}/clone", n.base))
        .bearer_auth("user-k8s")
        .json(&json!({"name": "vm1-root"}))
        .send()
        .await
        .unwrap();
    assert_eq!(clone.status().as_u16(), 201, "{}", clone.text().await.unwrap());
    let attach = call(&n, M::POST, "/volumes/vm1-root/attach", Some("user-k8s"), Some(json!({}))).await;
    assert!(attach != 401 && attach != 403, "attach is the user's to ask: {attach}");
    // …and never a destructive verb.
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{golden}"), Some("user-k8s"), None).await, 403, "a golden's delete");
    assert_eq!(call(&n, M::POST, "/slabs", Some("user-k8s"), Some(json!({"device_path": "/nope"}))).await, 403, "a format");
    assert_eq!(call(&n, M::PUT, "/forge", Some("user-k8s"), Some(json!({}))).await, 403, "forge on");

    // storage-viewer: reads only.
    assert_eq!(call(&n, M::GET, "/volumes", Some("viewer-k8s"), None).await, 200);
    assert_eq!(call(&n, M::POST, "/volumes", Some("viewer-k8s"), Some(json!({"name": "v", "size": "4M"}))).await, 403);

    // What was asked: list, get, create, update (the clone), and destroy.
    let sar: Vec<(String, String)> = asked
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b["kind"] == "SubjectAccessReview" && b["spec"]["user"] == "system:serviceaccount:vmimages:vmimages")
        .map(|b| {
            let ra = &b["spec"]["resourceAttributes"];
            (ra["resource"].as_str().unwrap().to_string(), ra["verb"].as_str().unwrap().to_string())
        })
        .collect();
    for want in [("volumes", "list"), ("volumes", "get"), ("volumes", "create"), ("volumes", "update"), ("volumes", "destroy"), ("slabs", "destroy")] {
        assert!(sar.iter().any(|(r, v)| r == want.0 && v == want.1), "{want:?} not asked: {sar:?}");
    }

    // No bearer, a bad one: the node token's rules, as before.
    assert_eq!(call(&n, M::GET, "/volumes", None, None).await, 401);
    assert_eq!(call(&n, M::GET, "/volumes", Some("nobody"), None).await, 401);
    assert_eq!(call(&n, M::GET, "/volumes", Some(NODE), None).await, 200);
    // A node with no apiserver named reviews nothing: a bearer is no credential.
    let m = node(false, None).await;
    assert_eq!(call(&m, M::GET, "/volumes", Some("user-k8s"), None).await, 401);
}

/// #219: a volume is renamed by `PATCH /api/v1/volumes/{id} {name}`: the
/// admin's (destructive), refused for a name another volume has, and for a
/// sealed one unless forced. The listing (the catalog a persist publishes)
/// shows the new name; lookups by the old name no longer find it.
#[tokio::test]
async fn a_volume_is_renamed_by_the_admin_and_a_golden_only_with_force() {
    use reqwest::Method as M;
    let n = node(false, None).await;
    let c = reqwest::Client::new();
    let pvc = volume(&n, "standby-pvc-ext4j-64m-1a2b", false).await;
    let _other = volume(&n, "taken", false).await;
    let golden = volume(&n, "base.golden", true).await;

    let rename = |id: String, body: Value, tok: &'static str| {
        let (c, base) = (c.clone(), n.base.clone());
        async move {
            let r = c.patch(format!("{base}/volumes/{id}")).bearer_auth(tok).json(&body).send().await.unwrap();
            let s = r.status().as_u16();
            (s, r.json::<Value>().await.unwrap_or_default())
        }
    };
    assert_eq!(rename(pvc.clone(), json!({"name": "default.data-web-0"}), NODE).await.0, 401, "the node token does not rename");
    let (s, body) = rename(pvc.clone(), json!({"name": "default.data-web-0"}), ADMIN).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!((body["previous"].as_str(), body["renamed"].as_bool()), (Some("standby-pvc-ext4j-64m-1a2b"), Some(true)));

    let list: Value = c.get(format!("{}/volumes", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = list["items"].as_array().or(list.as_array()).unwrap().iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"default.data-web-0") && !names.contains(&"standby-pvc-ext4j-64m-1a2b"), "{names:?}");
    assert_eq!(call(&n, M::GET, "/volumes/default.data-web-0", Some(NODE), None).await, 200, "found by its new name");
    assert_eq!(call(&n, M::GET, "/volumes/standby-pvc-ext4j-64m-1a2b", Some(NODE), None).await, 404);

    assert_eq!(rename(pvc.clone(), json!({"name": "taken"}), ADMIN).await.0, 409, "another volume's name");
    assert_eq!(rename(pvc.clone(), json!({"name": "a/b"}), ADMIN).await.0, 400);
    assert_eq!(rename(pvc.clone(), json!({"name": "default.data-web-0"}), ADMIN).await.1["renamed"], false, "the same name: nothing to do");

    let (s, body) = rename(golden.clone(), json!({"name": "base-v2.golden"}), ADMIN).await;
    assert_eq!(s, 409, "a golden goes by its name: {body}");
    let (s, body) = rename(golden.clone(), json!({"name": "base-v2.golden", "force": true}), ADMIN).await;
    assert_eq!(s, 200, "{body}");
    assert!(audit(&n).iter().any(|r| r["method"] == "PATCH" && r["who"] == "admin-token"), "a rename is audited");
}

/// #271: a sealed volume's digest is the sha256 of every byte, on the node
/// token; read once per seal and kept; an unsealed volume has none (409);
/// unsealed, written and sealed again, it is read afresh.
#[tokio::test]
async fn a_sealed_volumes_digest_is_every_byte_and_kept_for_its_seal() {
    use sha2::{Digest, Sha256};
    let n = node(false, None).await;
    let c = reqwest::Client::new();
    let id = volume(&n, "golden-x", false).await;
    let vid = stormblock::volume::VolumeId(id.parse().unwrap());
    let bytes = |fill: u8| vec![fill; (4 * MIB) as usize];
    let dev = n.state.volume_manager.lock().await.get_volume(&vid).unwrap();
    dev.write(0, &bytes(0x3C)).await.unwrap();
    dev.flush().await.unwrap();
    let digest = |q: &'static str| {
        let (c, base, id) = (c.clone(), n.base.clone(), id.clone());
        async move {
            let r = c.get(format!("{base}/volumes/{id}/digest{q}")).bearer_auth(NODE).send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or_default())
        }
    };
    assert_eq!(digest("").await.0, 409, "unsealed: no digest");
    n.state.volume_manager.lock().await.seal_volume(vid, None).await.unwrap();

    let (s, first) = digest("").await;
    assert_eq!(s, 200, "{first}");
    assert_eq!(first["sha256"], hex::encode(Sha256::digest(bytes(0x3C))), "every byte");
    assert_eq!(first["size_bytes"], 4 * MIB);
    assert_eq!(first["cached"], false);
    let (_, again) = digest("").await;
    assert_eq!((again["sha256"].clone(), again["cached"].clone()), (first["sha256"].clone(), json!(true)), "kept for the seal");
    let (_, refreshed) = digest("?refresh=true").await;
    assert_eq!((refreshed["sha256"].clone(), refreshed["cached"].clone()), (first["sha256"].clone(), json!(false)));
    assert_eq!(call(&n, reqwest::Method::GET, &format!("/volumes/golden-x/digest"), Some(NODE), None).await, 200, "by name too");

    // Unsealed, written, sealed again: the old digest is not the answer.
    assert_eq!(call(&n, reqwest::Method::DELETE, &format!("/volumes/{id}/seal"), Some(ADMIN), None).await, 200);
    assert_eq!(digest("").await.0, 409);
    dev.write(0, &bytes(0x7E)).await.unwrap();
    dev.flush().await.unwrap();
    n.state.volume_manager.lock().await.seal_volume(vid, None).await.unwrap();
    let (_, resealed) = digest("").await;
    assert_eq!(resealed["sha256"], hex::encode(Sha256::digest(bytes(0x7E))));
    assert_eq!(resealed["cached"], false);
}

/// #166: the web UI (feature `ui`) is behind the same check as the API: no
/// token, no page and no form; the node token reads its pages; its forms
/// (creating and deleting arrays, volumes, exports) need the admin token:
/// the node token is refused by the admin gate (401, as for any destructive
/// verb).
#[cfg(feature = "ui")]
#[tokio::test]
async fn the_web_ui_is_behind_the_token_and_its_forms_behind_the_admin_gate() {
    let n = node(false, None).await;
    let root = n.base.trim_end_matches("/api/v1").to_string();
    let call_ui = |m: reqwest::Method, path: &str, tok: Option<&str>| {
        let mut r = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap().request(m, format!("{root}{path}"));
        if let Some(t) = tok {
            r = r.bearer_auth(t);
        }
        r.form(&[("name", "ui-made"), ("size", "4M")]).send()
    };
    for p in ["/ui/", "/ui/volumes", "/"] {
        assert_eq!(call_ui(reqwest::Method::GET, p, None).await.unwrap().status(), 401, "GET {p} with no token");
    }
    assert_eq!(call_ui(reqwest::Method::GET, "/ui/volumes", Some(NODE)).await.unwrap().status(), 200, "the node token reads a page");
    for (m, p) in [(reqwest::Method::POST, "/ui/volumes"), (reqwest::Method::POST, "/ui/arrays"), (reqwest::Method::DELETE, "/ui/volumes/x")] {
        assert_eq!(call_ui(m.clone(), p, None).await.unwrap().status(), 401, "{m} {p} with no token");
        // As every destructive verb answers the node token (#274).
        let r = call_ui(m.clone(), p, Some(NODE)).await.unwrap();
        assert_eq!(r.status(), 401, "{m} {p} with the node token");
        assert!(r.text().await.unwrap().contains("admin token required"), "{m} {p}: the admin gate says why");
    }
    assert!(n.state.volume_manager.lock().await.list_volumes().await.iter().all(|v| v.1 != "ui-made"), "nothing was made");
    assert_ne!(call_ui(reqwest::Method::POST, "/ui/volumes", Some(ADMIN)).await.unwrap().status(), 401, "the admin token gets through the gate");
}

/// #422 (the owner's option 1 on stormbootx-test#2): a storage-user (with
/// `delete`, as stormcos#557 grants it) removes what it created itself and
/// nothing else. It imports an image, adds `boothost/<mac>`, the machine
/// claims (a host golden, its synonym and a boot clone, made for it), and
/// then it detaches and deletes the clone, the synonyms, the golden, the
/// image and the host record. Another storage-user gets 403 on each of
/// those, and on the node's own (unrecorded) volumes the first does too.
#[tokio::test]
async fn a_storage_user_deletes_what_it_created_and_nothing_else() {
    use reqwest::Method as M;
    const ME: &str = "tester-k8s";
    const THEM: &str = "other-tester-k8s";
    const SA: &str = "kubernetes:system:serviceaccount:stormbootx-test:runner";
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;
    let c = reqwest::Client::new();
    let theirs = volume(&n, "node-scratch", false).await;
    let theirs_sealed = volume(&n, "node-golden", true).await;

    // Import an image, labelled with its run.
    let image = n.dir.path().join("boot.raw");
    std::fs::write(&image, vec![0x5Au8; (4 * MIB) as usize]).unwrap();
    let r = c
        .post(format!("{}/volumes/import", n.base))
        .bearer_auth(ME)
        .json(&json!({"name": "boot-image", "file": image.to_str().unwrap(), "format": "raw",
                      "labels": {"storm.io/test-run": "run-1"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202, "{}", r.text().await.unwrap());
    let job: Value = r.json().await.unwrap();
    let mut img = Value::Null;
    for _ in 0..200 {
        img = c.get(format!("{}/volumes/import/{}", n.base, job["id"].as_str().unwrap())).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
        if img["state"] == "done" || img["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(img["state"], "done", "{img}");
    let v: Value = c.get(format!("{}/volumes/boot-image", n.base)).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["creator"], SA, "{v}");
    assert_eq!(v["labels"]["storm.io/test-run"], "run-1");

    // Its host, and the machine's claim.
    let mac = "mac-0a0b0c0d0e0f";
    assert_eq!(
        call(&n, M::POST, "/synonyms", Some(ME), Some(json!({"namespace": "boothost", "name": mac, "volume": "boot-image"}))).await,
        201
    );
    let claim: Value = c.post(format!("{}/synonyms/boothost/{mac}/claim", n.base)).json(&json!({})).send().await.unwrap().json().await.unwrap();
    let clone = claim["volume"]["id"].as_str().map(str::to_string).unwrap_or_else(|| panic!("the claim names its clone: {claim}"));
    let golden = claim["host_golden"]["volume"].as_str().map(str::to_string).unwrap_or_else(|| panic!("{claim}"));
    let s: Value = c.get(format!("{}/synonyms/hostgolden/{mac}", n.base)).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["creator"], SA, "the claim's host golden synonym is the host's creator's: {s}");
    let g: Value = c.get(format!("{}/volumes/{golden}", n.base)).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
    assert_eq!(g["creator"], SA, "the host golden too: {g}");
    let host: Value = c.get(format!("{}/boothost/{mac}", n.base)).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
    assert_eq!(host["creator"], SA, "{host}");

    // Somebody else's: 403 on every one.
    for (m, path) in [
        (M::DELETE, format!("/volumes/{clone}/attach")),
        (M::DELETE, format!("/volumes/{clone}")),
        (M::DELETE, format!("/synonyms/hostgolden/{mac}")),
        (M::DELETE, format!("/synonyms/boothost/{mac}")),
        (M::DELETE, format!("/volumes/{golden}")),
        (M::DELETE, "/volumes/boot-image".to_string()),
        (M::DELETE, format!("/boothost/{mac}")),
    ] {
        assert_eq!(call(&n, m, &path, Some(THEM), None).await, 403, "{path} by another storage-user");
    }
    // The node's own volumes (no record): never the storage-user's.
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{theirs}"), Some(ME), None).await, 403, "an unrecorded volume");
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{theirs}/attach"), Some(ME), None).await, 403);
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{theirs_sealed}"), Some(ME), None).await, 403);

    // Its own: removed, in the test's order.
    let detach = call(&n, M::DELETE, &format!("/volumes/{clone}/attach"), Some(ME), None).await;
    assert!(detach != 401 && detach != 403, "its own clone's detach: {detach}");
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{clone}"), Some(ME), None).await, 204, "its boot clone");
    assert_eq!(call(&n, M::DELETE, &format!("/synonyms/hostgolden/{mac}"), Some(ME), None).await, 200);
    assert_eq!(call(&n, M::DELETE, &format!("/synonyms/boothost/{mac}"), Some(ME), None).await, 200);
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{golden}"), Some(ME), None).await, 204, "its host golden (sealed)");
    assert_eq!(call(&n, M::DELETE, "/volumes/boot-image", Some(ME), None).await, 204, "its image (sealed)");
    assert_eq!(call(&n, M::DELETE, &format!("/boothost/{mac}"), Some(ME), None).await, 200, "its host record");
    assert_eq!(call(&n, M::GET, &format!("/boothost/{mac}"), Some(ME), None).await, 404, "gone");
    let left: Value = c.get(format!("{}/volumes", n.base)).bearer_auth(ME).send().await.unwrap().json().await.unwrap();
    let names: Vec<String> = left["items"].as_array().unwrap().iter().filter_map(|v| v["name"].as_str().map(str::to_string)).collect();
    assert!(!names.iter().any(|n| n.starts_with("boothost-") || n.starts_with("hostgolden-") || n == "boot-image"), "{names:?}");

    // Reviewed as `delete`, said in the audit log; the node's are still `destroy`.
    let a = audit(&n);
    assert!(a.iter().any(|r| r["who"] == SA && r["verb"] == "delete" && r["decision"] == "allowed"
        && r["reason"].as_str().is_some_and(|s| s.contains("its own"))), "{a:?}");
    assert!(a.iter().any(|r| r["who"] == SA && r["verb"] == "destroy" && r["decision"] == "refused"), "{a:?}");
    // The admin token's rules are unchanged.
    assert_eq!(call(&n, M::DELETE, &format!("/volumes/{theirs_sealed}"), Some(ADMIN), None).await, 204);
}

/// #384: a volume sealed on this node is digested in the background (no
/// request waits for it), and `GET …/digest` and health say so.
#[tokio::test]
async fn a_sealed_volumes_seal_digest_is_taken_in_the_background_and_reported() {
    let n = node(false, None).await;
    n.state.start_digests();
    let c = reqwest::Client::new();
    let id = volume(&n, "golden-d", false).await;
    let vid = stormblock::volume::VolumeId(id.parse().unwrap());
    let dev = n.state.volume_manager.lock().await.get_volume(&vid).unwrap();
    dev.write(0, &vec![0x3Cu8; (4 * MIB) as usize]).await.unwrap();
    dev.flush().await.unwrap();
    n.state.volume_manager.lock().await.seal_volume(vid, None).await.unwrap();

    let mut seal = Value::Null;
    for _ in 0..100 {
        let r = c.get(format!("{}/volumes/{id}/digest", n.base)).bearer_auth(NODE).send().await.unwrap();
        let v: Value = r.json().await.unwrap();
        if !v["seal"].is_null() {
            seal = v["seal"].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(seal["scheme"], "sb-seal-1", "the seal digest arrived: {seal}");
    assert_eq!(seal["verdict"], "intact");
    assert_eq!(seal["sha256"].as_str().unwrap().len(), 64);
    let h: Value = c.get(format!("{}/health", n.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["digests"]["intact"], 1, "{h}");
    assert_eq!(h["digests"]["mismatch"], 0);
}

/// #408: a volume belongs to a namespace. A family's bearer (a RoleBinding
/// in `nana` on forge's apiserver) makes, reads and lists its own
/// namespace's volumes and no other's; a name is unique within `nana`; a
/// namespace's quota is kept; `?namespace=` resolves a name.
#[tokio::test]
async fn a_familys_bearer_keeps_to_its_namespace_and_its_quota() {
    use reqwest::Method as M;
    const NANA: &str = "nana-k8s";
    let asked = Arc::new(Mutex::new(Vec::new()));
    let api = apiserver(asked.clone()).await;
    let n = node(false, Some(api)).await;
    let c = reqwest::Client::new();
    let stormcos_img = volume(&n, "img", false).await;

    // In its namespace: made; in the default one (nothing named): refused.
    let r = c.post(format!("{}/volumes", n.base)).bearer_auth(NANA)
        .json(&json!({"name": "img", "size": "4M", "namespace": "nana"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().await.unwrap());
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["namespace"], "nana", "{v}");
    let nana_img = v["id"].as_str().unwrap().to_string();
    assert_eq!(call(&n, M::POST, "/volumes", Some(NANA), Some(json!({"name": "x", "size": "4M"}))).await, 403, "stormcos's namespace");
    assert_eq!(call(&n, M::POST, "/volumes", Some(NODE), Some(json!({"name": "x", "size": "4M", "namespace": "Not_A_Label"}))).await, 400);

    // A name is unique within nana; the same name in stormcos is another volume.
    let again = c.post(format!("{}/volumes", n.base)).bearer_auth(NANA)
        .json(&json!({"name": "img", "size": "4M", "namespace": "nana"})).send().await.unwrap();
    assert_eq!(again.status().as_u16(), 409);
    let body: Value = again.json().await.unwrap();
    assert_eq!((body["code"].as_str(), body["volume"].as_str()), (Some("name_taken"), Some(nana_img.as_str())), "{body}");

    // `img` is two volumes; `?namespace=` says which.
    assert_eq!(call(&n, M::GET, "/volumes/img", Some(NODE), None).await, 409);
    let one: Value = c.get(format!("{}/volumes/img?namespace=nana", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["id"].as_str(), Some(nana_img.as_str()), "{one}");
    let one: Value = c.get(format!("{}/volumes/img?namespace=stormcos", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["id"].as_str(), Some(stormcos_img.as_str()), "{one}");

    // Its own volume, by id or by name in its namespace; never stormcos's.
    assert_eq!(call(&n, M::GET, &format!("/volumes/{nana_img}"), Some(NANA), None).await, 200);
    assert_eq!(call(&n, M::GET, "/volumes/img?namespace=nana", Some(NANA), None).await, 200);
    assert_eq!(call(&n, M::GET, &format!("/volumes/{stormcos_img}"), Some(NANA), None).await, 403);

    // A clone is in its source's namespace (a sealed source, as every clone's).
    {
        let mut vm = n.state.volume_manager.lock().await;
        vm.seal_volume(stormblock::volume::VolumeId(nana_img.parse().unwrap()), None).await.unwrap();
    }
    let cl = c.post(format!("{}/volumes/{nana_img}/clone", n.base)).bearer_auth(NANA)
        .json(&json!({"name": "img-clone"})).send().await.unwrap();
    assert_eq!(cl.status().as_u16(), 201, "{}", cl.text().await.unwrap());
    let cl: Value = cl.json().await.unwrap();
    assert_eq!(cl["namespace"], "nana", "{cl}");

    // Its listing: what it may list, nana's only.
    let list: Value = c.get(format!("{}/volumes", n.base)).bearer_auth(NANA).send().await.unwrap().json().await.unwrap();
    let mut names: Vec<(String, String)> = list["items"].as_array().unwrap().iter()
        .map(|v| (v["namespace"].as_str().unwrap().to_string(), v["name"].as_str().unwrap().to_string())).collect();
    names.sort();
    assert_eq!(names, vec![("nana".into(), "img".into()), ("nana".into(), "img-clone".into())], "{list}");
    // The node token: every namespace.
    let all: Value = c.get(format!("{}/volumes", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 3);

    // A quota: the admin's to set; past it, nothing more is made.
    assert_eq!(call(&n, M::PUT, "/namespaces/nana/quota", Some(NODE), Some(json!({"max_volumes": 2}))).await, 401, "destructive");
    assert_eq!(call(&n, M::PUT, "/namespaces/nana/quota", Some(ADMIN), Some(json!({"max_volumes": 2}))).await, 200);
    let over = c.post(format!("{}/volumes", n.base)).bearer_auth(NANA)
        .json(&json!({"name": "third", "size": "4M", "namespace": "nana"})).send().await.unwrap();
    assert_eq!(over.status().as_u16(), 403);
    let body: Value = over.json().await.unwrap();
    assert_eq!(body["code"], "quota_exceeded", "{body}");
    assert_eq!(body["usage"]["volumes"], 3, "the refused one counted, then undone: {body}");
    let ns: Value = c.get(format!("{}/namespaces/nana", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!((ns["usage"]["volumes"].as_u64(), ns["quota"]["max_volumes"].as_u64()), (Some(2), Some(2)), "{ns}");
    let listed: Value = c.get(format!("{}/namespaces", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    let rows: Vec<&str> = listed["namespaces"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(rows, vec!["nana", "stormcos"], "{listed}");
    // Kept across a restart of the store.
    assert_eq!(
        stormblock::mgmt::namespaces::Quotas::load(Some(n.dir.path())).get("nana").and_then(|q| q.max_volumes),
        Some(2)
    );
    assert_eq!(call(&n, M::DELETE, "/namespaces/nana/quota", Some(ADMIN), None).await, 204);
    assert_eq!(call(&n, M::POST, "/volumes", Some(NANA), Some(json!({"name": "third", "size": "4M", "namespace": "nana"}))).await, 201);

    // Moved to another namespace by the admin: an unsealed volume, a free name.
    let r = c.patch(format!("{}/volumes/third?namespace=nana", n.base)).bearer_auth(ADMIN)
        .json(&json!({"namespace": "stormcos", "name": "img"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409, "img is stormcos's already: {}", r.text().await.unwrap());
    let r = c.patch(format!("{}/volumes/third?namespace=nana", n.base)).bearer_auth(ADMIN)
        .json(&json!({"namespace": "stormcos"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap());
    let moved: Value = c.get(format!("{}/volumes/third", n.base)).bearer_auth(NODE).send().await.unwrap().json().await.unwrap();
    assert_eq!(moved["namespace"], "stormcos", "{moved}");

    // Every review of nana's bearer named a namespace when it was about one volume.
    let sar: Vec<Value> = asked.lock().unwrap().iter()
        .filter(|b| b["kind"] == "SubjectAccessReview" && b["spec"]["user"] == "system:serviceaccount:nana:builder")
        .map(|b| b["spec"]["resourceAttributes"].clone()).collect();
    assert!(sar.iter().any(|ra| ra["verb"] == "create" && ra["namespace"] == "nana"), "{sar:?}");
    assert!(sar.iter().any(|ra| ra["verb"] == "get" && ra["namespace"] == "stormcos"), "{sar:?}");
    assert!(sar.iter().any(|ra| ra["verb"] == "list" && ra["namespace"].is_null()), "the cluster-wide list first: {sar:?}");
}

/// #393: an image is built by the admin, into the images directory (or onto
/// a ublk device the engine attached): an output anywhere else is refused
/// before a byte is written, and one inside is built.
#[tokio::test]
async fn an_image_is_built_by_the_admin_into_the_images_directory_only() {
    let n = node(false, None).await;
    let spec = r#"
size = "64M"
[[pallet]]
name = "boot"
kind = "boot"
priority = 14
members = [ { name = "cmdline", role = "cmdline", kind = "bootconfig", text = "root=ublk0 ro" } ]
"#;
    let post = |tok: &'static str, body: Value| {
        let url = format!("{}/images/build", n.base);
        async move {
            let r = reqwest::Client::new().post(url).bearer_auth(tok).json(&body).send().await.unwrap();
            let status = r.status().as_u16();
            (status, r.text().await.unwrap())
        }
    };
    let outside = n.dir.path().join("elsewhere.img");
    let images = n.dir.path().join("images");

    let (st, _) = post(NODE, json!({ "spec_toml": spec, "out": images.join("a.img") })).await;
    assert_eq!(st, 401, "the node token does not build an image");

    for out in [outside.clone(), images.join("../elsewhere.img"), "/dev/ublkb0".into()] {
        let (st, body) = post(ADMIN, json!({ "spec_toml": spec, "out": out })).await;
        assert_eq!(st, 400, "{}: {body}", out.display());
        assert!(body.contains("images directory"), "{body}");
    }
    assert!(!outside.exists(), "nothing written outside");

    let (st, body) = post(ADMIN, json!({ "spec_toml": spec, "out": images.join("a.img") })).await;
    assert_eq!(st, 200, "{body}");
    assert!(std::fs::metadata(images.join("a.img")).unwrap().len() > 0);

    // A conversion goes to the same places.
    let r = reqwest::Client::new()
        .post(format!("{}/images/convert", n.base))
        .bearer_auth(ADMIN)
        .json(&json!({ "input": images.join("a.img"), "out": outside.with_extension("qcow2") }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert!(!outside.with_extension("qcow2").exists());
}

/// A release erase (#291): asked of the running node with the admin token,
/// planned, recorded as a ticket in the state volume before the answer, and
/// refused where it cannot run.
#[tokio::test]
async fn a_release_erase_is_planned_recorded_for_the_next_boot_and_refused_where_it_cannot_run() {
    use reqwest::Method as M;
    use stormblock::volume::metadata::Origin;
    let n = node(false, None).await;
    {
        let mut vm = n.state.volume_manager.lock().await;
        let g = vm.create_volume_any("fastetcd-data.golden", 4 * MIB).await.unwrap();
        vm.seal_volume(g, None).await.unwrap();
        vm.set_origin(g, Origin::Release);
        let c = vm.create_snapshot(g, "fastetcd-data").await.unwrap();
        vm.set_origin(c, Origin::Release);
        let p = vm.create_volume_any("pvc-0001", 4 * MIB).await.unwrap();
        vm.set_origin(p, Origin::Node);
        vm.create_volume_any("system-data", 4 * MIB).await.unwrap();
    }
    let ask = |tok: &'static str, body: Value| {
        let base = n.base.clone();
        async move {
            let r = reqwest::Client::new().post(format!("{base}/release")).bearer_auth(tok).json(&body).send().await.unwrap();
            let code = r.status().as_u16();
            (code, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let names = |v: &Value, k: &str| -> Vec<String> {
        let mut x: Vec<String> = v[k].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
        x.sort();
        x
    };

    // Destructive: never the node token.
    assert_eq!(call(&n, M::POST, "/release", Some(NODE), Some(json!({"dry_run": true}))).await, 401);
    let (code, dry) = ask(ADMIN, json!({"dry_run": true})).await;
    assert_eq!(code, 200, "{dry}");
    let plan = &dry["plan"];
    assert_eq!(names(plan, "keep"), ["fastetcd-data.golden", "system-data"]);
    assert_eq!(names(plan, "relay"), ["fastetcd-data"]);
    assert_eq!(names(plan, "erased"), ["pvc-0001"]);
    assert_eq!(plan["erase"], "once");

    // An engine with no state volume is not a node a release runs on.
    let (code, body) = ask(ADMIN, json!({})).await;
    assert_eq!((code, body["code"].as_str()), (409, Some("not_a_node")), "{body}");

    let state_dir = TempDir::new().unwrap();
    let _ = n.state.state_store.set(Arc::new(stormblock::state::StateStore::in_dir(state_dir.path())));
    // Not while a flow-over is still moving the node onto its disk.
    n.state.flow_over_remaining.store(5, std::sync::atomic::Ordering::Relaxed);
    let (code, body) = ask(ADMIN, json!({})).await;
    assert_eq!((code, body["code"].as_str()), (409, Some("flow_over_running")), "{body}");
    n.state.flow_over_remaining.store(-1, std::sync::atomic::Ordering::Relaxed);

    // Recorded: in the data directory and captured to the state volume
    // before the answer; never below `once`.
    let (code, body) = ask(ADMIN, json!({"erase": "none"})).await;
    assert_eq!(code, 202, "{body}");
    assert_eq!(body["ticket"]["erase"], "once");
    assert_eq!(body["ticket"]["state"], "pending");
    let kept: Value = serde_json::from_slice(&std::fs::read(state_dir.path().join("release-ticket.json")).unwrap()).unwrap();
    assert_eq!(kept["id"], body["ticket"]["id"]);
    let get = |p: &'static str| {
        let base = n.base.clone();
        async move { reqwest::Client::new().get(format!("{base}{p}")).bearer_auth(ADMIN).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    assert_eq!(get("/release").await["pending"], true);
    assert_eq!(get("/health").await["release"], "pending");

    // Cancelled: written, not removed (the state store keeps files).
    assert_eq!(call(&n, M::DELETE, "/release", Some(NODE), None).await, 401);
    assert_eq!(call(&n, M::DELETE, "/release", Some(ADMIN), None).await, 200);
    let kept: Value = serde_json::from_slice(&std::fs::read(state_dir.path().join("release-ticket.json")).unwrap()).unwrap();
    assert_eq!(kept["state"], "cancelled");
    assert_eq!(get("/release").await["pending"], false);
    assert!(get("/health").await.get("release").is_none());
    assert_eq!(call(&n, M::DELETE, "/release", Some(ADMIN), None).await, 404);
}
