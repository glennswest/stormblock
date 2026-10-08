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
//! Needs `STORMBLOCK_BIN` naming a built binary (ci-runtime-tests.sh, #222);
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
    client: reqwest::Client,
    _dir: TempDir,
}

/// A client carrying the node token the engine minted into its data dir. The
/// API is closed by default (#107); these tests predate that and sent nothing,
/// so every call answered 401 — found on their first run (#222).
fn authed(data: &std::path::Path) -> reqwest::Client {
    let token = std::fs::read_to_string(data.join("api_token")).expect("the engine mints api_token into its data dir");
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", token.trim()).parse().unwrap(),
    );
    reqwest::Client::builder().default_headers(h).build().unwrap()
}

/// Wait for the engine to answer its open health route.
async fn wait_up(base: &str) -> bool {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if let Ok(r) = client.get(format!("{base}/api/v1/health")).send().await {
            if r.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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
    assert!(wait_up(&base).await, "engine did not come up");
    let client = authed(&data);
    Engine { child, base, client, _dir: dir }
}

/// The binary under test (#222): required, like every other runtime test.
/// A test that skipped when it was unset passed without running anything.
fn bin() -> String {
    let b = std::env::var("STORMBLOCK_BIN")
        .expect("STORMBLOCK_BIN must name the stormblock binary under test (ci-runtime-tests.sh sets it)");
    assert!(std::path::Path::new(&b).exists(), "STORMBLOCK_BIN={b} does not exist");
    b
}

/// The request every consumer actually sends — a name and a size — against a
/// node that has storage and adopted it at startup.
#[tokio::test]
async fn a_plain_create_works_on_a_node_that_has_slabs() {
    let bin = bin();
    for role in ["system", "data"] {
        let e = start(&bin, role).await;
        let client = &e.client;

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
            // The shared subsystem is closed (#210): an attach names its host.
            .json(&serde_json::json!({"transport": "nvme-tcp", "host_nqn": "nqn.2026-09.lo.test:e2e-host"}))
            .send().await.unwrap();
        assert!(resp.status().is_success(), "{role}: attach {}", resp.status());
    }
}

/// A node with no storage at all says so, instead of naming a parameter.
#[tokio::test]
async fn a_node_with_no_slabs_says_that_rather_than_naming_a_parameter() {
    let bin = bin();
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
    assert!(wait_up(&base).await, "engine did not come up");
    let client = authed(&data);

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

/// #368: a panic in the serving engine ends the process, said on stderr with
/// a backtrace, so its supervisor restarts it. Before, a tokio worker's panic
/// on the Dell left the engine alive, silent and answering nothing.
#[test]
fn a_panic_in_the_daemon_aborts_it_and_says_why() {
    use std::os::unix::process::ExitStatusExt;
    let bin = bin();
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
        .env("STORMBLOCK_TEST_PANIC_AFTER_MS", "1500")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the engine is still running 30 s after a panic: alive and silent");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut err = String::new();
    use std::io::Read;
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!(status.signal(), Some(6), "aborted (SIGABRT), not exited: {status:?}\n{err}");
    assert!(err.contains("FATAL: daemon: panic on thread"), "{err}");
    assert!(err.contains("test panic in a spawned task"), "{err}");
}
