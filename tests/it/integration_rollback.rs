//! Rollback copies (#565): what installs set aside is said (`kind: rollback`,
//! `rollback {of, from, generation}`), and pruned to the newest `keep` of
//! each name, never one in use, a stage's, or anything while a stage waits.
//! A copy known by its name only is pruned only when asked, and a person's
//! `data@east` beside `data` is never one (#720).

use crate::common;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::Slab;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use stormblock::placement::topology::StorageTier;
use stormblock::volume::metadata::RollbackMark;
use stormblock::volume::VolumeManager;

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn serve(dir: &TempDir, vm: VolumeManager) -> (Arc<AppState>, String, tokio::task::JoinHandle<()>) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let h = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}"), h)
}

async fn names(c: &reqwest::Client, url: String) -> Vec<String> {
    let v: Value = c.get(url).send().await.unwrap().json().await.unwrap();
    let mut n: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    n.sort();
    n
}

fn mark(of: &str, from: &str, generation: u32) -> Option<RollbackMark> {
    Some(RollbackMark { of: of.into(), from: from.into(), generation, set_aside_at: 1_794_000_000 + generation as u64 })
}

#[tokio::test]
async fn rollback_copies_are_said_and_pruned_to_the_newest_of_each_name() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("slab.bin").to_string_lossy().to_string();
    let dev = FileDevice::open_with_capacity(&path, 32 * 1024 * 1024).await.unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(Slab::format(Arc::new(dev), SLOT, StorageTier::Hot).await.unwrap()).await;

    // forge1's shape: a data template and three copies of it from before
    // #565, recognised by name only.
    for n in ["cadvisor-data.golden", "cadvisor-data.golden@previous", "cadvisor-data.golden@previous.2", "cadvisor-data.golden@previous.3"] {
        let id = vm.create_volume_any(n, 1 << 20).await.unwrap();
        vm.seal_volume(id, None).await.unwrap();
    }
    // Marked copies of the node's logs; the oldest is exported.
    vm.create_volume_any("logs", 1 << 20).await.unwrap();
    let mut logs = Vec::new();
    for (g, from) in [(1, "11.90"), (2, "11.91"), (3, "11.92"), (4, "11.93")] {
        let id = vm.create_volume_any(&format!("logs@{from}"), 1 << 20).await.unwrap();
        vm.set_rollback(id, mark("logs", from, g)).unwrap();
        logs.push(id);
    }
    // A name with an `@` and nothing beside it: not a copy.
    vm.create_volume_any("odd@name", 1 << 20).await.unwrap();
    // A person's volumes beside their base (#720, finding 6f750e62ad35):
    // never copies. `data@east` carries the mark a start before #720 wrote
    // for it by name.
    vm.create_volume_any("data", 1 << 20).await.unwrap();
    let east = vm.create_volume_any("data@east", 1 << 20).await.unwrap();
    vm.set_rollback(east, Some(RollbackMark { of: "data".into(), from: "east".into(), generation: 1, set_aside_at: 0 })).unwrap();
    vm.create_volume_any("data@west", 1 << 20).await.unwrap();
    // A #122 stage's previous generation: its own bookkeeping, never pruned.
    vm.create_volume_any("svc", 1 << 20).await.unwrap();
    let staged_svc = vm.create_volume_any("svc@11.80", 1 << 20).await.unwrap();
    std::fs::write(
        dir.path().join("release-generations.json"),
        serde_json::to_vec(&serde_json::json!({
            "previous": { "version": "11.80", "volumes": [{ "id": staged_svc.0, "name": "svc" }] }
        }))
        .unwrap(),
    )
    .unwrap();

    let (state, base, server) = serve(&dir, vm).await;
    let c = reqwest::Client::new();
    state.exports.write().await.push(ExportEntry {
        id: uuid::Uuid::new_v4(),
        volume_id: logs[0].0,
        protocol: ExportProtocol::Nvmeof,
        target_id: "nqn.2026-09.lo.test:node".into(),
        status: ExportStatus::Active,
        lun_id: None,
        nsid: Some(3),
        host_nqn: None,
        subsystem: None,
        serve: false,
    });

    // The copies before #565 are marked at start, nothing deleted; the
    // mark on a person's `data@east` is cleared.
    assert_eq!(stormblock::mgmt::rollback::mark_by_name(&state).await, 3);
    assert!(state.volume_manager.lock().await.rollback(&east).is_none(), "data@east is no rollback copy");
    let rb = names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await;
    assert_eq!(
        rb,
        [
            "cadvisor-data.golden@previous",
            "cadvisor-data.golden@previous.2",
            "cadvisor-data.golden@previous.3",
            "logs@11.90",
            "logs@11.91",
            "logs@11.92",
            "logs@11.93",
        ]
    );
    // Out of the Volumes view: neither is something running uses.
    let vols = names(&c, format!("{base}/api/v1/volumes?kind=volume")).await;
    assert!(!vols.iter().any(|n| n.contains("@previous") || n.starts_with("logs@")), "{vols:?}");
    assert!(vols.contains(&"odd@name".to_string()) && vols.contains(&"svc@11.80".to_string()), "{vols:?}");
    for n in ["data", "data@east", "data@west"] {
        assert!(vols.contains(&n.to_string()), "{n} is a volume: {vols:?}");
    }
    let all: Value = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    let by = |n: &str| all["items"].as_array().unwrap().iter().find(|v| v["name"] == n).unwrap().clone();
    let r = by("cadvisor-data.golden@previous.2")["rollback"].clone();
    assert_eq!((r["of"].as_str(), r["from"].as_str(), r["generation"].as_u64()), (Some("cadvisor-data.golden"), Some("previous"), Some(2)));
    assert_eq!(by("logs@11.93")["rollback"]["generation"], 4);
    assert!(by("logs")["rollback"].is_null());

    // The list.
    let l: Value = c.get(format!("{base}/api/v1/rollback")).send().await.unwrap().json().await.unwrap();
    assert_eq!((l["keep"].as_u64(), l["prune"].as_str()), (Some(1), Some("boot-ok")));
    assert_eq!(l["copies"].as_array().unwrap().len(), 7);
    let inferred: Vec<&str> =
        l["copies"].as_array().unwrap().iter().filter(|c| c["inferred"] == true).map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(inferred.len(), 3, "the copies known by name only: {l}");
    assert!(inferred.iter().all(|n| n.starts_with("cadvisor-data.golden@")), "{inferred:?}");

    // A dry run says what would go, and deletes nothing.
    let r = c.post(format!("{base}/api/v1/rollback/prune")).json(&serde_json::json!({ "dry_run": true })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let d: Value = r.json().await.unwrap();
    let pick = |v: &Value| -> Vec<String> {
        let mut n: Vec<String> = v.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
        n.sort();
        n
    };
    // Only what an install marked is in the plan; the copies known by name
    // only are skipped and say why (#720).
    assert_eq!(pick(&d["pruned"]), ["logs@11.91", "logs@11.92"]);
    assert_eq!(pick(&d["kept"]), ["logs@11.93"]);
    assert_eq!(
        pick(&d["skipped"]),
        ["cadvisor-data.golden@previous", "cadvisor-data.golden@previous.2", "cadvisor-data.golden@previous.3", "logs@11.90"]
    );
    let why = |n: &str| d["skipped"].as_array().unwrap().iter().find(|x| x["name"] == n).unwrap()["why"].as_str().unwrap().to_string();
    assert!(why("cadvisor-data.golden@previous").contains("include_inferred"));
    assert!(why("logs@11.90").contains("in use"), "the exported one is never pruned");
    assert_eq!(names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await.len(), 7);

    // A stage waiting to be activated: an upgrade not confirmed, nothing goes.
    let gens = dir.path().join("release-generations.json");
    let before = std::fs::read(&gens).unwrap();
    std::fs::write(
        &gens,
        serde_json::to_vec(&serde_json::json!({
            "previous": { "version": "11.80", "volumes": [{ "id": staged_svc.0, "name": "svc" }] },
            "staged": { "version": "11.94", "volumes": [] }
        }))
        .unwrap(),
    )
    .unwrap();
    let r = c.post(format!("{base}/api/v1/rollback/prune")).send().await.unwrap();
    assert_eq!(r.status(), 409);
    let d: Value = r.json().await.unwrap();
    assert!(d["refused"].as_str().unwrap().contains("11.94"), "{d}");
    assert_eq!(names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await.len(), 7);
    std::fs::write(&gens, before).unwrap();

    // The prune, at the node's keep, as a boot-ok runs it: the newest marked
    // copy of each name stays, the copies known by name only all stay.
    let r = c.post(format!("{base}/api/v1/rollback/prune")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await,
        [
            "cadvisor-data.golden@previous",
            "cadvisor-data.golden@previous.2",
            "cadvisor-data.golden@previous.3",
            "logs@11.90",
            "logs@11.93"
        ]
    );
    let left = names(&c, format!("{base}/api/v1/volumes?kind=all")).await;
    for n in ["cadvisor-data.golden", "logs", "odd@name", "svc", "svc@11.80", "data", "data@east", "data@west"] {
        assert!(left.contains(&n.to_string()), "{n} kept: {left:?}");
    }

    // Asked for with include_inferred, after the list: the older copies
    // known by name go too. A person's volumes stay whatever is asked.
    let r = c
        .post(format!("{base}/api/v1/rollback/prune"))
        .json(&serde_json::json!({ "include_inferred": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await,
        ["cadvisor-data.golden@previous.3", "logs@11.90", "logs@11.93"]
    );

    // keep = 0, asked: every copy but the one in use goes.
    let r = c
        .post(format!("{base}/api/v1/rollback/prune"))
        .json(&serde_json::json!({ "keep": 0, "include_inferred": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(names(&c, format!("{base}/api/v1/volumes?kind=rollback")).await, ["logs@11.90"]);
    let left = names(&c, format!("{base}/api/v1/volumes?kind=all")).await;
    for n in ["data", "data@east", "data@west"] {
        assert!(left.contains(&n.to_string()), "{n} kept: {left:?}");
    }

    // Renamed back to its plain name, a copy is no longer one.
    state.exports.write().await.clear();
    {
        let mut vm = state.volume_manager.lock().await;
        let plain = vm.find_volume("logs").await.unwrap();
        vm.delete_volume(plain).await.unwrap();
        vm.rename_volume(logs[0], "logs").await.unwrap();
        assert!(vm.rollback(&logs[0]).is_none());
    }
    server.abort();
}

/// An install's prune is the admin's verb.
#[test]
fn the_prune_is_destructive() {
    use axum::http::Method;
    use stormblock::serve::api::{classify, Class};
    assert!(matches!(classify(&Method::POST, "/api/v1/rollback/prune", None), Class::Destructive));
    assert!(!matches!(classify(&Method::GET, "/api/v1/rollback", None), Class::Destructive));
}
