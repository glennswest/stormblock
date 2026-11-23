//! `/api/v1/blobs` and `/api/v1/blobstore` (#732) on a real engine, over
//! HTTP: the surface stormblock-registry#241 calls, with the node's token
//! (and an admin token set, which a blob delete does not need).

use crate::common;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 64 * 1024;
const TOKEN: &str = "node-token";

async fn state(dir: &TempDir) -> Arc<AppState> {
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("n1".to_string());
    c.management.api_token = Some(TOKEN.to_string());
    c.management.admin_token = Some("admin-token".to_string());
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

fn digest(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64).wrapping_mul(2654435761) >> 9) as u8 ^ seed).collect()
}

async fn json(r: reqwest::Response) -> (u16, Value) {
    let s = r.status().as_u16();
    let t = r.text().await.unwrap();
    (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

#[tokio::test]
async fn the_registry_surface_puts_reads_lists_uploads_and_deletes_by_digest() {
    let dir = TempDir::new().unwrap();
    let st = state(&dir).await;
    let base = serve(st.clone()).await;
    let c = reqwest::Client::new();
    let auth = format!("Bearer {TOKEN}");

    // No store yet, and nothing made by asking.
    let (s, v) = json(c.get(format!("{base}/api/v1/blobstore")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!((s, v["exists"].as_bool()), (200, Some(false)), "{v}");
    let (s, _) = json(c.get(format!("{base}/api/v1/blobs/{}", digest(b"x"))).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 404);
    // The token is asked for.
    let r = c.put(format!("{base}/api/v1/blobs/{}", digest(b"x"))).body("x").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);

    // A put, twice: made, then held.
    let layer = data(300_000, 1);
    let d = digest(&layer);
    let url = format!("{base}/api/v1/blobs/{d}");
    let (s, v) = json(c.put(&url).header("authorization", &auth).body(layer.clone()).send().await.unwrap()).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["size"].as_u64(), Some(300_000));
    let (s, v) = json(c.put(&url).header("authorization", &auth).body(layer.clone()).send().await.unwrap()).await;
    assert_eq!((s, v["outcome"].as_str()), (200, Some("existed")), "{v}");

    // A mismatch keeps nothing.
    let wrong = format!("{base}/api/v1/blobs/{}", digest(b"other"));
    let (s, v) = json(c.put(&wrong).header("authorization", &auth).body(layer.clone()).send().await.unwrap()).await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(c.head(&wrong).header("authorization", &auth).send().await.unwrap().status().as_u16(), 404);
    // A digest spelt wrong is refused.
    let (s, _) = json(c.put(format!("{base}/api/v1/blobs/sha256:ABC")).header("authorization", &auth).body("x").send().await.unwrap()).await;
    assert_eq!(s, 400);

    // Read: whole, a range, the stat, a range past the end.
    let r = c.get(&url).header("authorization", &auth).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["docker-content-digest"], d.as_str());
    assert_eq!(r.bytes().await.unwrap().as_ref(), &layer[..]);
    let r = c.get(&url).header("authorization", &auth).header("range", "bytes=70000-200000").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 70000-200000/300000");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &layer[70_000..200_001]);
    let r = c.get(&url).header("authorization", &auth).header("range", "bytes=-10").send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), &layer[300_000 - 10..]);
    let r = c.head(&url).header("authorization", &auth).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-length"], "300000");
    let r = c.get(&url).header("authorization", &auth).header("range", "bytes=300000-").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 416);

    // Small blobs: configs and manifests.
    let mut small = Vec::new();
    for i in 0..20u32 {
        let b = format!("{{\"config\":{i}}}").into_bytes();
        let (s, v) = json(c.put(format!("{base}/api/v1/blobs/{}", digest(&b))).header("authorization", &auth).body(b.clone()).send().await.unwrap()).await;
        assert_eq!(s, 201, "{v}");
        small.push(b);
    }

    // An upload session: chunks at the session's length, a wrong offset
    // refused with the length, the last chunk with the commit.
    let (s, v) = json(c.post(format!("{base}/api/v1/blobs/uploads")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 201, "{v}");
    let up = format!("{base}{}", v["location"].as_str().unwrap());
    let big = data(5 * SLOT as usize + 4321, 2);
    let (s, v) = json(c.patch(&up).header("authorization", &auth).header("content-range", "0-99999").body(big[..100_000].to_vec()).send().await.unwrap()).await;
    assert_eq!((s, v["length"].as_u64()), (202, Some(100_000)), "{v}");
    let (s, v) = json(c.patch(format!("{up}?offset=5")).header("authorization", &auth).body(vec![1u8; 10]).send().await.unwrap()).await;
    assert_eq!((s, v["length"].as_u64()), (416, Some(100_000)), "{v}");
    let (s, v) = json(c.patch(format!("{up}?offset=100000")).header("authorization", &auth).body(big[100_000..250_000].to_vec()).send().await.unwrap()).await;
    assert_eq!((s, v["length"].as_u64()), (202, Some(250_000)), "{v}");
    let (s, v) = json(c.get(&up).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!((s, v["length"].as_u64()), (200, Some(250_000)), "{v}");
    let bd = digest(&big);
    let (s, v) = json(c.put(format!("{up}?digest={bd}")).header("authorization", &auth).body(big[250_000..].to_vec()).send().await.unwrap()).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["size"].as_u64(), Some(big.len() as u64));
    let (s, _) = json(c.get(&up).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 404, "a committed session is gone");
    let got = c.get(format!("{base}/api/v1/blobs/{bd}")).header("authorization", &auth).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got.as_ref(), &big[..]);

    // An aborted session; its delete takes the node's token.
    let (_, v) = json(c.post(format!("{base}/api/v1/blobs/uploads")).header("authorization", &auth).send().await.unwrap()).await;
    let up2 = format!("{base}{}", v["location"].as_str().unwrap());
    c.patch(&up2).header("authorization", &auth).body(vec![9u8; 70_000]).send().await.unwrap();
    let (s, v) = json(c.delete(&up2).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!((s, v["aborted"].as_bool()), (200, Some(true)), "{v}");

    // The listing, paged.
    let (s, v) = json(c.get(format!("{base}/api/v1/blobs?limit=10")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 200);
    assert_eq!(v["blobs"].as_array().unwrap().len(), 10);
    let next = v["next"].as_str().unwrap().to_string();
    let (_, v2) = json(c.get(format!("{base}/api/v1/blobs?limit=100&after={next}")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(v2["blobs"].as_array().unwrap().len(), 12, "{v2}");
    assert!(v2["next"].is_null());

    // The store's numbers.
    let (s, v) = json(c.get(format!("{base}/api/v1/blobstore")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["blobs"].as_u64(), Some(22));
    assert_eq!(v["sessions"].as_u64(), Some(0));
    assert!(v["used_bytes"].as_u64().unwrap() > 0);
    assert!(v["pool_free_bytes"].as_u64().unwrap() > 0);

    // A scrub reads every blob back against its digest.
    let (s, _) = json(c.post(format!("{base}/api/v1/blobstore/scrub")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(s, 202);
    let mut report = Value::Null;
    for _ in 0..200 {
        let (_, v) = json(c.get(format!("{base}/api/v1/blobstore/scrub")).header("authorization", &auth).send().await.unwrap()).await;
        if v["running"] == false && v["finished_at"].is_u64() {
            report = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(report["checked"].as_u64(), Some(22), "{report}");
    assert_eq!(report["failed"].as_array().map(|a| a.len()), Some(0), "{report}");

    // Delete with the node's token (no admin token); a second is still 200.
    let (s, v) = json(c.delete(&url).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!((s, v["deleted"].as_bool()), (200, Some(true)), "{v}");
    let (s, v) = json(c.delete(&url).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!((s, v["deleted"].as_bool()), (200, Some(false)), "{v}");
    assert_eq!(c.get(&url).header("authorization", &auth).send().await.unwrap().status().as_u16(), 404);

    // The store opened again from its volume (as after an engine restart):
    // the journal replayed on the real thin volume.
    let old = st.blobs.lock().await.take().expect("the store is open");
    old.settle().await;
    drop(old);
    let got = c.get(format!("{base}/api/v1/blobs/{bd}")).header("authorization", &auth).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got.as_ref(), &big[..]);
    for b in &small {
        let got = c.get(format!("{base}/api/v1/blobs/{}", digest(b))).header("authorization", &auth).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(got.as_ref(), &b[..]);
    }
    assert_eq!(c.get(&url).header("authorization", &auth).send().await.unwrap().status().as_u16(), 404);
    let (_, v) = json(c.get(format!("{base}/api/v1/blobstore")).header("authorization", &auth).send().await.unwrap()).await;
    assert_eq!(v["blobs"].as_u64(), Some(21), "{v}");
}
