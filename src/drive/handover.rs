//! What the next server needs to know, written by the one that has it.
//!
//! A ublk device outlives the process that created it, which is what makes a
//! handover possible — but the kernel remembers only the device, not what is
//! behind it. It can say `/dev/ublkb4` exists and which pid serves it; it has
//! no idea that it is the volume called `stormblock-data`.
//!
//! So the server that creates the devices writes the mapping down, and the
//! server that adopts them reads it. Before this, the list was maintained by
//! hand in two places — `rd.stormblock.mount=` on the kernel command line and
//! the `--volume` list in the boot unit — which had to agree exactly and in
//! order. They stopped agreeing the first time the node gained a volume:
//! standing the incumbent down stops **every** device it serves, so the two
//! that were left off the list were abandoned mounted, returning EIO, and the
//! engine could not even be restarted because its own root was among them.
//!
//! Two hand-written lists that must agree is a defect whatever they contain.
//! There is one list now, on the kernel command line, and everything after it
//! is derived.
//!
//! **In `/run`, deliberately.** The mapping is true for this boot and no
//! other: device ids are assigned in creation order each time. `/run` is tmpfs
//! and the initramfs moves it into the new root across `switch_root`, so the
//! record survives exactly as long as it is true. Putting it on the slab would
//! outlive its own accuracy.

use serde::{Deserialize, Serialize};

/// Where the record lives. In `/run` because it is per-boot state; see above.
pub const DEFAULT_PATH: &str = "/run/stormblock/handover.json";

/// One exported device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// The ublk device id — `/dev/ublkb{dev_id}`.
    pub dev_id: u32,
    /// The volume behind it, by name. A name rather than a UUID because it is
    /// what the node's operator and its logs both use, and it is resolved
    /// through the same metadata the successor has already loaded.
    pub volume: String,
}

/// A local disk the boot laid out but did not fill.
///
/// The migration is minutes of background copying and the process that laid
/// the slabs has seconds to live: it is the initramfs engine, and `switch_root`
/// deletes the filesystem its binary came from the moment the successor takes
/// the ublk devices over. Running the copy there meant it was killed part-way
/// through, every time, leaving a slab that is real, incomplete, and unable to
/// boot the node — which is the shape the local-slab probe now has to reject.
///
/// So the long-lived process does the long-running job. The boot lays the
/// structure, which is fast and bounded, writes down what it laid, and the
/// engine that adopts the devices moves the extents at its leisure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowOver {
    /// The disk, for the log line. Nothing resolves anything through it.
    pub disk: String,
    /// The slab the goldens are migrating *into*, by id. By id rather than by
    /// role, because after the successor opens both the appliance's slabs and
    /// this disk's there are two system slabs registered and one of them is
    /// the source.
    pub system_slab: String,
    /// The local data slab, laid at the same time. Writable volumes belong on
    /// it — that is the whole point of taking the drive — and it is named here
    /// so the successor does not have to guess which of the two it is.
    pub data_slab: String,
    /// The data half moves in the background too (#285): the successor
    /// empties the appliance's data slabs into `data_slab` after the system
    /// half, as the volumes on them are written. Set by an initramfs that did
    /// not seed it before exporting; absent (an older record) means it did.
    #[serde(default)]
    pub data_flow: bool,
}

/// Where `boot-claim` leaves the install it was asked for (#148): written
/// when the claim answered `intent: install`, removed when it did not. The
/// initramfs reads its existence as "take the local disk with force";
/// `boot-local` carries it into the handover record.
pub const INSTALL_TICKET_PATH: &str = "/run/stormblock/install.json";

/// This machine's host secret (#247), as `boot-claim` was handed it: where
/// stormupdate reads it to re-point its own boothost, roll it back or set its
/// intent local, without a copy of the appliance's node token. 0600.
pub const HOST_SECRET_PATH: &str = "/run/stormblock/host-secret.json";
/// The same, kept in the engine's data directory, so a boot from the local
/// disk (no claim, nothing handed) still has the last one.
pub const HOST_SECRET_FILE: &str = "host-secret.json";

