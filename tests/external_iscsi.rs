//! External iSCSI target integration test.
//!
//! Connects to a real iSCSI target (e.g., MikroTik RouterOS) and performs
//! INQUIRY, READ CAPACITY, WRITE, READ, and data verification.
//!
//! Requires environment variables:
//!   ISCSI_PORTAL — IP address of the iSCSI target (e.g., "192.168.10.1")
//!   ISCSI_PORT   — TCP port (default "3260")
//!   ISCSI_IQN    — Target IQN
//!
//! Run: cargo test --test external_iscsi -- --ignored --nocapture

mod common;

use std::net::SocketAddr;

use common::iscsi_initiator::IscsiInitiator;

fn iscsi_addr() -> Option<SocketAddr> {
    let portal = std::env::var("ISCSI_PORTAL").ok()?;
    let port: u16 = std::env::var("ISCSI_PORT")
        .unwrap_or_else(|_| "3260".to_string())
        .parse()
        .ok()?;
    Some(SocketAddr::new(portal.parse().ok()?, port))
}

fn iscsi_iqn() -> String {
    std::env::var("ISCSI_IQN").unwrap_or_else(|_| {
        "iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-test1-raw".to_string()
    })
}

#[tokio::test]
#[ignore] // only run when ISCSI_PORTAL is set
async fn external_iscsi_discovery() {
    let addr = match iscsi_addr() {
        Some(a) => a,
        None => {
            eprintln!("ISCSI_PORTAL not set, skipping");
            return;
        }
    };

    eprintln!("Connecting to iSCSI target at {}...", addr);
    let mut initiator = IscsiInitiator::connect(addr).await
        .expect("TCP connect failed");

    let iqn = iscsi_iqn();
    eprintln!("Logging in to {}...", iqn);
    initiator.login("iqn.2026-03.io.stormblock:test", &iqn).await
        .expect("iSCSI login failed");

    eprintln!("INQUIRY...");
    let inquiry_data = initiator.inquiry().await.expect("INQUIRY failed");
    assert!(!inquiry_data.is_empty(), "inquiry returned no data");
    // Print vendor/product from inquiry (bytes 8-15 = vendor, 16-31 = product)
    if inquiry_data.len() >= 32 {
        let vendor = String::from_utf8_lossy(&inquiry_data[8..16]);
        let product = String::from_utf8_lossy(&inquiry_data[16..32]);
        eprintln!("  Vendor:  {}", vendor.trim());
        eprintln!("  Product: {}", product.trim());
    }

    eprintln!("READ CAPACITY...");
    let (total_blocks, block_size) = initiator.read_capacity().await
        .expect("READ CAPACITY failed");
    eprintln!("  Blocks: {}  Block size: {}  Total: {} MB",
        total_blocks, block_size, total_blocks * block_size as u64 / 1024 / 1024);

    eprintln!("LOGOUT...");
    initiator.logout().await.expect("logout failed");

    eprintln!("PASS: external iSCSI discovery + inquiry + capacity");
}

#[tokio::test]
#[ignore]
async fn external_iscsi_write_read_verify() {
    let addr = match iscsi_addr() {
        Some(a) => a,
        None => {
            eprintln!("ISCSI_PORTAL not set, skipping");
            return;
        }
    };
    let iqn = iscsi_iqn();

    eprintln!("Connecting to {}...", addr);
    let mut initiator = IscsiInitiator::connect(addr).await
        .expect("TCP connect failed");
    initiator.login("iqn.2026-03.io.stormblock:test", &iqn).await
        .expect("login failed");

    // Get capacity
    let (total_blocks, block_size) = initiator.read_capacity().await
        .expect("read capacity failed");
    eprintln!("Disk: {} blocks x {} bytes = {} MB",
        total_blocks, block_size, total_blocks * block_size as u64 / 1024 / 1024);

    // Write a test pattern at LBA 0 (one block)
    let mut write_data = vec![0u8; block_size as usize];
    for (i, byte) in write_data.iter_mut().enumerate() {
        *byte = ((i * 7 + 0xAB) & 0xFF) as u8;
    }
    eprintln!("WRITE 1 block at LBA 0...");
    initiator.write(0, &write_data).await.expect("write failed");

    // Read it back
    eprintln!("READ 1 block at LBA 0...");
    let read_data = initiator.read(0, 1).await.expect("read failed");

    // Verify
    assert_eq!(read_data.len(), write_data.len(),
        "read size mismatch: got {} expected {}", read_data.len(), write_data.len());
    assert_eq!(read_data, write_data, "data mismatch after write/read");
    eprintln!("PASS: write/read/verify OK ({} bytes)", write_data.len());

    initiator.logout().await.expect("logout failed");
}

#[tokio::test]
#[ignore]
async fn external_iscsi_multi_block_io() {
    let addr = match iscsi_addr() {
        Some(a) => a,
        None => {
            eprintln!("ISCSI_PORTAL not set, skipping");
            return;
        }
    };
    let iqn = iscsi_iqn();

    let mut initiator = IscsiInitiator::connect(addr).await
        .expect("TCP connect failed");
    initiator.login("iqn.2026-03.io.stormblock:test", &iqn).await
        .expect("login failed");

    let (_total_blocks, block_size) = initiator.read_capacity().await
        .expect("read capacity failed");

    // Write 4 blocks at LBA 8 with a pattern
    let num_blocks = 4u16;
    let total_bytes = num_blocks as usize * block_size as usize;
    let mut write_data = vec![0u8; total_bytes];
    for (i, byte) in write_data.iter_mut().enumerate() {
        *byte = ((i * 13 + 0x42) & 0xFF) as u8;
    }

    eprintln!("WRITE {} blocks ({} KB) at LBA 8...", num_blocks, total_bytes / 1024);
    initiator.write(8, &write_data).await.expect("multi-block write failed");

    eprintln!("READ {} blocks at LBA 8...", num_blocks);
    let read_data = initiator.read(8, num_blocks).await.expect("multi-block read failed");

    assert_eq!(read_data.len(), total_bytes);
    assert_eq!(read_data, write_data, "multi-block data mismatch");
    eprintln!("PASS: multi-block write/read/verify OK ({} KB)", total_bytes / 1024);

    initiator.logout().await.expect("logout failed");
}
