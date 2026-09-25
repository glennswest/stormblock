//! `/api/v1/fstemplates` over HTTP — the surface consumers actually call (#38).
//!
//! The unit tests in `src/fs/` cover the on-disk format and the lifecycle;
//! these cover the contract a CSI driver, mkube or stormblock-registry sees:
//! status codes, idempotency guards, and the promise that every clone comes
//! out with its own filesystem identity.

mod common;

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::fs::ext4;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::volume::{VolumeId, VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

async fn start(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

/// A node with one slab and a data dir, which is all templates need — no RAID
/// array, no export: a template clone is placed by the slab registry.
async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 2 * 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(Uuid::new_v4()), devices[0].clone())
        .await;

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    // Pinned off so the attach assertions below are the same on every machine.
    // The default is on, and a build host that happens to have `ublk_drv`
    // loaded would otherwise get a local device where a build host without it
    // gets nvme-tcp — a test whose answer depends on the kernel it ran on.
    config.management.ublk_transport = false;
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

/// The filesystem UUID a volume actually carries, read off its superblock.
async fn fs_uuid_on_disk(state: &AppState, volume_id: Uuid) -> Uuid {
    let dev: Arc<dyn BlockDevice> = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&VolumeId(volume_id)).expect("volume exists")
    };
    let layout = ext4::read_layout(&dev).await.unwrap();
    assert!(layout.clean, "a clone must be mountable read-write as handed out");
    // Handed out is handed out: it has to pass a real check, not merely parse.
    let report = ext4::check(&dev).await.unwrap();
    assert!(report.is_clean(), "clone fails fsck: {:?}", report.problems);
    layout.uuid
}

