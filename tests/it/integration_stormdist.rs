//! stormdist (#442) against the real engine: a sealed golden exported
//! (allocated ranges, device sha256, provenance), made into a signed
//! stormdist ISO by stormdist's own `make`, restored into another engine
//! through `POST /api/v1/stormdist/restores`; restored again (nothing
//! written); refused for a corrupt ISO, a wrong key and a diff on the wrong
//! base; and a restore whose engine copy does not match its manifest
//! aborted with no staging volume left and nothing published.

use crate::common;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use stormblock::drive::filedev::FileDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::volume::VolumeManager;
use stormdist::format::{Flavour, Kind, Range};
use stormdist::media::{Engine, Forge, Input, Media, Recipe};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1 << 20;
const SIZE: u64 = 16 * MIB;
const V1: &str = "golden-demo-0123456789ab";
const V2: &str = "golden-other-fedcba987654";

async fn engine(dir: &Path, tag: &str) -> (Arc<AppState>, String) {
    let mut config = StormBlockConfig::default();
    let data = dir.join(format!("{tag}-data"));
    std::fs::create_dir_all(&data).unwrap();
    config.management.data_dir = Some(data.to_string_lossy().to_string());
    let mut vm = VolumeManager::new(MIB);
    let dev = FileDevice::open_with_capacity(dir.join(format!("{tag}.bin")).to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}/api/v1"))
}

/// A sealed golden with data in a few places and holes between.
async fn golden(state: &AppState, name: &str, seed: u8) -> Vec<u8> {
    let mut image = vec![0u8; SIZE as usize];
    for (i, off) in [0u64, 3 * MIB, 3 * MIB + 512 * 1024, 9 * MIB, SIZE - MIB].iter().enumerate() {
        let len = if i == 1 { 256 * 1024 } else { MIB as usize / 2 };
        for (j, b) in image[*off as usize..*off as usize + len].iter_mut().enumerate() {
            *b = (j as u8).wrapping_mul(31).wrapping_add(seed).wrapping_add(i as u8) | 1;
        }
    }
    let mut vm = state.volume_manager.lock().await;
    let id = vm.create_volume_any(name, SIZE).await.unwrap();
    let dev = vm.get_volume(&id).unwrap();
    let mut off = 0;
    while off < SIZE {
        let chunk = &image[off as usize..(off + MIB) as usize];
        if chunk.iter().any(|b| *b != 0) {
            dev.write(off, chunk).await.unwrap();
        }
        off += MIB;
    }
    dev.flush().await.unwrap();
    vm.seal_volume(id, None).await.unwrap();
    image
}

async fn call(c: &reqwest::Client, m: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let mut r = c.request(m, url);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap_or(Value::Null))
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn pubhex(k: &SigningKey) -> String {
    hex::encode(k.verifying_key().to_bytes())
}

/// The export as a recipe input, its image dumped to a file.
fn input(export: &Value, image: &[u8], dir: &Path) -> Input {
    let name = export["name"].as_str().unwrap().to_string();
    let path = dir.join(format!("{name}.img"));
    std::fs::write(&path, image).unwrap();
    Input {
        name,
        component: export["component"].as_str().unwrap().into(),
        commit: export["commit"].as_str().unwrap().into(),
        image: path,
        size: export["size"].as_u64().unwrap(),
        device_sha256: export["device_sha256"].as_str().unwrap().into(),
        ranges: serde_json::from_value::<Vec<Range>>(export["ranges"].clone()).unwrap(),
    }
}

fn make(version: &str, inputs: Vec<Input>, forge: Option<Forge>, out: &Path, k: &SigningKey) {
    let members = inputs.iter().map(|i| i.name.clone()).collect();
    let recipe = Recipe {
        version: version.into(),
        flavour: vec![Flavour { name: "cilium".into(), members }],
        kind: Kind::Full,
        volumes: inputs,
        forge,
        boot: None,
    };
    stormdist::media::make(&recipe, out, k).unwrap();
}

