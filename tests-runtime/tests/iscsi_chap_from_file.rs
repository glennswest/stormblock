//! #164: the config file's `[iscsi]` settings take effect in the built
//! daemon. A file that names CHAP used to run a target with no
//! authentication: the flags' defaults overwrote the file, and the target was
//! built from the flags alone.
//!
//! Needs `STORMBLOCK_BIN` (ci-runtime-tests.sh). No root, no devices.

#[path = "../../tests/it/common/mod.rs"]
mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::iscsi_initiator::IscsiInitiator;
use tempfile::TempDir;

const TARGET: &str = "iqn.2026-10.lo.test:chap-from-file";
const INITIATOR: &str = "iqn.2026-10.lo.test:initiator";

fn bin() -> String {
    let b = std::env::var("STORMBLOCK_BIN")
        .expect("STORMBLOCK_BIN must name the stormblock binary under test (ci-runtime-tests.sh sets it)");
    assert!(std::path::Path::new(&b).exists(), "STORMBLOCK_BIN={b} does not exist");
    b
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn config(dir: &TempDir, iscsi: &str) -> (std::path::PathBuf, u16) {
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mgmt = free_port();
    let path = dir.path().join("stormblock.toml");
    std::fs::write(
        &path,
        format!(
            "[management]\nlisten_addr = \"127.0.0.1:{mgmt}\"\ndata_dir = {:?}\n\
             discovery_disabled = true\nublk_transport = false\n\n{iscsi}",
            data.to_str().unwrap()
        ),
    )
    .unwrap();
    (path, mgmt)
}

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

async fn connect(port: u16) -> IscsiInitiator {
    for _ in 0..50 {
        if let Ok(i) = IscsiInitiator::connect(format!("127.0.0.1:{port}").parse().unwrap()).await {
            return i;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the iSCSI target never listened on {port}");
}

#[tokio::test]
async fn chap_in_the_config_file_is_required_by_the_daemon_s_target() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let (path, mgmt) = config(
        &dir,
        &format!(
            "[iscsi]\nlisten_addr = \"127.0.0.1:{port}\"\ntarget_name = \"{TARGET}\"\n\
             chap_user = \"node\"\nchap_secret = \"from-the-file\"\n"
        ),
    );
    // No --iscsi-addr, --iscsi-target-name, --chap-*: everything from the file.
    let mut child = Command::new(bin())
        .args(["-c", path.to_str().unwrap(), "--no-nvmeof"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_up(&format!("http://127.0.0.1:{mgmt}")).await, "engine did not come up");

    // The file's address and name: the target answers there, as that name.
    let mut none = connect(port).await;
    let e = none.login(INITIATOR, TARGET).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "a login with no CHAP was admitted: {e}");

    let mut wrong = connect(port).await;
    assert!(wrong.login_chap(INITIATOR, TARGET, "node", "not-it").await.is_err(), "the wrong secret was admitted");

    let mut right = connect(port).await;
    right.login_chap(INITIATOR, TARGET, "node", "from-the-file").await.expect("the file's CHAP secret logs in");

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn half_a_chap_pair_in_the_file_stops_the_daemon() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let (path, _) = config(&dir, &format!("[iscsi]\nlisten_addr = \"127.0.0.1:{port}\"\nchap_user = \"node\"\n"));
    let out = Command::new(bin())
        .args(["-c", path.to_str().unwrap(), "--no-nvmeof"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a daemon with half a CHAP pair started");
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("chap_secret"), "{said}");
}
