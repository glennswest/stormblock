//! The engine as a process, driven the way a consumer drives it.
//!
//! Every other test in this suite builds an `AppState` in-process and calls
//! the router directly. That is not the same thing as the binary a node runs:
//! it skips config parsing, drive adoption at startup, and every default the
//! CLI supplies — which is exactly where `POST /api/v1/volumes {name, size}`
//! turned out to be refused for wanting an `array_id` that slab placement
//! made obsolete. Tests that agree with each other prove nothing; this one
//! runs the real thing.
//!
//! Skipped unless `STORMBLOCK_BIN` names a built binary, so it costs nothing
//! in an ordinary `cargo test`:
//!
//! ```text
//! cargo build --release
//! STORMBLOCK_BIN=target/release/stormblock cargo test --test integration_engine_e2e
//! ```

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

struct Engine {
    child: Child,
    base: String,
    _dir: TempDir,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Start the real binary over a file-backed slab of `role`, as a node whose
/// storage is only that.
async fn start(bin: &str, role: &str) -> Engine {
    let dir = TempDir::new().unwrap();
    let disk = dir.path().join("disk1.img");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let f = std::fs::File::create(&disk).unwrap();
    f.set_len(512 * 1024 * 1024).unwrap();
    drop(f);

    let formatted = Command::new(bin)
        .args(["slab", "format", "--role", role, disk.to_str().unwrap()])
        .output()
        .expect("slab format");
    assert!(formatted.status.success(), "slab format: {formatted:?}");

    let mgmt = free_port();
    let nvmeof = free_port();
    let config = dir.path().join("stormblock.toml");
    std::fs::write(
        &config,
        format!(
            "[[drives]]\npath = {:?}\n\n[management]\nlisten_addr = \"127.0.0.1:{mgmt}\"\n\
             data_dir = {:?}\nnode_name = \"e2e\"\ndiscovery_disabled = true\n\
             ublk_transport = false\nadvertised_addr = \"127.0.0.1\"\n",
            disk.to_str().unwrap(),
            data.to_str().unwrap()
        ),
    )
    .unwrap();

    let child = Command::new(bin)
        .args([
            "-c",
            config.to_str().unwrap(),
            "--no-iscsi",
            "--nvmeof-addr",
            &format!("127.0.0.1:{nvmeof}"),
            "--nvmeof-nqn",
            "nqn.2026-09.lo.test:e2e",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engine");

    let base = format!("http://127.0.0.1:{mgmt}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(format!("{base}/api/v1/slabs")).send().await.is_ok() {
            return Engine { child, base, _dir: dir };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("engine did not come up");
}

fn bin() -> Option<String> {
    let b = std::env::var("STORMBLOCK_BIN").ok()?;
    std::path::Path::new(&b).exists().then_some(b)
}

/// The request every consumer actually sends — a name and a size — against a
/// node that has storage and adopted it at startup.
#[tokio::test]
async fn a_plain_create_works_on_a_node_that_has_slabs() {
    let Some(bin) = bin() else { return };
    for role in ["system", "data"] {
        let e = start(&bin, role).await;
        let client = reqwest::Client::new();

        // The drive's slab was adopted at startup, without being formatted
        // again — that is what makes a plain create placeable.
        let slabs: serde_json::Value = client
            .get(format!("{}/api/v1/slabs", e.base))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(slabs["count"], 1, "{role}: the drive's slab is adopted");
        assert_eq!(slabs["items"][0]["role"], role);

        let resp = client
            .post(format!("{}/api/v1/volumes", e.base))
            .json(&serde_json::json!({"name": "plain", "size": "64M"}))
            .send().await.unwrap();
        assert_eq!(resp.status(), 201, "{role}: {}", resp.text().await.unwrap());
        let body: serde_json::Value = resp.json().await.unwrap();
        // …and it is placed in the role the node actually has (#93).
        assert_eq!(body["role"], role);
        assert_eq!(body["writable"], true);

        // The write path is the point: a volume that cannot allocate reads as
        // zeros and fails every write, which is how #92 presented.
        let id = body["id"].as_str().unwrap().to_string();
        let resp = client
            .post(format!("{}/api/v1/volumes/{id}/attach", e.base))
            .json(&serde_json::json!({"transport": "nvme-tcp"}))
            .send().await.unwrap();
        assert!(resp.status().is_success(), "{role}: attach {}", resp.status());
    }
}

/// A node with no storage at all says so, instead of naming a parameter.
#[tokio::test]
async fn a_node_with_no_slabs_says_that_rather_than_naming_a_parameter() {
    let Some(bin) = bin() else { return };
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mgmt = free_port();
    let config = dir.path().join("stormblock.toml");
    std::fs::write(
        &config,
        format!(
            "[management]\nlisten_addr = \"127.0.0.1:{mgmt}\"\ndata_dir = {:?}\n\
             discovery_disabled = true\nublk_transport = false\n",
            data.to_str().unwrap()
        ),
    )
    .unwrap();
    let mut child = Command::new(&bin)
        .args(["-c", config.to_str().unwrap(), "--no-iscsi", "--no-nvmeof"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{mgmt}");
    let client = reqwest::Client::new();
    let mut up = false;
    for _ in 0..100 {
        if client.get(format!("{base}/api/v1/slabs")).send().await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "engine did not come up");

    let resp = client
        .post(format!("{base}/api/v1/volumes"))
        .json(&serde_json::json!({"name": "nowhere", "size": "64M"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let text = resp.text().await.unwrap();
    assert!(text.contains("no slabs"), "{text}");

    let _ = child.kill();
    let _ = child.wait();
}