async fn run(c: &reqwest::Client, api: &str, body: Value) -> Value {
    let (code, job) = call(c, reqwest::Method::POST, format!("{api}/stormdist/restores"), Some(body)).await;
    assert_eq!(code, 202, "{job}");
    let id = job["id"].as_str().unwrap().to_string();
    for _ in 0..600 {
        let (_, v) = call(c, reqwest::Method::GET, format!("{api}/stormdist/restores/{id}"), None).await;
        if v["state"] != "running" {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("restore {id} did not finish");
}

async fn names(state: &AppState) -> Vec<String> {
    state.volume_manager.lock().await.list_volumes().await.into_iter().map(|v| v.1).collect()
}

async fn bytes_of(state: &AppState, name: &str) -> Vec<u8> {
    let vm = state.volume_manager.lock().await;
    let id = vm.find_volume(name).await.unwrap();
    assert!(vm.get_volume_handle(&id).unwrap().is_sealed(), "{name} is sealed");
    let dev = vm.get_volume(&id).unwrap();
    let mut b = vec![0u8; dev.capacity_bytes() as usize];
    dev.read(0, &mut b).await.unwrap();
    b
}

#[tokio::test]
async fn a_golden_goes_out_on_a_stormdist_iso_and_comes_back_into_another_engine() {
    let dir = TempDir::new().unwrap();
    let c = reqwest::Client::new();
    let (src, src_api) = engine(dir.path(), "src").await;
    let image = golden(&src, V1, 1).await;

    // The export: allocated ranges only, sorted, the whole device's digest.
    let (code, ex) = call(&c, reqwest::Method::GET, format!("{src_api}/stormdist/goldens/{V1}"), None).await;
    assert_eq!(code, 200, "{ex}");
    assert_eq!(ex["component"], "demo");
    assert_eq!(ex["commit"], "0123456789ab");
    assert_eq!(ex["provenance_from"], "name");
    assert_eq!(ex["size"], SIZE);
    assert_eq!(ex["device_sha256"], hex::encode(Sha256::digest(&image)));
    let ranges: Vec<Range> = serde_json::from_value(ex["ranges"].clone()).unwrap();
    let mut end = 0;
    for r in &ranges {
        assert!(r.offset >= end && r.length > 0, "sorted, merged: {ranges:?}");
        end = r.offset + r.length;
    }
    // 0, 3 (two writes in one MiB), 9 and 15 MiB: four 1 MiB extents.
    assert_eq!(ex["allocated_bytes"], 4 * MIB, "{ranges:?}");
    for (i, b) in image.iter().enumerate() {
        if *b != 0 {
            assert!(ranges.iter().any(|r| (r.offset..r.offset + r.length).contains(&(i as u64))), "byte {i} outside every range");
        }
    }
    // An unsealed volume has no export.
    src.volume_manager.lock().await.create_volume_any("golden-open-0123456789ab", MIB).await.unwrap();
    let (code, _) = call(&c, reqwest::Method::GET, format!("{src_api}/stormdist/goldens/golden-open-0123456789ab"), None).await;
    assert_eq!(code, 409);

    // A full ISO, with a forge inventory: the golden is a retention root.
    let k = key();
    let iso = dir.path().join("v1.iso");
    let forge = Forge { roots: vec![V1.into()], releases: vec![] };
    make("1.0", vec![input(&ex, &image, dir.path())], Some(forge), &iso, &k);

    // Restored into an empty engine.
    let (dst, dst_api) = engine(dir.path(), "dst").await;
    let job = run(&c, &dst_api, json!({ "iso": iso, "key": pubhex(&k) })).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["report"]["written"], json!([V1]));
    assert_eq!(bytes_of(&dst, V1).await, image);
    assert!(!names(&dst).await.iter().any(|n| n.starts_with("stormdist-staging-")));
    // It exports as it went out, its provenance now recorded.
    let (code, back) = call(&c, reqwest::Method::GET, format!("{dst_api}/stormdist/goldens/{V1}"), None).await;
    assert_eq!(code, 200, "{back}");
    assert_eq!(back["device_sha256"], ex["device_sha256"]);
    assert_eq!(back["provenance_from"], "record");
    assert_eq!(back["ranges"], ex["ranges"]);
    // The forge's inventory: its root and the ISO's signed catalogue.
    let (_, f) = call(&c, reqwest::Method::GET, format!("{dst_api}/stormdist/forge"), None).await;
    assert_eq!(f["forge"]["roots"], json!([V1]), "{f}");
    assert_eq!(f["forge"]["releases"].as_array().unwrap().len(), 1, "{f}");

    // Again: nothing written, nothing new.
    let job = run(&c, &dst_api, json!({ "iso": iso, "key": pubhex(&k) })).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["report"]["kept"], json!([V1]));
    assert_eq!(job["report"]["bytes_written"], 0);
    let (_, f2) = call(&c, reqwest::Method::GET, format!("{dst_api}/stormdist/forge"), None).await;
    assert_eq!(f2, f, "a repeated restore changes nothing");

    // A diff ISO: v2 adds a golden; the chain is v1 then the diff.
    let image2 = golden(&src, V2, 2).await;
    let (_, ex2) = call(&c, reqwest::Method::GET, format!("{src_api}/stormdist/goldens/{V2}"), None).await;
    let full2 = dir.path().join("v2.iso");
    make(
        "2.0",
        vec![input(&ex, &image, dir.path()), input(&ex2, &image2, dir.path())],
        None,
        &full2,
        &k,
    );
    let vk = k.verifying_key();
    let diff = dir.path().join("v1-v2.iso");
    stormdist::media::diff(
        &Media::open(&iso, &vk, None).unwrap(),
        &Media::open(&full2, &vk, None).unwrap(),
        &diff,
        &k,
    )
    .unwrap();
    // On the wrong base: refused, nothing changed.
    let other = dir.path().join("v1b.iso");
    make("1.0b", vec![input(&ex2, &image2, dir.path())], None, &other, &k);
    let before = names(&dst).await;
    let job = run(&c, &dst_api, json!({ "iso": diff, "base": [other], "key": pubhex(&k) })).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(names(&dst).await, before);
    // On its base: the new golden comes over, the old one is kept.
    let job = run(&c, &dst_api, json!({ "iso": diff, "base": [iso], "key": pubhex(&k) })).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["report"]["written"], json!([V2]));
    assert_eq!(job["report"]["kept"], json!([V1]));
    assert_eq!(bytes_of(&dst, V2).await, image2);

    // A wrong key, and a corrupt chunk: refused, nothing changed.
    let (fresh, fresh_api) = engine(dir.path(), "fresh").await;
    let job = run(&c, &fresh_api, json!({ "iso": iso, "key": pubhex(&SigningKey::from_bytes(&[9u8; 32])) })).await;
    assert_eq!(job["state"], "failed", "{job}");
    let mut bad = std::fs::read(&iso).unwrap();
    let probe = &image[9 * MIB as usize..9 * MIB as usize + 64];
    let at = bad.windows(probe.len()).position(|w| w == probe).expect("the chunk is in the ISO");
    bad[at + 10] ^= 0xff;
    let corrupt = dir.path().join("corrupt.iso");
    std::fs::write(&corrupt, &bad).unwrap();
    let job = run(&c, &fresh_api, json!({ "iso": corrupt, "key": pubhex(&k) })).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap().contains("chunk") || job["error"].as_str().unwrap().contains("hash"), "{job}");
    assert!(names(&fresh).await.is_empty(), "{:?}", names(&fresh).await);
}

