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
    /// A vhost-user-blk socket for a VMM on this node (#82). Only ever when
    /// asked for: nothing but a VMM can use it. `/api/v1` attach only.
    VhostUserBlk,
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
                "vhost_user_blk" | "vhost-user-blk" | "vhost-user" | "vhost" => Ok(WantTransport::VhostUserBlk),
                other => Err(format!("transport {other:?}: use nvme_tcp, ublk or vhost_user_blk")),
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
    /// What the server's thread releases as it exits, before `done` (#502).
    exit: Exit,
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
    mounts_of(device_path).into_iter().next()
}

/// Where `device_path` is first mounted, and a path this process reaches it
/// by (`/proc/<pid>/root/<mount point>`, any mount namespace): what the fill
/// of a volume is read from (#690).
pub fn reach_of(device_path: &str) -> Option<(String, String)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(device_path).ok()?;
        let rdev = m.rdev();
        let want = format!("{}:{}", libc::major(rdev), libc::minor(rdev));
        let t = mount_table();
        let at = t.get(&want)?.first()?.clone();
        Some((at, t.reach(&want)?.clone()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_path;
        None
    }
}

/// Every place `device_path` is mounted on the node, in every mount
/// namespace (#658): a service's `/state`, a pod's `/var/log/pods`, not only
/// what PID 1 and this engine see. The node's own namespace first.
pub fn mounts_of(device_path: &str) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(m) = std::fs::metadata(device_path) else { return Vec::new() };
        let rdev = m.rdev();
        let want = format!("{}:{}", libc::major(rdev), libc::minor(rdev));
        mount_table().get(&want).cloned().unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_path;
        Vec::new()
    }
}

/// The node's mounts by device: `maj:min` → mount points, each once, in the
/// order first seen, at most [`MOUNTS_PER_DEVICE`] each.
#[derive(Debug, Default)]
pub struct MountTable {
    by_dev: HashMap<String, Vec<String>>,
    seen: std::collections::HashSet<(String, String)>,
    /// Mounts past [`MOUNTS_PER_DEVICE`] on one device, not kept.
    pub over: usize,
    /// Per device, its first mount as a path this process reaches (#690):
    /// `/proc/<pid>/root` + the mount point in that pid's namespace.
    reach: HashMap<String, String>,
}

/// Mount points kept for one device. A device's first mount is what every
/// caller asks for; dozens are a node's ordinary binds of one volume.
pub const MOUNTS_PER_DEVICE: usize = 256;

/// Lines read from one namespace's mountinfo (#685). A node has hundreds;
/// a mount propagated into itself (a Bidirectional hostPath over the tree
/// it is mounted in) multiplies them without end, and the engine must not
/// be the process that reads them all into memory it has locked.
pub const MOUNTINFO_LINES: usize = 50_000;

impl MountTable {
    fn add(&mut self, dev: &str, mp: &str, root: Option<&str>) {
        if let Some(root) = root {
            self.reach.entry(dev.to_string()).or_insert_with(|| format!("{root}{mp}"));
        }
        if self.seen.contains(&(dev.to_string(), mp.to_string())) {
            return;
        }
        let v = self.by_dev.entry(dev.to_string()).or_default();
        if v.len() >= MOUNTS_PER_DEVICE {
            self.over += 1;
            return;
        }
        v.push(mp.to_string());
        self.seen.insert((dev.to_string(), mp.to_string()));
    }

    /// One mountinfo table's lines, at most `max`; how many were read, and
    /// whether the table had more.
    pub fn take(&mut self, lines: impl Iterator<Item = String>, max: usize) -> (usize, bool) {
        self.take_in(None, lines, max)
    }

    /// [`MountTable::take`] for the namespace `root` (`/proc/<pid>/root`)
    /// reaches, so a mount there can be `statfs`ed from here (#690).
    pub fn take_in(&mut self, root: Option<&str>, lines: impl Iterator<Item = String>, max: usize) -> (usize, bool) {
        let mut n = 0;
        for line in lines {
            if n == max {
                return (n, true);
            }
            n += 1;
            let mut f = line.split(' ');
            let (Some(_), Some(_), Some(dev), Some(_), Some(mp)) = (f.next(), f.next(), f.next(), f.next(), f.next()) else { continue };
            self.add(dev, mp, root);
        }
        (n, false)
    }

