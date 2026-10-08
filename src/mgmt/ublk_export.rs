//! Dynamic ublk exports for the CSI `/v1` attach path.
//!
//! When `management.ublk_transport` is on and a volume is attached on the same
//! node that holds its master, the engine exports the backing block device as
//! a local `/dev/ublkbN` and hands the CSI node that path instead of NVMe-oF
//! coordinates — no network round trip for the common master-local case.
//!
//! ublk is Linux 6.0+ only and needs `ublk_drv` loaded. Availability is probed
//! once at construction; when it is unavailable (non-Linux, module not loaded)
//! `ensure` returns `None` and the caller falls back to nvme-tcp. This is why
//! the probe matters: returning a `/dev/ublkbN` path that never materializes
//! would wedge NodeStage on the CSI side, whereas nvme-tcp always works.

use std::collections::HashMap;
use std::sync::Arc;

use crate::drive::BlockDevice;

/// Decide whether an attach should be served over ublk rather than nvme-tcp.
///
/// Pure policy, kept separate so it is testable without a kernel: ublk is
/// offered only when the operator enabled it, the attaching node is *this*
/// node, and the volume is backed locally here (this node holds the master).
/// Read-write attach already requires the caller to be the master node, so in
/// practice this is "enabled and local".
pub fn should_offer_ublk(
    enabled: bool,
    request_node: &str,
    local_node: &str,
    locally_backed: bool,
) -> bool {
    enabled && locally_backed && request_node == local_node
}

/// Which transport an attach asks for (#149).
///
/// An attach request names who is asking, not where the I/O will come from.
/// An orchestrator attaching on the master's behalf for a remote initiator —
/// a RAID head, a consumer on another machine — is *on* the master, so the
/// engine would offer it a local ublk device it cannot use. Naming the
/// transport is how such a caller says "network".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantTransport {
    /// The engine's choice: ublk when it can, nvme-tcp otherwise.
    Any,
    /// Only a local ublk device; refused where there cannot be one.
    Ublk,
    /// Only NVMe-oF/TCP coordinates, even on the master.
    NvmeTcp,
}

impl WantTransport {
    /// `ublk`, `nvme_tcp` (the tag `AttachInfo` answers with), `nvme-tcp` or
    /// `nvmeof`; absent is [`WantTransport::Any`].
    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        match s.map(|t| t.trim().to_ascii_lowercase()) {
            None => Ok(WantTransport::Any),
            Some(t) => match t.as_str() {
                "" | "any" => Ok(WantTransport::Any),
                "ublk" => Ok(WantTransport::Ublk),
                "nvme_tcp" | "nvme-tcp" | "nvmeof" | "nvme" => Ok(WantTransport::NvmeTcp),
                other => Err(format!("transport {other:?}: use nvme_tcp or ublk")),
            },
        }
    }
}

