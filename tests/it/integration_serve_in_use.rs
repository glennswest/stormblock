//! An ephemeral export withdrawn while the node still has its volume as a
//! device keeps the volume (#267).
//!
//! pvetest1, 11.73: the kubelet pulled an image as a registry clone, attached
//! it over ublk and mounted it under its containers. The registry reaped the
//! clone (claimed and never bound), the export's portal had no session — the
//! node reads it over ublk — so the serving GC withdrew the export and
//! deleted the volume under the mounted filesystem. The containers'
//! executables then read zeros and other volumes' data, and crashed.

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::serve::ctx::ServeContext;
use stormblock::serve::status::MkStatus;
use stormblock::serve::wiring::{WireProto, WireState, Wiring, WiringTable};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::thin::VolumeError;
use stormblock::volume::{VolumeId, VolumeManager};
use tempfile::TempDir;

use crate::common;

const SLOT: u64 = 4096;

async fn node(dir: &TempDir) -> (Arc<AppState>, Arc<ServeContext>) {
    let devices = common::create_file_devices(dir, 2, 32 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config.clone(), vm, reg, gem));

    let cfg = config.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let wiring = WiringTable::load(&cfg.data_dir);
    let reactor = Arc::new(ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false }));
    let ctx = Arc::new(ServeContext::new(
        cfg,
        state.clone(),
        Arc::new(MkStatus::new()),
        None,
        reactor,
        wiring,
    ));
    (state, ctx)
}

/// The volume manager refuses to delete a held volume, whichever path asks,
/// and says who holds it.
#[tokio::test]
async fn a_held_volume_is_not_deleted() {
    let dir = TempDir::new().unwrap();
    let (state, _ctx) = node(&dir).await;
    let mut vm = state.volume_manager.lock().await;
    let id = vm.create_volume_any("clone-busybox-1", 1 << 20).await.unwrap();
    vm.holds().hold(id.0, "ublk device /dev/ublkb9");

    match vm.delete_volume(id).await {
        Err(VolumeError::InUse { by, .. }) => assert_eq!(by, vec!["ublk device /dev/ublkb9".to_string()]),
        other => panic!("a held volume must not be deleted: {other:?}"),
    }
    assert!(vm.get_volume(&id).is_some(), "still there");

    vm.holds().release(id.0, "ublk device /dev/ublkb9");
    vm.delete_volume(id).await.expect("released: deleted");
}

/// The #267 chain at the serving layer: the export is withdrawn, the volume
/// it was ephemeral for is still a device on the node, so it stays, data and
/// all, and goes on the first pass after the device lets go of it.
#[tokio::test]
async fn an_ephemeral_volume_in_use_outlives_its_withdrawn_export() {
    let dir = TempDir::new().unwrap();
    let (state, ctx) = node(&dir).await;
    let (id, handle) = {
        let mut vm = state.volume_manager.lock().await;
        let id = vm.create_volume_any("clone-test-1", 1 << 20).await.unwrap();
        (id, vm.get_volume(&id).unwrap())
    };
    let pattern: Vec<u8> = (0..(256 * 1024)).map(|i| (i % 251) as u8).collect();
    handle.write(0, &pattern).await.unwrap();
    handle.flush().await.unwrap();

    // What the kubelet's ublk attach takes.
    let holds = state.volume_manager.lock().await.holds();
    holds.hold(id.0, "ublk device /dev/ublkb5");

    // What the registry's drop left: the export withdrawn, the row ephemeral.
    let export_id = uuid::Uuid::new_v4();
    ctx.wiring.lock().await.exports.push(Wiring {
        export_id,
        volume_id: id.0,
        protocol: WireProto::Nvmeof,
        lun: None,
        portal_port: 0,
        iqn: String::new(),
        nqn: None,
        state: WireState::Withdrawn,
        ephemeral: true,
        host_nqn: None,
    });

    stormblock::serve::reconcile::pass(&ctx).await.unwrap();
    assert!(state.volume_manager.lock().await.get_volume(&id).is_some(), "kept while in use");
    assert!(
        ctx.wiring.lock().await.exports.iter().any(|r| r.export_id == export_id),
        "the withdrawn row is kept, so the volume is not leaked"
    );
    let mut back = vec![0u8; pattern.len()];
    handle.read(0, &mut back).await.unwrap();
    assert_eq!(back, pattern, "the mounted device still reads what it held");

    // A second pass changes nothing while it is still held.
    stormblock::serve::reconcile::pass(&ctx).await.unwrap();
    assert!(state.volume_manager.lock().await.get_volume(&id).is_some());

    // Detached: the next pass deletes it and drops the row.
    holds.release(id.0, "ublk device /dev/ublkb5");
    stormblock::serve::reconcile::pass(&ctx).await.unwrap();
    assert!(state.volume_manager.lock().await.get_volume(&VolumeId(id.0)).is_none(), "deleted once detached");
    assert!(!ctx.wiring.lock().await.exports.iter().any(|r| r.export_id == export_id));
}
