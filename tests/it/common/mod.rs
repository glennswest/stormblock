#![allow(dead_code, unused_imports)]

pub mod iscsi_initiator;
pub mod nvmeof_initiator;

use std::net::SocketAddr;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use stormblock::drive::BlockDevice;
use stormblock::drive::filedev::FileDevice;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};
use stormblock::target::iscsi::{IscsiConfig, IscsiTarget};
use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use stormblock::target::reactor::{ReactorConfig, ReactorPool};

/// Create `count` file-backed block devices in `dir`, each with `capacity` bytes.
pub async fn create_file_devices(dir: &TempDir, count: usize, capacity: u64) -> Vec<Arc<dyn BlockDevice>> {
    let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
    for i in 0..count {
        let path = dir.path().join(format!("dev-{i}.bin"));
        let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), capacity)
            .await
            .expect("failed to create file device");
        devices.push(Arc::new(dev));
    }
    devices
}

/// Create a RAID-1 mirror from FileDevices, then a ThinVolume on top.
/// Returns (TempDir, volume as BlockDevice, VolumeManager).
pub async fn setup_raid1_volume(
    capacity_per_drive: u64,
    volume_size: u64,
) -> (TempDir, Arc<dyn BlockDevice>, VolumeManager) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let devices = create_file_devices(&dir, 2, capacity_per_drive).await;

    let array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .expect("failed to create RAID-1 array");
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    let vol_id = vm.create_volume("test-vol", volume_size, array_id).await
        .expect("failed to create volume");
    let vol = vm.get_volume(&vol_id).expect("volume not found");

    (dir, vol, vm)
}

/// Start an iSCSI target on an ephemeral port. Returns (listen address, server task handle).
pub async fn start_iscsi_target(
    device: Arc<dyn BlockDevice>,
    config: IscsiConfig,
) -> (SocketAddr, RunningIscsiTarget) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let addr = listener.local_addr().expect("local_addr failed");

    let target = Arc::new(IscsiTarget::new(config));
    target.add_lun(0, device).await;

    let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
    let runner = target.clone();
    let handle = tokio::spawn(async move {
        let _ = runner.run_with_listener(listener, &reactor).await;
    });

    wait_for_listener(addr).await;
    (addr, RunningIscsiTarget { target, handle })
}

/// A running target, kept reachable so a test can ask it what it thinks its
/// state is rather than inferring it from the wire.
pub struct RunningIscsiTarget {
    target: Arc<IscsiTarget>,
    handle: JoinHandle<()>,
}

impl RunningIscsiTarget {
    pub fn target(&self) -> &Arc<IscsiTarget> {
        &self.target
    }

    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// Start an NVMe-oF/TCP target on an ephemeral port. Returns (listen address, server task handle).
pub async fn start_nvmeof_target(
    device: Arc<dyn BlockDevice>,
    config: NvmeofConfig,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let addr = listener.local_addr().expect("local_addr failed");

    let mut target = NvmeofTarget::new(config);
    target.add_namespace(1, device);
    let target = Arc::new(target);

    let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
    let handle = tokio::spawn(async move {
        let _ = target.run_with_listener(listener, &reactor).await;
    });

    wait_for_listener(addr).await;
    (addr, handle)
}

/// Poll TCP connect until the server is ready (max 2 seconds).
pub async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("server at {addr} did not become ready in time");
}

/// The stormblock binary a runtime test drives (`tests-runtime/`): the built
/// golden's binary, named by `STORMBLOCK_BIN`. Runtime tests run against what
/// ships, not against whatever cargo happened to build next to them.
pub fn stormblock_bin() -> String {
    std::env::var("STORMBLOCK_BIN")
        .expect("STORMBLOCK_BIN must name the stormblock binary under test (tests-runtime/ runs against a built binary)")
}

/// The format new slabs are written in is process-wide
/// (`slab::set_default_format`), and `cargo test` runs this binary's tests
/// as threads of one process: a test that writes format 1 for a while holds
/// this, so another cannot put format 2 back under it (#585). Dropped, it
/// puts format 2 back (also when the test panics), then lets go.
pub struct SlabFormatGuard {
    _held: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for SlabFormatGuard {
    fn drop(&mut self) {
        stormblock::drive::slab::set_default_format(stormblock::drive::slab::SLAB_VERSION_2);
    }
}

pub async fn slab_format_lock() -> SlabFormatGuard {
    static FORMAT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SlabFormatGuard { _held: FORMAT.lock().await }
}

/// The NVMe/TCP initiator's settings are the process's: its command timeout
/// (`nvmeof_dev::set_io_timeout`), `STORMBLOCK_NVME_TCP_QUEUES` and the NICs
/// it binds paths to (`STORMBLOCK_IFADDRS`). A test that changes them holds
/// this, so two such tests never run together, and on drop (also when the
/// test panics) they are put back for the tests after it (#585).
pub struct NvmeTcpEnv {
    _held: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for NvmeTcpEnv {
    fn drop(&mut self) {
        stormblock::drive::nvmeof_dev::set_io_timeout(None);
        std::env::remove_var("STORMBLOCK_NVME_TCP_QUEUES");
        std::env::remove_var("STORMBLOCK_IFADDRS");
    }
}

pub async fn nvme_tcp_env(io_timeout_secs: Option<u64>) -> NvmeTcpEnv {
    static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let held = ENV.lock().await;
    stormblock::drive::nvmeof_dev::set_io_timeout(io_timeout_secs.map(std::time::Duration::from_secs));
    NvmeTcpEnv { _held: held }
}
