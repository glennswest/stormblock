//! Who holds the engine's locks, and who waits on them (#365, #364).
//!
//! The Dell's engine stalled for hours behind its volume manager and slab
//! registry, and the log could say only that they were HELD. The watchdog's
//! answer was a dump of every thread and task every few seconds, which
//! flooded the log path until the shipper dropped 21,191 lines, the lines
//! naming the holder among them.
//!
//! So the three engine-wide locks (the volume manager, the slab registry,
//! the extent map) are [`TrackedMutex`] and [`TrackedRwLock`]: the tokio
//! locks they wrap, with the same methods, whose guards record **who** holds
//! them (the activity: an API request by id and route, or a named background
//! task), since when, and who waits. Then:
//!
//! * a hold over [`HOLD_LOG`] is logged when it ends, with its holder and its
//!   length; a wait over [`HOLD_LOG`] likewise, with what it waited on;
//! * [`snapshot`] answers who holds and who waits right now, for
//!   `/debug/locks` and the watchdog's one-line stall summary;
//! * an API request's time spent waiting on locks is added up
//!   ([`Activity::lock_wait`]) and logged with the request.
//!
//! Every record is held by a guard and removed when the guard drops, so an
//! early return, an error, a panic or a cancelled request leaves nothing
//! behind (#364: nothing forgets a lock).

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// A hold or a wait longer than this is logged when it ends.
pub const HOLD_LOG: Duration = Duration::from_secs(1);
/// A hold longer than this is logged at WARN rather than INFO.
pub const HOLD_WARN: Duration = Duration::from_secs(10);

/// What the current task is doing, for naming it as a lock holder.
#[derive(Clone)]
pub struct Activity {
    pub name: Arc<str>,
    wait_us: Arc<AtomicU64>,
    /// An API request (#364): the shared volume manager's persists inside it
    /// are owed, and made by the request's middleware after every lock is
    /// released, before it answers.
    api: bool,
    owes_persist: Arc<std::sync::atomic::AtomicBool>,
}

impl Activity {
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Activity {
            name: name.into(),
            wait_us: Arc::new(AtomicU64::new(0)),
            api: false,
            owes_persist: Default::default(),
        }
    }
    /// An API request's activity (see `api`).
    pub fn request(name: impl Into<Arc<str>>) -> Self {
        Activity { api: true, ..Activity::new(name) }
    }
    /// Whether a persist was owed inside this activity (and clears it).
    pub fn take_owed_persist(&self) -> bool {
        self.owes_persist.swap(false, Ordering::SeqCst)
    }
    /// Time spent waiting on tracked locks inside this activity so far.
    pub fn lock_wait(&self) -> Duration {
        Duration::from_micros(self.wait_us.load(Ordering::Relaxed))
    }
}

tokio::task_local! {
    static ACTIVITY: Activity;
}

/// Run `fut` as `activity`: every tracked lock it takes is held in that name.
pub async fn scope<F: Future>(activity: Activity, fut: F) -> F::Output {
    ACTIVITY.scope(activity, fut).await
}

/// Run `fut` under `name` (a background task's).
pub async fn named<F: Future>(name: impl Into<Arc<str>>, fut: F) -> F::Output {
    ACTIVITY.scope(Activity::new(name), fut).await
}

/// `tokio::spawn`, keeping the spawner's activity name: a task a request
/// starts holds its locks in the request's name.
pub fn spawn_inheriting<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match ACTIVITY.try_with(|a| a.name.clone()) {
        Ok(name) => tokio::spawn(named(name, fut)),
        Err(_) => tokio::spawn(fut),
    }
}

/// [`spawn_inheriting`] for work a request starts that changes the shared
/// volume manager (#364): the task runs as a request, so its persists are
/// owed rather than made under the manager's lock, and `pay` makes the owed
/// persist once the work is done and has let go of every lock. A request
/// that awaits it answers after that; one that gave up does not stop it.
pub fn spawn_owing<F, P, PF>(fut: F, pay: P) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    P: FnOnce() -> PF + Send + 'static,
    PF: Future<Output = ()> + Send + 'static,
{
    let name = ACTIVITY.try_with(|a| a.name.clone()).unwrap_or_else(|_| "background work".into());
    let activity = Activity::request(name);
    tokio::spawn(async move {
        let out = scope(activity.clone(), fut).await;
        if activity.take_owed_persist() {
            scope(activity, pay()).await;
        }
        out
    })
}

