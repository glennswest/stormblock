//! `/api/v1/moves` — the volume-level move over HTTP (#20).
//!
//! The unit tests in `volume::relocate` cover the mechanism. These cover the
//! parts that only exist at the API layer: the offline guard, the two-call
//! commit shape, and the ledger surviving a restart.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use stormblock::drive::BlockDevice;
use stormblock::fs::ext4;
use stormblock::fs::files::SeedFile;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeId, VolumeManager};

const SLOT: u64 = 1024 * 1024;

async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 1024 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let registry = vm.registry().clone();
    let gem = vm.gem().clone();

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    Arc::new(AppState::new(config, vm, registry, gem))
}

async fn start(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = stormblock::mgmt::api::router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

/// A formatted volume with something on it worth not losing.
async fn seeded(state: &AppState, name: &str, size: u64) -> VolumeId {
    let id = state.volume_manager.lock().await.create_volume_any(name, size).await.unwrap();
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    ext4::format(&dev, &ext4::Ext4Params::default()).await.unwrap();
    stormblock::fs::files::write_files(
        &dev,
        &[
            SeedFile::new("/etc/hostname", b"storm".to_vec()),
            SeedFile::new("/var/blob.bin", vec![0x5Cu8; 200 * 1024]),
        ],
    )
    .await
    .unwrap();
    id
}

/// The whole shape: copy, verify, source intact, commit, source gone.
#[tokio::test]
async fn a_move_is_two_calls_and_the_source_survives_the_first() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let source = seeded(&state, "var", 128 * 1024 * 1024).await;
    let (base, server) = start(state.clone()).await;
    let c = reqwest::Client::new();

    let resp = c
        .post(format!("{base}/api/v1/moves"))
        .json(&json!({
            "volume_id": source.0,
            "target_name": "var-small",
            "target_size": "48M",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    let mv = &body["move"];
    assert_eq!(mv["state"], "ready_to_commit");
    assert_eq!(mv["verified"], true);
    assert!(mv["files_copied"].as_u64().unwrap() > 0, "{body}");
    let move_id = mv["id"].as_str().unwrap().to_string();
    let target: uuid::Uuid = mv["target_volume_id"].as_str().unwrap().parse().unwrap();

    // Nothing destructive has happened yet: source, target and the way back
    // all exist at once.
    {
        let vm = state.volume_manager.lock().await;
        assert!(vm.get_volume(&source).is_some(), "the source is still here");
        assert!(vm.get_volume(&VolumeId(target)).is_some());
        let rollback: uuid::Uuid =
            mv["rollback_snapshot_id"].as_str().unwrap().parse().unwrap();
        assert!(vm.get_volume(&VolumeId(rollback)).is_some());
    }

    // Listed, and gettable by id.
    let listed: Value =
        c.get(format!("{base}/api/v1/moves")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["count"], 1);
    let one: Value = c
        .get(format!("{base}/api/v1/moves/{move_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["id"], move_id.as_str());

    // Commit is the only thing that removes anything.
    let done: Value = c
        .post(format!("{base}/api/v1/moves/{move_id}/commit"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["state"], "committed");
    {
        let vm = state.volume_manager.lock().await;
        assert!(vm.get_volume(&source).is_none(), "the source went at commit");
        assert!(vm.get_volume(&VolumeId(target)).is_some(), "the data is on the target");
    }

    // Committing twice is a conflict, not a second delete.
    let again = c
        .post(format!("{base}/api/v1/moves/{move_id}/commit"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);

    server.abort();
}

/// A move copies a static filesystem, so a volume something is still serving
/// is refused rather than copied halfway.
#[tokio::test]
async fn an_exported_volume_cannot_be_moved() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let source = seeded(&state, "var", 128 * 1024 * 1024).await;

    state.exports.write().await.push(ExportEntry {
        id: uuid::Uuid::new_v4(),
        volume_id: source.0,
        protocol: ExportProtocol::Iscsi,
        target_id: "iqn.2024.io.stormblock:var".to_string(),
        status: ExportStatus::Active,
        lun_id: Some(0),
        nsid: None,
    });

    let (base, server) = start(state.clone()).await;
    let c = reqwest::Client::new();

    let resp = c
        .post(format!("{base}/api/v1/moves"))
        .json(&json!({
            "volume_id": source.0,
            "target_name": "var-small",
            "target_size": "48M",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"].as_str().or_else(|| body["message"].as_str()).unwrap_or_default();
    assert!(msg.contains("still served"), "{body}");

    // And nothing was created on the way to refusing.
    let names: Vec<String> = state
        .volume_manager
        .lock()
        .await
        .list_volumes()
        .await
        .into_iter()
        .map(|(_, n, _, _)| n)
        .collect();
    assert_eq!(names, vec!["var".to_string()], "{names:?}");

    server.abort();
}

/// Aborting is free: the target goes and the source is exactly as it was.
#[tokio::test]
async fn aborting_returns_everything_to_where_it_started() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let source = seeded(&state, "var", 128 * 1024 * 1024).await;
    let (base, server) = start(state.clone()).await;
    let c = reqwest::Client::new();

    let body: Value = c
        .post(format!("{base}/api/v1/moves"))
        .json(&json!({
            "volume_id": source.0,
            "target_name": "var-small",
            "target_size": "48M",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let move_id = body["move"]["id"].as_str().unwrap().to_string();
    let target: uuid::Uuid = body["move"]["target_volume_id"].as_str().unwrap().parse().unwrap();

    let done: Value = c
        .post(format!("{base}/api/v1/moves/{move_id}/abort"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["state"], "aborted");

    let vm = state.volume_manager.lock().await;
    assert!(vm.get_volume(&VolumeId(target)).is_none());
    assert!(vm.get_volume(&source).is_some());
    drop(vm);

    // The source still reads what it always did.
    let dev = state.volume_manager.lock().await.get_volume(&source).unwrap();
    let v = fio_ext4::Volume::open(ext4::VolumeDevice::opaque(dev)).await.unwrap();
    assert_eq!(v.read("/etc/hostname").await.unwrap(), b"storm");

    server.abort();
}

/// A move interrupted between its copy and its commit has to still be
/// nameable afterwards — otherwise a restart leaves two volumes and no record
/// of which is which.
#[tokio::test]
async fn the_move_ledger_survives_a_restart() {
    let dir = TempDir::new().unwrap();
    let move_id;
    let target;
    {
        let state = setup(&dir).await;
        let source = seeded(&state, "var", 128 * 1024 * 1024).await;
        let (base, server) = start(state.clone()).await;
        let c = reqwest::Client::new();

        let body: Value = c
            .post(format!("{base}/api/v1/moves"))
            .json(&json!({
                "volume_id": source.0,
                "target_name": "var-small",
                "target_size": "48M",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        move_id = body["move"]["id"].as_str().unwrap().to_string();
        target = body["move"]["target_volume_id"].as_str().unwrap().to_string();
        server.abort();
        // The process ends here, mid-move.
    }

    // A fresh AppState over the same data dir — a restart.
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    let vm = VolumeManager::new(SLOT);
    let registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let reborn = Arc::new(AppState::new(config, vm, registry, gem));

    let moves = reborn.moves.read().await;
    let mv = moves.get(&move_id.parse().unwrap()).expect("the move came back");
    assert_eq!(mv.state, stormblock::volume::relocate::MoveState::ReadyToCommit);
    assert_eq!(mv.target.to_string(), target);
    assert!(mv.verified);
}