/// `{appliance, host, secret}`: who it is for, and the secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSecret {
    pub appliance: String,
    pub host: String,
    pub secret: String,
}

impl HostSecret {
    pub fn read(path: &std::path::Path) -> Option<HostSecret> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Written 0600 through a dot-file (which the state capture skips).
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(dir)?;
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let tmp = dir.join(format!(".{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(|e| std::io::Error::other(e.to_string()))?)?;
        drop(f);
        std::fs::rename(&tmp, path)
    }
}

/// Keep the host secret across boots (#247): one handed this boot (`run`)
/// goes into the data directory; with none handed (a boot from the local
/// disk), the kept one comes back to `run`. Returns which way it went.
pub fn carry_host_secret(run: &std::path::Path, data_dir: &std::path::Path) -> Option<&'static str> {
    let kept = data_dir.join(HOST_SECRET_FILE);
    match (HostSecret::read(run), HostSecret::read(&kept)) {
        (Some(now), Some(old)) if now == old => None,
        (Some(now), _) => now.write(&kept).ok().map(|_| "kept"),
        (None, Some(old)) => old.write(run).ok().map(|_| "restored"),
        (None, None) => None,
    }
}

/// Where `boot-claim` notes that the appliance stated **no** intent at all
/// (#236): an engine older than v20 (#148), which serves none. Written when
/// the claim reply carries no `intent`, removed when it does. The initramfs
/// reads it as "an install without an intent": a boot that claims and takes
/// a local disk lays a fresh slab rather than keeping the old data half. Once
/// the appliance states intents, this is never written and the intent
/// decides.
pub const NO_INTENT_PATH: &str = "/run/stormblock/no-intent";

/// An install the appliance asked for: who to tell, and about which clone,
/// once the flow-over is done and the disk boots on its own (#148). The
/// appliance sets the machine's intent back to `local` only for the clone a
/// claim handed out under `install`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallTicket {
    /// The appliance's base URL, as `boot-claim` was given it.
    pub boothost: String,
    /// The host's name on the appliance (what the claim resolved to).
    pub host: String,
    /// The boot clone's volume id — the claim reply's `volume.id`.
    pub volume: String,
}

impl InstallTicket {
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("encode install ticket: {e}")))?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)
    }

    pub fn read(path: &std::path::Path) -> Option<InstallTicket> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }
}

/// The install a node still has to prove (#220), kept in the engine's data
/// directory and so, through the state volume, on the disk it was laid on.
/// The installer's successor writes it `laid` once the appliance has taken
/// the disk as laid; the first boot that runs from local slabs only reports
/// `booted` and rewrites it `reported` (or `refused`). Rewritten, never
/// removed: the state volume keeps every file it has captured, so a removed
/// one would come back at the next restore.
pub const INSTALL_REPORT_FILE: &str = "install-report.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallReport {
    #[serde(flatten)]
    pub ticket: InstallTicket,
    /// `laid`, `reported` or `refused`.
    pub state: String,
    pub laid_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl InstallReport {
    pub fn path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(INSTALL_REPORT_FILE)
    }

    pub fn read(dir: &std::path::Path) -> Option<InstallReport> {
        serde_json::from_slice(&std::fs::read(Self::path(dir)).ok()?).ok()
    }

    /// Written through a dot-file (which the state capture skips) and renamed.
    pub fn write(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{INSTALL_REPORT_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("encode install report: {e}")))?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, Self::path(dir))
    }
}

/// Whether a boot ran from the machine's own disk alone: a handover record
/// with no flow-over and no install, and every slab a local path (#220). A
/// claimed clone is an `nvme-tcp://` (or `iscsi://`, `http://`) URI.
pub fn local_only_boot(record: &Record) -> bool {
    record.flow_over.is_none()
        && record.install.is_none()
        && !record.slabs.is_empty()
        && record.slabs.iter().all(|p| {
            !(p.starts_with("nvme-tcp://") || p.starts_with("iscsi://") || p.starts_with("http://") || p.starts_with("https://"))
        })
}

