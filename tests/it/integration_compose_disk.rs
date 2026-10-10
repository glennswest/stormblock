//! `/api/v1/volumes/compose/{pallet,disk}` over HTTP — a per-node disk as a
//! chain of goldens, seen the way stormboot or a registry sees it.
//!
//! The unit tests in `src/volume/disk.rs` prove the map; these prove the
//! contract: names resolve, the version follows, the disk reads back as a
//! GPT through the engine's own device, and a second disk of the same layout
//! costs nothing.

use crate::common;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::pallet::gpt::Gpt;
use stormblock::pallet::format::Pallet;
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

async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 2 * 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(Uuid::new_v4()), devices[0].clone())
        .await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

/// A sealed golden of `size` bytes filled with `fill`, made through the engine.
async fn golden(state: &AppState, name: &str, size: u64, fill: u8) -> Uuid {
    let mut vm = state.volume_manager.lock().await;
    let id = vm.create_volume_any(name, size).await.unwrap();
    let h = vm.get_volume_handle(&id).unwrap();
    h.write(0, &vec![fill; size as usize]).await.unwrap();
    vm.seal_volume(id, None).await.unwrap();
    id.0
}

async fn free_slots(state: &AppState) -> u64 {
    state.slab_registry.read().await.total_free_slots()
}

