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
                    let ok = (in_group("storage-admins") || stormcert || user || viewer) && ra["group"] == "storage.storm.io";
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
/// (creating and deleting arrays, volumes, exports) need the admin token.
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
        assert_eq!(call_ui(m.clone(), p, Some(NODE)).await.unwrap().status(), 403, "{m} {p} with the node token");
    }
    assert!(n.state.volume_manager.lock().await.list_volumes().await.iter().all(|v| v.1 != "ui-made"), "nothing was made");
    assert_ne!(call_ui(reqwest::Method::POST, "/ui/volumes", Some(ADMIN)).await.unwrap().status(), 403, "the admin token gets through the gate");
}