/// Everything the successor needs to take over without being told.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Record {
    /// The slab(s) the volumes live on, as they were opened.
    pub slabs: Vec<String>,
    /// An explicit metadata directory, if one was used. Normally absent: a
    /// slab built by `image build` carries its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
    /// Every device that was exported, in device order.
    pub devices: Vec<Device>,
    /// A local disk laid out by this boot, waiting to be filled. Absent on a
    /// node that has no local disk, and on every record written before this
    /// field existed — which is why it defaults rather than being required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_over: Option<FlowOver>,
    /// A local disk carrying this node's layout, to be made bootable on its
    /// own: the ESP and the boot pallets of the image this node booted, copied
    /// into the disk's boot area (#123). Named whenever the node has such a
    /// disk — freshly laid, updated, or already up to date — because a disk
    /// installed before local boot existed holds every golden and still has
    /// nothing firmware can start. The successor does it after the flow-over,
    /// if any, has finished: a disk that boots before its slabs are complete
    /// boots into a probe that rejects them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_boot: Option<String>,
    /// This boot is an install the appliance asked for (#148): once the
    /// flow-over and the local boot are done, the successor tells the
    /// appliance, which sets the machine's intent back to `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallTicket>,
    /// What an install over this node's disk did with its data half (#311):
    /// the release installed, the volumes kept, set aside and to migrate. The
    /// successor writes it into the node's release generations for
    /// stormupdate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<crate::image::install::Report>,
    /// The version of the engine serving the devices this record names (#189):
    /// written by `boot-local`, and rewritten by each `adopt-ublk` once it
    /// serves, so the next handover compares with the real incumbent. 11.45
    /// shipped a v19.1.4 engine over a v16.1.0 initramfs; the successor's
    /// handover depends on the incumbent's own stand-down, flush and exit.
    /// Absent in a record an engine older than this wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
}

/// This engine's version, as it is written into a handover record.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `adopt-ublk` does when the incumbent's version is not its own (#189).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VersionPolicy {
    /// Say so loudly, and take over.
    #[default]
    Warn,
    /// Refuse before standing the incumbent down: it serves on.
    Refuse,
}

impl std::str::FromStr for VersionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "warn" | "" => Ok(VersionPolicy::Warn),
            "refuse" => Ok(VersionPolicy::Refuse),
            o => Err(format!("{o:?}: warn or refuse")),
        }
    }
}

/// The incumbent's version against this engine's (#189).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    Same,
    Differs(String),
    /// No record, or one written before records carried a version.
    Unknown,
}

pub fn check_version(record: Option<&Record>, mine: &str) -> VersionCheck {
    match record.and_then(|r| r.engine_version.as_deref()) {
        Some(v) if v == mine => VersionCheck::Same,
        Some(v) => VersionCheck::Differs(v.to_string()),
        None => VersionCheck::Unknown,
    }
}

impl VersionCheck {
    /// What to say, and whether `policy` lets the takeover go on.
    pub fn verdict(&self, mine: &str, policy: VersionPolicy) -> (String, bool) {
        match self {
            VersionCheck::Same => (format!("the incumbent engine is v{mine}, the same as this one"), true),
            VersionCheck::Differs(v) => (
                format!(
                    "the incumbent engine is v{v} and this one is v{mine}: the handover relies on \
                     the incumbent's own stand-down, flush and exit, so an engine of another \
                     version (an initramfs older than its release) is a hazard"
                ),
                policy == VersionPolicy::Warn,
            ),
            VersionCheck::Unknown => (
                format!(
                    "the incumbent recorded no engine version (it predates #189, so it is older \
                     than this v{mine}): the handover relies on its own stand-down, flush and exit"
                ),
                policy == VersionPolicy::Warn,
            ),
        }
    }
}

