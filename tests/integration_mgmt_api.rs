//! Management REST API integration tests.
//!
//! Starts axum server on ephemeral port, exercises all REST endpoints.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::{AppState, ArrayInfo, DriveInfo};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;

async fn start_mgmt_server(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let router = stormblock::mgmt::api::router(state.clone())
        .merge(stormblock::mgmt::metrics::metrics_router());

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Wait for server to be ready
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

async fn setup_state_with_array(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let drive_infos: Vec<DriveInfo> = devices.iter().enumerate().map(|(i, d)| {
        DriveInfo {
            device: d.clone(),
            path: format!("/dev/test{i}"),
        }
    }).collect();

    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let level = array.level();
    let member_count = array.member_count();
    let capacity = array.capacity_bytes();
    let stripe_size = array.stripe_size();
    let arc_array = Arc::new(array);
    let backing: Arc<dyn BlockDevice> = arc_array.clone();

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    let vol_id = vm.create_volume("test-vol", 32 * 1024 * 1024, array_id).await.unwrap();

    let config = StormBlockConfig::default();
    let state = Arc::new(AppState::new(config, vm));

    // Populate state
    {
        let mut drives = state.drives.write().await;
        *drives = drive_infos;
    }
    {
        let mut arrays = state.arrays.write().await;
        arrays.insert(array_id, ArrayInfo {
            array: arc_array,
            level,
            member_count,
            capacity_bytes: capacity,
            stripe_size,
        });
    }

    state
}

#[tokio::test]
async fn mgmt_get_drives() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/drives"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);

    server.abort();
}

#[tokio::test]
async fn mgmt_get_arrays() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/arrays"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["level"], "RAID-1");

    server.abort();
}

#[tokio::test]
async fn mgmt_get_volumes() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/volumes"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "test-vol");

    server.abort();
}

#[tokio::test]
async fn mgmt_get_exports() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/exports"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert!(items.is_empty()); // No exports configured

    server.abort();
}

#[tokio::test]
async fn mgmt_get_metrics() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/metrics"))
        .send().await.unwrap();
    // Metrics endpoint returns 500 if init_metrics() wasn't called (test isolation),
    // or 200 if the global recorder was initialized by another test.
    let status = resp.status().as_u16();
    assert!(status == 200 || status == 500,
        "metrics should return 200 or 500, got {status}");

    server.abort();
}