async fn post(client: &reqwest::Client, url: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = client.post(url).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Everything below is in slots: a member's span is its golden rounded up to
/// one, so the arithmetic only reads if the sizes are.
const SLOT: u64 = DEFAULT_EXTENT_SIZE;

#[tokio::test]
async fn a_pallet_and_a_disk_compose_over_http_and_read_back_as_a_gpt() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    golden(&state, "kernel.golden", 3 * SLOT, 0x4B).await;
    golden(&state, "initrd.golden", 2 * SLOT, 0x49).await;
    golden(&state, "esp.golden", 2 * SLOT, 0xE5).await;
    let kernel_len = 2 * SLOT + 4096;
    let before = free_slots(&state).await;

    // A boot pallet out of the goldens, by name.
    let (status, pallet) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/pallet"),
        serde_json::json!({
            "name": "boot-v1",
            "pallet": "boot",
            "kind": "boot",
            "version_label": "6.12.0",
            "members": [
                {"name": "kernel", "role": "kernel", "kind": "kernel", "volume": "kernel.golden", "len": kernel_len.to_string()},
                {"name": "initramfs", "role": "initramfs", "kind": "initramfs", "volume": "initrd.golden"},
                {"name": "cmdline", "role": "cmdline", "kind": "bootconfig", "text": "root=/dev/nvme0n1p2 ro"}
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{pallet}");
    assert_eq!(pallet["sealed"], true);
    assert_eq!(pallet["fs"]["kind"], "pallet");
    assert_eq!(pallet["pallet"]["version"], 1);
    assert_eq!(pallet["pallet"]["lba"], 4096);
    assert_eq!(pallet["pallet"]["shared_bytes"], 5 * SLOT);
    assert_eq!(pallet["pallet"]["members"][0]["offset"], SLOT);
    assert_eq!(pallet["pallet"]["members"][0]["len"], kernel_len);
    assert_eq!(pallet["pallet"]["members"][0]["shared"], true);
    assert_eq!(pallet["pallet"]["members"][1]["offset"], 4 * SLOT, "after the whole kernel golden");
    assert_eq!(pallet["pallet"]["members"][2]["shared"], false);
    assert_eq!(before - free_slots(&state).await, 2, "a header slot and a cmdline slot");

    // The disk: ESP then the pallet, by name.
    let before = free_slots(&state).await;
    let (status, disk) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/disk"),
        serde_json::json!({
            "name": "node1.disk",
            "partitions": [
                {"volume": "esp.golden", "name": "EFI", "type": "esp"},
                {"volume": "boot-v1", "priority": 5}
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{disk}");
    assert_eq!(disk["fs"]["kind"], "gpt");
    assert_eq!(disk["disk"]["lba"], 4096);
    assert_eq!(disk["disk"]["gpt_minted"], true);
    assert_eq!(disk["disk"]["written_bytes"], 0);
    assert_eq!(disk["disk"]["partitions"][0]["start_bytes"], SLOT);
    assert_eq!(disk["disk"]["partitions"][1]["start_bytes"], 3 * SLOT);
    assert_eq!(before - free_slots(&state).await, 2, "only the two GPT goldens are new");
    let disk_id = Uuid::parse_str(disk["id"].as_str().unwrap()).unwrap();
    let disk_guid = Uuid::parse_str(disk["disk"]["disk_guid"].as_str().unwrap()).unwrap();

    // Read back through the engine's device, the way a target serves it.
    let dev: Arc<dyn BlockDevice> = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&VolumeId(disk_id)).unwrap()
    };
    let gpt = Gpt::read(&dev).await.unwrap();
    assert_eq!(gpt.block_size, 4096);
    assert_eq!(Uuid::from_bytes_le(gpt.disk_guid), disk_guid);
    let parts: Vec<_> = gpt.partitions().collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].1.name, "EFI");
    assert_eq!(parts[1].1.name, "boot-v1");
    let view = gpt.view(&dev, parts[1].0).unwrap();
    let p = Pallet::read(&view).await.unwrap();
    assert_eq!(p.name(), "boot");
    p.verify_all(&view).await.unwrap();
    let k = p.find("kernel").unwrap();
    let mut buf = vec![0u8; 4096];
    p.read_member(&k, &view, 0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0x4B));
    let mut esp = vec![0u8; 4096];
    gpt.view(&dev, parts[0].0).unwrap().read_at(0, &mut esp).await.unwrap();
    assert!(esp.iter().all(|&b| b == 0xE5));

    // A second node of the same layout: nothing minted, nothing written,
    // same PARTUUIDs.
    let before = free_slots(&state).await;
    let (status, two) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/disk"),
        serde_json::json!({
            "name": "node2.disk",
            "partitions": [
                {"volume": "esp.golden", "name": "EFI", "type": "esp"},
                {"volume": "boot-v1", "priority": 5}
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{two}");
    assert_eq!(two["disk"]["gpt_minted"], false);
    assert_eq!(two["disk"]["head_golden"], disk["disk"]["head_golden"]);
    assert_eq!(two["disk"]["partitions"][1]["partuuid"], disk["disk"]["partitions"][1]["partuuid"]);
    assert_eq!(free_slots(&state).await, before, "a second disk is a map");

    // A new version of the pallet: the version follows, and the initramfs
    // is shared again.
    golden(&state, "kernel2.golden", 3 * SLOT, 0x4C).await;
    let (status, v2) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/pallet"),
        serde_json::json!({
            "name": "boot-v2",
            "pallet": "boot",
            "kind": "boot",
            "members": [
                {"name": "kernel", "role": "kernel", "kind": "kernel", "volume": "kernel2.golden"},
                {"name": "initramfs", "role": "initramfs", "kind": "initramfs", "volume": "initrd.golden"},
                {"name": "cmdline", "role": "cmdline", "kind": "bootconfig", "text": "root=/dev/nvme0n1p2 ro"}
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{v2}");
    assert_eq!(v2["pallet"]["version"], 2);

    // What is refused: a member with both sources, an unknown volume, an
    // unknown type, a size too small.
    let (status, _) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/pallet"),
        serde_json::json!({"name": "bad", "members": [{"name": "k", "role": "kernel", "volume": "kernel.golden", "text": "x"}]}),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/pallet"),
        serde_json::json!({"name": "bad", "members": [{"name": "k", "role": "kernel", "volume": "nope"}]}),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/disk"),
        serde_json::json!({"name": "bad", "partitions": [{"volume": "boot-v1", "type": "wat"}]}),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/disk"),
        serde_json::json!({"name": "bad", "size": (2 * SLOT).to_string(), "partitions": [{"volume": "boot-v1"}]}),
    )
    .await;
    assert_eq!(status, 400);

    server.abort();
}

/// #228: a disk composed at 512-byte LBAs is *presented* at 512 — what
/// firmware parses its GPT and FAT in — and so is every clone of it; a plain
/// volume can ask for 512 too, and nothing else is accepted.
#[tokio::test]
async fn a_disk_composed_at_512_is_presented_at_512_and_its_clones_too() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    golden(&state, "esp512.golden", 2 * SLOT, 0xE5).await;
    let (status, disk) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/disk"),
        serde_json::json!({
            "name": "boot512.disk",
            "lba": 512,
            "partitions": [{"volume": "esp512.golden", "name": "EFI", "type": "esp"}]
        }),
    )
    .await;
    assert_eq!(status, 201, "{disk}");
    assert_eq!(disk["disk"]["lba"], 512);
    assert_eq!(disk["lba"], 512, "the volume is presented at the size its table was laid in");
    let disk_id = Uuid::parse_str(disk["id"].as_str().unwrap()).unwrap();

    let dev: Arc<dyn BlockDevice> = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&VolumeId(disk_id)).unwrap()
    };
    assert_eq!(dev.block_size(), 512);
    let gpt = Gpt::read(&dev).await.unwrap();
    assert_eq!(gpt.block_size, 512);
    // The primary header where firmware on a 512-byte medium looks: LBA 1.
    let mut lba1 = vec![0u8; 512];
    dev.read(512, &mut lba1).await.unwrap();
    assert_eq!(&lba1[..8], b"EFI PART");

    // A clone — what a boot claim hands a machine — keeps it.
    let (status, sealed) = post(&client, &format!("{base}/api/v1/volumes/{disk_id}/seal"), serde_json::json!({"force": true})).await;
    assert_eq!(status, 200, "{sealed}");
    let (status, clone) = post(
        &client,
        &format!("{base}/api/v1/volumes/{disk_id}/clone"),
        serde_json::json!({"name": "boot512-clone"}),
    )
    .await;
    assert!(status == 200 || status == 201, "{status} {clone}");
    let clone_id = clone["id"].as_str().or_else(|| clone["volume"]["id"].as_str()).unwrap().to_string();
    let got: serde_json::Value = client
        .get(format!("{base}/api/v1/volumes/{clone_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["lba"], 512, "{got}");

    // A plain create: 512 when asked, 4096 otherwise, nothing else.
    let (status, v) = post(&client, &format!("{base}/api/v1/volumes"), serde_json::json!({"name": "esp-raw", "size": "64M", "lba": 512})).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(v["lba"], 512);
    let (status, v) = post(&client, &format!("{base}/api/v1/volumes"), serde_json::json!({"name": "data", "size": "64M"})).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(v["lba"], 4096);
    let (status, _) = post(&client, &format!("{base}/api/v1/volumes"), serde_json::json!({"name": "odd", "size": "64M", "lba": 1024})).await;
    assert_eq!(status, 400);
    let (status, _) = post(
        &client,
        &format!("{base}/api/v1/volumes"),
        serde_json::json!({"name": "c", "from_template": "esp512.golden", "lba": 512}),
    )
    .await;
    assert_eq!(status, 400, "a clone takes its source's size");

    server.abort();
}

/// #378: a composed pallet is signed without rebuilding it: its message is
/// read, signed with a key that stays with the signer, and attached. The
/// signed pallet is a sealed copy-on-write clone (one new slot, the header);
/// the source, which composed disks share, is unchanged.
#[tokio::test]
async fn a_composed_pallet_is_signed_into_a_sealed_clone_and_the_source_is_left_alone() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();
    golden(&state, "kernel.golden", 2 * SLOT, 0x4B).await;
    let (status, pallet) = post(
        &client,
        &format!("{base}/api/v1/volumes/compose/pallet"),
        serde_json::json!({
            "name": "boot-v1", "pallet": "boot", "kind": "boot", "version_label": "6.12.0",
            "members": [
                {"name": "kernel", "role": "kernel", "kind": "kernel", "volume": "kernel.golden"},
                {"name": "cmdline", "role": "cmdline", "kind": "bootconfig", "text": "root=/dev/nvme0n1p2 ro"}
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{pallet}");
    let src = pallet["id"].as_str().unwrap().to_string();

    let rep: serde_json::Value = client.get(format!("{base}/api/v1/volumes/{src}/pallet-signature")).send().await.unwrap().json().await.unwrap();
    assert_eq!(rep["signature"], "unsigned", "{rep}");
    let message = hex::decode(rep["message"].as_str().unwrap()).unwrap();
    assert_eq!(message.len(), stormblock_pallet_format::SIGNING_MESSAGE_LEN);

    // The signer's side: a key that never reaches the engine.
    let (pk, sig) = stormblock::pallet::sign::sign(&[42u8; 32], &message).unwrap();
    let (other_pk, _) = stormblock::pallet::sign::sign(&[43u8; 32], &message).unwrap();
    let url = format!("{base}/api/v1/volumes/{src}/pallet-signature");
    let (status, refused) = post(&client, &url, serde_json::json!({ "public_key": hex::encode(other_pk), "signature": hex::encode(sig) })).await;
    assert_eq!(status, 400, "another key's: {refused}");

    let before = free_slots(&state).await;
    let (status, signed) = post(&client, &url, serde_json::json!({ "public_key": hex::encode(pk), "signature": hex::encode(sig) })).await;
    assert_eq!(status, 201, "{signed}");
    let key_id = hex::encode(stormblock::pallet::sign::key_id(&pk));
    assert_eq!(signed["signature"], format!("ed25519:{key_id}"));
    assert_eq!(signed["name"], "boot-v1.signed");
    assert_eq!(before - free_slots(&state).await, 1, "one new slot: the header");
    let new = Uuid::parse_str(signed["volume"].as_str().unwrap()).unwrap();

    // The signed pallet: sealed, parses, verifies with the key, and every
    // member still checks against the unchanged manifest digest.
    let dev = {
        let vm = state.volume_manager.lock().await;
        assert!(vm.is_sealed(&stormblock::volume::VolumeId(new)));
        vm.get_volume(&stormblock::volume::VolumeId(new)).unwrap()
    };
    let mut head = vec![0u8; 64 * 1024];
    dev.read(0, &mut head).await.unwrap();
    let p = stormblock_pallet_format::Pallet::parse(&head).unwrap();
    match p.sb.signature() {
        stormblock_pallet_format::Signature::Ed25519 { key_id: k, signature } => {
            assert_eq!(hex::encode(k), key_id);
            assert!(stormblock::pallet::sign::verifies(&pk, &p.sb.signing_message(), &signature));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(hex::encode(p.sb.manifest_digest), rep["manifest_digest"].as_str().unwrap());

    // The source is unchanged.
    let after: serde_json::Value = client.get(format!("{base}/api/v1/volumes/{src}/pallet-signature")).send().await.unwrap().json().await.unwrap();
    assert_eq!(after["signature"], "unsigned");
    server.abort();
}
