//! #487: a golden's slot counts after its clones are deleted.
//!
//! server1 12.40 (v21.2.2): the kubelet cloned the cilium golden for a root,
//! gave up on it (stormpump's VolumeRegister timed out) and deleted it at
//! once, beside other clones and deletes; from then on every clone of the
//! golden was refused by #370's check ("1 extent(s) map slots its slab does
//! not count for it"), until the engine restarted. The calls go through the
//! API as the kubelet makes them: POST …/{golden}/clone, DELETE …/{clone}.
//! After every round the golden passes the check, a new clone of it is made,
//! and each slot it maps has the owner and the share count it had before.

use crate::common;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::volume::{VolumeId, VolumeManager, DEFAULT_EXTENT_SIZE};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 2 * 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(Uuid::new_v4()), devices[0].clone()).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, reg, gem))
}

async fn start(state: Arc<AppState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    format!("http://{addr}")
}

/// Every slot the golden maps: (slab, slot) → (owner, share count, state).
async fn counts(state: &AppState, golden: Uuid) -> BTreeMap<(Uuid, u64), (Uuid, u32, String)> {
    let (gem, reg) = {
        let m = state.volume_manager.lock().await;
        (m.gem().clone(), m.registry().clone())
    };
    let legs: Vec<_> = {
        let g = gem.read().await;
        g.volume_extents(&VolumeId(golden)).map(|it| it.flat_map(|(_, l)| l.legs().collect::<Vec<_>>()).collect()).unwrap_or_default()
    };
    let reg = reg.read().await;
    let mut out = BTreeMap::new();
    for leg in legs {
        let s = reg.get(&leg.slab_id).unwrap().get_slot(leg.slot_idx).await.unwrap();
        out.insert((leg.slab_id.0, leg.slot_idx), (s.volume_id.0, s.ref_count, format!("{:?}", s.state)));
    }
    out
}

async fn clone(c: &reqwest::Client, url: &str, golden: Uuid, name: &str) -> Result<Uuid, String> {
    let r = c
        .post(format!("{url}/api/v1/volumes/{golden}/clone"))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = r.status();
    let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
    if status != 201 {
        return Err(format!("{status}: {body}"));
    }
    let id = body["id"].as_str().or_else(|| body["volume_id"].as_str()).ok_or_else(|| format!("no id in {body}"))?;
    Ok(id.parse().unwrap())
}

async fn delete(c: &reqwest::Client, url: &str, id: Uuid) {
    let r = c.delete(format!("{url}/api/v1/volumes/{id}")).send().await.unwrap();
    assert!(r.status().is_success(), "DELETE {id}: {}", r.status());
}

