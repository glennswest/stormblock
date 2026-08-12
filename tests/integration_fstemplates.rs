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

#[tokio::test]
async fn delete_purges_only_with_force_once_clones_exist() {
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
    client
        .post(format!("{url}/api/v1/fstemplates/t/clone"))
        .json(&serde_json::json!({ "name": "c" }))
        .send()
        .await
        .unwrap();

    let refused = client
        .delete(format!("{url}/api/v1/fstemplates/{id}?purge=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409);

    let purged = client
        .delete(format!("{url}/api/v1/fstemplates/{id}?purge=true&force=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(purged.status(), 200);
    let body: serde_json::Value = purged.json().await.unwrap();
    assert_eq!(body["purged_volumes"].as_array().unwrap().len(), 2);

    assert_eq!(
        client.get(format!("{url}/api/v1/fstemplates/{id}")).send().await.unwrap().status(),
        404
    );

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
    // A plain volume still needs its array.
    assert_eq!(
        client
            .post(format!("{url}/api/v1/volumes"))
            .json(&serde_json::json!({ "name": "x", "size": "64M" }))
            .send()
            .await
            .unwrap()
            .status(),
        400
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
    // It still presents the whole filesystem, shared.
    assert_eq!(clone["virtual_size_bytes"], 512 * 1024 * 1024);
    assert!(clone["allocated_bytes"].as_u64().unwrap() > clone_cost * DEFAULT_EXTENT_SIZE);

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
