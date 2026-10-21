//! The portable boot server (#474), driven the way a node drives it: the
//! boothost claim over HTTP, then the engine's own NVMe/TCP initiator on the
//! URI the claim hands back.
//!
//! What a laptop serving one image has to get right: the node reads the
//! image; its writes (stormuefi's boot count, a diskless node's data) land in
//! its own overlay and never in the file; another node sees the image as it
//! is; a host the claim did not name is refused at Connect; an install that
//! reports its disk laid is told `local` next time.

use std::net::SocketAddr;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock_nvmeof::boothost::{Boothost, Intent, Policy};
use stormblock_nvmeof::image::Image;
use stormblock_nvmeof::server::BootTarget;
use tempfile::TempDir;

struct Served {
    http: String,
    _dir: TempDir,
    image: std::path::PathBuf,
    original: Vec<u8>,
}

async fn serve(any_host: bool) -> Served {
    let dir = TempDir::new().unwrap();
    let image = dir.path().join("release.img");
    // 4 MiB and a tail: not a whole number of blocks, as an ISO need not be.
    let original: Vec<u8> = (0..(4 << 20) + 1000).map(|i: usize| (i * 7 % 253) as u8).collect();
    std::fs::write(&image, &original).unwrap();
    let img = Arc::new(Image::open(&image, 4096).unwrap());
    let target = BootTarget::new(img, dir.path().join("overlays"), "nqn.2026-10.lo.storm:test-serve", 1 << 20);

    let nvme = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nvme_port = nvme.local_addr().unwrap().port();
    tokio::spawn(target.clone().run(nvme));

    let boot = Boothost::new(
        target,
        Policy {
            hosts: vec![("server8".into(), Some("ac:1f:6b:8a:a3:96".into())), ("server7".into(), None)],
            any_host,
            intent: Intent::Install,
            nvme_port,
            advertise: None,
        },
    );
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    let app = boot.router().into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(http, app).await.unwrap() });
    Served { http: base, _dir: dir, image, original }
}

async fn claim(s: &Served, name: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/synonyms/boothost/{name}/claim", s.http))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(serde_json::Value::Null))
}

/// The URI a claim hands back, with the host NQN the node presents.
fn uri_of(reply: &serde_json::Value) -> String {
    let a = &reply["attach"];
    format!("{}&hostnqn={}", a["uri"].as_str().unwrap(), a["host_nqns"][0].as_str().unwrap())
}

#[tokio::test]
async fn a_node_claims_attaches_and_writes_only_into_its_own_overlay() {
    let s = serve(false).await;

    // What stormbootx asks first: an engine version above 19.3.0.
    let h: serde_json::Value = reqwest::get(format!("{}/api/v1/health", s.http)).await.unwrap().json().await.unwrap();
    assert_eq!(h["status"], "ok");
    assert_eq!(h["version"], stormblock_nvmeof::boothost::API_VERSION);
    let i: serde_json::Value = reqwest::get(format!("{}/api/v1/synonyms/boothost/server8/intent", s.http))
        .await.unwrap().json().await.unwrap();
    assert_eq!(i["intent"], "install");

    let (st, a) = claim(&s, "server8", serde_json::json!({"agent": {"name": "stormbootx"}})).await;
    assert_eq!(st, 201, "{a}");
    // stormbootx takes the first "address" etc. in the body: attach first.
    let text = a.to_string();
    assert!(text.starts_with("{\"attach\""), "{text}");
    assert_eq!(a["attach"]["address"], "127.0.0.1");
    assert_eq!(a["attach"]["host_nqns"][0], "nqn.2026-09.lo.storm:host-server8");
    assert_eq!(a["host"]["name"], "server8");

    // The engine's initiator reads the image through it, tail zero-filled.
    let dev = stormblock::drive::open_path(&uri_of(&a), false).await.unwrap();
    assert_eq!(dev.capacity_bytes(), 1025 * 4096);
    let mut buf = vec![0u8; 1 << 20];
    dev.read(0, &mut buf).await.unwrap();
    assert_eq!(&buf[..], &s.original[..1 << 20]);
    let mut tail = vec![0u8; 4096];
    dev.read(1024 * 4096, &mut tail).await.unwrap();
    assert_eq!(&tail[..1000], &s.original[1024 * 4096..]);
    assert!(tail[1000..].iter().all(|&b| b == 0));

    // A write (larger than a capsule: R2T) lands in the overlay.
    dev.write(8192, &vec![0xAB; 64 * 1024]).await.unwrap();
    dev.flush().await.unwrap();
    let mut back = vec![0u8; 64 * 1024 + 8192];
    dev.read(4096, &mut back).await.unwrap();
    assert_eq!(&back[..4096], &s.original[4096..8192]);
    assert!(back[4096..4096 + 64 * 1024].iter().all(|&b| b == 0xAB));
    assert_eq!(std::fs::read(&s.image).unwrap(), s.original, "the image file is never written");

    // Another node, by its MAC on a default claim: the image as it is.
    let (st, b) = claim(&s, "default", serde_json::json!({"mac": "AC:1F:6B:8A:A3:96"})).await;
    assert_eq!((st, b["host"]["name"].as_str()), (201, Some("server8")), "{b}");
    let (st, c) = claim(&s, "server7", serde_json::json!({})).await;
    assert_eq!(st, 201);
    let other = stormblock::drive::open_path(&uri_of(&c), false).await.unwrap();
    let mut seen = vec![0u8; 64 * 1024];
    other.read(8192, &mut seen).await.unwrap();
    assert_eq!(&seen[..], &s.original[8192..8192 + 64 * 1024]);

    // server7's claim does not admit server8's NQN.
    let stolen = format!("{}&hostnqn=nqn.2026-09.lo.storm:host-server8", c["attach"]["uri"].as_str().unwrap());
    assert!(stormblock::drive::open_path(&stolen, false).await.is_err(), "another host's NQN is refused at Connect");

    // Not named, no --any-host: 404; a default claim with no MAC: 400.
    assert_eq!(claim(&s, "stranger", serde_json::json!({})).await.0, 404);
    assert_eq!(claim(&s, "default", serde_json::json!({})).await.0, 400);

    // The install reports its disk laid: the next boot is local.
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/synonyms/boothost/server8/installed", s.http))
        .json(&serde_json::json!({"volume": b["volume"]["id"], "stage": "laid"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let i: serde_json::Value = reqwest::get(format!("{}/api/v1/synonyms/boothost/server8/intent", s.http))
        .await.unwrap().json().await.unwrap();
    assert_eq!(i["intent"], "local");
}

#[tokio::test]
async fn any_host_serves_a_bench_of_unnamed_machines() {
    let s = serve(true).await;
    let (st, a) = claim(&s, "default", serde_json::json!({"mac": "52:54:00:12:34:56"})).await;
    assert_eq!(st, 201, "{a}");
    assert_eq!(a["host"]["name"], "mac-525400123456");
    assert_eq!(a["host"]["provisional"], true);
    let dev = stormblock::drive::open_path(&uri_of(&a), false).await.unwrap();
    let mut buf = vec![0u8; 4096];
    dev.read(0, &mut buf).await.unwrap();
    assert_eq!(&buf[..], &s.original[..4096]);
    let (st, n) = claim(&s, "blade3", serde_json::json!({})).await;
    assert_eq!((st, n["host"]["name"].as_str()), (201, Some("blade3")));
}