/// The current task's name: its activity, else its tokio task id, else the
/// thread's name.
pub fn current_name() -> Arc<str> {
    if let Ok(n) = ACTIVITY.try_with(|a| a.name.clone()) {
        return n;
    }
    if let Some(id) = tokio::task::try_id() {
        return format!("task {id}").into();
    }
    std::thread::current().name().unwrap_or("a thread").to_string().into()
}

/// Inside an API request: note that the shared volume manager owes a
/// persist, to be made by the request's middleware once every lock is
/// released (#364). `false` outside one: persist now, as always.
pub fn owe_persist() -> bool {
    ACTIVITY
        .try_with(|a| {
            if a.api {
                a.owes_persist.store(true, Ordering::SeqCst);
            }
            a.api
        })
        .unwrap_or(false)
}

fn add_wait(d: Duration) {
    let _ = ACTIVITY.try_with(|a| a.wait_us.fetch_add(d.as_micros() as u64, Ordering::Relaxed));
}

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct Entry {
    id: u64,
    who: Arc<str>,
    since: Instant,
    write: bool,
    /// The tokio task that took it, for [`assert_not_held`].
    task: Option<tokio::task::Id>,
}

/// One lock's holders and waiters.
pub struct LockState {
    name: &'static str,
    holders: StdMutex<Vec<Entry>>,
    waiters: StdMutex<Vec<Entry>>,
}

fn all() -> &'static StdMutex<Vec<Weak<LockState>>> {
    static ALL: OnceLock<StdMutex<Vec<Weak<LockState>>>> = OnceLock::new();
    ALL.get_or_init(Default::default)
}

impl LockState {
    fn new(name: &'static str) -> Arc<Self> {
        let s = Arc::new(LockState { name, holders: Default::default(), waiters: Default::default() });
        let mut a = all().lock().unwrap_or_else(|e| e.into_inner());
        a.retain(|w| w.strong_count() > 0);
        a.push(Arc::downgrade(&s));
        s
    }

    fn wait(self: &Arc<Self>, write: bool) -> WaitMark {
        let e = Entry { id: NEXT.fetch_add(1, Ordering::Relaxed), who: current_name(), since: Instant::now(), write, task: tokio::task::try_id() };
        self.waiters.lock().unwrap_or_else(|p| p.into_inner()).push(e.clone());
        WaitMark { state: self.clone(), entry: e }
    }

    fn hold(self: &Arc<Self>, write: bool) -> HoldMark {
        let e = Entry { id: NEXT.fetch_add(1, Ordering::Relaxed), who: current_name(), since: Instant::now(), write, task: tokio::task::try_id() };
        self.holders.lock().unwrap_or_else(|p| p.into_inner()).push(e.clone());
        HoldMark { state: self.clone(), entry: e }
    }
}

fn mode(write: bool, rw: bool) -> &'static str {
    match (rw, write) {
        (false, _) => "",
        (true, true) => " (write)",
        (true, false) => " (read)",
    }
}

/// A waiter's record, removed when the wait ends however it ends (acquired,
/// or the waiting future dropped).
struct WaitMark {
    state: Arc<LockState>,
    entry: Entry,
}

impl WaitMark {
    fn done(self, rw: bool) {
        let waited = self.entry.since.elapsed();
        add_wait(waited);
        if waited >= HOLD_LOG {
            tracing::info!(
                "lock: {} waited {:.1}s for the {}{}",
                self.entry.who,
                waited.as_secs_f64(),
                self.state.name,
                mode(self.entry.write, rw)
            );
        }
    }
}

impl Drop for WaitMark {
    fn drop(&mut self) {
        let mut w = self.state.waiters.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = w.iter().position(|e| e.id == self.entry.id) {
            w.remove(i);
        }
    }
}

/// A holder's record, removed when its guard drops.
struct HoldMark {
    state: Arc<LockState>,
    entry: Entry,
}

impl HoldMark {
    fn release(&self, rw: bool) {
        let held = self.entry.since.elapsed();
        if held >= HOLD_WARN {
            tracing::warn!(
                "lock: the {}{} was held {:.1}s by {}",
                self.state.name,
                mode(self.entry.write, rw),
                held.as_secs_f64(),
                self.entry.who
            );
        } else if held >= HOLD_LOG {
            tracing::info!(
                "lock: the {}{} was held {:.1}s by {}",
                self.state.name,
                mode(self.entry.write, rw),
                held.as_secs_f64(),
                self.entry.who
            );
        }
    }
}