/// The engine's own copy is checked against the manifest at commit: one
/// that does not match aborts the restore, and its staging volume goes.
#[tokio::test]
async fn a_restore_whose_copy_does_not_match_its_manifest_publishes_nothing() {
    let dir = TempDir::new().unwrap();
    let (st, _) = engine(dir.path(), "e").await;
    let rt = tokio::runtime::Handle::current();
    let st2 = st.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut e = stormblock::mgmt::stormdist::Restore::new(st2, rt, None, "test".into());
        let release = stormdist::format::Release {
            format: stormdist::format::FORMAT,
            version: "1".into(),
            flavour: vec![],
            kind: Kind::Full,
            base: None,
            members: vec![],
            chunks: 0,
            bytes: 0,
            forge_sha256: None,
        };
        e.begin(&release, None).unwrap();
        let data = vec![5u8; MIB as usize];
        let golden = stormdist::format::Golden {
            name: "golden-x-0123456789ab".into(),
            component: "x".into(),
            commit: "0123456789ab".into(),
            size: 2 * MIB,
            // The digest of a blank device, not of what is written.
            device_sha256: hex::encode(Sha256::digest(vec![0u8; 2 * MIB as usize])),
            chunk_size: stormdist::format::CHUNK_SIZE,
            extents: vec![stormdist::format::Extent { offset: 0, length: MIB, sha256: hex::encode(Sha256::digest(&data)) }],
        };
        e.create(&golden).unwrap();
        e.write(&golden.name, 0, &data).unwrap();
        let r = e.commit();
        if r.is_err() {
            e.abort();
        }
        r
    })
    .await
    .unwrap();
    let err = out.expect_err("a copy that does not match is not published");
    assert!(format!("{err:#}").contains("not published"), "{err:#}");
    assert!(names(&st).await.is_empty(), "{:?}", names(&st).await);
}
