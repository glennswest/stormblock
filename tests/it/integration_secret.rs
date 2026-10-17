//! Secret volumes (#74, decision A): what stormcos's `stormcert-data` (the
//! node CA's key) becomes. Never cloned, snapshotted, composed, copied,
//! exported or read out over the API; attached as a local ublk device by one
//! consumer; erased on delete, whatever the node's default; sealed and
//! rotated by a synonym, the engine keeping one previous copy.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::volume::{VolumeError, VolumeId, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1024 * 1024;

fn holds(raw: &[u8], pat: &[u8]) -> bool {
    raw.windows(pat.len()).any(|w| w == pat)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_volume_is_never_copied_or_exposed_and_is_erased_when_deleted() {
    let dir = TempDir::new().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 96 * MIB).await.unwrap(),
    );
    let mut vm = VolumeManager::new(MIB);
    vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("engine").display().to_string());
    std::fs::create_dir_all(dir.path().join("engine")).unwrap();
    // A node that does not erase by default: a secret's delete still does.
    config.erase.default = stormblock::drive::erase::EraseLevel::None;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    state.start_eraser().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let base = format!("http://{addr}/api/v1");
    let c = reqwest::Client::new();

    // Made secret on create; the key written as stormcert would.
    let r = c.post(format!("{base}/volumes")).json(&serde_json::json!({"name": "stormcert-data", "size": "8M", "secret": true})).send().await.unwrap();
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["secret"], true, "{v}");
    let id: uuid::Uuid = v["id"].as_str().unwrap().parse().unwrap();
    let key = b"-----NODE-CA-PRIVATE-KEY-74-----";
    {
        let d = state.volume_manager.lock().await.get_volume(&VolumeId(id)).unwrap();
        let data: Vec<u8> = key.iter().copied().cycle().take(2 * MIB as usize).collect();
        d.write(0, &data).await.unwrap();
        d.flush().await.unwrap();
    }
    let other = state.volume_manager.lock().await.create_volume_any("other", 8 * MIB).await.unwrap();
    let got: serde_json::Value = c.get(format!("{base}/volumes/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["secret"], true, "{got}");
    let list: serde_json::Value = c.get(format!("{base}/volumes")).send().await.unwrap().json().await.unwrap();
    let mine = list["items"].as_array().unwrap().iter().find(|x| x["id"] == id.to_string()).unwrap().clone();
    assert_eq!(mine["secret"], true, "the listing says so: {mine}");
    let o = list["items"].as_array().unwrap().iter().find(|x| x["id"] == other.0.to_string()).unwrap().clone();
    assert!(o.get("secret").is_none(), "an ordinary volume says nothing: {o}");

    // Every door that copies or exposes it: 409, saying why.
    let refused = |r: reqwest::Response, door: &'static str| async move {
        let code = r.status().as_u16();
        let body = r.text().await.unwrap();
        assert_eq!(code, 409, "{door}: {body}");
        assert!(body.contains("secret"), "{door} says why: {body}");
    };
    refused(c.post(format!("{base}/volumes/{id}/clone")).json(&serde_json::json!({"name": "copy"})).send().await.unwrap(), "clone").await;
    refused(
        c.post(format!("{base}/volumes")).json(&serde_json::json!({"name": "copy2", "from_template": id.to_string()})).send().await.unwrap(),
        "create from it",
    )
    .await;
    refused(c.post(format!("{base}/volumes/{id}/attach")).json(&serde_json::json!({"transport": "nvme_tcp"})).send().await.unwrap(), "NVMe/TCP attach").await;
    refused(c.post(format!("{base}/volumes/{id}/attach")).json(&serde_json::json!({"transport": "vhost_user_blk"})).send().await.unwrap(), "vhost-user-blk").await;
    refused(c.post(format!("{base}/volumes/{id}/attach")).json(&serde_json::json!({"node": "elsewhere"})).send().await.unwrap(), "another node").await;
    refused(c.post(format!("{base}/exports")).json(&serde_json::json!({"volume_id": id, "protocol": "nvmeof"})).send().await.unwrap(), "export").await;
    refused(c.get(format!("{base}/volumes/{id}/content")).send().await.unwrap(), "content").await;
    refused(c.get(format!("{base}/volumes/{id}/files?path=/etc/x")).send().await.unwrap(), "file read").await;
    refused(
        c.post(format!("{base}/moves")).json(&serde_json::json!({"volume_id": id, "target_size": "16M"})).send().await.unwrap(),
        "move",
    )
    .await;
    let lun = c.post(format!("{base}/luns")).json(&serde_json::json!({"backing": {"type": "volume", "volume_id": id}})).send().await.unwrap();
    assert!(!lun.status().is_success(), "an iSCSI LUN of it is refused");
    assert!(lun.text().await.unwrap().contains("secret"));
    // A local ublk device is the one attach; here there is no ublk, and it says so.
    let r = c.post(format!("{base}/volumes/{id}/attach")).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("local ublk device only"));
    // And in the manager, for every caller that is not an HTTP door.
    {
        let mut vm = state.volume_manager.lock().await;
        let s = VolumeId(id);
        let secret = |r: Result<VolumeId, VolumeError>| matches!(r, Err(VolumeError::Secret { .. }));
        assert!(secret(vm.create_snapshot(s, "snap").await), "snapshot");
        assert!(matches!(vm.create_snapshots_atomic(&[(s, "g1".into()), (other, "g2".into())]).await, Err(VolumeError::Secret { .. })), "group snapshot");
        assert!(secret(vm.compose_volume("composed", None, &[(s, 0)]).await), "composition");
        assert!(secret(vm.copy_volume(s, "copied", stormblock::drive::slab::SlabRole::Data).await), "copy");
        assert!(vm.create_snapshot(other, "ordinary-snap").await.is_ok(), "an ordinary volume clones as before");
    }

    // One way: never made ordinary again; the mark is the admin's.
    let r = c.put(format!("{base}/volumes/{id}/secret")).json(&serde_json::json!({"secret": false})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    state.set_auth(stormblock::serve::api::AuthConfig { api_token: Some("tok".into()), admin_token: Some("adm".into()), audit_only: false });
    let mark = |tok: &'static str| c.put(format!("{base}/volumes/{}/secret", other.0)).bearer_auth(tok).json(&serde_json::json!({"secret": true})).send();
    assert_eq!(mark("tok").await.unwrap().status(), 401, "the node token does not mark");
    assert_eq!(mark("adm").await.unwrap().status(), 200);
    let g: serde_json::Value = c.get(format!("{base}/volumes/{}/secret", other.0)).bearer_auth("tok").send().await.unwrap().json().await.unwrap();
    assert_eq!(g["secret"], true);
    state.set_auth(stormblock::serve::api::AuthConfig { api_token: None, admin_token: None, audit_only: false });

    // Rotation: new material sealed, a synonym re-pointed; one previous kept.
    let mut gen = Vec::new();
    for n in ["ca-1", "ca-2", "ca-3"] {
        let v: serde_json::Value = c.post(format!("{base}/volumes")).json(&serde_json::json!({"name": n, "size": "4M", "secret": true})).send().await.unwrap().json().await.unwrap();
        let vid = v["id"].as_str().unwrap().to_string();
        let r = c.post(format!("{base}/volumes/{vid}/seal")).json(&serde_json::json!({"force": true})).send().await.unwrap();
        assert!(r.status().is_success(), "a secret seals: {}", r.text().await.unwrap());
        gen.push(vid);
    }
    let r = c.post(format!("{base}/synonyms")).json(&serde_json::json!({"namespace": "secrets", "name": "node-ca", "volume": gen[0]})).send().await.unwrap();
    assert!(r.status().is_success());
    let r: serde_json::Value = c.put(format!("{base}/synonyms/secrets/node-ca")).json(&serde_json::json!({"volume": gen[1]})).send().await.unwrap().json().await.unwrap();
    assert!(r.get("rotated_out").is_none(), "one previous is kept: {r}");
    let r: serde_json::Value = c.put(format!("{base}/synonyms/secrets/node-ca")).json(&serde_json::json!({"volume": gen[2]})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["rotated_out"], serde_json::json!([gen[0]]), "the one before it goes: {r}");
    let vm = state.volume_manager.lock().await;
    assert!(vm.get_volume_handle(&VolumeId(gen[0].parse().unwrap())).is_none(), "deleted");
    assert!(vm.get_volume_handle(&VolumeId(gen[1].parse().unwrap())).is_some(), "the previous kept, for a rollback");
    drop(vm);
    let back = c.post(format!("{base}/synonyms/secrets/node-ca/rollback")).send().await.unwrap();
    assert!(back.status().is_success(), "a rollback to the previous works");

    // Deleted: erased, though this node erases nothing by default.
    let r = c.delete(format!("{base}/volumes/{id}")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut all = vec![0u8; dev.capacity_bytes() as usize];
        dev.read(0, &mut all).await.unwrap();
        if !holds(&all, key) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the key is still on the drive");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server.abort();
}