impl Drop for HoldMark {
    fn drop(&mut self) {
        let mut h = self.state.holders.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = h.iter().position(|e| e.id == self.entry.id) {
            h.remove(i);
        }
    }
}

/// The short name of a lock over `T`: the three engine-wide ones by what
/// they are, anything else by its type.
fn name_of<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    match full.rsplit("::").next().unwrap_or(full) {
        "VolumeManager" => "volume manager",
        "SlabRegistry" => "slab registry",
        "GlobalExtentMap" => "extent map",
        _ => full,
    }
}

// ── Mutex ─────────────────────────────────────────────────────────────────

/// A `tokio::sync::Mutex` whose holder and waiters are known.
pub struct TrackedMutex<T> {
    inner: Arc<tokio::sync::Mutex<T>>,
    state: Arc<LockState>,
}

impl<T> TrackedMutex<T> {
    pub fn new(value: T) -> Self {
        TrackedMutex { inner: Arc::new(tokio::sync::Mutex::new(value)), state: LockState::new(name_of::<T>()) }
    }

    pub async fn lock(&self) -> TrackedMutexGuard<'_, T> {
        let wait = self.state.wait(true);
        let guard = self.inner.lock().await;
        wait.done(false);
        TrackedMutexGuard { guard, hold: self.state.hold(true) }
    }

    pub fn try_lock(&self) -> Result<TrackedMutexGuard<'_, T>, tokio::sync::TryLockError> {
        let guard = self.inner.try_lock()?;
        Ok(TrackedMutexGuard { guard, hold: self.state.hold(true) })
    }

    pub async fn lock_owned(self: Arc<Self>) -> OwnedTrackedMutexGuard<T> {
        let wait = self.state.wait(true);
        let guard = self.inner.clone().lock_owned().await;
        wait.done(false);
        OwnedTrackedMutexGuard { guard, hold: self.state.hold(true) }
    }
}

pub struct TrackedMutexGuard<'a, T> {
    guard: tokio::sync::MutexGuard<'a, T>,
    hold: HoldMark,
}

impl<T> Drop for TrackedMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.hold.release(false);
    }
}

impl<T> Deref for TrackedMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

pub struct OwnedTrackedMutexGuard<T> {
    guard: tokio::sync::OwnedMutexGuard<T>,
    hold: HoldMark,
}

impl<T> Drop for OwnedTrackedMutexGuard<T> {
    fn drop(&mut self) {
        self.hold.release(false);
    }
}

impl<T> Deref for OwnedTrackedMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for OwnedTrackedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

// ── RwLock ────────────────────────────────────────────────────────────────

/// A `tokio::sync::RwLock` whose holders (readers and the writer) and
/// waiters are known.
pub struct TrackedRwLock<T> {
    inner: tokio::sync::RwLock<T>,
    state: Arc<LockState>,
}

impl<T> TrackedRwLock<T> {
    pub fn new(value: T) -> Self {
        TrackedRwLock { inner: tokio::sync::RwLock::new(value), state: LockState::new(name_of::<T>()) }
    }

    pub async fn read(&self) -> TrackedReadGuard<'_, T> {
        let wait = self.state.wait(false);
        let guard = self.inner.read().await;
        wait.done(true);
        TrackedReadGuard { guard, hold: self.state.hold(false) }
    }

    pub async fn write(&self) -> TrackedWriteGuard<'_, T> {
        let wait = self.state.wait(true);
        let guard = self.inner.write().await;
        wait.done(true);
        TrackedWriteGuard { guard, hold: self.state.hold(true) }
    }

    pub fn try_read(&self) -> Result<TrackedReadGuard<'_, T>, tokio::sync::TryLockError> {
        let guard = self.inner.try_read()?;
        Ok(TrackedReadGuard { guard, hold: self.state.hold(false) })
    }

    pub fn try_write(&self) -> Result<TrackedWriteGuard<'_, T>, tokio::sync::TryLockError> {
        let guard = self.inner.try_write()?;
        Ok(TrackedWriteGuard { guard, hold: self.state.hold(true) })
    }
}

