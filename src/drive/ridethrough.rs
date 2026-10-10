//! Riding through a controller (HBA) reset on a local drive (#391).
//!
//! A node's system and data slabs can share a SAS HBA with a whole shelf
//! (the Dell: `sda` and the NetApp, on one mpt3sas). When that HBA resets
//! (SCSI error handling escalating to a host reset, because a shelf drive is
//! formatting, failing or being pulled), the node's own disk vanishes for
//! seconds and I/O comes back failed: a transport error, not a medium error.
//! Passing that up fails the volume, ublk answers EIO, and the root
//! filesystem and etcd see it. That is what kills a node when a shelf on the
//! same controller misbehaves.
//!
//! [`RideThrough`] wraps every local block device. An I/O that fails with a
//! **transport-class** error is retried, with backoff, for a bounded window
//! (60 s by default, `STORMBLOCK_TRANSPORT_WINDOW_SECS`; `0` turns it off).
//! Callers simply wait: a ublk request is a task of its own (#264), so the
//! kernel sees a slow request, not an error. A **medium** or **protection**
//! error (`ENODATA`, `EILSEQ`) is the media speaking and goes up at once.
//!
//! While a drive is being ridden through it is listed (`stalls()`, health's
//! `drives_unreachable`); when it answers again that is said with how long it
//! took. When the window runs out the error goes up as before (a redundant
//! volume's leg fails, an unreplicated volume answers the error) and the
//! drive stays listed as given up, with an ERROR line: loud, never a node
//! that quietly lost its disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// The default window: a host reset with a slow re-scan is tens of seconds.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
const FIRST_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// The ride-through window from the environment: `None` when turned off.
pub fn window() -> Option<Duration> {
    match std::env::var("STORMBLOCK_TRANSPORT_WINDOW_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(s)),
        None => Some(DEFAULT_WINDOW),
    }
}

/// Whether an error is the path to the drive rather than the drive's media:
/// what a controller reset, a dropped link or an offlined device returns.
pub fn is_transport_error(e: &DriveError) -> bool {
    match e {
        DriveError::DeviceNotReady => true,
        DriveError::Io(io) => match io.raw_os_error() {
            Some(code) => matches!(
                code,
                libc::EIO
                    | libc::ENXIO
                    | libc::EAGAIN
                    | libc::EBUSY
                    | libc::ENODEV
                    | libc::ENOLINK
                    | libc::ETIMEDOUT
                    | libc::EREMOTEIO
                    | libc::ESHUTDOWN
                    | libc::ECONNRESET
            ),
            None => false,
        },
        _ => false,
    }
}

/// A drive being ridden through, or given up on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stall {
    pub path: String,
    /// Unix seconds of the first failed I/O.
    pub since: u64,
    pub last_error: String,
    pub retries: u64,
    /// The window ran out: errors go up now, until the drive answers.
    pub gave_up: bool,
    /// When the drive stopped answering: the window runs from here, for
    /// every I/O, not from each I/O's own start.
    #[serde(skip)]
    pub(crate) started: Option<Instant>,
}

fn stalls_map() -> &'static Mutex<HashMap<String, Stall>> {
    static M: OnceLock<Mutex<HashMap<String, Stall>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every drive being ridden through or given up on, for health.