impl Record {
    /// The volume names in device order, which is the order an adopting server
    /// must present them in.
    pub fn volumes_in_device_order(&self) -> Vec<String> {
        let mut d = self.devices.clone();
        d.sort_by_key(|e| e.dev_id);
        d.into_iter().map(|e| e.volume).collect()
    }

    /// Write it where the successor will look.
    ///
    /// Atomically, because the successor may start at any moment: a torn
    /// record would be worse than none, since none falls back to the explicit
    /// list and half a record does not.
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("encode handover record: {e}")))?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)
    }

    /// Read it, or `None` when there is none — which is not an error. A node
    /// where the devices were created by something that predates this record
    /// still adopts, from the explicit list.
    pub fn read(path: &std::path::Path) -> Option<Record> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// The order of a handover (#171): the incumbent is stood down and gone
/// **before** the successor reads anything from the slabs.
///
/// The other order — restore, then stand down — left a window in which the
/// incumbent kept serving: every slot it allocated there was missing from the
/// successor's map, looked free to it, and could be handed out again, and the
/// incumbent's shutdown rewrote slot-table sectors from its own, older copy.
/// Nothing showed until a restore after a power cut. The cost of this order
/// is that a successor that then fails to restore leaves the devices held in
/// recovery with no server: [`take_over_retrying`] is what pays it (#190).
pub async fn take_over<T, SD, SDF, R, RF>(stand_down: SD, restore: R) -> anyhow::Result<T>
where
    SD: FnOnce() -> SDF,
    SDF: std::future::Future<Output = anyhow::Result<()>>,
    R: FnOnce() -> RF,
    RF: std::future::Future<Output = anyhow::Result<T>>,
{
    stand_down().await?;
    restore().await
}

/// How long a restore after the stand-down is retried (#190).
#[derive(Debug, Clone, Copy)]
pub struct RestoreRetry {
    /// Give up once this much has passed since the first attempt.
    pub budget: std::time::Duration,
    /// The first wait; doubled after every failure, up to `max_wait`.
    pub first_wait: std::time::Duration,
    pub max_wait: std::time::Duration,
}

/// `STORMBLOCK_ADOPT_RESTORE_SECS`: seconds to keep retrying (default 120).
pub const RESTORE_SECS_ENV: &str = "STORMBLOCK_ADOPT_RESTORE_SECS";

impl RestoreRetry {
    /// The node's: two minutes (an appliance or a link coming back), waits
    /// from 1 s doubling to 15 s.
    pub fn from_env() -> Self {
        let secs = std::env::var(RESTORE_SECS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(120);
        RestoreRetry {
            budget: std::time::Duration::from_secs(secs),
            first_wait: std::time::Duration::from_secs(1),
            max_wait: std::time::Duration::from_secs(15),
        }
    }
}

/// Why a handover did not complete.
#[derive(Debug)]
pub enum TakeOverError {
    /// The incumbent was not stood down: it still serves, nothing is lost.
    StandDown(anyhow::Error),
    /// The incumbent is gone and every restore failed: the devices are held
    /// in recovery with no server until something adopts them.
    Restore { attempts: u32, last: anyhow::Error },
}

impl std::fmt::Display for TakeOverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TakeOverError::StandDown(e) => write!(f, "standing the incumbent down: {e:#}"),
            TakeOverError::Restore { attempts, last } => {
                write!(f, "restore failed {attempts} time(s) after the incumbent exited: {last:#}")
            }
        }
    }
}

impl std::error::Error for TakeOverError {}