pub struct TrackedReadGuard<'a, T> {
    guard: tokio::sync::RwLockReadGuard<'a, T>,
    hold: HoldMark,
}

impl<T> Drop for TrackedReadGuard<'_, T> {
    fn drop(&mut self) {
        self.hold.release(true);
    }
}

impl<T> Deref for TrackedReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

pub struct TrackedWriteGuard<'a, T> {
    guard: tokio::sync::RwLockWriteGuard<'a, T>,
    hold: HoldMark,
}

impl<T> Drop for TrackedWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.hold.release(true);
    }
}

impl<T> Deref for TrackedWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

// ── Nothing held across I/O ──────────────────────────────────────────────

/// Does the current task hold the lock called `lock` (`"volume manager"`)?
pub fn current_task_holds(lock: &str) -> bool {
    let Some(me) = tokio::task::try_id() else { return false };
    let states: Vec<Arc<LockState>> = {
        let a = all().lock().unwrap_or_else(|e| e.into_inner());
        a.iter().filter_map(|w| w.upgrade()).collect()
    };
    states.iter().filter(|s| s.name == lock).any(|s| {
        s.holders.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|e| e.task == Some(me))
    })
}

/// The rule of #364: the volume manager's lock is never held across I/O.
/// Called where the engine flushes a device: a holder there is reported, at
/// WARN once a minute, and with `STORMBLOCK_LOCK_ASSERT=1` (the #364 tests)
/// is a panic, so a path that breaks the rule fails its test.
pub fn assert_not_held(lock: &str, across: &str) {
    if !current_task_holds(lock) {
        return;
    }
    let strict = std::env::var_os("STORMBLOCK_LOCK_ASSERT").as_deref() == Some(std::ffi::OsStr::new("1"));
    let msg = format!("lock: the {lock} is held by {} across {across} (#364)", current_name());
    if strict {
        panic!("{msg}");
    }
    static LAST: StdMutex<Option<Instant>> = StdMutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.map(|t| t.elapsed() >= Duration::from_secs(60)).unwrap_or(true) {
        *last = Some(Instant::now());
        tracing::warn!("{msg}");
    }
}

// ── What is held now ──────────────────────────────────────────────────────

/// One holder or waiter, as reported.
#[derive(Debug, Clone)]
pub struct Party {
    pub who: Arc<str>,
    pub secs: f64,
    pub write: bool,
}

/// One lock's holders and waiters right now, longest first.
#[derive(Debug, Clone)]
pub struct LockView {
    pub lock: &'static str,
    pub holders: Vec<Party>,
    pub waiters: Vec<Party>,
}

/// Every tracked lock, now.
pub fn snapshot() -> Vec<LockView> {
    let states: Vec<Arc<LockState>> = {
        let a = all().lock().unwrap_or_else(|e| e.into_inner());
        a.iter().filter_map(|w| w.upgrade()).collect()
    };
    let view = |v: &StdMutex<Vec<Entry>>| -> Vec<Party> {
        let mut p: Vec<Party> = v
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| Party { who: e.who.clone(), secs: e.since.elapsed().as_secs_f64(), write: e.write })
            .collect();
        p.sort_by(|a, b| b.secs.total_cmp(&a.secs));
        p
    };
    states
        .iter()
        .map(|s| LockView { lock: s.name, holders: view(&s.holders), waiters: view(&s.waiters) })
        .collect()
}

/// `snapshot` as text: one line per lock that is held or waited on, naming
/// its oldest holder and how many wait. Names routes and task names, never
/// volume data.
pub fn summary() -> String {
    let mut out = String::new();
    for v in snapshot() {
        if v.holders.is_empty() && v.waiters.is_empty() {
            continue;
        }
        out.push_str(&line(&v));
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("locks: none held\n");
    }
    out
}