#[tokio::test]
async fn create_seals_in_one_call_and_lists() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({
            "name": "ext4-64m",
            "size": "64M",
            "label": "storm",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let t = &body["template"];
    assert_eq!(t["state"], "ready", "formatting happens here, so no second call");
    assert_eq!(t["fs"], "ext4");
    // The default is what `mke2fs -t ext4` writes: journal, checksums, and the
    // seed that keeps a clone's UUID stamp a single write.
    assert_eq!(t["journal"], true);
    assert_eq!(t["metadata_csum"], true);
    assert_eq!(t["metadata_csum_seed"], true);
    assert!(t["sealed_volume_id"].is_string());
    assert!(t["fs_uuid"].is_string());

    // Fetchable by name as well as by id — consumers know the name.
    let by_name: serde_json::Value = client
        .get(format!("{url}/api/v1/fstemplates/ext4-64m"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(by_name["id"], t["id"]);

    let list: serde_json::Value = client
        .get(format!("{url}/api/v1/fstemplates"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 1);

    // A duplicate name is a conflict, not a second template.
    let dup = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "ext4-64m", "size": "64M" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 409);

    server.abort();
}

#[tokio::test]
async fn every_clone_gets_its_own_filesystem_uuid() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "golden", "size": "64M", "label": "storm" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let template_uuid: Uuid = created["template"]["fs_uuid"].as_str().unwrap().parse().unwrap();

    // Two clones, one through each door: the template's clone endpoint and
    // from_template on the volume API.
    let a: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/golden/clone"))
        .json(&serde_json::json!({ "name": "pvc-a" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let b_resp = client
        .post(format!("{url}/api/v1/volumes"))
        .json(&serde_json::json!({ "name": "pvc-b", "from_template": "golden" }))
        .send()
        .await
        .unwrap();
    assert_eq!(b_resp.status(), 201);
    let b: serde_json::Value = b_resp.json().await.unwrap();

    let a_id: Uuid = a["volume_id"].as_str().unwrap().parse().unwrap();
    let b_id: Uuid = b["id"].as_str().unwrap().parse().unwrap();

    let a_uuid = fs_uuid_on_disk(&state, a_id).await;
    let b_uuid = fs_uuid_on_disk(&state, b_id).await;
    assert_ne!(a_uuid, b_uuid, "two clones must not share an identity");
    assert_ne!(a_uuid, template_uuid);
    assert_ne!(b_uuid, template_uuid);
    // The reported UUID is the one on disk, not a hopeful guess.
    assert_eq!(a["fs_uuid"].as_str().unwrap().parse::<Uuid>().unwrap(), a_uuid);
    assert_eq!(b["fs_uuid"].as_str().unwrap().parse::<Uuid>().unwrap(), b_uuid);

    // The template itself keeps its own.
    let sealed: Uuid = created["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(fs_uuid_on_disk(&state, sealed).await, template_uuid);

    let count = client
        .get(format!("{url}/api/v1/fstemplates/golden"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["clones"]
        .as_u64()
        .unwrap();
    assert_eq!(count, 2);

    server.abort();
}

#[tokio::test]
async fn clone_grows_but_never_shrinks() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "t", "size": "64M" }))
        .send()
        .await
        .unwrap();

    let big: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "big", "size": "128M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(big["size_bytes"], 128 * 1024 * 1024);

    // Asking for less than the template leaves the volume alone — shrinking
    // would cut into a filesystem that does not know about it.
    let small: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "small", "size": "16M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(small["size_bytes"], 64 * 1024 * 1024);

    server.abort();
}

#[tokio::test]
async fn journal_variants_coexist() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    for (name, journal) in [("ext4-nojournal-256m", false), ("ext4-journal-256m", true)] {
        let body: serde_json::Value = client
            .post(format!("{url}/api/v1/fstemplates"))
            .json(&serde_json::json!({ "name": name, "size": "256M", "journal": journal }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["template"]["journal"], journal, "{name}");

        let sealed: Uuid = body["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
        let dev: Arc<dyn BlockDevice> = {
            let vm = state.volume_manager.lock().await;
            vm.get_volume(&VolumeId(sealed)).unwrap()
        };
        let l = ext4::read_layout(&dev).await.unwrap();
        assert_eq!(l.has_journal, journal, "{name} on disk");
        assert!(!l.needs_recovery, "{name} must not ship with a replay pending");
    }

    server.abort();
}

/// Features are chosen per template in `mke2fs -O` terms. The default is what
/// `mke2fs -t ext4` writes, which is also what RouterOS's own format-drive
/// produces; a consumer that predates any of it turns that bit off by name.
#[tokio::test]
async fn features_are_a_per_template_choice() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    for (name, wide) in [("ext4-narrow-256m", false), ("ext4-256m", true)] {
        let features = if wide { None } else { Some("^64bit,^metadata_csum") };
        let body: serde_json::Value = client
            .post(format!("{url}/api/v1/fstemplates"))
            .json(&serde_json::json!({ "name": name, "size": "256M", "features": features }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["template"]["64bit"], wide, "{name}");
        assert_eq!(body["template"]["metadata_csum"], wide, "{name}");

        let sealed: Uuid = body["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
        let dev: Arc<dyn BlockDevice> = {
            let vm = state.volume_manager.lock().await;
            vm.get_volume(&VolumeId(sealed)).unwrap()
        };
        let l = ext4::read_layout(&dev).await.unwrap();
        assert_eq!(l.sixty_four_bit, wide, "{name} on disk");
        assert!(l.clean);
        assert!(ext4::check(&dev).await.unwrap().is_clean(), "{name} fails fsck");
    }

    // A clone of the default template keeps the features and still gets its
    // own identity: metadata_csum is on, and the seed that comes with it is
    // what keeps the stamp a single superblock write.
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/ext4-256m/clone"))
        .json(&serde_json::json!({ "name": "wide-clone" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id: Uuid = clone["volume_id"].as_str().unwrap().parse().unwrap();
    let dev: Arc<dyn BlockDevice> = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&VolumeId(id)).unwrap()
    };
    let l = ext4::read_layout(&dev).await.unwrap();
    assert!(l.sixty_four_bit && l.metadata_csum && l.csum_seed);
    assert!(l.clean);
    assert_eq!(clone["fs_uuid"].as_str().unwrap().parse::<Uuid>().unwrap(), l.uuid);
    let report = ext4::check(&dev).await.unwrap();
    assert!(report.is_clean(), "stamping invalidated checksums: {:?}", report.problems);

    server.abort();
}

#[tokio::test]
async fn unsealed_templates_cannot_be_cloned_and_seal_verifies() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    // The two-phase form: a raw volume for an initiator to format.
    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "external", "size": "64M", "format": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["template"]["state"], "awaiting_format");
    let id = created["template"]["id"].as_str().unwrap().to_string();
    let raw: Uuid = created["template"]["raw_volume_id"].as_str().unwrap().parse().unwrap();

    let refused = client
        .post(format!("{url}/api/v1/fstemplates/external/clone"))
        .json(&serde_json::json!({ "name": "too-early" }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409, "an unsealed template is not cloneable");

    // Nothing has formatted it, so sealing must refuse rather than snapshot
    // whatever happens to be there.
    let unformatted = client
        .post(format!("{url}/api/v1/fstemplates/{id}/seal"))
        .send()
        .await
        .unwrap();
    assert_eq!(unformatted.status(), 409);

    // Format it the way an initiator would, then dirty the superblock the way
    // an unclean unmount does (stormblock-registry#10). No metadata_csum here,
    // so patching the flags by hand leaves a superblock that still parses —
    // the state flags are what is under test, not checksum handling.
    {
        let dev: Arc<dyn BlockDevice> = {
            let vm = state.volume_manager.lock().await;
            vm.get_volume(&VolumeId(raw)).unwrap()
        };
        ext4::format(
            &dev,
            &ext4::Ext4Params {
                features: Some("^metadata_csum".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // s_state at 0x3A and s_feature_incompat at 0x60, both relative to the
        // superblock's home 1024 bytes in.
        let mut block = vec![0u8; 4096];
        dev.read(0, &mut block).await.unwrap();
        let sb = 1024usize;
        block[sb + 0x3A..sb + 0x3C].copy_from_slice(&0x0003u16.to_le_bytes()); // VALID_FS|ERROR_FS
        let incompat = u32::from_le_bytes(block[sb + 0x60..sb + 0x64].try_into().unwrap());
        block[sb + 0x60..sb + 0x64].copy_from_slice(&(incompat | 0x0004).to_le_bytes()); // RECOVER
        let mut done = 0;
        while done < block.len() {
            done += dev.write(done as u64, &block[done..]).await.unwrap();
        }
        dev.flush().await.unwrap();
    }

    let dirty = client
        .post(format!("{url}/api/v1/fstemplates/{id}/seal"))
        .send()
        .await
        .unwrap();
    assert_eq!(dirty.status(), 409, "VALID_FS alone is not enough to seal");
    let why: serde_json::Value = dirty.json().await.unwrap();
    let msg = why["error"].as_str().unwrap();
    assert!(msg.contains("ERROR_FS"), "{msg}");
    assert!(msg.contains("RECOVER"), "{msg}");

    // force is the operator's escape hatch.
    let forced = client
        .post(format!("{url}/api/v1/fstemplates/{id}/seal?force=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(forced.status(), 200);
    assert_eq!(forced.json::<serde_json::Value>().await.unwrap()["state"], "ready");

    server.abort();
}

/// Deleting a template takes its volume with it, and leaves its clones alone
/// (#47). `?purge=false` is the way to keep the volume — and what a node that
/// does keep one ends up with is exactly what the orphan endpoint reports.
#[tokio::test]
async fn delete_purges_its_volume_and_spares_the_clones() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "t", "size": "64M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["template"]["id"].as_str().unwrap().to_string();
    // Sealed: the scratch volume is gone, and only the snapshot remains.
    assert!(created["template"]["raw_volume_id"].is_null());

    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "c" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let clone_id = clone["volume_id"].as_str().unwrap().to_string();

    // A descendant no longer blocks the purge: the clone holds its own
    // refcounted reference to every extent it inherited.
    let purged = client
        .delete(format!("{url}/api/v1/fstemplates/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(purged.status(), 200);
    let body: serde_json::Value = purged.json().await.unwrap();
    assert_eq!(body["purged_volumes"].as_array().unwrap().len(), 1);

    assert_eq!(
        client.get(format!("{url}/api/v1/fstemplates/{id}")).send().await.unwrap().status(),
        404
    );
    assert_eq!(
        client.get(format!("{url}/api/v1/volumes/{clone_id}")).send().await.unwrap().status(),
        200,
        "the clone outlives the template it came from"
    );

    server.abort();
}

/// The state #47 found a node in, and the way out of it.
#[tokio::test]
async fn kept_volumes_show_up_as_orphans_and_are_reclaimable() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "t", "size": "64M" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["template"]["id"].as_str().unwrap().to_string();
    let sealed = created["template"]["sealed_volume_id"].as_str().unwrap().to_string();
    client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "pvc-1" }))
        .send()
        .await
        .unwrap();

    // A live template is not debris.
    let clean: serde_json::Value = client
        .get(format!("{url}/api/v1/fstemplates/orphans"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clean["count"], 0);

    // The old behaviour, asked for explicitly: forget the template, keep its
    // volume. Nothing afterwards can name that volume from the store.
    let kept = client
        .delete(format!("{url}/api/v1/fstemplates/{id}?purge=false"))
        .send()
        .await
        .unwrap();
    assert_eq!(kept.status(), 200);
    assert_eq!(kept.json::<serde_json::Value>().await.unwrap()["purged_volumes"][0], serde_json::Value::Null);

    let found: serde_json::Value = client
        .get(format!("{url}/api/v1/fstemplates/orphans"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["count"], 1);
    assert_eq!(found["orphans"][0]["volume_id"], sealed);

    let reclaimed: serde_json::Value = client
        .delete(format!("{url}/api/v1/fstemplates/orphans"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reclaimed["count"], 1);
    assert_eq!(
        client.get(format!("{url}/api/v1/volumes/{sealed}")).send().await.unwrap().status(),
        404
    );

    // And the consumer's clone was never in the set.
    let volumes: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = volumes["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"pvc-1"), "{names:?}");

    server.abort();
}

#[tokio::test]
async fn unknown_template_and_bad_input_are_reported_precisely() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client.get(format!("{url}/api/v1/fstemplates/nope")).send().await.unwrap().status(),
        404
    );
    assert_eq!(
        client
            .post(format!("{url}/api/v1/volumes"))
            .json(&serde_json::json!({ "name": "x", "from_template": "nope" }))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // No size at all.
    assert_eq!(
        client
            .post(format!("{url}/api/v1/fstemplates"))
            .json(&serde_json::json!({ "name": "x" }))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    // A filesystem this engine does not write.
    assert_eq!(
        client
            .post(format!("{url}/api/v1/fstemplates"))
            .json(&serde_json::json!({ "name": "x", "size": "64M", "fs": "xfs" }))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    // A plain volume needs somewhere to be placed, and this node has slabs —
    // so it is created. It used to need an `array_id`, which slab placement
    // made obsolete: a volume's extents pick their own slabs, and demanding
    // an array binding refused the one request every consumer sends.
    assert_eq!(
        client
            .post(format!("{url}/api/v1/volumes"))
            .json(&serde_json::json!({ "name": "x", "size": "64M" }))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );

    server.abort();
}

/// Physical slots in use across every slab — what a clone actually costs the
/// pool, as opposed to the shared extents it maps.
async fn slots_in_use(state: &AppState) -> u64 {
    let reg = state.slab_registry.read().await;
    reg.iter().map(|(_, slab)| slab.allocated_slots()).sum()
}

/// A template is cheap to keep, and a clone costs almost nothing on top: the
/// clone shares every extent and copies only the one the UUID stamp lands in.
/// That ratio is the whole point of the feature.
#[tokio::test]
async fn a_clone_costs_one_extent_not_a_filesystem() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let before_template = slots_in_use(&state).await;
    client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "t", "size": "512M" }))
        .send()
        .await
        .unwrap();
    let template_cost = slots_in_use(&state).await - before_template;
    // A 512 MiB ext4 describes ~8 MiB of inode tables alone; writing it as a
    // template must not materialise them.
    assert!(
        template_cost * DEFAULT_EXTENT_SIZE < 64 * 1024 * 1024,
        "the template took {template_cost} slots"
    );

    let before_clone = slots_in_use(&state).await;
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes"))
        .json(&serde_json::json!({ "name": "c", "from_template": "t" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let clone_cost = slots_in_use(&state).await - before_clone;
    assert_eq!(clone_cost, 1, "a clone should copy exactly the stamped extent");
    // It still presents the whole filesystem, shared. `allocated_bytes` is
    // what the clone costs (5e4e5d3) and `shared_bytes` what it maps through
    // its template, so the sharing shows in the second.
    assert_eq!(clone["virtual_size_bytes"], 512 * 1024 * 1024);
    assert_eq!(clone["allocated_bytes"].as_u64().unwrap(), clone_cost * DEFAULT_EXTENT_SIZE);
    assert!(clone["shared_bytes"].as_u64().unwrap() > 0, "the clone maps its template's extents");

    server.abort();
}

/// The engine can check and repair a filesystem on a volume nobody has
/// mounted — which is the only way a RouterOS volume gets fscked at all, since
/// RouterOS has neither an fsck nor a clean unmount for a network disk.
#[tokio::test]
async fn volumes_can_be_checked_and_repaired_in_place() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "t", "size": "64M" }))
        .send()
        .await
        .unwrap();
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "c" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clone["verified"], true, "clones are checked before hand-off");
    let vol = clone["volume_id"].as_str().unwrap();

    let report: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{vol}/fsck"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["clean"], true, "{report}");
    assert_eq!(report["exit_code"], 0);
    assert!(report["problems"].as_array().unwrap().is_empty());
    assert!(report["directories"].as_u64().unwrap() >= 1, "root at least");

    // A volume that does not exist is a 404, not a crash.
    let missing = client
        .post(format!("{url}/api/v1/volumes/{}/fsck", Uuid::new_v4()))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    server.abort();
}

/// A template can ship content, and every clone inherits it without the file
/// ever being written again. This is the piece that needs no mount, no loop
/// device and no attach — the engine writes into its own volume.
#[tokio::test]
async fn templates_can_carry_files_that_clones_inherit() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({
            "name": "seeded",
            "size": "64M",
            "files": [
                { "path": "/etc/hostname", "contents": "router\n" },
                { "path": "/etc/conf.d/net", "contents": "dhcp\n" },
                { "path": "/boot.bin", "contents_base64": "AAECAw==" },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(body["template"]["state"], "ready", "{body}");
    let seeded = body["template"]["seeded"].as_array().unwrap();
    assert_eq!(seeded.len(), 3);

    // The clone carries the content, and is still a filesystem that checks out.
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/seeded/clone"))
        .json(&serde_json::json!({ "name": "pvc" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clone["verified"], true);
    let vol = clone["volume_id"].as_str().unwrap();

    let file: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{vol}/files?path=/etc/hostname"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    use base64::Engine;
    let got = base64::engine::general_purpose::STANDARD
        .decode(file["contents_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(got, b"router\n", "clone did not inherit the seeded file");

    // A binary file survives the round trip byte for byte.
    let bin: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{vol}/files?path=/boot.bin"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bin["contents_base64"], "AAECAw==");

    // A directory reads as a listing.
    let etc: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{vol}/files?path=/etc"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = etc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"hostname"), "{names:?}");
    assert!(names.contains(&"conf.d"), "{names:?}");

    // Writing into a clone afterwards leaves it checkable, and does not reach
    // back into the template it came from.
    let wrote: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{vol}/files"))
        .json(&serde_json::json!({
            "files": [{ "path": "/etc/hostname", "contents": "pvc-1\n" }],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(wrote["clean"], true, "{wrote}");

    let sealed: Uuid = body["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    let template_file: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{sealed}/files?path=/etc/hostname"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let template_got = base64::engine::general_purpose::STANDARD
        .decode(template_file["contents_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(template_got, b"router\n", "the clone wrote through to its template");

    server.abort();
}

// ---------------------------------------------------------------- standing by

use stormblock::fs::template::{self, ClaimSpec, TemplateSpec};

/// vm + store handles, which is what the template layer takes.
async fn parts(dir: &TempDir) -> Arc<AppState> {
    setup(dir).await
}

/// `create` formats and seals in one call, so this is already Ready.
async fn sealed_template(state: &AppState, name: &str) -> Uuid {
    let spec = TemplateSpec::new(name, 64 * 1024 * 1024);
    let t = template::create(&state.volume_manager, &state.fstemplates, &spec)
        .await
        .expect("create");
    assert_eq!(t.state, stormblock::fs::TemplateState::Ready);
    t.id
}

/// A claim mints its clone now, and nothing is left standing by (#137).
///
/// #55 kept a pre-minted clone per template because a mint was believed to
/// cost seconds. It is a snapshot, one superblock write and one metadata
/// persist, so there is nothing to mint ahead — and no volume on the node
/// that nobody asked for.
#[tokio::test]
async fn a_claim_mints_now_and_leaves_nothing_standing() {
    let dir = TempDir::new().unwrap();
    let state = parts(&dir).await;
    let id = sealed_template(&state, "pvc-64m").await;
    let before = state.volume_manager.lock().await.list_volumes().await.len();

    let a = template::claim(&state.volume_manager, &state.fstemplates, &id.to_string(), &ClaimSpec::default())
        .await
        .unwrap();
    let b = template::claim(&state.volume_manager, &state.fstemplates, &id.to_string(), &ClaimSpec::default())
        .await
        .unwrap();
    assert_ne!(a.volume_id, b.volume_id, "every claim is its own clone");
    assert_ne!(a.fs_uuid, b.fs_uuid, "and its own filesystem identity");
    assert!(a.verified && b.verified, "the stamp was read back");

    // Two claims, two volumes — and nothing else appeared, now or later.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let vm = state.volume_manager.lock().await;
    let names: Vec<String> = vm.list_volumes().await.into_iter().map(|(_, n, ..)| n).collect();
    assert_eq!(names.len(), before + 2, "{names:?}");
    assert!(names.iter().all(|n| !n.starts_with("standby-")), "{names:?}");
    assert_eq!(names.iter().filter(|n| n.starts_with("claim-pvc-64m-")).count(), 2, "{names:?}");
    drop(vm);
    assert_eq!(state.fstemplates.lock().await.get(&id).unwrap().clones, 2);
}

/// A clone of a sealed blank is not fsck'd one by one (#137): the blank was
/// checked when it was sealed and cannot change, and the one write a mint
/// makes — the superblock — is read back. What that hands out is still a
/// clean filesystem with its own identity, which a full check confirms here.
#[tokio::test]
async fn a_clone_of_a_sealed_blank_needs_no_fsck_of_its_own() {
    let dir = TempDir::new().unwrap();
    let state = parts(&dir).await;
    let id = sealed_template(&state, "blank-verify").await;
    let spec = stormblock::fs::CloneSpec::new("pvc-x");
    assert!(spec.verify, "asking to verify is still the default");
    let c = template::clone_template(&state.volume_manager, &state.fstemplates, &id.to_string(), &spec)
        .await
        .unwrap();
    assert!(c.verified);

    let dev = state.volume_manager.lock().await.get_volume(&c.volume_id).unwrap();
    let layout = ext4::read_layout(&dev).await.unwrap();
    assert_eq!(Some(layout.uuid), c.fs_uuid, "the identity the clone reports is on disk");
    let t = state.fstemplates.lock().await.get(&id).unwrap().clone();
    assert_ne!(Some(layout.uuid), t.fs_uuid, "and it is not the blank's");
    assert!(ext4::check(&dev).await.unwrap().is_clean(), "and the clone is a clean filesystem");
}

/// On upgrade the clone each template *recorded* as standing is deleted —
/// and only that one. A claimed clone kept its `standby-…` name (there is no
/// rename), so deleting by name would delete somebody's PVC.
#[tokio::test]
async fn retiring_standing_clones_deletes_only_the_unclaimed_one() {
    let dir = TempDir::new().unwrap();
    let state = parts(&dir).await;
    let id = sealed_template(&state, "legacy").await;
    let mint = |name: &'static str| {
        let state = state.clone();
        async move {
            template::clone_template(
                &state.volume_manager,
                &state.fstemplates,
                &id.to_string(),
                &stormblock::fs::CloneSpec::new(name),
            )
            .await
            .unwrap()
        }
    };
    let unclaimed = mint("standby-legacy-aaaaaaaa").await;
    let claimed = mint("standby-legacy-bbbbbbbb").await;
    // What a store written by #55 records: the one still waiting.
    state.fstemplates.lock().await.get_mut(&id).unwrap().standing = Some(stormblock::fs::StandingClone {
        volume_id: unclaimed.volume_id.0,
        fs_uuid: unclaimed.fs_uuid,
        size_bytes: unclaimed.size_bytes,
        verified: true,
    });

    let gone = template::retire_standing(&state.volume_manager, &state.fstemplates).await;
    assert_eq!(gone, vec![unclaimed.volume_id]);
    {
        let vm = state.volume_manager.lock().await;
        assert!(vm.get_volume(&unclaimed.volume_id).is_none(), "the unclaimed clone is gone");
        assert!(vm.get_volume(&claimed.volume_id).is_some(), "a claimed one is somebody's");
    }
    assert!(state.fstemplates.lock().await.get(&id).unwrap().standing.is_none());
    assert!(
        template::retire_standing(&state.volume_manager, &state.fstemplates).await.is_empty(),
        "idempotent"
    );
}

/// A store written before #137 still reads, and what it said about a
/// standing clone is never written back.
#[test]
fn a_store_with_a_standing_clone_still_reads_and_forgets_it() {
    let json = serde_json::json!({
        "id": Uuid::new_v4(), "name": "old", "fs": "ext4", "size_bytes": 1024,
        "state": "ready", "sealed_volume_id": Uuid::new_v4(),
        "standing": { "volume_id": Uuid::new_v4(), "size_bytes": 1024, "verified": true }
    });
    let t: stormblock::fs::FsTemplate = serde_json::from_value(json).unwrap();
    assert!(t.standing.is_some());
    let back = serde_json::to_value(&t).unwrap();
    assert!(back.get("standing").is_none(), "{back}");
    assert!(t.json().get("standing").is_none());
}

/// Over HTTP: a claim answers with a fresh clone, and the standby surface is
/// gone.
#[tokio::test]
async fn claim_over_http_mints_and_the_standby_surface_is_gone() {
    let dir = TempDir::new().unwrap();
    let state = parts(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();
    let id = sealed_template(&state, "http-claim").await;

    let claimed: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates/{id}/claim"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(claimed["volume_id"].is_string(), "{claimed}");
    assert_eq!(claimed["verified"], true);
    assert!(claimed.get("from_standby").is_none(), "{claimed}");

    for (method, path) in [
        (reqwest::Method::GET, "/api/v1/fstemplates/standby".to_string()),
        (reqwest::Method::POST, "/api/v1/fstemplates/standby".to_string()),
        (reqwest::Method::POST, format!("/api/v1/fstemplates/{id}/standby")),
    ] {
        let s = client.request(method.clone(), format!("{url}{path}")).send().await.unwrap().status();
        assert!(s == 404 || s == 405, "{method} {path} answered {s}");
    }
    server.abort();
}

/// #76: a template is a volume that has been sealed. The sealed volume shows
/// as sealed with its filesystem on `GET /api/v1/volumes/{id}`, a clone taken
/// through any door records its parent and carries its own identity, a
/// sealed volume is cloneable by name through `from_template`, and a plain
/// volume with a filesystem on it can be sealed and cloned with no template
/// object at all.
#[tokio::test]
async fn a_template_is_a_sealed_volume_and_any_sealed_volume_clones() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "base", "size": "64M" }))
        .send().await.unwrap().json().await.unwrap();
    let sealed: Uuid = created["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    assert!(created["template"]["raw_volume_id"].is_null(), "one template, one volume");

    // The volume record says what the template used to say alone.
    let vol: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{sealed}"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(vol["sealed"], true);
    assert_eq!(vol["fs"]["kind"], "ext4");
    assert_eq!(vol["fs"]["metadata_csum_seed"], true);
    assert_eq!(vol["fs_uuid"], created["template"]["fs_uuid"]);

    // Cloning the sealed volume directly: parent recorded, identity fresh.
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{sealed}/clone"))
        .json(&serde_json::json!({ "name": "pvc-direct" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(clone["parent"].as_str().unwrap(), sealed.to_string());
    assert_eq!(clone["sealed"], false);
    assert_ne!(clone["fs_uuid"], vol["fs_uuid"], "a clone never shares its source's identity");
    let clone_id: Uuid = clone["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(fs_uuid_on_disk(&state, clone_id).await.to_string(), clone["fs_uuid"].as_str().unwrap());

    // The same clone via `from_template` naming the *volume* — one namespace.
    let by_name: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes"))
        .json(&serde_json::json!({ "name": "pvc-by-volume", "from_template": sealed.to_string() }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(by_name["parent"].as_str().unwrap(), sealed.to_string());
    assert_ne!(by_name["fs_uuid"], vol["fs_uuid"]);
    assert_ne!(by_name["fs_uuid"], clone["fs_uuid"]);

    // A plain snapshot of something with a filesystem is stamped too.
    let snap: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/snapshots"))
        .json(&serde_json::json!({ "name": "snap-of-clone", "source_volume_id": clone_id }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(snap["parent"].as_str().unwrap(), clone_id.to_string());
    assert_ne!(snap["fs_uuid"], clone["fs_uuid"]);

    // Lineage, from the grandchild up.
    let lineage: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{}/lineage", snap["id"].as_str().unwrap()))
        .send().await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = lineage["lineage"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![snap["id"].as_str().unwrap(), clone["id"].as_str().unwrap(), &sealed.to_string()]);

    // Writes to the sealed volume are refused; sealing a clone makes it a
    // golden of its own, with no template object anywhere.
    let seal: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{clone_id}/seal"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(seal["sealed"], true);
    let second: reqwest::Response = client
        .post(format!("{url}/api/v1/volumes/{clone_id}/clone"))
        .json(&serde_json::json!({ "name": "pvc-from-golden-2" }))
        .send().await.unwrap();
    assert_eq!(second.status(), 201);
    let second: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second["parent"].as_str().unwrap(), clone_id.to_string());
    assert_ne!(second["fs_uuid"], clone["fs_uuid"]);

    // An unsealed volume cannot be cloned through the clone door.
    let refused = client
        .post(format!("{url}/api/v1/volumes/{}/clone", by_name["id"].as_str().unwrap()))
        .json(&serde_json::json!({ "name": "nope" }))
        .send().await.unwrap();
    assert_eq!(refused.status(), 409);

    server.abort();
}

/// #78: attach is a volume operation. A volume that never went through /v1
/// gets its attach parameters from `POST /api/v1/volumes/{id}/attach`, the
/// same shape /v1 returns; asking for a ublk device on a node that cannot
/// offer one is refused rather than silently downgraded.
#[tokio::test]
async fn any_engine_volume_can_be_attached_through_the_volume_door() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "base", "size": "64M" }))
        .send().await.unwrap().json().await.unwrap();
    let sealed: Uuid = created["template"]["sealed_volume_id"].as_str().unwrap().parse().unwrap();
    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{sealed}/clone"))
        .json(&serde_json::json!({ "name": "pvc-1" }))
        .send().await.unwrap().json().await.unwrap();
    let clone_id = clone["id"].as_str().unwrap().to_string();

    // Nothing attached yet.
    let before: serde_json::Value = client
        .get(format!("{url}/api/v1/volumes/{clone_id}/attach"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(before["attached"], false);

    // The engine's choice with the transport turned off: nvme-tcp, /v1 shape.
    let resp = client
        .post(format!("{url}/api/v1/volumes/{clone_id}/attach"))
        .json(&serde_json::json!({}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let info: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(info["transport"], "nvme_tcp");
    assert!(info["nqn"].as_str().unwrap().starts_with("nqn."));
    assert!(info["addresses"].as_array().unwrap().len() == 1);

    // ublk explicitly, where it cannot be served: refused, not downgraded. A
    // caller that named a transport must not be given a different one and told
    // it succeeded.
    let resp = client
        .post(format!("{url}/api/v1/volumes/{clone_id}/attach"))
        .json(&serde_json::json!({ "transport": "ublk" }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);

    // Detach is idempotent; an unknown volume is 404.
    let resp = client.delete(format!("{url}/api/v1/volumes/{clone_id}/attach")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client.delete(format!("{url}/api/v1/volumes/{clone_id}/attach")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .post(format!("{url}/api/v1/volumes/{}/attach", Uuid::new_v4()))
        .json(&serde_json::json!({}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 404);

    server.abort();
}

/// #80: the engine serves its own Kubernetes-shaped resources. Discovery
/// names the group and its kinds; a sealed golden shows up as a `Volume`
/// with spec/status; get works by uuid or by name; a spec patch is a verb;
/// drives, slabs and nodes list; and a watch opens with ADDED events.
#[tokio::test]
async fn the_engine_serves_kubernetes_shaped_resources() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let apis: serde_json::Value = client.get(format!("{url}/apis")).send().await.unwrap().json().await.unwrap();
    assert_eq!(apis["kind"], "APIGroupList");
    assert_eq!(apis["groups"][0]["name"], "storage.storm.io");
    let res: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1")).send().await.unwrap().json().await.unwrap();
    assert_eq!(res["kind"], "APIResourceList");
    let names: Vec<&str> = res["resources"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["volumes", "slabs", "drives", "nodes"]);

    let created: serde_json::Value = client
        .post(format!("{url}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "name": "base", "size": "64M" }))
        .send().await.unwrap().json().await.unwrap();
    let sealed = created["template"]["sealed_volume_id"].as_str().unwrap().to_string();

    let vols: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1/volumes")).send().await.unwrap().json().await.unwrap();
    assert_eq!(vols["kind"], "VolumeList");
    assert_eq!(vols["apiVersion"], "storage.storm.io/v1");
    let v = vols["items"].as_array().unwrap().iter().find(|v| v["metadata"]["name"] == sealed).expect("the golden is a Volume");
    assert_eq!(v["kind"], "Volume");
    assert_eq!(v["spec"]["sealed"], true);
    assert_eq!(v["spec"]["fs"]["kind"], "ext4");
    assert_eq!(v["status"]["health"], "healthy");
    assert_eq!(v["metadata"]["labels"]["storm.io/name"], "fstemplate-base-raw");

    // By name as well as by uuid, and a label selector.
    let by_name: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1/volumes/fstemplate-base-raw")).send().await.unwrap().json().await.unwrap();
    assert_eq!(by_name["metadata"]["name"], sealed);
    let sel: serde_json::Value = client
        .get(format!("{url}/apis/storage.storm.io/v1/volumes?labelSelector=storm.io/name=fstemplate-base-raw"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(sel["items"].as_array().unwrap().len(), 1);
    let missing = client.get(format!("{url}/apis/storage.storm.io/v1/volumes/nope")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    let body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(body["kind"], "Status");
    assert_eq!(body["reason"], "NotFound");

    // A spec patch is a verb: unseal, then seal again.
    let patched: serde_json::Value = client
        .patch(format!("{url}/apis/storage.storm.io/v1/volumes/{sealed}"))
        .json(&serde_json::json!({ "spec": { "sealed": false } }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(patched["spec"]["sealed"], false);
    let bad = client
        .patch(format!("{url}/apis/storage.storm.io/v1/volumes/{sealed}"))
        .json(&serde_json::json!({ "spec": { "redundancy": "raid9" } }))
        .send().await.unwrap();
    assert_eq!(bad.status(), 422);
    client
        .patch(format!("{url}/apis/storage.storm.io/v1/volumes/{sealed}"))
        .json(&serde_json::json!({ "spec": { "sealed": true } }))
        .send().await.unwrap();

    let slabs: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1/slabs")).send().await.unwrap().json().await.unwrap();
    assert_eq!(slabs["kind"], "SlabList");
    assert_eq!(slabs["items"].as_array().unwrap().len(), 1);
    assert!(slabs["items"][0]["spec"]["domain"].as_str().unwrap().starts_with("drive="));
    let nodes: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1/nodes")).send().await.unwrap().json().await.unwrap();
    assert_eq!(nodes["kind"], "NodeList");
    assert!(nodes["items"].as_array().unwrap().iter().any(|n| n["status"]["local"] == true));
    let drives: serde_json::Value = client.get(format!("{url}/apis/storage.storm.io/v1/drives")).send().await.unwrap().json().await.unwrap();
    assert_eq!(drives["kind"], "DriveList");

    // A watch opens with what exists.
    let mut resp = client.get(format!("{url}/apis/storage.storm.io/v1/volumes?watch=1")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let first = resp.chunk().await.unwrap().expect("an ADDED event");
    let line = String::from_utf8_lossy(&first);
    let ev: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(ev["type"], "ADDED");
    assert_eq!(ev["object"]["kind"], "Volume");

    server.abort();
}

/// A VM disk becomes a golden: a qcow2 (with an MBR disk inside, as a cloud
/// image would carry) is imported through the job API, only its allocated
/// clusters are written, the result is sealed with `fs.kind = mbr`, and a
/// clone gets its own disk signature. No filesystem the engine writes is
/// involved anywhere.
#[tokio::test]
async fn a_qcow2_disk_image_imports_into_a_sealed_golden_and_clones_with_its_own_identity() {
    use stormblock::image::decode::qcow2::testimg::{self, C};

    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (url, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    // Cluster 0: an MBR with one partition; cluster 3: some "filesystem".
    let mut mbr = vec![0u8; 4096];
    mbr[440..444].copy_from_slice(&0xCAFE_F00Du32.to_le_bytes());
    mbr[446 + 4] = 0x83;
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let payload: Vec<u8> = (0..4096).map(|i| (i % 199) as u8).collect();
    let img = testimg::build(12, &[C::Data(mbr), C::Hole, C::Zero, C::Compressed(payload.clone())]);
    let path = dir.path().join("cloud.img"); // a cloud image called .img is a qcow2
    std::fs::write(&path, &img).unwrap();

    let job: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/import"))
        .json(&serde_json::json!({ "name": "cloud-golden", "file": path.to_str().unwrap() }))
        .send().await.unwrap().json().await.unwrap();
    let id = job["id"].as_str().unwrap().to_string();
    let mut st = job;
    for _ in 0..200 {
        if st["state"] == "done" || st["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        st = client.get(format!("{url}/api/v1/volumes/import/{id}")).send().await.unwrap().json().await.unwrap();
    }
    assert_eq!(st["state"], "done", "{st}");
    assert_eq!(st["format"], "qcow2", "detected by magic, not by the .img name");
    assert_eq!(st["virtual_size"], 4 * 4096);
    assert_eq!(st["written_bytes"], 2 * 4096, "only the two clusters with data");
    assert_eq!(st["fs"]["kind"], "mbr");
    let vol = st["volume_id"].as_str().unwrap().to_string();

    let v: serde_json::Value = client.get(format!("{url}/api/v1/volumes/{vol}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["sealed"], true);
    assert_eq!(v["fs"]["kind"], "mbr");
    let golden_sig = v["fs_uuid"].as_str().unwrap().to_string();

    let clone: serde_json::Value = client
        .post(format!("{url}/api/v1/volumes/{vol}/clone"))
        .json(&serde_json::json!({ "name": "vm-1" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(clone["parent"].as_str().unwrap(), vol);
    assert_ne!(clone["fs_uuid"].as_str().unwrap(), golden_sig, "a clone is its own disk");
    // The clone's content is the image's: the compressed cluster came through.
    let clone_id: Uuid = clone["id"].as_str().unwrap().parse().unwrap();
    let dev = state.volume_manager.lock().await.get_volume(&VolumeId(clone_id)).unwrap();
    let mut buf = vec![0u8; 4096];
    dev.read(3 * 4096, &mut buf).await.unwrap();
    assert_eq!(buf, payload);
    dev.read(4096, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0), "a hole reads as zeros");

    // An unsupported format says so, up front.
    let bad = client
        .post(format!("{url}/api/v1/volumes/import"))
        .json(&serde_json::json!({ "name": "x", "file": "/nowhere", "format": "vhd" }))
        .send().await.unwrap();
    assert_eq!(bad.status(), 400);

    server.abort();
}
