//! Emulated drives for scale tests (#208, stormcos#92): drives that report
//! 256 TiB or 1 PiB, store only what is written, enrol like real ones, and
//! fail on command.

use crate::common;
use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::emulated::{self, EmulatedSpec};
use stormblock::drive::slab::{Slab, SlabFormat};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::{DriveConfig, StormBlockConfig};
use stormblock::mgmt::AppState;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::redundancy::RedundancyPolicy;
use stormblock::volume::{CreateOptions, HealthState, VolumeManager};
use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1 << 20;
const TIB: u64 = 1 << 40;
const PIB: u64 = 1 << 50;

fn uniq(p: &str) -> String {
    format!("{p}-{}", uuid::Uuid::new_v4().simple())
}

/// A mirrored volume across three 256 TiB emulated drives: one drive fails,
/// reads go on, the volume is rebuilt onto the third, and every byte is
/// there. The drives store only what was written.
#[tokio::test]
async fn a_mirror_on_256_tib_drives_survives_one_failing_and_rebuilds() {
    let mut mgr = VolumeManager::new(MIB);
    let mut drives = Vec::new();
    for _ in 0..3 {
        let uri = format!("emulated://{}?size=256T", uniq("m"));
        let dev = stormblock::drive::open_path(&uri, false).await.unwrap();
        assert_eq!(dev.capacity_bytes(), 256 * TIB);
        let t = std::time::Instant::now();
        let slab = Slab::format_with(dev.clone(), SlabFormat::new(MIB, StorageTier::Hot)).await.unwrap();
        assert!(t.elapsed().as_secs() < 30, "formatting a 256 TiB slab took {:?}", t.elapsed());
        assert!(slab.total_slots() > (256 * TIB / MIB) * 99 / 100);
        mgr.add_slab(slab).await;
        drives.push(uri);
    }
    let id = mgr
        .create_volume_with("m", 64 * MIB, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
        .await
        .unwrap();
    let v = mgr.get_volume(&id).unwrap();
    let data: Vec<u8> = (0..8 * MIB as usize).map(|i| (i % 249) as u8).collect();
    v.write(0, &data).await.unwrap();
    v.flush().await.unwrap();

    // The drive under one leg of extent 0 fails.
    let leg_slab = {
        let gem = mgr.gem().read().await;
        gem.lookup(id, 0).unwrap().primary().slab_id
    };
    let failing = mgr.registry().read().await.get(&leg_slab).unwrap().device().id().path.clone();
    assert!(emulated::set_failed(&failing, true));

    let mut back = vec![0u8; data.len()];
    v.read(0, &mut back).await.unwrap();
    assert_eq!(back, data, "the other leg answers");
    // A write in place reaches both legs: the failed one is found out.
    v.write(0, &data[..MIB as usize]).await.unwrap();
    v.flush().await.unwrap();
    let h = mgr.health(&id).await.unwrap();
    let failed = mgr.get_volume_handle(&id).unwrap().failed_slabs();
    assert_eq!(h.state, HealthState::Degraded, "{h:?}; failed slabs {failed:?}; leg slab {leg_slab:?}");

    let report = mgr.resync_volume(id, false).await.unwrap();
    assert!(report.legs_rebuilt > 0 && report.errors.is_empty(), "{report:?}");
    assert_eq!(mgr.health(&id).await.unwrap().state, HealthState::Healthy);
    // Every byte from the rebuilt pair, the failed drive still failed.
    let mut back = vec![0u8; data.len()];
    v.read(0, &mut back).await.unwrap();
    assert_eq!(back, data);
    let gem = mgr.gem().read().await;
    for vext in 0..8u64 {
        let loc = gem.lookup(id, vext).unwrap();
        assert_eq!(loc.leg_count(), 2);
        assert!(loc.legs().all(|l| l.slab_id != leg_slab), "extent {vext} still on the failed drive");
    }
    drop(gem);

    // 768 TiB of drives; what they hold is the data, its copies and the
    // slabs' records - megabytes.
    let stored: u64 = drives
        .iter()
        .map(|u| emulated::get(&EmulatedSpec::parse(u).unwrap().unwrap().name).unwrap().stored_bytes())
        .sum();
    assert!(stored < 256 * MIB, "the drives hold {stored} bytes");
}

/// `[[drives]] kind = "emulated"` spells an `emulated://` path.
#[test]
fn a_config_entry_names_an_emulated_drive() {
    let d = DriveConfig {
        kind: Some("emulated".into()),
        size: Some("1P".into()),
        backing: Some("/var/tmp/emu".into()),
        ..Default::default()
    };
    let uri = d.device_path(3).unwrap();
    assert_eq!(uri, "emulated://emu3?size=1P&backing=/var/tmp/emu");
    let s = EmulatedSpec::parse(&uri).unwrap().unwrap();
    assert_eq!(s.size, PIB);
    assert!(DriveConfig { kind: Some("emulated".into()), ..Default::default() }.device_path(0).is_err());
    assert!(DriveConfig { kind: Some("tape".into()), ..Default::default() }.device_path(0).is_err());
    assert_eq!(DriveConfig { path: "/dev/sdb".into(), ..Default::default() }.device_path(0).unwrap(), "/dev/sdb");
}

/// Over the API: a 1 PiB drive opens and is flagged emulated, a slab is
/// formatted on it, and it fails and recovers on command. A real drive is
/// not the emulator's to fail.
#[tokio::test]
async fn an_emulated_petabyte_drive_enrols_and_fails_over_the_api() {
    let dir = TempDir::new().unwrap();
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let vm = VolumeManager::new(MIB);
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let api = format!("http://{addr}/api/v1");
    let c = reqwest::Client::new();

    let uri = format!("emulated://{}?size=1P", uniq("api"));
    let r = c.post(format!("{api}/drives")).json(&json!({ "path": uri })).send().await.unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let d: Value = r.json().await.unwrap();
    assert_eq!(d["device_type"], "Emulated");
    assert_eq!(d["capacity_bytes"], PIB);
    assert_eq!(d["emulated"]["failed"], false);
    let id = d["uuid"].as_str().unwrap().to_string();

    let t = std::time::Instant::now();
    let r = c
        .post(format!("{api}/slabs"))
        .json(&json!({ "device_path": uri, "role": "data" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let s: Value = r.json().await.unwrap();
    assert!(s["total_slots"].as_u64().unwrap() > (PIB / MIB) * 99 / 100, "{s}");
    eprintln!("1 PiB slab formatted in {:?}", t.elapsed());

    let r = c.post(format!("{api}/drives/{id}/emulate")).json(&json!({ "failed": true })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let listed: Value = c.get(format!("{api}/drives")).send().await.unwrap().json().await.unwrap();
    let mine = listed["items"].as_array().unwrap().iter().find(|x| x["uuid"] == id.as_str()).unwrap().clone();
    assert_eq!(mine["emulated"]["failed"], true, "{mine}");
    let dev = stormblock::drive::open_path(&uri, false).await.unwrap();
    assert!(dev.read(0, &mut [0u8; 4096]).await.is_err(), "a failed drive answers EIO");
    let r = c.post(format!("{api}/drives/{id}/emulate")).json(&json!({ "failed": false })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    dev.read(0, &mut [0u8; 4096]).await.unwrap();

    let file = dir.path().join("real.img");
    let _ = stormblock::drive::filedev::FileDevice::open_with_capacity(file.to_str().unwrap(), 64 * MIB).await.unwrap();
    let r = c.post(format!("{api}/drives")).json(&json!({ "path": file.to_str().unwrap() })).send().await.unwrap();
    assert_eq!(r.status(), 201);
    let real: Value = r.json().await.unwrap();
    assert!(real.get("emulated").is_none());
    let r = c
        .post(format!("{api}/drives/{}/emulate", real["uuid"].as_str().unwrap()))
        .json(&json!({ "failed": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

/// #300 (mkfs.ext4.rs#10): an ext4 template of 1 PiB formatted in core, as
/// `POST /api/v1/fstemplates` makes one (create, format, seal: the seal runs
/// a full check), on two emulated 1 PiB drives backed by directories, so the
/// process's memory is the engine's and the formatter's, not the data's.
/// mkfs-ext4 v3.0.0 could not format 1 PiB in 32 GiB; v4's format streams.
/// `STORMBLOCK_PIB_TEST_SIZE` (e.g. 64T) runs it smaller.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "minutes: run on its own (cargo test --release --test it ... -- --ignored)"]
async fn an_ext4_template_of_a_petabyte_is_formatted_in_core() {
    let size = std::env::var("STORMBLOCK_PIB_TEST_SIZE")
        .ok()
        .and_then(|s| emulated::parse_size(&s))
        .unwrap_or(PIB);
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(MIB);
    let mut drives = Vec::new();
    for d in 0..2 {
        let uri = format!("emulated://{}?size=1P&backing={}/d{d}", uniq("pib"), dir.path().display());
        let dev = stormblock::drive::open_path(&uri, false).await.unwrap();
        let slab = Slab::format_with(dev, SlabFormat::new(MIB, StorageTier::Hot).with_auto_metadata(PIB)).await.unwrap();
        vm.add_slab(slab).await;
        drives.push(uri);
    }
    let vm = stormblock::lockwatch::TrackedMutex::new(vm);
    let store = tokio::sync::Mutex::new(stormblock::fs::template::TemplateStore::default());
    let t = std::time::Instant::now();
    let tmpl = stormblock::fs::template::create(&vm, &store, &stormblock::fs::template::TemplateSpec::new("pib", size))
        .await
        .expect("the template");
    let took = t.elapsed();
    let held: u64 = drives
        .iter()
        .map(|u| emulated::get(&EmulatedSpec::parse(u).unwrap().unwrap().name).unwrap().stored_bytes())
        .sum();
    eprintln!("{size}-byte ext4 template ready in {took:?}; the drives hold {} MiB", held >> 20);
    assert!(matches!(tmpl.state, stormblock::fs::template::TemplateState::Ready), "{:?}", tmpl.state);
}