/// One lock's line: `the volume manager: held 41.2s by POST … (req 39); 6
/// waiting, the longest 40.1s (GET … (req 42))`.
pub fn line(v: &LockView) -> String {
    let held = match v.holders.first() {
        Some(h) if v.holders.len() == 1 => format!("held {:.1}s by {}{}", h.secs, h.who, if h.write { " (write)" } else { "" }),
        Some(h) => format!(
            "held by {} ({} readers), the oldest {:.1}s by {}",
            v.holders.len(),
            v.holders.iter().filter(|p| !p.write).count(),
            h.secs,
            h.who
        ),
        None => "free".to_string(),
    };
    let waiting = match v.waiters.first() {
        Some(w) => format!("; {} waiting, the longest {:.1}s ({})", v.waiters.len(), w.secs, w.who),
        None => String::new(),
    };
    format!("the {}: {held}{waiting}", v.lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VolumeManager;

    #[tokio::test]
    async fn a_holder_is_named_and_gone_when_its_guard_drops() {
        let m = Arc::new(TrackedMutex::new(VolumeManager));
        let g = named("POST /api/v1/volumes/x/clone (req 39)", async { m.lock().await }).await;
        let held = |s: &[LockView]| -> Vec<String> {
            s.iter().filter(|v| v.lock == "volume manager").flat_map(|v| v.holders.iter().map(|h| h.who.to_string())).collect()
        };
        assert_eq!(held(&snapshot()), vec!["POST /api/v1/volumes/x/clone (req 39)".to_string()]);
        // A waiter is listed while it waits, and gone when it is dropped
        // (a request whose client went away).
        let m2 = m.clone();
        let waiter = tokio::spawn(named("GET /api/v1/volumes (req 42)", async move { m2.lock().await; }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let v = snapshot().into_iter().find(|v| v.lock == "volume manager").unwrap();
        assert_eq!(v.waiters.len(), 1);
        assert!(line(&v).contains("held") && line(&v).contains("1 waiting") && line(&v).contains("req 42"), "{}", line(&v));
        waiter.abort();
        let _ = waiter.await;
        assert!(snapshot().iter().find(|v| v.lock == "volume manager").unwrap().waiters.is_empty());
        drop(g);
        assert!(held(&snapshot()).is_empty(), "a dropped guard leaves no holder");
    }

    #[tokio::test]
    async fn a_panic_while_holding_leaves_the_lock_free_and_unclaimed() {
        let m = Arc::new(TrackedMutex::new(VolumeManager));
        let m2 = m.clone();
        let r = tokio::spawn(async move {
            let _g = m2.lock().await;
            panic!("an operation fails halfway");
        })
        .await;
        assert!(r.is_err());
        assert!(m.try_lock().is_ok(), "the next caller proceeds at once");
        let v = snapshot().into_iter().find(|v| v.lock == "volume manager");
        assert!(v.map(|v| v.holders.is_empty()).unwrap_or(true));
    }

    /// Nothing exits holding a lock (#364): a holder cancelled while it
    /// waits on I/O (a request whose client went away, a job aborted), and
    /// one that returns an error halfway, leave the lock free and no holder
    /// named.
    #[tokio::test]
    async fn a_holder_cancelled_mid_io_or_failing_leaves_the_lock_free_and_unclaimed() {
        let m = Arc::new(TrackedMutex::new(VolumeManager));
        let unclaimed = || {
            snapshot()
                .into_iter()
                .filter(|v| v.lock == "volume manager")
                .all(|v| v.holders.iter().all(|h| !h.who.contains("cancelled") && !h.who.contains("failing")))
        };

        let m2 = m.clone();
        let t = tokio::spawn(named("cancelled holder", async move {
            let _g = m2.lock().await;
            tokio::time::sleep(Duration::from_secs(3600)).await; // the I/O that never answers
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(m.try_lock().is_err(), "held while it waits");
        t.abort();
        let _ = t.await;
        assert!(m.try_lock().is_ok(), "free once cancelled");
        assert!(unclaimed());

        async fn halfway(m: &TrackedMutex<VolumeManager>) -> Result<(), &'static str> {
            let _g = m.lock().await;
            tokio::task::yield_now().await;
            Err("the device said no")?;
            Ok(())
        }
        assert!(named("failing holder", halfway(&m)).await.is_err());
        assert!(m.try_lock().is_ok(), "free after an error");
        assert!(unclaimed());
    }

    #[tokio::test]
    async fn lock_wait_is_added_up_for_the_activity() {
        let m = Arc::new(TrackedMutex::new(VolumeManager));
        let g = m.lock().await;
        let a = Activity::new("GET /api/v1/fstemplates (req 7)");
        let a2 = a.clone();
        let m2 = m.clone();
        let t = tokio::spawn(scope(a2, async move {
            let _g = m2.lock().await;
        }));
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(g);
        t.await.unwrap();
        assert!(a.lock_wait() >= Duration::from_millis(100), "{:?}", a.lock_wait());
    }
}