pub fn stalls() -> Vec<Stall> {
    let mut v: Vec<Stall> = stalls_map().lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// [`RideThrough::wrap`] for a boxed device.
pub fn wrap_box(dev: Box<dyn BlockDevice>) -> Box<dyn BlockDevice> {
    match window() {
        Some(w) => Box::new(RideThrough::new(Arc::from(dev), w)),
        None => dev,
    }
}

/// A local drive that rides through transport errors (see the module doc).
pub struct RideThrough {
    inner: Arc<dyn BlockDevice>,
    window: Duration,
}

impl RideThrough {
    pub fn new(inner: Arc<dyn BlockDevice>, window: Duration) -> Self {
        RideThrough { inner, window }
    }

    /// Wrap `dev` with the environment's window, or hand it back as it is
    /// when the ride-through is turned off.
    pub fn wrap(dev: Arc<dyn BlockDevice>) -> Arc<dyn BlockDevice> {
        match window() {
            Some(w) => Arc::new(RideThrough::new(dev, w)),
            None => dev,
        }
    }

    fn path(&self) -> String {
        self.inner.id().path.clone()
    }

    /// One failed attempt: `Ok(backoff)` to try again after it, or `Err` to
    /// give up (outside the window, or not a transport error).
    fn on_error(&self, op: &str, offset: u64, e: DriveError, started: &mut Option<Instant>, backoff: &mut Duration) -> Result<Duration, DriveError> {
        if !is_transport_error(&e) {
            return Err(e);
        }
        let path = self.path();
        let first = started.is_none();
        started.get_or_insert_with(Instant::now);
        metrics::counter!("stormblock_transport_retries_total", "drive" => path.clone()).increment(1);
        let mut map = stalls_map().lock().unwrap_or_else(|p| p.into_inner());
        let st = map.entry(path.clone()).or_insert_with(|| Stall {
            path: path.clone(),
            since: now_secs(),
            last_error: String::new(),
            retries: 0,
            gave_up: false,
            started: Some(Instant::now()),
        });
        st.last_error = format!("{op} at {offset}: {e}");
        st.retries += 1;
        // Given up already: each I/O tries once and fails at once, so a
        // drive that is gone costs no caller the window again.
        if st.gave_up {
            return Err(e);
        }
        let waited = st.started.map(|t| t.elapsed()).unwrap_or_default();
        if waited >= self.window {
            if !st.gave_up {
                tracing::error!(
                    "{path}: no answer for {}s ({op} at {offset}: {e}) — the drive is unreachable; its I/O fails \
                     from now on until it answers. Its controller may have been reset and not come back (#391)",
                    waited.as_secs()
                );
            }
            st.gave_up = true;
            return Err(e);
        }
        if first && st.retries == 1 {
            tracing::warn!(
                "{path}: {op} at {offset} failed with {e} — riding through (a controller reset?), retrying for up to {}s (#391)",
                self.window.as_secs()
            );
        }
        let wait = *backoff;
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
        Ok(wait)
    }

    /// The I/O went through: the drive is no longer listed, and how long it
    /// was out is said.
    fn on_success(&self, started: Option<Instant>) {
        let removed = stalls_map().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.path());
        if let Some(st) = removed {
            let took = started.map(|t| t.elapsed().as_millis()).unwrap_or(0);
            tracing::warn!(
                "{}: answering again after {} retries ({} ms on this I/O; first failure at {}, last: {})",
                st.path, st.retries, took, st.since, st.last_error
            );
        }
    }
}