    pub fn get(&self, dev: &str) -> Option<&Vec<String>> {
        self.by_dev.get(dev)
    }

    /// The path that reaches `dev`'s first mount from this process (#690).
    pub fn reach(&self, dev: &str) -> Option<&String> {
        self.reach.get(dev)
    }
}

/// One mountinfo table's text into `out` (tests).
pub fn take_mountinfo(text: &str, out: &mut MountTable) {
    out.take(text.lines().map(str::to_string), usize::MAX);
}

/// The node's mounts by device, from each mount namespace once (PID 1's
/// first), read at most every two seconds: a listing or a watch asks for
/// every volume at once. While one caller reads the tables, the others are
/// given the last ones (#685): a slow read never queues the runtime's
/// threads behind it.
#[cfg(target_os = "linux")]
fn mount_table() -> Arc<MountTable> {
    type Cached = Option<(std::time::Instant, Arc<MountTable>)>;
    static CACHE: std::sync::Mutex<Cached> = std::sync::Mutex::new(None);
    static READING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let cached = || CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some((at, t)) = cached() {
        if at.elapsed() < std::time::Duration::from_secs(2) {
            return t;
        }
    }
    let _reading = match READING.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => match cached() {
            Some((_, t)) => return t,
            None => READING.lock().unwrap_or_else(|e| e.into_inner()),
        },
    };
    // Read by another caller while this one waited.
    if let Some((at, t)) = cached() {
        if at.elapsed() < std::time::Duration::from_secs(2) {
            return t;
        }
    }
    let t = Arc::new(read_mount_tables());
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), t.clone()));
    t
}

#[cfg(target_os = "linux")]
fn read_mount_tables() -> MountTable {
    use std::io::BufRead;
    static SAID: std::sync::Mutex<Option<std::collections::HashSet<std::ffi::OsString>>> = std::sync::Mutex::new(None);
    let mut out = MountTable::default();
    let mut seen: std::collections::HashSet<std::ffi::OsString> = std::collections::HashSet::new();
    let mut pids: Vec<String> = vec!["1".into(), "self".into()];
    if let Ok(rd) = std::fs::read_dir("/proc") {
        pids.extend(rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.bytes().all(|b| b.is_ascii_digit())));
    }
    for pid in pids {
        let Ok(ns) = std::fs::read_link(format!("/proc/{pid}/ns/mnt")) else { continue };
        let ns = ns.into_os_string();
        if !seen.insert(ns.clone()) {
            continue;
        }
        let Ok(f) = std::fs::File::open(format!("/proc/{pid}/mountinfo")) else { continue };
        let lines = std::io::BufReader::new(f).lines().map_while(Result::ok);
        let root = format!("/proc/{pid}/root");
        let (_, more) = out.take_in(Some(&root), lines, MOUNTINFO_LINES);
        if more {
            let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
            if said.get_or_insert_with(Default::default).insert(ns.clone()) {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                tracing::warn!(
                    "mounts: the mount namespace {} of pid {pid} ({}) has more than {MOUNTINFO_LINES} mounts; only the first are read. \
                     A mount propagated back into its own tree multiplies like this (#685)",
                    ns.to_string_lossy(),
                    comm.trim()
                );
            }
        }
    }
    out
}

