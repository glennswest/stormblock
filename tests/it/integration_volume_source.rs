//! #737: a sealed volume as the source of an import and of an image build's
//! members and ESP, read in place, as a pallet publish's `source: volume`
//! member is: the registry keeps each blob as a raw sealed volume with its
//! length (stormblock-registry#244, #248).

use crate::common;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::source::{SourceError, VolumeSource};
use stormblock::volume::thin::VolumeError;
use stormblock::volume::{VolumeId, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 64 * 1024;
const ADMIN: &str = "Bearer admin-token";

async fn state(dir: &TempDir, images: Option<&std::path::Path>) -> Arc<AppState> {
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("n1".to_string());
    c.management.api_token = Some("node-token".to_string());
    c.management.admin_token = Some("admin-token".to_string());
    c.images.dir = images.map(|p| p.to_string_lossy().to_string());
    let devices = common::create_file_devices(dir, 2, 96 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    Arc::new(AppState::new(c, vm, reg, gem))
}

async fn serve(state: Arc<AppState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    format!("http://{addr}")
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64).wrapping_mul(2654435761) >> 9) as u8 ^ seed).collect()
}

async fn json(r: reqwest::Response) -> (u16, Value) {
    let s = r.status().as_u16();
    let t = r.text().await.unwrap();
    (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

/// A volume holding `bytes` (rounded up to its slots with a non-zero tail,
/// so a `byte_len` that is not honoured shows), sealed unless `seal` is
/// false: a registry blob volume.
async fn blob_volume(state: &AppState, name: &str, bytes: &[u8], seal: bool) -> VolumeId {
    let mut vm = state.volume_manager.lock().await;
    let size = (bytes.len() as u64).div_ceil(SLOT).max(1) * SLOT;
    let id = vm.create_volume_any(name, size).await.unwrap();
    let mut all = vec![0xEEu8; size as usize];
    all[..bytes.len()].copy_from_slice(bytes);
    vm.get_volume_handle(&id).unwrap().write(0, &all).await.unwrap();
    if seal {
        vm.seal_volume(id, None).await.unwrap();
    }
    id
}

async fn read_volume(state: &AppState, id: VolumeId, len: usize) -> Vec<u8> {
    let dev = state.volume_manager.lock().await.get_volume(&id).expect("the volume");
    let mut buf = vec![0u8; len.div_ceil(4096) * 4096];
    dev.read(0, &mut buf).await.unwrap();
    buf.truncate(len);
    buf
}

/// The source itself: sealed only, `byte_len` within the volume, reads
/// short at `byte_len`, a delete refused while held, an unseal failing the
/// read, the hold gone with the last clone.
#[tokio::test]
async fn a_volume_source_is_sealed_held_and_read_to_its_byte_len() {
    let dir = TempDir::new().unwrap();
    let st = state(&dir, None).await;
    let bytes = data(3 * SLOT as usize + 777, 5);
    let id = blob_volume(&st, "blob-a", &bytes, true).await;
    let open = blob_volume(&st, "not-sealed", b"x", false).await;
    let vm = st.volume_manager.lock().await;

    assert!(matches!(VolumeSource::open(&vm, VolumeId(uuid::Uuid::new_v4()), None, "t").await, Err(SourceError::NotFound(_))));
    assert!(matches!(VolumeSource::open(&vm, open, None, "t").await, Err(SourceError::Conflict(_))));
    assert!(matches!(VolumeSource::open(&vm, id, Some(4 * SLOT + 1), "t").await, Err(SourceError::Invalid(_))));
    // No size given: the whole volume, padding and all.
    assert_eq!(VolumeSource::open(&vm, id, None, "t").await.unwrap().len(), 4 * SLOT);

    let src = VolumeSource::open(&vm, id, Some(bytes.len() as u64), "a test import").await.unwrap();
    assert_eq!(src.len(), bytes.len() as u64);
    let mut buf = vec![0u8; 70_001];
    src.read_exact_at(SLOT - 3, &mut buf).await.unwrap();
    assert_eq!(&buf[..], &bytes[SLOT as usize - 3..SLOT as usize - 3 + 70_001]);
    let mut tail = vec![0u8; 10_000];
    assert_eq!(src.read_at(bytes.len() as u64 - 100, &mut tail).await.unwrap(), 100);
    assert_eq!(&tail[..100], &bytes[bytes.len() - 100..]);
    assert_eq!(src.read_at(bytes.len() as u64, &mut tail).await.unwrap(), 0);
    assert!(src.read_exact_at(bytes.len() as u64 - 100, &mut tail).await.is_err());
    {
        use stormblock::pallet::MemberContent;
        assert_eq!(src.byte_len(), bytes.len() as u64);
        let mut x = vec![0u8; 200];
        assert!(MemberContent::read_at(&src, bytes.len() as u64 - 100, &mut x).await.is_err());
    }
    drop(vm);

    // Held: a delete is refused, naming who holds it, by every clone.
    let second = src.clone();
    drop(src);
    match st.volume_manager.lock().await.delete_volume(id).await {
        Err(VolumeError::InUse { by, .. }) => assert_eq!(by, vec!["a test import".to_string()]),
        e => panic!("{e:?}"),
    }
    // Unsealed under it: the next read fails rather than read other bytes.
    st.volume_manager.lock().await.unseal_volume(id).await.unwrap();
    let mut one = vec![0u8; 512];
    match second.read_exact_at(0, &mut one).await {
        Err(SourceError::Io(m)) => assert!(m.contains("unsealed"), "{m}"),
        e => panic!("{e:?}"),
    }
    // Sealed again: still not the seal it began on.
    st.volume_manager.lock().await.seal_volume(id, None).await.unwrap();
    assert!(second.read_exact_at(0, &mut one).await.is_err());
    drop(second);
    st.volume_manager.lock().await.delete_volume(id).await.unwrap();
}

/// Over HTTP: `volumes/import` with `volume` + `byte_len` (a qcow2 with a
/// compressed cluster, by id; a raw ISO, by name), and an image build whose
/// member and ESP are volumes; each source volume unchanged and free again
/// afterwards; refusals before anything is made.
#[tokio::test]
async fn imports_and_image_builds_read_a_sealed_volume_in_place() {
    use stormblock::drive::filedev::FileDevice;
    use stormblock::pallet::format::MemberExt;
    use stormblock::pallet::PalletStore;

    let dir = TempDir::new().unwrap();
    let images = dir.path().join("images");
    std::fs::create_dir_all(&images).unwrap();
    let st = state(&dir, Some(&images)).await;
    let base = serve(st.clone()).await;
    let c = reqwest::Client::new();

    let import = |body: Value| {
        let (c, base) = (c.clone(), base.clone());
        async move {
            let (s, v) = json(c.post(format!("{base}/api/v1/volumes/import")).header("authorization", ADMIN).json(&body).send().await.unwrap()).await;
            if s != 202 {
                return (s, v);
            }
            let id = v["id"].as_str().unwrap().to_string();
            for _ in 0..600 {
                let (_, v) = json(c.get(format!("{base}/api/v1/volumes/import/{id}")).header("authorization", ADMIN).send().await.unwrap()).await;
                if v["state"] == "done" || v["state"] == "failed" {
                    return (s, v);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("import {id} never ended");
        }
    };

    // --- import: a qcow2 by id, an ISO by name
    use stormblock::image::decode::qcow2::testimg::{build, C};
    let a: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    let z: Vec<u8> = (0..65536).map(|i| ((i * 7) % 253) as u8).collect();
    let qcow = build(16, &[C::Data(a.clone()), C::Hole, C::Compressed(z.clone()), C::Zero]);
    let vq = blob_volume(&st, "blob-qcow2", &qcow, true).await;
    let iso = data(3 * 65536 + 2048, 33);
    let vi = blob_volume(&st, "blob-iso", &iso, true).await;

    let (s, v) = import(serde_json::json!({ "name": "from-qcow2", "volume": vq.0.to_string(), "byte_len": qcow.len(), "verify": false })).await;
    assert_eq!((s, v["state"].as_str()), (202, Some("done")), "{v}");
    assert_eq!((v["format"].as_str(), v["virtual_size"].as_u64()), (Some("qcow2"), Some(4 * 65536)), "{v}");
    assert!(v["source"].as_str().unwrap().contains(&vq.0.to_string()), "{v}");
    let got = read_volume(&st, VolumeId(v["volume_id"].as_str().unwrap().parse().unwrap()), 4 * 65536).await;
    assert_eq!(&got[..65536], &a[..]);
    assert!(got[65536..2 * 65536].iter().all(|&b| b == 0));
    assert_eq!(&got[2 * 65536..3 * 65536], &z[..]);

    let (s, v) = import(serde_json::json!({ "name": "from-iso", "volume": "blob-iso", "byte_len": iso.len(), "format": "iso", "verify": false })).await;
    assert_eq!((s, v["state"].as_str()), (202, Some("done")), "{v}");
    assert_eq!(v["virtual_size"].as_u64(), Some(iso.len() as u64), "{v}: byte_len, not the volume's padded size");
    assert_eq!(read_volume(&st, VolumeId(v["volume_id"].as_str().unwrap().parse().unwrap()), iso.len()).await, iso);

    // Unchanged, still sealed, and no longer held.
    assert_eq!(read_volume(&st, vq, qcow.len()).await, qcow);
    assert_eq!(read_volume(&st, vi, iso.len()).await, iso);
    {
        let vm = st.volume_manager.lock().await;
        assert!(vm.is_sealed(&vq) && vm.is_sealed(&vi));
        assert!(vm.holds().held_by(vq.0).is_empty() && vm.holds().held_by(vi.0).is_empty());
    }

    // Refusals, each before a job: missing 404, not sealed 409, byte_len
    // past the end 400, byte_len alone 400, two sources 400.
    let unsealed = blob_volume(&st, "still-open", &iso, false).await;
    let missing = uuid::Uuid::new_v4().to_string();
    for (body, code) in [
        (serde_json::json!({ "name": "x", "volume": missing }), 404),
        (serde_json::json!({ "name": "x", "volume": unsealed.0.to_string() }), 409),
        (serde_json::json!({ "name": "x", "volume": vi.0.to_string(), "byte_len": 4 * SLOT + 1 }), 400),
        (serde_json::json!({ "name": "x", "file": "/x", "byte_len": 10 }), 400),
        (serde_json::json!({ "name": "x", "volume": vi.0.to_string(), "file": "/x" }), 400),
    ] {
        let (s, v) = import(body.clone()).await;
        assert_eq!(s, code, "{body}: {v}");
    }
    let (_, v) = json(c.get(format!("{base}/api/v1/volumes/import")).header("authorization", ADMIN).send().await.unwrap()).await;
    assert_eq!(v["count"].as_u64(), Some(2), "{v}");
    {
        let vm = st.volume_manager.lock().await;
        assert!(vm.holds().held_by(vi.0).is_empty(), "a refused import holds nothing");
    }

    // --- image build: a member and the ESP from volumes
    let kernel = data(200_000, 21);
    let vk = blob_volume(&st, "blob-kernel", &kernel, true).await;
    let esp = data(300 * 1024 + 512, 77);
    let ve = blob_volume(&st, "blob-esp", &esp, true).await;
    let out = images.join("vol.img");
    let spec = serde_json::json!({
        "name": "volume-image",
        "esp": { "from_volume": ve.0.to_string(), "byte_len": esp.len() },
        "pallet": [{ "name": "boot", "members": [
            { "name": "vmlinuz", "role": "kernel", "volume": vk.0.to_string(), "byte_len": kernel.len() },
            { "name": "cmdline", "role": "cmdline", "text": "console=ttyS0" },
        ]}],
    });
    let (s, v) = json(c.post(format!("{base}/api/v1/images/build")).header("authorization", ADMIN)
        .json(&serde_json::json!({ "spec": spec, "out": out.to_string_lossy() })).send().await.unwrap()).await;
    assert_eq!(s, 200, "{v}");
    let img: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(out.to_str().unwrap()).await.unwrap());
    let mut store = PalletStore::default();
    store.add_drive("vol.img", img);
    let found = store.scan().await;
    assert_eq!(found.len(), 1, "one pallet in the image");
    let pallet = store.open(&found[0]).await.unwrap();
    let m = pallet.members().into_iter().find(|m| m.name() == "vmlinuz").expect("the volume member");
    assert_eq!((m.digest_hex(), m.byte_len), (hex::encode(Sha256::digest(&kernel)), kernel.len() as u64));
    // The ESP's bytes are in the image, at a block boundary.
    let raw = std::fs::read(&out).unwrap();
    let at = (0..raw.len() - esp.len()).step_by(512).find(|&o| raw[o..o + 64] == esp[..64]).expect("the ESP's bytes");
    assert_eq!(&raw[at..at + esp.len()], &esp[..]);
    {
        let vm = st.volume_manager.lock().await;
        assert!(vm.holds().held_by(vk.0).is_empty() && vm.holds().held_by(ve.0).is_empty(), "released after the build");
    }
    assert_eq!(read_volume(&st, vk, kernel.len()).await, kernel);

    // Refused before anything is written: missing 404, not sealed 409, two
    // sources 400.
    let out2 = images.join("refused.img");
    for (spec, code) in [
        (serde_json::json!({ "name": "x", "pallet": [{ "name": "boot", "members": [{ "name": "k", "role": "kernel", "volume": missing }] }] }), 404),
        (serde_json::json!({ "name": "x", "esp": { "from_volume": unsealed.0.to_string() }, "pallet": [] }), 409),
        (serde_json::json!({ "name": "x", "pallet": [{ "name": "boot", "members": [{ "name": "k", "role": "kernel", "volume": vk.0.to_string(), "text": "x" }] }] }), 400),
        (serde_json::json!({ "name": "x", "esp": { "from_volume": ve.0.to_string(), "from_dir": "/x" }, "pallet": [] }), 400),
    ] {
        let (s, v) = json(c.post(format!("{base}/api/v1/images/build")).header("authorization", ADMIN)
            .json(&serde_json::json!({ "spec": spec, "out": out2.to_string_lossy() })).send().await.unwrap()).await;
        assert_eq!(s, code, "{spec}: {v}");
        assert!(!out2.exists());
    }
}