#[async_trait]
impl BlockDevice for RideThrough {
    fn id(&self) -> &DeviceId {
        self.inner.id()
    }
    fn drive_id(&self) -> DeviceId {
        self.inner.drive_id()
    }
    fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }
    fn discard_granularity(&self) -> u32 {
        self.inner.discard_granularity()
    }
    fn device_type(&self) -> DriveType {
        self.inner.device_type()
    }
    fn smart_status(&self) -> DriveResult<SmartData> {
        self.inner.smart_status()
    }
    fn media_errors(&self) -> u64 {
        self.inner.media_errors()
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let (mut started, mut backoff) = (None, FIRST_BACKOFF);
        loop {
            match self.inner.read(offset, buf).await {
                Ok(n) => {
                    if started.is_some() {
                        self.on_success(started);
                    }
                    return Ok(n);
                }
                Err(e) => tokio::time::sleep(self.on_error("read", offset, e, &mut started, &mut backoff)?).await,
            }
        }
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let (mut started, mut backoff) = (None, FIRST_BACKOFF);
        loop {
            match self.inner.write(offset, buf).await {
                Ok(n) => {
                    if started.is_some() {
                        self.on_success(started);
                    }
                    return Ok(n);
                }
                Err(e) => tokio::time::sleep(self.on_error("write", offset, e, &mut started, &mut backoff)?).await,
            }
        }
    }

    async fn flush(&self) -> DriveResult<()> {
        let (mut started, mut backoff) = (None, FIRST_BACKOFF);
        loop {
            match self.inner.flush().await {
                Ok(()) => {
                    if started.is_some() {
                        self.on_success(started);
                    }
                    return Ok(());
                }
                Err(e) => tokio::time::sleep(self.on_error("flush", 0, e, &mut started, &mut backoff)?).await,
            }
        }
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let (mut started, mut backoff) = (None, FIRST_BACKOFF);
        loop {
            match self.inner.discard(offset, len).await {
                Ok(()) => {
                    if started.is_some() {
                        self.on_success(started);
                    }
                    return Ok(());
                }
                Err(e) => tokio::time::sleep(self.on_error("discard", offset, e, &mut started, &mut backoff)?).await,
            }
        }
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> DriveResult<()> {
        let (mut started, mut backoff) = (None, FIRST_BACKOFF);
        loop {
            match self.inner.write_zeroes(offset, len).await {
                Ok(()) => {
                    if started.is_some() {
                        self.on_success(started);
                    }
                    return Ok(());
                }
                Err(e) => tokio::time::sleep(self.on_error("write-zeroes", offset, e, &mut started, &mut backoff)?).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// A drive that fails its next `n` I/Os with `errno`, then answers.
    struct Flaky {
        id: DeviceId,
        failures: AtomicU32,
        errno: i32,
        data: Mutex<Vec<u8>>,
        calls: AtomicU64,
    }

    impl Flaky {
        fn new(path: &str, failures: u32, errno: i32) -> Arc<Flaky> {
            Arc::new(Flaky {
                id: DeviceId { uuid: uuid::Uuid::new_v4(), serial: "F".into(), model: "flaky".into(), path: path.into(), wwn: String::new() },
                failures: AtomicU32::new(failures),
                errno,
                data: Mutex::new(vec![0u8; 1 << 20]),
                calls: AtomicU64::new(0),
            })
        }
        fn fail(&self) -> Option<DriveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let left = self.failures.load(Ordering::SeqCst);
            if left > 0 {
                self.failures.store(left - 1, Ordering::SeqCst);
                return Some(DriveError::Io(std::io::Error::from_raw_os_error(self.errno)));
            }
            None
        }
    }

    #[async_trait]
    impl BlockDevice for Flaky {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn capacity_bytes(&self) -> u64 {
            1 << 20
        }
        fn block_size(&self) -> u32 {
            512
        }
        fn optimal_io_size(&self) -> u32 {
            4096
        }
        fn device_type(&self) -> DriveType {
            DriveType::File
        }
        async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
            if let Some(e) = self.fail() {
                return Err(e);
            }
            let d = self.data.lock().unwrap();
            buf.copy_from_slice(&d[offset as usize..offset as usize + buf.len()]);
            Ok(buf.len())
        }
        async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
            if let Some(e) = self.fail() {
                return Err(e);
            }
            self.data.lock().unwrap()[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }
        async fn flush(&self) -> DriveResult<()> {
            match self.fail() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
            Ok(())
        }
    }

    /// A controller reset: the I/O fails a few times with transport errors,
    /// then goes through. The caller sees only a slow write, and the drive
    /// is listed while it is out and not after.
    #[tokio::test]
    async fn a_transport_error_is_ridden_through_and_the_caller_never_sees_it() {
        for errno in [libc::EIO, libc::ENOLINK, libc::ENXIO, libc::ETIMEDOUT] {
            let path = format!("/dev/ride-{errno}");
            let flaky = Flaky::new(&path, 4, errno);
            let dev = RideThrough::new(flaky.clone(), Duration::from_secs(30));
            dev.write(4096, &[7u8; 4096]).await.expect("the write goes through once the drive answers");
            assert_eq!(flaky.calls.load(Ordering::SeqCst), 5, "errno {errno}: four failures and the attempt that answered");
            let mut back = vec![0u8; 4096];
            dev.read(4096, &mut back).await.unwrap();
            assert_eq!(back, vec![7u8; 4096]);
            dev.flush().await.unwrap();
            assert!(!stalls().iter().any(|s| s.path == path), "answering again, it is no longer listed");
        }
    }

    /// The media speaking (a medium or protection error) goes up at once.
    #[tokio::test]
    async fn a_medium_error_is_not_retried() {
        for errno in [libc::ENODATA, libc::EILSEQ, libc::EINVAL] {
            let flaky = Flaky::new(&format!("/dev/medium-{errno}"), 1, errno);
            let dev = RideThrough::new(flaky.clone(), Duration::from_secs(30));
            assert!(dev.write(0, &[1u8; 512]).await.is_err(), "errno {errno}");
            assert_eq!(flaky.calls.load(Ordering::SeqCst), 1, "errno {errno}: not retried");
        }
    }

    /// A drive that does not come back within the window: the error goes up,
    /// the drive is listed as given up; when it answers again it is cleared.
    #[tokio::test]
    async fn a_drive_gone_past_the_window_fails_loudly_and_clears_when_back() {
        let path = "/dev/gone-past-window";
        let flaky = Flaky::new(path, u32::MAX, libc::ENOLINK);
        let dev = RideThrough::new(flaky.clone(), Duration::from_millis(500));
        let t = Instant::now();
        assert!(dev.write(0, &[1u8; 512]).await.is_err());
        assert!(t.elapsed() >= Duration::from_millis(500), "it waited for the window");
        let st = stalls().into_iter().find(|s| s.path == path).expect("listed");
        assert!(st.gave_up && st.retries > 1, "{st:?}");
        flaky.failures.store(0, Ordering::SeqCst);
        dev.write(0, &[1u8; 512]).await.unwrap();
        assert!(!stalls().iter().any(|s| s.path == path), "cleared once it answers");
    }
}