/// How long a detach waits for its device to be gone before it answers
/// (#502). A server stops in milliseconds; the bound is for one that does
/// not, whose volume then stays held (a delete answers 409) until it does.
pub const DETACH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether something holds `device_path` open exclusively: a mount, also
/// while it is still being made (#502). The mount table names a mount only
/// once it is made, and on server1's SMR disk the mount of a container root
/// took 30 s; the detach in between stopped the device under it. An
/// exclusive open (`O_EXCL` on a block device) fails `EBUSY` while one is
/// held. Our own probe holds the claim for no longer than the open.
pub fn claimed(device_path: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(device_path) {
            Ok(_) => false,
            Err(e) => e.raw_os_error() == Some(libc::EBUSY),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_path;
        false
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
/// then the device path or why it did not come up (#554).
type Answer = tokio::sync::watch::Receiver<Option<Result<String, String>>>;

/// The first half of an attach, under the export lock (#231).
pub enum Begin {
    /// Already exported: its device.
    Ready(String),
    /// No device can be made here: why (#554).
    Unavailable(String),
    /// Another attach of this volume is bringing its device up: wait for it.
    Wait(Answer),
    /// A server was started: wait for its device without the lock, then
    /// [`UblkExportManager::finish`].
    Started(Launch, tokio::sync::watch::Sender<Option<Result<String, String>>>),
}

/// What holds a volume while its ublk device is coming up (#493).
const STARTING: &str = "a ublk device being started";

/// Why no device is made on a node without ublk (#554: said only then, never
/// for a device that did not come up).
pub const NOT_AVAILABLE: &str = "ublk is not available on this node (kernel ublk_drv not loaded)";

/// Why [`attach_local`] made no device.
#[derive(Debug)]
pub enum LocalAttachError {
    /// The volume was deleted while its device came up (#493): the caller
    /// answers 404.
    Deleted(String),
    /// No device was made, and why (#554): ublk is not on this node, or the
    /// device did not come up. The caller may use NVMe/TCP instead.
    NoDevice(String),
}

/// [`attach`] for a volume of this engine: when the volume was deleted
/// while its device came up (a delete that came before the device's hold),
/// the device is taken down again and says so: the caller answers 404
/// rather than a device serving a volume that is gone (#493).
pub async fn attach_local(
    state: &Arc<crate::mgmt::AppState>,
    volume_id: &str,
    volume: uuid::Uuid,
    device: Arc<dyn BlockDevice>,
) -> Result<String, LocalAttachError> {
    let path = attach(state, volume_id, device).await.map_err(LocalAttachError::NoDevice)?;
    if !state.volume_presence.contains(&crate::volume::VolumeId(volume)) {
        state.ublk_exports.lock().await.remove(volume_id);
        return Err(LocalAttachError::Deleted(format!("volume {volume} was deleted while its ublk device was being started")));
    }
    Ok(path)
}

/// Shared by an export and its server's thread (#502): whether the thread
/// has exited, and the hold a detach left for it to release. One lock for
/// both, so a hold handed over is released exactly once, by whichever comes
/// second, and always before `done` says the device is gone.
type Exit = Arc<std::sync::Mutex<ExitState>>;

#[derive(Default)]
struct ExitState {
    exited: bool,
    release: Option<(uuid::Uuid, String)>,
}

/// The server's thread is done: release what a detach left, then say so.
fn server_exited(holds: &crate::volume::holds::ServeHolds, exit: &Exit, done: &std::sync::atomic::AtomicBool) {
    let release = {
        let mut x = exit.lock().unwrap_or_else(|p| p.into_inner());
        x.exited = true;
        x.release.take()
    };
    if let Some((volume, what)) = release {
        holds.release(volume, &what);
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// A ublk server started for an attach and not yet ready (#231).
pub struct Launch {
    volume_uuid: uuid::Uuid,
    shutdown: tokio::sync::watch::Sender<bool>,
    #[cfg(target_os = "linux")]
    server: Option<Arc<crate::drive::ublk::UblkServer>>,
    done: Arc<std::sync::atomic::AtomicBool>,
    exit: Exit,
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

    /// Whether the device has gone live: before that its node may be one a
    /// deleted device of the same number left behind (#493).
    fn live(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.server.as_ref().is_some_and(|s| s.is_live())
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

/// How long the kernel has to assign a device id, and then to make the
/// block device appear (#231). The id's wait was 1 s until #554: on forge1
/// (12.64), right after a flow-over's validation, attaches that succeeded
/// took 1.7-2.0 s and five whose id came later than 1 s failed together. An
/// id slower than [`ID_SLOW`] is said with its time.
const ID_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const ID_SLOW: std::time::Duration = std::time::Duration::from_millis(500);
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
    let asked = tokio::time::Instant::now();
    let deadline = asked + id_wait;
    let id = loop {
        if let Some(id) = dev_id() {
            break id;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the kernel assigned no device in {:.1} s", id_wait.as_secs_f64()));
        }
        tokio::time::sleep(POLL).await;
    };
    let took = asked.elapsed();
    if took >= ID_SLOW {
        tracing::warn!("ublk: the kernel took {} ms to assign /dev/ublkb{id} (#554)", took.as_millis());
    }
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

/// Attach `volume_id` as a ublk device: its path, or why there is none (ublk
/// is not available, or the device did not come up, #554); the caller may
/// use NVMe/TCP instead.
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
) -> Result<String, String> {
    let begun = state.ublk_exports.lock().await.begin(volume_id, device);
    let mut answer = match begun {
        Begin::Ready(path) => return Ok(path),
        Begin::Unavailable(why) => return Err(why),
        Begin::Wait(rx) => rx,
        Begin::Started(launch, tx) => {
            let rx = tx.subscribe();
            let (state, volume_id) = (state.clone(), volume_id.to_string());
            tokio::spawn(async move {
                let ready = wait_ready(
                    || launch.dev_id(),
                    // Live first: a node of this number may be a deleted
                    // device's, not yet removed (#493: ENXIO on open).
                    |p| launch.live() && std::path::Path::new(p).exists(),
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
            return Err("the attach bringing the device up went away".into());
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
        // Its device taken away by another process (#299): the record goes,
        // and this attach makes a new device.
        #[cfg(target_os = "linux")]
        if self.exports.get(volume_id).is_some_and(|e| crate::drive::ublk::is_lost(&e.device_path)) {
            tracing::warn!(volume = volume_id, "its ublk device was taken away by another process; attaching a new one (#299)");
            if let Some(p) = self.exports.get(volume_id).map(|e| e.device_path.clone()) {
                let _ = self.remove(volume_id);
                crate::drive::ublk::forget_lost(&p);
            }
        }
        if let Some(e) = self.exports.get(volume_id) {
            return Begin::Ready(e.device_path.clone());
        }
        if let Some(rx) = self.pending.get(volume_id) {
            return Begin::Wait(rx.clone());
        }
        if !self.available {
            return Begin::Unavailable(NOT_AVAILABLE.into());
        }
        match self.start(volume_id, device) {
            Some(launch) => {
                // Held from now, not from when the device is ready (#493): a
                // delete in between went through and the device came up on a
                // volume that was gone (the mount's EINVAL, ENXIO, EUCLEAN).
                self.holds.hold(launch.volume_uuid, STARTING);
                let (tx, rx) = tokio::sync::watch::channel(None);
                self.pending.insert(volume_id.to_string(), rx);
                Begin::Started(launch, tx)
            }
            None => Begin::Unavailable("the ublk server could not be started (the engine's log says why)".into()),
        }
    }

    /// The second half of an attach (#231), under the lock again once the
    /// device is ready or is not coming: record the export, or take its
    /// server down. Returns what the attach answers.
    pub fn finish(&mut self, volume_id: &str, launch: Launch, ready: Result<String, String>) -> Result<String, String> {
        self.pending.remove(volume_id);
        let device_path = match ready {
            Ok(p) => p,
            Err(why) => {
                tracing::error!(volume = volume_id, "ublk export did not come up: {why}");
                let _ = launch.shutdown.send(true);
                // Held until the server has exited (#554): told to stop, it
                // can still bring the device up, and on forge1 the kernel's
                // partition scan read the volume the kubelet had deleted
                // meanwhile. A delete before then is refused, as for a
                // detach still stopping (#502).
                let mut x = launch.exit.lock().unwrap_or_else(|p| p.into_inner());
                #[cfg(target_os = "linux")]
                let serving = launch.server.is_some();
                #[cfg(not(target_os = "linux"))]
                let serving = false;
                if x.exited || !serving {
                    drop(x);
                    self.holds.release(launch.volume_uuid, STARTING);
                } else {
                    x.release = Some((launch.volume_uuid, STARTING.to_string()));
                }
                return Err(format!("the ublk device did not come up: {why}"));
            }
        };
        self.holds.release(launch.volume_uuid, STARTING);
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
                exit: launch.exit,
                held: Some((launch.volume_uuid, what)),
            },
        );
        tracing::info!(
            volume = volume_id,
            device = %device_path,
            existing_before = %launch.before,
            "ublk export created"
        );
        Ok(device_path)
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

    /// The volume whose ublk device this engine created by an attach is at
    /// `device_path`. An adopted boot device (the root, the node's mounted
    /// volumes) is never one: nothing may be written over it (#393).
    pub fn attached_volume_at(&self, device_path: &str) -> Option<String> {
        self.exports
            .iter()
            .find(|(_, e)| e.device_path == device_path)
            .map(|(v, _)| v.clone())
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
    ///
    /// The volume stays held until the device's server has exited (#502):
    /// a server told to stop goes on serving what the kernel already queued,
    /// and a mount still opening the device reads through it. Released at
    /// once, the hold let server1's kubelet delete a container root while
    /// stormpump's mount was still reading it, and the mount read a volume
    /// that had been deleted (EIO). The returned wait says when the device
    /// is gone, for a caller that wants to answer only then.
    pub fn remove(&mut self, volume_id: &str) -> ShutdownWait {
        let Some(e) = self.exports.remove(volume_id) else { return ShutdownWait::none() };
        #[cfg(target_os = "linux")]
        {
            // The server removes the kernel device on exit.
            let _ = e.shutdown.send(true);
            if e.server.is_some() {
                if let Some((volume, what)) = &e.held {
                    let stopping = format!("{what} being stopped");
                    self.holds.hold(*volume, stopping.clone());
                    self.holds.release(*volume, what);
                    let mut x = e.exit.lock().unwrap_or_else(|p| p.into_inner());
                    if x.exited {
                        drop(x);
                        self.holds.release(*volume, &stopping);
                    } else {
                        // Released by the server's thread as it exits.
                        x.release = Some((*volume, stopping));
                    }
                }
                return ShutdownWait { devices: vec![(e.device_path, e.done)] };
            }
        }
        self.release(&e);
        ShutdownWait::none()
    }

    #[cfg(target_os = "linux")]
    fn start(&mut self, volume_id: &str, device: Arc<dyn BlockDevice>) -> Option<Launch> {
        use crate::drive::ublk::UblkServer;

        // One more export must fit under the fd limit (#522): refused here,
        // said, rather than half-made with EMFILE in one of its queues.
        if let Err(why) = room_for_one(&*device) {
            tracing::error!("ublk export of {volume_id} refused: {why}");
            return None;
        }

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
        let exit: Exit = Default::default();
        let (thread_exit, thread_holds) = (exit.clone(), self.holds.clone());
        // UblkServer::run() holds non-Send raw pointers, so it must run on a
        // dedicated OS thread with its own runtime (same pattern as the
        // boot-iscsi ublk export in main.rs).
        std::thread::Builder::new()
            .name(format!("ublk-csi-{id}"))
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("ublk-csi {id}: runtime init failed: {e}");
                        // Said done: an attach waiting on it, and a shutdown,
                        // must not wait for a server that never ran (#510).
                        server_exited(&thread_holds, &thread_exit, &thread_done);
                        return;
                    }
                };
                // Unlisted before the runtime goes (#510): the list held
                // every export's handle, and with it its fds, for good.
                let listed = crate::mgmt::debug::register_runtime(format!("ublk-export-{id}"), rt.handle().clone());
                rt.block_on(async move {
                    if let Err(e) = runner.run(rx).await {
                        tracing::error!("ublk-csi {id}: export failed: {e}");
                    }
                });
                drop(listed);
                drop(rt);
                // Last thing this thread does: the device is stopped, deleted
                // and its queues closed, so a shutdown waiting on it can stop
                // waiting (#105).
                server_exited(&thread_holds, &thread_exit, &thread_done);
            })
            .ok()?;
        // Ready when the kernel has made the device: waited for by the
        // caller, with no lock held (#231, `wait_ready`).
        let _ = volume_id;
        Some(Launch { volume_uuid, shutdown, server: Some(server), done, exit, before })
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
            // The volume's own id when the key is one, as a real attach has.
            volume_uuid: uuid::Uuid::parse_str(volume_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            shutdown,
            #[cfg(target_os = "linux")]
            server: None,
            done: Default::default(),
            exit: Default::default(),
            before: String::new(),
        };
        self.holds.hold(launch.volume_uuid, STARTING);
        let (tx, rx) = tokio::sync::watch::channel(None);
        self.pending.insert(volume_id.to_string(), rx);
        (Begin::Started(launch, tx), down)
    }

    /// [`insert_fake`](Self::insert_fake) with a server behind it (never
    /// run: no kernel), as a real export has: the returned flag is its
    /// thread's "exited" (#502).
    #[cfg(target_os = "linux")]
    pub(crate) fn insert_fake_served(&mut self, volume_id: &str, path: &str) -> impl FnOnce() + use<> {
        let spec = crate::drive::emulated::EmulatedSpec::parse(&format!(
            "emulated://ublk-fake-{}?size=16M",
            uuid::Uuid::new_v4().simple()
        ))
        .unwrap()
        .unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(crate::drive::emulated::open(&spec).unwrap());
        self.insert_fake(volume_id, path);
        let e = self.exports.get_mut(volume_id).unwrap();
        e.server = Some(Arc::new(crate::drive::ublk::UblkServer::new(dev)));
        let (holds, exit, done) = (self.holds.clone(), e.exit.clone(), e.done.clone());
        move || server_exited(&holds, &exit, &done)
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
                exit: Default::default(),
                held,
            },
        );
    }
}

/// Whether one more export over `device` fits under the fd limit (#522),
/// or why not: what an attach that got no device says to its caller.
pub fn room_for_one(device: &dyn BlockDevice) -> Result<(), String> {
    crate::fdlimit::room_for(crate::drive::ublk::fds_for(device)).map(|_| ())
}

#[cfg(test)]
mod tests {

    /// #658: the mount tables of every namespace, read into one map by
    /// device: each mount point once, the first namespace's first.
    #[test]
    fn mount_tables_merged() {
        let mut m = MountTable::default();
        take_mountinfo("36 25 259:3 / / rw - ext4 /dev/ublkb3 rw\n37 25 259:15 / /state rw - ext4 /dev/ublkb15 rw\n", &mut m);
        take_mountinfo("90 80 259:15 / /state rw - ext4 /dev/ublkb15 rw\n91 80 259:12 / /var/log/pods rw - ext4 /dev/ublkb12 rw\n", &mut m);
        assert_eq!(m.get("259:3").unwrap(), &vec!["/".to_string()]);
        assert_eq!(m.get("259:15").unwrap(), &vec!["/state".to_string()], "the same mount in two namespaces, once");
        assert_eq!(m.get("259:12").unwrap(), &vec!["/var/log/pods".to_string()], "a pod's own namespace counts");
    }

    /// #685: a table that multiplied is read only so far, and a device's
    /// mounts are kept to a bound; the first mount is still the first.
    #[test]
    fn a_multiplied_mount_table_is_read_within_bounds() {
        let lines = (0..200_000).map(|i| format!("{i} 1 259:10 / /var/lib/kubelet{} rw - ext4 /dev/ublkb10 rw", "/x".repeat(i % 7)));
        let mut m = MountTable::default();
        let (read, more) = m.take(lines, MOUNTINFO_LINES);
        assert_eq!((read, more), (MOUNTINFO_LINES, true));
        let v = m.get("259:10").unwrap();
        assert_eq!(v.len(), 7, "each distinct mount point once");
        assert_eq!(v[0], "/var/lib/kubelet");

        let distinct = (0..10_000).map(|i| format!("{i} 1 259:10 / /p/{i} rw - ext4 /dev/ublkb10 rw"));
        let mut m = MountTable::default();
        assert_eq!(m.take(distinct, MOUNTINFO_LINES), (10_000, false));
        assert_eq!(m.get("259:10").unwrap().len(), MOUNTS_PER_DEVICE);
        assert_eq!(m.over, 10_000 - MOUNTS_PER_DEVICE);
        assert_eq!(m.get("259:10").unwrap()[0], "/p/0");
    }

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
        assert_eq!(answer.borrow().clone(), Some(Ok("/dev/ublkb9".to_string())));
        slow.await.unwrap();
        assert_eq!(mgr.lock().await.device_path("vol-a").as_deref(), Some("/dev/ublkb9"));
        assert!(!mgr.lock().await.is_pending("vol-a"));
        assert!(!*down_a.borrow_and_update(), "a device that came up is kept");

        // One that never comes up: refused, its server told to go, nothing
        // left pending, and the next attach starts afresh.
        let (begun, mut down_b) = mgr.lock().await.begin_fake("vol-b");
        let Begin::Started(launch, _tx) = begun else { panic!("started") };
        let ready = wait_ready(|| None, |_| true, |_| None, std::time::Duration::from_millis(30), NODE_WAIT).await;
        let why = mgr.lock().await.finish("vol-b", launch, ready).unwrap_err();
        assert!(why.contains("did not come up") && why.contains("assigned no device"), "{why}");
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

    /// #493: a volume is held from the moment its device starts, not from
    /// when it is ready: the kubelet's root sweep deleted roots while their
    /// devices came up, and the devices then served volumes that were gone.
    /// The hold passes to the device when it comes up, and goes when it
    /// does not.
    #[test]
    fn a_volume_is_held_while_its_device_comes_up() {
        let holds = crate::volume::holds::ServeHolds::default();
        let mut mgr = UblkExportManager::new().with_holds(holds.clone());
        let a = uuid::Uuid::new_v4();
        let (begun, _down) = mgr.begin_fake(&a.to_string());
        let Begin::Started(launch, _tx) = begun else { panic!("started") };
        assert_eq!(holds.held_by(a), vec![STARTING.to_string()], "held while the device starts");
        assert_eq!(mgr.finish(&a.to_string(), launch, Ok("/dev/ublkb85".into())).as_deref(), Ok("/dev/ublkb85"));
        assert_eq!(holds.held_by(a), vec!["ublk device /dev/ublkb85".to_string()], "then by the device");

        let b = uuid::Uuid::new_v4();
        let (begun, _down) = mgr.begin_fake(&b.to_string());
        let Begin::Started(launch, _tx) = begun else { panic!("started") };
        assert!(!holds.held_by(b).is_empty());
        assert!(mgr.finish(&b.to_string(), launch, Err("never came up".into())).is_err());
        assert!(holds.held_by(b).is_empty(), "a device that did not come up, with no server, holds nothing");
    }

    /// #554: forge1 (12.64): five devices whose id came later than the wait
    /// failed together; their servers were told to stop, the hold went at
    /// once, the kubelet deleted the clones, and the kernel's partition scan
    /// then read the deleted volumes through devices that came up after all.
    /// A failed start now holds its volume until its server has exited, so a
    /// delete in between is refused; and the attach says why it failed,
    /// never that ublk is not on the node.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_that_did_not_come_up_holds_its_volume_until_its_server_exits() {
        let holds = crate::volume::holds::ServeHolds::default();
        let mut mgr = UblkExportManager::new().with_holds(holds.clone());
        let a = uuid::Uuid::new_v4();
        let (begun, mut down) = mgr.begin_fake(&a.to_string());
        let Begin::Started(mut launch, _tx) = begun else { panic!("started") };
        // A server behind it, as a real start has (never run: no kernel).
        let spec = crate::drive::emulated::EmulatedSpec::parse(&format!(
            "emulated://ublk-fake-{}?size=16M",
            uuid::Uuid::new_v4().simple()
        ))
        .unwrap()
        .unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(crate::drive::emulated::open(&spec).unwrap());
        launch.server = Some(Arc::new(crate::drive::ublk::UblkServer::new(dev)));
        let (exit, done) = (launch.exit.clone(), launch.done.clone());

        let why = mgr.finish(&a.to_string(), launch, Err("the kernel assigned no device in 5.0 s".into())).unwrap_err();
        assert!(why.contains("did not come up") && why.contains("assigned no device"), "{why}");
        assert!(!why.contains("not available"), "{why}");
        assert!(*down.borrow_and_update(), "its server is told to stop");
        assert_eq!(holds.held_by(a), vec![STARTING.to_string()], "held while its server may still bring it up");
        assert!(!mgr.is_pending(&a.to_string()), "a new attach may start");

        server_exited(&holds, &exit, &done);
        assert!(holds.held_by(a).is_empty(), "let go once the server has exited");
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // Exited before the start was judged failed: let go at once.
        let b = uuid::Uuid::new_v4();
        let (begun, _down) = mgr.begin_fake(&b.to_string());
        let Begin::Started(mut launch, _tx) = begun else { panic!("started") };
        let spec = crate::drive::emulated::EmulatedSpec::parse(&format!(
            "emulated://ublk-fake-{}?size=16M",
            uuid::Uuid::new_v4().simple()
        ))
        .unwrap()
        .unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(crate::drive::emulated::open(&spec).unwrap());
        launch.server = Some(Arc::new(crate::drive::ublk::UblkServer::new(dev)));
        server_exited(&holds, &launch.exit.clone(), &launch.done.clone());
        assert!(mgr.finish(&b.to_string(), launch, Err("x".into())).is_err());
        assert!(holds.held_by(b).is_empty(), "a server already gone holds nothing");
    }

    /// #554: no ublk on the node is the only "not available"; a device
    /// that did not come up says why.
    #[test]
    fn only_a_node_without_ublk_says_ublk_is_not_available() {
        let mut mgr = UblkExportManager::new();
        mgr.available = false;
        let spec = crate::drive::emulated::EmulatedSpec::parse("emulated://ublk-na?size=16M").unwrap().unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(crate::drive::emulated::open(&spec).unwrap());
        let Begin::Unavailable(why) = mgr.begin("v", dev) else { panic!("unavailable") };
        assert_eq!(why, NOT_AVAILABLE);
    }

    /// #502: a detached device holds its volume until its server has
    /// exited, not from the moment it is told to stop: server1's kubelet
    /// detached and deleted a container root while stormpump's mount was
    /// still reading through its device, and the mount read a deleted volume.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_detached_device_holds_its_volume_until_its_server_exits() {
        let holds = crate::volume::holds::ServeHolds::default();
        let mut mgr = UblkExportManager::new().with_holds(holds.clone());
        let a = uuid::Uuid::new_v4();
        let exits = mgr.insert_fake_served(&a.to_string(), "/dev/ublkb90");
        let wait = mgr.remove(&a.to_string());
        assert!(!mgr.is_exported(&a.to_string()), "gone from the table at once");
        assert_eq!(
            holds.held_by(a),
            vec!["ublk device /dev/ublkb90 being stopped".to_string()],
            "held while its server still serves"
        );
        let (x, ()) = tokio::join!(wait.settle(std::time::Duration::from_secs(5)), async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(!holds.held_by(a).is_empty(), "held for as long as the server serves");
            exits();
        });
        assert!(x.is_empty(), "the wait ends once the server exits");
        // Released by the time the wait says the device is gone: a delete
        // right after a detach is never refused by its own device.
        assert!(holds.held_by(a).is_empty(), "released before the device is said to be gone");

        // A server that exited before the detach: released at the detach.
        let b = uuid::Uuid::new_v4();
        let exits = mgr.insert_fake_served(&b.to_string(), "/dev/ublkb91");
        exits();
        let wait = mgr.remove(&b.to_string());
        assert!(holds.held_by(b).is_empty(), "nothing left to wait for");
        assert!(wait.settle(std::time::Duration::from_millis(10)).await.is_empty());
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