struct Export {
    device_path: String,
    /// Fires the ublk server's shutdown watch on teardown (DEL_DEV).
    #[cfg(target_os = "linux")]
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Kept so a volume resize can be pushed down to the kernel device (#19).
    /// Without it the volume grows and `/dev/ublkbN` stays the size it was
    /// given at `SET_PARAMS`, so `xfs_growfs` finds nothing to grow into.
    /// `None` only in tests, which inject an export without a kernel behind it.
    #[cfg(target_os = "linux")]
    server: Option<Arc<crate::drive::ublk::UblkServer>>,
    /// Set by the export's own thread when `run` has returned — that is, when
    /// STOP_DEV and DEL_DEV are done and its io_uring queues are closed.
    ///
    /// A flag rather than a `JoinHandle` because shutdown has to be *bounded*:
    /// `join` cannot be given a deadline, and the one thing a stop must not do
    /// is wait forever (#105).
    done: Arc<std::sync::atomic::AtomicBool>,
    /// The hold this device keeps on its volume (#267): the engine's id of
    /// the volume behind it, and the words it was taken with.
    held: Option<(uuid::Uuid, String)>,
}

/// Teardowns in flight: what was signalled, and how to wait for it.
///
/// Returned by [`UblkExportManager::shutdown_all`] so the caller can do
/// something else — flush metadata — while the kernel devices go away, and
/// then wait for them with a deadline it chooses.
pub struct ShutdownWait {
    devices: Vec<(String, Arc<std::sync::atomic::AtomicBool>)>,
}

impl ShutdownWait {
    /// Nothing was signalled.
    pub fn none() -> Self {
        ShutdownWait { devices: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Wait for every signalled export to finish its teardown, up to `budget`.
    /// Returns the devices that were still going when it ran out — which is
    /// what a log line should name, because a process that exits over one of
    /// those is how an unreapable thread is made.
    pub async fn settle(self, budget: std::time::Duration) -> Vec<String> {
        use std::sync::atomic::Ordering;
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let outstanding: Vec<String> = self
                .devices
                .iter()
                .filter(|(_, done)| !done.load(Ordering::SeqCst))
                .map(|(path, _)| path.clone())
                .collect();
            if outstanding.is_empty() || tokio::time::Instant::now() >= deadline {
                return outstanding;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

/// Tracks the live per-volume ublk exports on this node.
/// Where a block device is mounted, if anywhere.
///
/// By device number, against PID 1's mount table and this process's own: a
/// claim's filesystem is mounted by stormpump in the node's namespace, and
/// the engine runs in a container of its own, so either may hold it.
///
/// **Why it matters:** the devices this engine creates are recoverable
/// (`UBLK_F_USER_RECOVERY`). Stopping a device's server while a filesystem is
/// mounted on it does not fail the filesystem's I/O — the kernel queues it for
/// a server that never comes back, and every `sync` on the node then sits in
/// `submit_bio_wait` for ever. A detach under a mount wedged the R230 that way.
pub fn mounted_at(device_path: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let rdev = std::fs::metadata(device_path).ok()?.rdev();
        let (maj, min) = (libc::major(rdev), libc::minor(rdev));
        let want = format!("{maj}:{min}");
        for table in ["/proc/1/mountinfo", "/proc/self/mountinfo"] {
            let Ok(text) = std::fs::read_to_string(table) else { continue };
            for line in text.lines() {
                let f: Vec<&str> = line.split(' ').collect();
                if f.len() > 4 && f[2] == want {
                    return Some(f[4].to_string());
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_path;
        None
    }
}

pub struct UblkExportManager {
    exports: HashMap<String, Export>,
    /// Devices this engine serves but did not create here: the boot devices
    /// an adopting engine took over from the initramfs, by volume id →
    /// device path. Counted as in use, never stopped by a detach — their
    /// lifetime is the handover's (#138).
    adopted: HashMap<String, String>,
    /// Next /dev/ublkbN id to hand out (Linux only consumes this).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    next_id: u32,
    available: bool,
    /// Shared with the volume manager: a volume served here is held, and is
    /// not deleted under its device (#267).
    holds: crate::volume::holds::ServeHolds,
    /// Attaches whose device is still coming up (#231), by volume: a second
    /// attach of the same volume waits for the first's answer instead of
    /// starting another server. The lock is not held while one comes up.
    pending: HashMap<String, Answer>,
}

/// What an attach that is still coming up will answer: `None` until it has,
/// then the device path or `None` (it did not come up).
type Answer = tokio::sync::watch::Receiver<Option<Option<String>>>;

/// The first half of an attach, under the export lock (#231).
pub enum Begin {
    /// Already exported: its device.
    Ready(String),
    /// ublk is not available here.
    Unavailable,
    /// Another attach of this volume is bringing its device up: wait for it.
    Wait(Answer),
    /// A server was started: wait for its device without the lock, then
    /// [`UblkExportManager::finish`].
    Started(Launch, tokio::sync::watch::Sender<Option<Option<String>>>),
}

/// A ublk server started for an attach and not yet ready (#231).
pub struct Launch {
    volume_uuid: uuid::Uuid,
    shutdown: tokio::sync::watch::Sender<bool>,
    #[cfg(target_os = "linux")]
    server: Option<Arc<crate::drive::ublk::UblkServer>>,
    done: Arc<std::sync::atomic::AtomicBool>,
    before: String,
}

impl Launch {
    /// The kernel's id for the device, once assigned.
    fn dev_id(&self) -> Option<u32> {
        #[cfg(target_os = "linux")]
        {
            self.server.as_ref().and_then(|s| s.dev_id())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

/// How long the kernel has to assign a device id, and then to make the
/// block device appear (#231: unchanged from the blocking loops it replaces).
const ID_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
const NODE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Wait, asynchronously, until a started device can be handed out (#231):
/// the kernel has assigned it an id, its block device exists, and nothing is
/// mounted on it. The probes are arguments so a test can hold a device back.
pub async fn wait_ready(
    dev_id: impl Fn() -> Option<u32>,
    exists: impl Fn(&str) -> bool,
    mounted: impl Fn(&str) -> Option<String>,
    id_wait: std::time::Duration,
    node_wait: std::time::Duration,
) -> Result<String, String> {
    // The path is not known until the kernel has assigned one. Bounded: a
    // device that has not appeared in a second is not going to, and
    // returning a path that never materialises would wedge whatever tries to
    // open it.
    let deadline = tokio::time::Instant::now() + id_wait;
    let id = loop {
        if let Some(id) = dev_id() {
            break id;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("the kernel assigned no device".into());
        }
        tokio::time::sleep(POLL).await;
    };
    let device_path = format!("/dev/ublkb{id}");
    // The id is assigned at ADD_DEV, but the *block device* appears only at
    // START_DEV — a ~60 ms gap on this hardware — and whoever is handed this
    // path opens it immediately: qemu lost that race and died at "Could not
    // open '/dev/ublkb40': No such file or directory" while the log had
    // already said "ublk export created". A path is not a result until it
    // can be opened.
    let deadline = tokio::time::Instant::now() + node_wait;
    while !exists(&device_path) {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the device node {device_path} never appeared"));
        }
        tokio::time::sleep(POLL).await;
    }
    // **Never take a device something is already mounted on.**
    //
    // Belt and braces over the kernel's own allocation: this node runs its
    // root filesystem on a ublk device, and an export that lands on the same
    // number takes the root out from under a running system — observed as
    // "lost async page write", a journal I/O error and a filesystem the
    // kernel puts into shutdown state, with nothing saying a block device had
    // been replaced.
    //
    // Checked against the mount table rather than against our own
    // bookkeeping, because the devices that matter most were created by a
    // process that no longer exists — the initramfs — and are not in any
    // table this process holds.
    if let Some(user) = mounted(&device_path) {
        return Err(format!(
            "the kernel handed back {device_path}, already mounted on {user} — attaching here would \
             take that filesystem away"
        ));
    }
    Ok(device_path)
}

/// Attach `volume_id` as a ublk device: its path, or `None` when ublk is not
/// available or the device did not come up (the caller uses NVMe/TCP).
///
/// The export lock is held only to look and to record (#231). The wait for
/// the kernel — usually ~60 ms, up to 6 s — runs on a task of its own with
/// no lock held, so other attaches and every management request that reads
/// the export table go on meanwhile, and a caller that gives up leaves
/// nothing half made: the task records the export or takes the server down.
pub async fn attach(
    state: &Arc<crate::mgmt::AppState>,
    volume_id: &str,
    device: Arc<dyn BlockDevice>,
) -> Option<String> {
    let begun = state.ublk_exports.lock().await.begin(volume_id, device);
    let mut answer = match begun {
        Begin::Ready(path) => return Some(path),
        Begin::Unavailable => return None,
        Begin::Wait(rx) => rx,
        Begin::Started(launch, tx) => {
            let rx = tx.subscribe();
            let (state, volume_id) = (state.clone(), volume_id.to_string());
            tokio::spawn(async move {
                let ready = wait_ready(
                    || launch.dev_id(),
                    |p| std::path::Path::new(p).exists(),
                    mounted_on,
                    ID_WAIT,
                    NODE_WAIT,
                )
                .await;
                let path = state.ublk_exports.lock().await.finish(&volume_id, launch, ready);
                let _ = tx.send(Some(path));
            });
            rx
        }
    };
    loop {
        if let Some(path) = answer.borrow().clone() {
            return path;
        }
        if answer.changed().await.is_err() {
            return None;
        }
    }
}

impl Default for UblkExportManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UblkExportManager {
    pub fn new() -> Self {
        UblkExportManager {
            exports: HashMap::new(),
            adopted: HashMap::new(),
            next_id: 0,
            available: ublk_available(),
            holds: Default::default(),
            pending: HashMap::new(),
        }
    }

    /// Hold every volume served here in the volume manager's `holds`, so it
    /// refuses to delete one under its device (#267).
    pub fn with_holds(mut self, holds: crate::volume::holds::ServeHolds) -> Self {
        self.holds = holds;
        self
    }

    fn release(&self, export: &Export) {
        if let Some((volume, what)) = &export.held {
            self.holds.release(*volume, what);
        }
    }

    /// Whether ublk exports can actually be created on this host.
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn device_path(&self, volume_id: &str) -> Option<String> {
        self.exports.get(volume_id).map(|e| e.device_path.clone())
    }

    /// The first half of an attach (#231), under the lock: the device if
    /// the volume is already exported (a repeat attach returns the same one),
    /// the answer to wait for if another attach is bringing it up, else a
    /// server started for it. See [`attach`].
    pub fn begin(&mut self, volume_id: &str, device: Arc<dyn BlockDevice>) -> Begin {
        if let Some(e) = self.exports.get(volume_id) {
            return Begin::Ready(e.device_path.clone());
        }
        if let Some(rx) = self.pending.get(volume_id) {
            return Begin::Wait(rx.clone());
        }
        if !self.available {
            return Begin::Unavailable;
        }
        match self.start(volume_id, device) {
            Some(launch) => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                self.pending.insert(volume_id.to_string(), rx);
                Begin::Started(launch, tx)
            }
            None => Begin::Unavailable,
        }
    }

    /// The second half of an attach (#231), under the lock again once the
    /// device is ready or is not coming: record the export, or take its
    /// server down. Returns what the attach answers.
    pub fn finish(&mut self, volume_id: &str, launch: Launch, ready: Result<String, String>) -> Option<String> {
        self.pending.remove(volume_id);
        let device_path = match ready {
            Ok(p) => p,
            Err(why) => {
                tracing::error!(volume = volume_id, "ublk export did not come up: {why}");
                let _ = launch.shutdown.send(true);
                return None;
            }
        };
        let what = format!("ublk device {device_path}");
        self.holds.hold(launch.volume_uuid, what.clone());
        self.exports.insert(
            volume_id.to_string(),
            Export {
                device_path: device_path.clone(),
                #[cfg(target_os = "linux")]
                shutdown: launch.shutdown,
                #[cfg(target_os = "linux")]
                server: launch.server,
                done: launch.done,
                held: Some((launch.volume_uuid, what)),
            },
        );
        tracing::info!(
            volume = volume_id,
            device = %device_path,
            existing_before = %launch.before,
            "ublk export created"
        );
        Some(device_path)
    }

    /// Whether an attach of `volume_id` is still bringing its device up.
    pub fn is_pending(&self, volume_id: &str) -> bool {
        self.pending.contains_key(volume_id)
    }

    /// Record a boot device an adopting engine is serving (#138).
    pub fn record_adopted(&mut self, volume_id: &str, device_path: String) {
        // Served for as long as this engine runs: the root, the mounted
        // volumes. Never released.
        if let Ok(v) = uuid::Uuid::parse_str(volume_id) {
            self.holds.hold(v, format!("ublk device {device_path} (adopted)"));
        }
        self.adopted.insert(volume_id.to_string(), device_path);
    }

    /// Every ublk device this engine serves, created here or adopted: volume
    /// id → device path.
    pub fn devices(&self) -> Vec<(String, String)> {
        self.exports
            .iter()
            .map(|(v, e)| (v.clone(), e.device_path.clone()))
            .chain(self.adopted.iter().map(|(v, d)| (v.clone(), d.clone())))
            .collect()
    }

    /// Whether this node currently exports `volume_id` as a ublk device —
    /// including a boot device it adopted, which is as much in use.
    pub fn is_exported(&self, volume_id: &str) -> bool {
        self.adopted.contains_key(volume_id) || self.exports.contains_key(volume_id)
    }

    /// Push a new size down to the kernel device backing `volume_id`.
    ///
    /// `Ok(false)` means there is nothing to tell — no ublk export for this
    /// volume — which is the common case and not a failure. `Err` means there
    /// is a device and it could not be resized, which the caller must surface:
    /// the volume is now bigger than the block device anything above it sees.
    #[cfg(target_os = "linux")]
    pub fn update_size(&self, volume_id: &str, new_capacity_bytes: u64) -> Result<bool, String> {
        let Some(export) = self.exports.get(volume_id) else { return Ok(false) };
        let Some(server) = export.server.as_ref() else {
            return Err(format!("ublk export for {volume_id} has no server handle"));
        };
        server.update_size(new_capacity_bytes).map(|()| true).map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn update_size(&self, _volume_id: &str, _new_capacity_bytes: u64) -> Result<bool, String> {
        Ok(false)
    }

    /// Take every export down, for a process that is stopping (#105).
    ///
    /// This is not tidiness. A ublk export's queue threads sit in
    /// `io_uring_enter` waiting for the kernel to hand them I/O; a process
    /// that exits without STOP_DEV and DEL_DEV leaves them there, and a thread
    /// stuck in the kernel cannot be reaped — forge carried a defunct process
    /// in its unit's cgroup for four days, and *every* restart after it ended
    /// in "failed mode" because systemd found something it could not kill.
    ///
    /// Signals, and returns; the caller waits with a deadline of its own.
    #[cfg(target_os = "linux")]
    pub fn shutdown_all(&mut self) -> ShutdownWait {
        let exports: Vec<Export> = self.exports.drain().map(|(_, e)| e).collect();
        let devices = exports
            .into_iter()
            .map(|export| {
                let _ = export.shutdown.send(true);
                self.release(&export);
                (export.device_path, export.done)
            })
            .collect();
        ShutdownWait { devices }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn shutdown_all(&mut self) -> ShutdownWait {
        let exports: Vec<Export> = self.exports.drain().map(|(_, e)| e).collect();
        for e in &exports {
            self.release(e);
        }
        ShutdownWait::none()
    }

    /// Tear down the export for `volume_id`, if any (detach / delete).
    pub fn remove(&mut self, volume_id: &str) {
        if let Some(_e) = self.exports.remove(volume_id) {
            self.release(&_e);
            #[cfg(target_os = "linux")]
            {
                // Best effort: the server removes the kernel device on exit.
                let _ = _e.shutdown.send(true);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn start(&mut self, volume_id: &str, device: Arc<dyn BlockDevice>) -> Option<Launch> {
        use crate::drive::ublk::UblkServer;

        // **The kernel picks the number, not us.**
        //
        // This asked for `/dev/ublkb0`, then `1`, and so on from a counter
        // that started at zero in a fresh process — while the node was already
        // serving 39 devices from boot. Worse, `UblkServer::run` treats a
        // *requested* id as "clean up whatever is there": it sends STOP_DEV
        // and DEL_DEV for that id first, so the first API attach on a booted
        // node tore down `/dev/ublkb0`. On the machine this was found on that
        // device carried `/var/log/pods`, whose filesystem the kernel then put
        // into shutdown state — "lost async page write", every later write
        // EIO, and the VM whose start triggered it reported a missing
        // directory. Nothing anywhere said a device had been deleted.
        //
        // Asking with no id makes the kernel allocate a free one and skips
        // that cleanup entirely, which is the only way to be sure of not
        // taking a device somebody else is using.
        // What the kernel already has, before asking it for anything.
        //
        // Logged because the one question that could not be answered after a
        // node lost its root device was "did device 0 exist a moment before
        // the export took that number". A list here answers it in the log the
        // next time, at the cost of one readdir per attach.
        let before = existing_devices();
        // The engine's id of the volume behind the device: the key is the
        // /v1 id on that surface, which is not always the engine's.
        let volume_uuid = device.id().uuid;
        // Only a counter for thread names — the identity comes from the
        // kernel. Taken now: several attaches can be starting at once (#231).
        let seq = self.next_id;
        self.next_id += 1;
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let server = Arc::new(UblkServer::new(device));
        let runner = server.clone();
        let id = seq;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_done = done.clone();
        // UblkServer::run() holds non-Send raw pointers, so it must run on a
        // dedicated OS thread with its own runtime (same pattern as the
        // boot-iscsi ublk export in main.rs).
        std::thread::Builder::new()
            .name(format!("ublk-csi-{id}"))
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => {
                        crate::mgmt::debug::register_runtime(format!("ublk-export-{id}"), rt.handle().clone());
                        rt
                    }
                    Err(e) => {
                        tracing::error!("ublk-csi {id}: runtime init failed: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = runner.run(rx).await {
                        tracing::error!("ublk-csi {id}: export failed: {e}");
                    }
                });
                // Last thing this thread does: the device is stopped, deleted
                // and its queues closed, so a shutdown waiting on it can stop
                // waiting (#105).
                thread_done.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .ok()?;
        // Ready when the kernel has made the device: waited for by the
        // caller, with no lock held (#231, `wait_ready`).
        let _ = volume_id;
        Some(Launch { volume_uuid, shutdown, server: Some(server), done, before })
    }

    #[cfg(not(target_os = "linux"))]
    fn start(&mut self, _volume_id: &str, _device: Arc<dyn BlockDevice>) -> Option<Launch> {
        None
    }
}

/// Every ublk device the kernel currently has, as a list.
///
/// Read from sysfs rather than remembered: the devices that matter were made
/// by processes that no longer exist.
#[cfg(target_os = "linux")]
fn existing_devices() -> String {
    let mut ids: Vec<u32> = std::fs::read_dir("/sys/class/ublk-char")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("ublkc")?.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

#[cfg(not(target_os = "linux"))]
fn existing_devices() -> String {
    String::new()
}

/// Where `device` is mounted, if it is.
///
/// `/proc/self/mounts` rather than a cached list: the devices that matter here
/// were created by a process that no longer exists (the initramfs) and appear
/// in no table this process keeps.
#[cfg(target_os = "linux")]
fn mounted_on(device: &str) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        if f.next() == Some(device) {
            return f.next().map(|m| m.to_string());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn mounted_on(_device: &str) -> Option<String> {
    None
}

/// Probe whether the ublk control device is usable on this host.
#[cfg(target_os = "linux")]
fn ublk_available() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/ublk-control").is_ok()
}

#[cfg(not(target_os = "linux"))]
fn ublk_available() -> bool {
    false
}

// Registry bookkeeping is exercised without a kernel by injecting a fake
// export; the real device-creation path is covered on-metal (dev.g8.lo).
// Crate-visible so the attach handlers can be tested as they behave on a
// node where ublk works (#149).
#[cfg(test)]
impl UblkExportManager {
    /// [`begin`](Self::begin) as if ublk were here and a server had started
    /// (no kernel): the launch's shutdown is the returned receiver (#231).
    pub(crate) fn begin_fake(&mut self, volume_id: &str) -> (Begin, tokio::sync::watch::Receiver<bool>) {
        let (shutdown, down) = tokio::sync::watch::channel(false);
        if let Some(e) = self.exports.get(volume_id) {
            return (Begin::Ready(e.device_path.clone()), down);
        }
        if let Some(rx) = self.pending.get(volume_id) {
            return (Begin::Wait(rx.clone()), down);
        }
        let launch = Launch {
            volume_uuid: uuid::Uuid::new_v4(),
            shutdown,
            #[cfg(target_os = "linux")]
            server: None,
            done: Default::default(),
            before: String::new(),
        };
        let (tx, rx) = tokio::sync::watch::channel(None);
        self.pending.insert(volume_id.to_string(), rx);
        (Begin::Started(launch, tx), down)
    }

    pub(crate) fn insert_fake(&mut self, volume_id: &str, path: &str) {
        // Held like a real one when the key is an engine volume id.
        let held = uuid::Uuid::parse_str(volume_id).ok().map(|v| {
            let what = format!("ublk device {path}");
            self.holds.hold(v, what.clone());
            (v, what)
        });
        self.exports.insert(
            volume_id.to_string(),
            Export {
                device_path: path.to_string(),
                #[cfg(target_os = "linux")]
                shutdown: tokio::sync::watch::channel(false).0,
                #[cfg(target_os = "linux")]
                server: None,
                done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                held,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #231: the wait for a device sleeps asynchronously: the runtime it
    /// runs on (one thread here) goes on with everything else meanwhile.
    #[tokio::test(flavor = "current_thread")]
    async fn waiting_for_a_device_never_blocks_the_runtime() {
        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                t.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        let at = std::time::Instant::now();
        let ready = std::time::Duration::from_millis(200);
        let path = wait_ready(
            || (at.elapsed() >= ready).then_some(7),
            |_| at.elapsed() >= ready * 2,
            |_| None,
            ID_WAIT,
            NODE_WAIT,
        )
        .await;
        assert_eq!(path.as_deref(), Ok("/dev/ublkb7"));
        let n = ticks.load(std::sync::atomic::Ordering::Relaxed);
        assert!(n >= 30, "the runtime ran other work while it waited: {n} ticks in 400 ms");
        ticker.abort();
    }

    /// #231: the checks the blocking loops made still hold — no id, no
    /// device node, a mounted device: each an error, within its bound.
    #[tokio::test]
    async fn a_device_that_does_not_come_up_is_refused_in_time() {
        let ms = std::time::Duration::from_millis;
        let t = std::time::Instant::now();
        let r = wait_ready(|| None, |_| true, |_| None, ms(50), ms(50)).await;
        assert!(r.unwrap_err().contains("no device"));
        let r = wait_ready(|| Some(3), |_| false, |_| None, ms(50), ms(50)).await;
        assert!(r.unwrap_err().contains("/dev/ublkb3 never appeared"));
        let r = wait_ready(|| Some(0), |_| true, |_| Some("/".into()), ms(50), ms(50)).await;
        assert!(r.unwrap_err().contains("already mounted on /"));
        assert!(t.elapsed() < ms(1000), "bounded: {:?}", t.elapsed());
    }

    /// #231: while one volume's device comes up, the export table is free:
    /// another lookup answers at once; a second attach of the same volume
    /// waits for the first's answer and starts nothing; a device that does
    /// not come up takes its server down and leaves nothing pending.
    #[tokio::test]
    async fn the_export_table_is_free_while_a_device_comes_up() {
        let mgr = Arc::new(tokio::sync::Mutex::new(UblkExportManager::new()));
        mgr.lock().await.insert_fake("other", "/dev/ublkb1");

        let (begun, mut down_a) = mgr.lock().await.begin_fake("vol-a");
        let Begin::Started(launch, tx) = begun else { panic!("started") };
        assert!(mgr.lock().await.is_pending("vol-a"));
        // The device takes 300 ms, waited for with no lock held, as `attach`
        // does.
        let m = mgr.clone();
        let slow = tokio::spawn(async move {
            let at = std::time::Instant::now();
            let ready = wait_ready(
                || (at.elapsed() >= std::time::Duration::from_millis(300)).then_some(9),
                |_| true,
                |_| None,
                ID_WAIT,
                NODE_WAIT,
            )
            .await;
            let path = m.lock().await.finish("vol-a", launch, ready);
            let _ = tx.send(Some(path));
        });
        // Meanwhile: the table answers, at once.
        let t = std::time::Instant::now();
        assert_eq!(mgr.lock().await.device_path("other").as_deref(), Some("/dev/ublkb1"));
        assert!(t.elapsed() < std::time::Duration::from_millis(100), "not held: {:?}", t.elapsed());
        // A second attach of vol-a waits for the first.
        let (again, _) = mgr.lock().await.begin_fake("vol-a");
        let Begin::Wait(mut answer) = again else { panic!("waits for the attach under way") };
        answer.changed().await.unwrap();
        assert_eq!(answer.borrow().clone(), Some(Some("/dev/ublkb9".to_string())));
        slow.await.unwrap();
        assert_eq!(mgr.lock().await.device_path("vol-a").as_deref(), Some("/dev/ublkb9"));
        assert!(!mgr.lock().await.is_pending("vol-a"));
        assert!(!*down_a.borrow_and_update(), "a device that came up is kept");

        // One that never comes up: refused, its server told to go, nothing
        // left pending, and the next attach starts afresh.
        let (begun, mut down_b) = mgr.lock().await.begin_fake("vol-b");
        let Begin::Started(launch, _tx) = begun else { panic!("started") };
        let ready = wait_ready(|| None, |_| true, |_| None, std::time::Duration::from_millis(30), NODE_WAIT).await;
        assert_eq!(mgr.lock().await.finish("vol-b", launch, ready), None);
        assert!(*down_b.borrow_and_update(), "its server is taken down");
        assert!(!mgr.lock().await.is_pending("vol-b"));
        assert_eq!(mgr.lock().await.device_path("vol-b"), None);
        let (fresh, _) = mgr.lock().await.begin_fake("vol-b");
        assert!(matches!(fresh, Begin::Started(..)), "a later attach starts a new one");
    }

    #[test]
    fn offer_policy_requires_enabled_local_and_backed() {
        // Happy path: enabled, same node, locally backed.
        assert_eq!(WantTransport::parse(None), Ok(WantTransport::Any));
        for t in ["nvme_tcp", "nvme-tcp", "NVMeoF"] {
            assert_eq!(WantTransport::parse(Some(t)), Ok(WantTransport::NvmeTcp), "{t}");
        }
        assert_eq!(WantTransport::parse(Some("ublk")), Ok(WantTransport::Ublk));
        assert!(WantTransport::parse(Some("iscsi")).is_err());
        assert!(should_offer_ublk(true, "node-a", "node-a", true));
        // Disabled by config.
        assert!(!should_offer_ublk(false, "node-a", "node-a", true));
        // Attaching node is not this node (remote reader / migration target).
        assert!(!should_offer_ublk(true, "node-b", "node-a", true));
        // Not backed locally (this node holds no master replica).
        assert!(!should_offer_ublk(true, "node-a", "node-a", false));
    }

    #[test]
    fn ensure_is_idempotent_and_remove_clears() {
        let mut mgr = UblkExportManager::new();
        mgr.insert_fake("vol-1", "/dev/ublkb7");
        assert_eq!(mgr.device_path("vol-1").as_deref(), Some("/dev/ublkb7"));
        mgr.remove("vol-1");
        assert_eq!(mgr.device_path("vol-1"), None);
        // Removing an unknown volume is a no-op.
        mgr.remove("vol-unknown");
    }

    #[tokio::test]
    async fn settle_returns_when_every_teardown_has_finished() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let quick = Arc::new(AtomicBool::new(false));
        let wait = ShutdownWait {
            devices: vec![("/dev/ublkb1".to_string(), quick.clone())],
        };
        let flip = quick.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            flip.store(true, Ordering::SeqCst);
        });
        let stuck = wait.settle(std::time::Duration::from_secs(5)).await;
        assert!(stuck.is_empty(), "a finished teardown must not be reported stuck");
    }

    /// The case that matters: a teardown that never finishes is *named* and
    /// the stop carries on. Waiting for it is what produced the minute-long
    /// restart in #105.
    #[tokio::test]
    async fn settle_gives_up_and_names_what_is_stuck() {
        use std::sync::atomic::AtomicBool;
        let wait = ShutdownWait {
            devices: vec![
                ("/dev/ublkb1".to_string(), Arc::new(AtomicBool::new(true))),
                ("/dev/ublkb2".to_string(), Arc::new(AtomicBool::new(false))),
            ],
        };
        let started = std::time::Instant::now();
        let stuck = wait.settle(std::time::Duration::from_millis(100)).await;
        assert_eq!(stuck, vec!["/dev/ublkb2".to_string()]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "settle must be bounded");
    }

    #[tokio::test]
    async fn shutdown_all_empties_the_registry() {
        let mut mgr = UblkExportManager::new();
        mgr.insert_fake("vol-1", "/dev/ublkb7");
        mgr.insert_fake("vol-2", "/dev/ublkb8");
        let wait = mgr.shutdown_all();
        assert!(mgr.device_path("vol-1").is_none());
        assert!(mgr.device_path("vol-2").is_none());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(wait.len(), 2);
            // Nothing sets these flags — the fakes have no thread — so the
            // budget is what ends the wait.
            let stuck = wait.settle(std::time::Duration::from_millis(50)).await;
            assert_eq!(stuck.len(), 2);
        }
        #[cfg(not(target_os = "linux"))]
        assert!(wait.is_empty());
    }

    /// A device holds its volume in the volume manager's holds from the
    /// moment it serves it until it is torn down, and an adopted boot device
    /// for as long as the engine runs (#267).
    #[test]
    fn a_served_volume_is_held_until_its_device_goes() {
        let holds = crate::volume::holds::ServeHolds::default();
        let mut mgr = UblkExportManager::new().with_holds(holds.clone());
        let (a, b, root) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        mgr.insert_fake(&a.to_string(), "/dev/ublkb7");
        mgr.insert_fake(&b.to_string(), "/dev/ublkb8");
        mgr.record_adopted(&root.to_string(), "/dev/ublkb0".to_string());
        assert_eq!(holds.held_by(a), vec!["ublk device /dev/ublkb7".to_string()]);
        assert_eq!(holds.held_by(root), vec!["ublk device /dev/ublkb0 (adopted)".to_string()]);

        mgr.remove(&a.to_string());
        assert!(holds.held_by(a).is_empty(), "detached: released");
        assert!(!holds.held_by(b).is_empty());

        let _ = mgr.shutdown_all();
        assert!(holds.held_by(b).is_empty(), "a stop releases what it tore down");
        assert!(!holds.held_by(root).is_empty(), "the adopted root is held for the engine's life");
    }

    #[test]
    fn unavailable_host_declines_so_caller_uses_nvme_tcp() {
        let mgr = UblkExportManager { available: false, ..UblkExportManager::new() };
        // No panic, just None — nvme-tcp fallback. (device is never touched.)
        assert!(mgr.device_path("vol-x").is_none());
    }
}
