//! The pool rebalanced onto an added slab (#154): planned first, then moved
//! in the background, every byte kept, a volume's legs kept apart.

use crate::common;
use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::Slab;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::redundancy::RedundancyPolicy;
use stormblock::volume::{CreateOptions, VolumeManager};
use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn file_slab(dir: &TempDir, tag: &str) -> Slab {
    let path = dir.path().join(format!("{tag}.bin")).to_string_lossy().to_string();
    let dev = FileDevice::open_with_capacity(&path, 2 << 20).await.unwrap();
    Slab::format(Arc::new(dev), SLOT, StorageTier::Hot).await.unwrap()
}

async fn serve(dir: &TempDir, vm: VolumeManager) -> (Arc<AppState>, String) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}/api/v1/slabs/rebalance"))
}

/// Used slots per slab, by id.
async fn used(state: &AppState) -> std::collections::HashMap<String, u64> {
    let reg = state.slab_registry.read().await;
    reg.iter().map(|(id, s)| (id.0.to_string(), s.total_slots() - s.free_slots())).collect()
}

async fn wait_done(c: &reqwest::Client, url: &str) -> Value {
    for _ in 0..600 {
        let st: Value = c.get(url).send().await.unwrap().json().await.unwrap();
        if st["state"] != "running" {
            return st;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the rebalance did not finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_added_slab_takes_its_share_and_every_byte_stays() {
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(file_slab(&dir, "a").await).await;
    let id = vm.create_volume_with("v", 64 * SLOT, CreateOptions::default()).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    for e in 0..40u64 {
        v.write(e * SLOT, &vec![e as u8 + 1; SLOT as usize]).await.unwrap();
    }
    v.flush().await.unwrap();
    // The drive added later: empty.
    vm.add_slab(file_slab(&dir, "b").await).await;
    let (state, url) = serve(&dir, vm).await;
    let c = reqwest::Client::new();

    let before = used(&state).await;
    let dry: Value = c.post(&url).json(&json!({})).send().await.unwrap().json().await.unwrap();
    assert_eq!(dry["dry_run"], true, "a dry run is the default: {dry:#}");
    let planned = dry["plan"]["moves"].as_u64().unwrap();
    assert!((15..=25).contains(&planned), "about half of 40 extents: {dry:#}");
    assert_eq!(used(&state).await, before, "a dry run moves nothing");

    let r = c.post(&url).json(&json!({ "dry_run": false })).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let st = wait_done(&c, &url).await;
    assert_eq!(st["state"], "done", "{st:#}");
    assert_eq!(st["moved"].as_u64().unwrap(), planned, "{st:#}");
    assert_eq!(st["failed"], 0);
    let after: Vec<u64> = used(&state).await.into_values().collect();
    assert!(after.iter().max().unwrap() - after.iter().min().unwrap() <= 2, "even: {after:?}");

    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for e in 0..40u64 {
        let mut b = vec![0u8; SLOT as usize];
        v.read(e * SLOT, &mut b).await.unwrap();
        assert!(b.iter().all(|x| *x == e as u8 + 1), "extent {e}");
    }
    // Balanced now: the next plan has nothing to do.
    let again: Value = c.post(&url).json(&json!({})).send().await.unwrap().json().await.unwrap();
    assert_eq!(again["plan"]["moves"], 0, "{again:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mirror_never_gets_two_legs_on_one_slab() {
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(file_slab(&dir, "a").await).await;
    vm.add_slab(file_slab(&dir, "b").await).await;
    let id = vm.create_volume_with("m", 64 * SLOT, CreateOptions::redundant(RedundancyPolicy::mirror(2))).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    for e in 0..30u64 {
        v.write(e * SLOT, &vec![e as u8 + 7; SLOT as usize]).await.unwrap();
    }
    v.flush().await.unwrap();
    vm.add_slab(file_slab(&dir, "c").await).await;
    let (state, url) = serve(&dir, vm).await;
    let c = reqwest::Client::new();
    let r = c.post(&url).json(&json!({ "dry_run": false })).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let st = wait_done(&c, &url).await;
    assert_eq!(st["failed"], 0, "{st:#}");
    assert!(st["moved"].as_u64().unwrap() > 0, "the new slab took legs: {st:#}");
    let g = state.gem.read().await;
    let map = g.get_volume_map(&id).unwrap();
    for (e, loc) in map.extents.iter() {
        let slabs: std::collections::HashSet<_> = loc.legs().map(|l| l.slab_id).collect();
        assert_eq!(slabs.len(), 2, "extent {e}: two legs on two slabs");
    }
    drop(g);
    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for e in 0..30u64 {
        let mut b = vec![0u8; SLOT as usize];
        v.read(e * SLOT, &mut b).await.unwrap();
        assert!(b.iter().all(|x| *x == e as u8 + 7), "extent {e}");
    }
}