async fn settled(state: &AppState, golden: Uuid, before: &BTreeMap<(Uuid, u64), (Uuid, u32, String)>, round: &str) {
    VolumeManager::persist_detached(&state.volume_manager).await;
    if let Err(e) = VolumeManager::check_golden_slots_unlocked(&state.volume_manager, VolumeId(golden)).await {
        panic!("{round}: the golden fails #370's check: {e}");
    }
    let after = counts(state, golden).await;
    assert_eq!(&after, before, "{round}: the golden's slots changed (owner, count, state)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_golden_keeps_its_slot_counts_when_its_clones_are_deleted_at_once() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let url = start(state.clone()).await;
    let c = reqwest::Client::new();

    let created: serde_json::Value = c
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "cilium", "size": "64M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let golden: Uuid = created["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    VolumeManager::persist_detached(&state.volume_manager).await;
    let before = counts(&state, golden).await;
    assert!(!before.is_empty(), "the golden maps slots");

    // 1. A clone deleted at once, then another clone.
    let a = clone(&c, &url, golden, "root-a").await.unwrap();
    delete(&c, &url, a).await;
    settled(&state, golden, &before, "clone, delete at once").await;
    let b = clone(&c, &url, golden, "root-b").await.expect("the golden clones again");
    delete(&c, &url, b).await;
    settled(&state, golden, &before, "a second clone and delete").await;

    // 2. A delete while the clone's records are still being written: a sync
    // running beside every call.
    let stop = Arc::new(AtomicBool::new(false));
    let syncer = {
        let (st, stop) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                VolumeManager::persist_detached(&st.volume_manager).await;
                tokio::task::yield_now().await;
            }
        })
    };
    for round in 0..3 {
        // 3. Many at once, as an agent pod's roots are: clones and deletes
        // overlapping, of the same golden.
        let mut tasks = Vec::new();
        for i in 0..8 {
            let (c, url) = (c.clone(), url.clone());
            tasks.push(tokio::spawn(async move {
                let id = clone(&c, &url, golden, &format!("root-{round}-{i}")).await?;
                delete(&c, &url, id).await;
                Ok::<_, String>(())
            }));
        }
        for t in tasks {
            t.await.unwrap().expect("a clone of the golden");
        }
        // Half kept, half deleted together.
        let mut kept = Vec::new();
        for i in 0..6 {
            kept.push(clone(&c, &url, golden, &format!("held-{round}-{i}")).await.unwrap());
        }
        let mut dels = Vec::new();
        for id in kept {
            let (c, url) = (c.clone(), url.clone());
            dels.push(tokio::spawn(async move { delete(&c, &url, id).await }));
        }
        for d in dels {
            d.await.unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    syncer.await.unwrap();
    settled(&state, golden, &before, "many clones and deletes at once, syncs beside them").await;
    let last = clone(&c, &url, golden, "root-last").await.expect("the golden clones after it all");
    assert_ne!(last, golden);
}

/// Reads slow while `on` is set: a copy-on-write's read of the old slot
/// takes long enough for a delete to come between it and the remap.
struct SlowReads {
    inner: Arc<dyn stormblock::drive::BlockDevice>,
    on: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl stormblock::drive::BlockDevice for SlowReads {
    fn id(&self) -> &stormblock::drive::DeviceId {
        self.inner.id()
    }
    fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }
    fn device_type(&self) -> stormblock::drive::DriveType {
        self.inner.device_type()
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> stormblock::drive::DriveResult<usize> {
        if self.on.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        self.inner.read(offset, buf).await
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> stormblock::drive::DriveResult<usize> {
        self.inner.write(offset, buf).await
    }
    async fn write_zeroes(&self, offset: u64, len: u64) -> stormblock::drive::DriveResult<()> {
        self.inner.write_zeroes(offset, len).await
    }
    async fn flush(&self) -> stormblock::drive::DriveResult<()> {
        self.inner.flush().await
    }
    async fn discard(&self, offset: u64, len: u64) -> stormblock::drive::DriveResult<()> {
        self.inner.discard(offset, len).await
    }
}

/// #487 on server1: the kubelet deleted a root while a write to it was still
/// copying one of the golden's shared extents. The delete took the clone's
/// share of the slot, and the copy-on-write let go of it again once it had
/// read the old data: the golden's slot was freed under it, and #370 refused
/// every clone of the golden after. Now the delete waits for the write, the
/// golden's counts are unchanged, and a write that comes after the delete
/// began is refused rather than remapping a deleted volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_waits_for_a_copy_on_write_in_flight_and_the_golden_keeps_its_share() {
    use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
    use stormblock::drive::BlockDevice;
    let dir = TempDir::new().unwrap();
    let on = Arc::new(AtomicBool::new(false));
    let devices = common::create_file_devices(&dir, 1, 2 * 1024 * 1024 * 1024).await;
    let dev: Arc<dyn BlockDevice> = Arc::new(SlowReads { inner: devices[0].clone(), on: on.clone() });
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    let slab = Slab::format_with(
        dev,
        SlabFormat::new(DEFAULT_EXTENT_SIZE, stormblock::placement::topology::StorageTier::Hot).with_role(SlabRole::System),
    )
    .await
    .unwrap();
    vm.add_slab(slab).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem.clone()));
    let url = start(state.clone()).await;
    let c = reqwest::Client::new();

    let created: serde_json::Value = c
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "cilium", "size": "64M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let golden: Uuid = created["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    VolumeManager::persist_detached(&state.volume_manager).await;
    let before = counts(&state, golden).await;
    let root = clone(&c, &url, golden, "cilium-root").await.unwrap();
    VolumeManager::persist_detached(&state.volume_manager).await;

    // An extent the root still shares with the golden (not the superblock's,
    // which the clone's stamp already copied).
    let shared = {
        let g = gem.read().await;
        let root_map: std::collections::HashMap<u64, _> =
            g.volume_extents(&VolumeId(root)).unwrap().map(|(v, l)| (v, l.primary())).collect();
        g.volume_extents(&VolumeId(golden))
            .unwrap()
            .find(|(v, l)| root_map.get(v) == Some(&l.primary()))
            .map(|(v, _)| v)
            .expect("the root shares an extent with the golden")
    };
    let handle = state.volume_manager.lock().await.get_volume(&VolumeId(root)).unwrap();

    on.store(true, Ordering::Relaxed);
    let writer = {
        let h = handle.clone();
        tokio::spawn(async move { h.write(shared * DEFAULT_EXTENT_SIZE + 8192, &[0x5a; 4096]).await })
    };
    // The write is reading the old slot; the kubelet gives up on the root.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let del = {
        let (c, url) = (c.clone(), url.clone());
        tokio::spawn(async move { delete(&c, &url, root).await })
    };
    // A write that comes once the delete is under way.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let late = {
        let h = handle.clone();
        tokio::spawn(async move { h.write(shared * DEFAULT_EXTENT_SIZE, &[0x6b; 4096]).await })
    };
    writer.await.unwrap().expect("the write in flight finishes");
    del.await.unwrap();
    on.store(false, Ordering::Relaxed);
    assert!(late.await.unwrap().is_err(), "a write behind the delete is refused");
    settled(&state, golden, &before, "a delete beside a copy-on-write in flight").await;
    clone(&c, &url, golden, "cilium-root-2").await.expect("the golden clones again");
}