/// [`take_over`], with the restore retried (#190). The incumbent is gone
/// once the stand-down returns, so a restore that fails leaves the node's
/// devices — its root among them — with no server. What fails there is
/// usually passing (the appliance's target, a link), so it is tried again
/// with backoff for `retry.budget`, every failure said through `report`
/// (attempt number, error, the wait before the next); after that the error
/// says so and the caller fails loudly.
pub async fn take_over_retrying<T, SD, SDF, R, RF>(
    stand_down: SD,
    mut restore: R,
    retry: RestoreRetry,
    mut report: impl FnMut(u32, &anyhow::Error, Option<std::time::Duration>),
) -> Result<T, TakeOverError>
where
    SD: FnOnce() -> SDF,
    SDF: std::future::Future<Output = anyhow::Result<()>>,
    R: FnMut() -> RF,
    RF: std::future::Future<Output = anyhow::Result<T>>,
{
    stand_down().await.map_err(TakeOverError::StandDown)?;
    let start = std::time::Instant::now();
    let mut wait = retry.first_wait;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match restore().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let left = retry.budget.saturating_sub(start.elapsed());
                if left.is_zero() {
                    report(attempts, &e, None);
                    return Err(TakeOverError::Restore { attempts, last: e });
                }
                let next = wait.min(left);
                report(attempts, &e, Some(next));
                tokio::time::sleep(next).await;
                wait = (wait * 2).min(retry.max_wait);
            }
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn quick(budget_ms: u64) -> RestoreRetry {
        RestoreRetry {
            budget: Duration::from_millis(budget_ms),
            first_wait: Duration::from_millis(5),
            max_wait: Duration::from_millis(20),
        }
    }

    /// #190: a restore that fails after the stand-down is tried again, and
    /// the incumbent is stood down once.
    #[tokio::test]
    async fn a_restore_that_fails_twice_is_retried_until_it_succeeds() {
        let downs = AtomicU32::new(0);
        let tries = AtomicU32::new(0);
        let mut reported = Vec::new();
        let got = take_over_retrying(
            || async {
                downs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || async {
                if tries.fetch_add(1, Ordering::SeqCst) < 2 {
                    anyhow::bail!("appliance not answering")
                }
                Ok(7)
            },
            quick(5_000),
            |n, _, next| reported.push((n, next.is_some())),
        )
        .await
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(downs.load(Ordering::SeqCst), 1, "stood down once");
        assert_eq!(tries.load(Ordering::SeqCst), 3);
        assert_eq!(reported, vec![(1, true), (2, true)], "every failure said, with a next try");
    }

    /// Past the budget it gives up and says how often it tried: the caller
    /// fails loudly with the devices still held.
    #[tokio::test]
    async fn a_restore_that_never_succeeds_gives_up_after_the_budget() {
        let t0 = std::time::Instant::now();
        let mut last = None;
        let e = take_over_retrying(
            || async { Ok(()) },
            || async { Err::<(), _>(anyhow::anyhow!("no slab")) },
            quick(100),
            |n, _, next| last = Some((n, next)),
        )
        .await
        .unwrap_err();
        let TakeOverError::Restore { attempts, last: why } = e else { panic!("{e}") };
        assert!(attempts >= 3, "retried: {attempts}");
        assert!(why.to_string().contains("no slab"));
        assert_eq!(last, Some((attempts, None)), "the last failure says there is no next try");
        assert!(t0.elapsed() >= Duration::from_millis(100));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    /// A stand-down that fails is not a restore failure: the incumbent still
    /// serves, and no restore is tried.
    #[tokio::test]
    async fn a_failed_stand_down_restores_nothing() {
        let tries = AtomicU32::new(0);
        let e = take_over_retrying(
            || async { anyhow::bail!("ublk control refused") },
            || async {
                tries.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            quick(100),
            |_, _, _| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(e, TakeOverError::StandDown(_)));
        assert_eq!(tries.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_record() -> Record {
        Record {
            slabs: vec!["/dev/sda4".into()],
            meta: None,
            flow_over: None,
            local_boot: None,
            install: None,
            installed: None,
            engine_version: None,
            devices: vec![
                Device { dev_id: 0, volume: "stormpump".into() },
                Device { dev_id: 2, volume: "sbregistry".into() },
                Device { dev_id: 1, volume: "stormblock".into() },
            ],
        }
    }

    /// #247: a secret handed this boot is kept; with none handed, the kept
    /// one comes back; both written 0600.
    #[test]
    fn a_host_secret_is_kept_across_boots() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (run, data) = (dir.path().join("run/host-secret.json"), dir.path().join("engine"));
        let a = HostSecret { appliance: "http://forge:9090".into(), host: "server1".into(), secret: "s-1".into() };
        assert_eq!(carry_host_secret(&run, &data), None, "nothing anywhere");
        a.write(&run).unwrap();
        assert_eq!(carry_host_secret(&run, &data), Some("kept"));
        assert_eq!(HostSecret::read(&data.join(HOST_SECRET_FILE)), Some(a.clone()));
        assert_eq!(carry_host_secret(&run, &data), None, "already kept");
        // A boot from the local disk: /run is fresh, the kept one returns.
        std::fs::remove_file(&run).unwrap();
        assert_eq!(carry_host_secret(&run, &data), Some("restored"));
        assert_eq!(HostSecret::read(&run), Some(a.clone()));
        assert_eq!(std::fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o600);
        // The next claim's secret replaces the kept one.
        let b = HostSecret { secret: "s-2".into(), ..a };
        b.write(&run).unwrap();
        assert_eq!(carry_host_secret(&run, &data), Some("kept"));
        assert_eq!(HostSecret::read(&data.join(HOST_SECRET_FILE)), Some(b));
    }

    /// #220: only a boot from the machine's own disk alone counts as the
    /// first boot off a laid disk.
    #[test]
    fn a_local_only_boot_is_one_with_no_claim_and_no_flow_over() {
        let r = a_record();
        assert!(local_only_boot(&r));
        let mut claimed = a_record();
        claimed.slabs = vec!["nvme-tcp://10.0.0.1:4420/nqn.x:host:server1?nsid=1".into()];
        assert!(!local_only_boot(&claimed), "a claimed clone");
        let mut mixed = a_record();
        mixed.slabs.push("iscsi://10.0.0.1:3260/iqn.x".into());
        assert!(!local_only_boot(&mixed), "any remote slab");
        let mut flowing = a_record();
        flowing.flow_over = Some(FlowOver { disk: "/dev/sda".into(), system_slab: "a".into(), data_slab: "b".into(), data_flow: false });
        assert!(!local_only_boot(&flowing), "the install boot itself");
        let mut installing = a_record();
        installing.install = Some(InstallTicket { boothost: "http://f".into(), host: "h".into(), volume: "v".into() });
        assert!(!local_only_boot(&installing), "an install in hand");
        let mut none = a_record();
        none.slabs.clear();
        assert!(!local_only_boot(&none), "no slabs at all");
    }

    #[test]
    fn volumes_come_back_in_device_order() {
        // Written in whatever order the exports were assembled; read back in
        // the order the kernel numbered them, because that is the order an
        // adopting server has to hand them over in.
        assert_eq!(
            a_record().volumes_in_device_order(),
            vec!["stormpump", "stormblock", "sbregistry"]
        );
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("sb-handover-{}", std::process::id()));
        let path = dir.join("handover.json");
        let rec = a_record();
        rec.write(&path).expect("writes");
        assert_eq!(Record::read(&path), Some(rec));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_without_a_flow_over_still_reads() {
        // Every record written before the field existed lacks it, and a node
        // mid-upgrade reads one of those with a binary that has it. Absence
        // has to mean "no local disk", not "unreadable record" — which would
        // send the successor to the explicit --volume list and stand down
        // every device it was supposed to adopt.
        let json = br#"{"slabs":["/dev/sda4"],"devices":[{"dev_id":0,"volume":"stormpump"}]}"#;
        let rec: Record = serde_json::from_slice(json).expect("reads without flow_over");
        assert_eq!(rec.flow_over, None);
        assert_eq!(rec.local_boot, None);
        assert_eq!(rec.install, None);
        assert_eq!(rec.volumes_in_device_order(), vec!["stormpump"]);
    }

    #[test]
    fn a_local_boot_round_trips() {
        let mut rec = a_record();
        rec.local_boot = Some("/dev/sda".into());
        let bytes = serde_json::to_vec(&rec).expect("encodes");
        assert_eq!(serde_json::from_slice::<Record>(&bytes).expect("decodes"), rec);
    }

    #[test]
    fn a_flow_over_round_trips() {
        let mut rec = a_record();
        rec.flow_over = Some(FlowOver {
            disk: "/dev/sda".into(),
            system_slab: "8aa6b985-3f4d-4130-99cb-154b56dcb68b".into(),
            data_slab: "f86ee673-57da-4aa5-961c-168c263de265".into(),
            data_flow: false,
        });
        let bytes = serde_json::to_vec(&rec).expect("encodes");
        assert_eq!(serde_json::from_slice::<Record>(&bytes).expect("decodes"), rec);
    }

    #[test]
    fn a_missing_record_is_not_an_error() {
        // The fallback is the explicit --volume list, so absence has to be
        // reported as absence rather than as a failure.
        assert_eq!(Record::read(std::path::Path::new("/nonexistent/handover.json")), None);
    }

    #[test]
    fn an_install_ticket_round_trips_on_its_own_and_in_the_record() {
        let t = InstallTicket {
            boothost: "http://forge:9090".into(),
            host: "server1".into(),
            volume: "8aa6b985-3f4d-4130-99cb-154b56dcb68b".into(),
        };
        let dir = std::env::temp_dir().join(format!("sb-ticket-{}", std::process::id()));
        let path = dir.join("install.json");
        t.write(&path).expect("writes");
        assert_eq!(InstallTicket::read(&path), Some(t.clone()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut rec = a_record();
        rec.install = Some(t);
        let bytes = serde_json::to_vec(&rec).expect("encodes");
        assert_eq!(serde_json::from_slice::<Record>(&bytes).expect("decodes"), rec);
    }

    /// #189: the record carries the incumbent's version; a mismatch, or a
    /// record with none, is said and refused only when asked.
    #[test]
    fn the_incumbents_version_is_recorded_and_compared() {
        let mut rec = a_record();
        // A record from before #189 still reads, and has no version.
        let old = serde_json::to_string(&rec).unwrap();
        assert!(!old.contains("engine_version"));
        let back: Record = serde_json::from_str(&old).unwrap();
        assert_eq!(check_version(Some(&back), "20.0.0"), VersionCheck::Unknown);
        assert_eq!(check_version(None, "20.0.0"), VersionCheck::Unknown);

        rec.engine_version = Some("16.1.0".into());
        let back: Record = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back.engine_version.as_deref(), Some("16.1.0"));
        let c = check_version(Some(&back), "20.0.0");
        assert_eq!(c, VersionCheck::Differs("16.1.0".into()));
        let (said, go) = c.verdict("20.0.0", VersionPolicy::Warn);
        assert!(go && said.contains("v16.1.0") && said.contains("v20.0.0"), "{said}");
        assert!(!c.verdict("20.0.0", VersionPolicy::Refuse).1);
        assert!(!VersionCheck::Unknown.verdict("20.0.0", VersionPolicy::Refuse).1);
        assert!(VersionCheck::Unknown.verdict("20.0.0", VersionPolicy::Warn).1);

        rec.engine_version = Some(ENGINE_VERSION.into());
        let c = check_version(Some(&rec), ENGINE_VERSION);
        assert_eq!(c, VersionCheck::Same);
        assert!(c.verdict(ENGINE_VERSION, VersionPolicy::Refuse).1, "the same version is never refused");

        assert_eq!("refuse".parse::<VersionPolicy>(), Ok(VersionPolicy::Refuse));
        assert_eq!("WARN".parse::<VersionPolicy>(), Ok(VersionPolicy::Warn));
        assert!("maybe".parse::<VersionPolicy>().is_err());
    }
}
