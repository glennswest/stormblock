//! #163: `--data-dir` on the command line is the node's data directory, not
//! only the volume manager's. A daemon started with it and no
//! `[management] data_dir` had no `/serve/v1`, kept synonyms and templates in
//! memory, and put no token file in it.
//!
//! Needs `STORMBLOCK_BIN` (ci-runtime-tests.sh). No root, no devices.

use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

fn bin() -> String {
    let b = std::env::var("STORMBLOCK_BIN")
        .expect("STORMBLOCK_BIN must name the stormblock binary under test (ci-runtime-tests.sh sets it)");
    assert!(std::path::Path::new(&b).exists(), "STORMBLOCK_BIN={b} does not exist");
    b
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn the_data_dir_flag_is_the_whole_node_s_data_dir() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("from-the-flag");
    std::fs::create_dir_all(&data).unwrap();
    let mgmt = free_port();
    let config = dir.path().join("stormblock.toml");
    // No data_dir in the file: only the flag names one.
    std::fs::write(
        &config,
        format!("[management]\nlisten_addr = \"127.0.0.1:{mgmt}\"\ndiscovery_disabled = true\nublk_transport = false\n"),
    )
    .unwrap();
    let mut child = Command::new(bin())
        .args(["-c", config.to_str().unwrap(), "--data-dir", data.to_str().unwrap(), "--no-iscsi", "--no-nvmeof"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{mgmt}");
    let open = reqwest::Client::new();
    let mut up = false;
    for _ in 0..100 {
        if open.get(format!("{base}/api/v1/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "engine did not come up");

    // The token file is minted into the flag's directory…
    let token = std::fs::read_to_string(data.join("api_token")).expect("api_token in the --data-dir directory");
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(reqwest::header::AUTHORIZATION, format!("Bearer {}", token.trim()).parse().unwrap());
    let client = reqwest::Client::builder().default_headers(h).build().unwrap();

    // …and /serve/v1 is mounted, which it is only with a data directory.
    let r = client.get(format!("{base}/serve/v1/status")).send().await.unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(r.status().is_success(), "/serve/v1: {}", r.status());
}
