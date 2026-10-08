//! What the engine is doing when its API stops answering (#269).
//!
//! A node has no ssh, and on server3 (11.79) the API stopped answering during
//! a flow-over with nothing in the log to say why. This is the evidence:
//!
//! * **A watchdog**, on an OS thread of its own so that it works when the
//!   async runtime is the thing that is stuck. Every request the API is
//!   serving is registered; one that has waited more than
//!   [`STALL_AFTER`] is logged with what the engine was doing then (below),
//!   and kept for `GET /debug/stalls`. The watchdog also notices a runtime
//!   that has stopped running tasks at all (its heartbeat goes stale).
//! * **`GET /debug/tasks`**: every async task of the API's runtime, with the
//!   `.await` it is parked on — a tokio task dump. Needs `--cfg
//!   tokio_unstable` (set in `.cargo/config.toml`); a build without it says
//!   so. The ublk devices' current-thread runtimes are named but never dumped:
//!   tracing one while it serves I/O re-enters it and panics it (#334).
//! * **`GET /debug/threads`**: every OS thread, its state and kernel stack:
//!   what a thread blocked in a syscall (a flush, an io_uring wait) is
//!   waiting for.
//! * **`GET /debug/locks`**: whether the volume manager, the extent map and
//!   the slab registry are held right now.
//!
//! All of it is read-only and carries no volume data, and it is open like
//! `/api/v1/health`: the supervisor asking holds no node token. Open does not
//! mean all of it (#283). Without the node or admin token (or a node-CA
//! client certificate):
//!
//! * a request in flight is its method, its age and its route family
//!   (`/api/v1/volumes/…`): never a volume id, a name or a boothost tag;
//! * a remote slab's flushes are named by transport, not by the URI that
//!   attaching it takes;
//! * the watchdog's reports are given as their open summary;
//! * `/debug/threads` has no kernel stacks.
//!
//! A task dump pauses the runtime it traces, so `/debug/tasks` makes one at a
//! time and answers from it for [`TASKS_FRESH`]: asking in a loop costs one
//! dump every few seconds, not one per request.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::mgmt::AppState;

/// A request waiting longer than this is a stall.
pub const STALL_AFTER: Duration = Duration::from_secs(10);
/// A stall still going is logged again this often.
const REPORT_AGAIN: Duration = Duration::from_secs(30);
/// At most one full capture (tasks, threads) per this interval.
const CAPTURE_EVERY: Duration = Duration::from_secs(60);
/// How many watchdog reports are kept for `/debug/stalls`.
const KEEP_REPORTS: usize = 8;
/// How long one task dump answers `/debug/tasks` (#283).
pub const TASKS_FRESH: Duration = Duration::from_secs(5);

struct InFlight {
    method: String,
    path: String,
    since: Instant,
    reported: Option<Instant>,
}

fn start_instant() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn inflight() -> &'static Mutex<HashMap<u64, InFlight>> {
    static M: OnceLock<Mutex<HashMap<u64, InFlight>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// A watchdog report: the open summary and the whole of it (#283).
struct Report {
    open: String,
    full: String,
}

fn reports() -> &'static Mutex<VecDeque<Report>> {
    static R: OnceLock<Mutex<VecDeque<Report>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn runtimes() -> &'static Mutex<Vec<(String, tokio::runtime::Handle)>> {
    static R: OnceLock<Mutex<Vec<(String, tokio::runtime::Handle)>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Milliseconds since start, last time the API's runtime ran the heartbeat.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn since_start_ms() -> u64 {
    start_instant().elapsed().as_millis() as u64
}

/// Name a runtime, so `/debug/tasks` and the watchdog dump its tasks too.
pub fn register_runtime(name: impl Into<String>, handle: tokio::runtime::Handle) {
    let mut r = runtimes().lock().unwrap_or_else(|e| e.into_inner());
    // A runtime that has gone (an export torn down) is dropped from the list
    // when a dump finds it closed; the list stays small.
    r.push((name.into(), handle));
}

struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        inflight().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// Middleware: every request is registered while it is being served. A
/// request a client gave up on is removed when its future is dropped.
pub async fn track(req: Request, next: Next) -> Response {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    inflight().lock().unwrap_or_else(|e| e.into_inner()).insert(
        id,
        InFlight {
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            since: Instant::now(),
            reported: None,
        },
    );
    let _guard = Registered(id);
    next.run(req).await
}

/// The open diagnostic routes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/debug/stalls", get(stalls))
        .route("/debug/tasks", get(tasks))
        .route(
            "/debug/threads",
            get(|full: Option<axum::Extension<crate::mgmt::auth::FullView>>| async move { text(threads_view(full.is_some())) }),
        )
        .route("/debug/locks", get(locks_route))
        .with_state(state)
}

fn text(body: String) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

async fn stalls(full: Option<axum::Extension<crate::mgmt::auth::FullView>>) -> Response {
    let full = full.is_some();
    let mut out = String::new();
    if !full {
        out.push_str("open view: paths, remote devices and stacks need the node token (#283)\n");
    }
    out.push_str(&in_flight_view(Duration::ZERO, full));
    out.push_str(&crate::drive::flushgate::summary_view(Duration::from_secs(300), full));
    out.push_str(&shingled_report());
    let r = reports().lock().unwrap_or_else(|e| e.into_inner());
    out.push_str(&format!("\n{} watchdog report(s) kept, newest last\n", r.len()));
    for rep in r.iter() {
        out.push_str("\n");
        out.push_str(if full { &rep.full } else { &rep.open });
    }
    text(out)
}

/// One task dump at a time, answering for [`TASKS_FRESH`] (#283): a dump
/// pauses the runtime it traces, and the route is open.
async fn tasks() -> Response {
    static LAST: tokio::sync::Mutex<Option<(Instant, String)>> = tokio::sync::Mutex::const_new(None);
    // Callers queue here while one dump runs, and are answered by it.
    let mut last = LAST.lock().await;
    if let Some((at, dump)) = last.as_ref() {
        if at.elapsed() < TASKS_FRESH {
            return text(format!("(taken {:.1}s ago)\n{dump}", at.elapsed().as_secs_f64()));
        }
    }
    let dump = task_dump(Duration::from_secs(10)).await;
    *last = Some((Instant::now(), dump.clone()));
    TASK_DUMPS.fetch_add(1, Ordering::Relaxed);
    text(dump)
}

/// Task dumps `/debug/tasks` has taken (tests).
pub static TASK_DUMPS: AtomicU64 = AtomicU64::new(0);

/// A path as the open view shows it (#283): the API prefix and the resource,
/// nothing after — `/api/v1/volumes/…`, never an id, a name or a tag.
pub fn route_family(path: &str) -> String {
    const PREFIX: &[&str] = &["api", "apis", "serve", "mk", "v1", "storage.storm.io", "debug"];
    let mut out = String::new();
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    for seg in segs.by_ref() {
        out.push('/');
        out.push_str(seg);
        if !PREFIX.contains(&seg) {
            break;
        }
    }
    if segs.next().is_some() {
        out.push_str("/…");
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

async fn locks_route(State(state): State<Arc<AppState>>) -> Response {
    text(locks(&state))
}

/// The zoned or drive-managed SMR disks this engine opened (#282): their
/// flushes take seconds once a sustained write has filled their cache, so a
/// stall over one of them is the disk before it is anything else.
fn shingled_report() -> String {
    let disks = crate::drive::identity::shingled_disks();
    if disks.is_empty() {
        return String::new();
    }
    let mut out = format!("shingled (SMR) disks: {}\n", disks.len());
    for d in disks {
        out.push_str(&format!("  /dev/{} {}: {:?}\n", d.disk, d.model, d.recording));
    }
    out
}

/// Requests in flight longer than `over`, oldest first: all of each path for
/// the full view, its route family for the open one (#283).
fn in_flight_view(over: Duration, full: bool) -> String {
    let m = inflight().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<&InFlight> = m.values().filter(|r| r.since.elapsed() >= over).collect();
    v.sort_by_key(|r| r.since);
    let mut out = format!("{} API request(s) in flight", m.len());
    if over > Duration::ZERO {
        out.push_str(&format!(", {} waiting over {}s", v.len(), over.as_secs()));
    }
    out.push('\n');
    for r in v {
        let path = if full { r.path.clone() } else { route_family(&r.path) };
        out.push_str(&format!("  {:>7.1}s  {} {}\n", r.since.elapsed().as_secs_f64(), r.method, path));
    }
    out
}

/// Whether the engine's three big locks are held, right now.
pub fn locks(state: &AppState) -> String {
    let held = |b: bool| if b { "free" } else { "HELD" };
    let vm = state.volume_manager.try_lock().is_ok();
    let gem_w = state.gem.try_write().is_ok();
    let gem_r = state.gem.try_read().is_ok();
    let reg_w = state.slab_registry.try_write().is_ok();
    let reg_r = state.slab_registry.try_read().is_ok();
    format!(
        "volume manager (mutex): {}\n\
         extent map (rwlock): write {} / read {}\n\
         slab registry (rwlock): write {} / read {}\n\
         (a read that is HELD means a writer holds it or is queued for it)\n",
        held(vm),
        held(gem_w),
        held(gem_r),
        held(reg_w),
        held(reg_r),
    )
}

/// Every OS thread of this process: name, state, what it waits in, and its
/// kernel stack (readable as root, which the engine is on a node).
pub fn threads() -> String {
    threads_view(true)
}

/// [`threads`], with the kernel stacks only in the full view (#283).
pub fn threads_view(stacks: bool) -> String {
    let mut out = String::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return "cannot read /proc/self/task\n".into();
    };
    let mut tids: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    tids.sort_by_key(|t| t.parse::<u64>().unwrap_or(0));
    let read = |tid: &str, f: &str| {
        std::fs::read_to_string(format!("/proc/self/task/{tid}/{f}")).unwrap_or_default()
    };
    out.push_str(&format!("{} thread(s)\n", tids.len()));
    for tid in &tids {
        let comm = read(tid, "comm");
        let state = read(tid, "status")
            .lines()
            .find(|l| l.starts_with("State:"))
            .map(|l| l.trim_start_matches("State:").trim().to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "\n{tid} {}  [{}]  wchan={}\n",
            comm.trim(),
            state,
            read(tid, "wchan").trim()
        ));
        if !stacks {
            continue;
        }
        let stack = read(tid, "stack");
        for l in stack.lines().take(16) {
            out.push_str(&format!("    {l}\n"));
        }
    }
    out
}

/// Every task of every registered runtime, with where it is parked.
pub async fn task_dump(limit: Duration) -> String {
    #[cfg(all(tokio_unstable, target_os = "linux"))]
    {
        let handles: Vec<(String, tokio::runtime::Handle)> =
            runtimes().lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = String::new();
        for (name, h) in handles {
            out.push_str(&format!("=== runtime {name} ===\n"));
            // Never a current-thread runtime (#334). Its `dump()` holds the
            // core while it polls every task in trace mode, and our I/O
            // futures are not tokio's: tracing runs them, and one that
            // finishes and releases a tokio lock wakes another task on the
            // same runtime — `schedule()` borrows the core again and the
            // runtime panics ("RefCell already borrowed"). Each adopted or
            // exported ublk device runs on one: the panic killed its server
            // and its I/O hung. Their threads are in /debug/threads.
            if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                out.push_str(
                    "not dumped: a current-thread runtime (a ublk device's) is not safe to \
                     trace while it serves I/O (#334) — see /debug/threads\n",
                );
                continue;
            }
            let h2 = h.clone();
            // Dumped on its own runtime, by a task it runs.
            let dump = h.spawn(async move { h2.dump().await });
            match tokio::time::timeout(limit, dump).await {
                Ok(Ok(d)) => {
                    let tasks = d.tasks();
                    out.push_str(&format!("{} task(s)\n", tasks.iter().count()));
                    for (i, t) in tasks.iter().enumerate() {
                        out.push_str(&format!("--- task {i} ---\n{}\n", t.trace()));
                    }
                }
                Ok(Err(e)) => out.push_str(&format!("could not dump: {e}\n")),
                Err(_) => out.push_str(&format!(
                    "no dump within {}s: a worker of this runtime is not yielding (blocked \
                     synchronously) — see /debug/threads\n",
                    limit.as_secs()
                )),
            }
        }
        out
    }
    #[cfg(not(all(tokio_unstable, target_os = "linux")))]
    {
        let _ = limit;
        "task dumps need a build with --cfg tokio_unstable (.cargo/config.toml); \
         see /debug/threads\n"
            .to_string()
    }
}

/// Start the heartbeat and the watchdog thread (once per process).
pub fn start(state: Arc<AppState>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    start_instant();
    register_runtime("api", tokio::runtime::Handle::current());
    let handle = tokio::runtime::Handle::current();
    tokio::spawn(async {
        loop {
            HEARTBEAT.store(since_start_ms(), Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let spawned = std::thread::Builder::new().name("api-watchdog".into()).spawn(move || {
        let mut last_capture: Option<Instant> = None;
        let mut starved_since: Option<Instant> = None;
        let mut last_ublk: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            // A ublk device that has not answered a request (#337): said
            // every 30 s while it lasts, with its op and age, so a consumer
            // whose fsync never returns can be told it is below it.
            let stuck = crate::drive::ublk::stuck(crate::mgmt::api::UBLK_STUCK_AFTER);
            if !stuck.is_empty() && last_ublk.map(|t| t.elapsed() >= REPORT_AGAIN).unwrap_or(true) {
                last_ublk = Some(Instant::now());
                let list: Vec<String> = stuck
                    .iter()
                    .take(16)
                    .map(|s| format!("{} {} ({}) q{} tag {}: {:.0}s", s.op, s.ublk, s.device, s.queue, s.tag, s.secs))
                    .collect();
                tracing::warn!(
                    "ublk: {} request(s) unanswered for {}s or more: {}{}",
                    stuck.len(),
                    crate::mgmt::api::UBLK_STUCK_AFTER.as_secs(),
                    list.join("; "),
                    if stuck.len() > 16 { "; …" } else { "" }
                );
            }
            let beat_age = since_start_ms().saturating_sub(HEARTBEAT.load(Ordering::Relaxed));
            let starved = beat_age > 5_000;
            if starved && starved_since.is_none() {
                starved_since = Some(Instant::now());
            } else if !starved {
                starved_since = None;
            }
            // Which stalls to report now: (whole, open) for each.
            let due: Vec<(String, String)> = {
                let mut m = inflight().lock().unwrap_or_else(|e| e.into_inner());
                m.values_mut()
                    .filter(|r| r.since.elapsed() >= STALL_AFTER)
                    .filter(|r| r.reported.map(|t| t.elapsed() >= REPORT_AGAIN).unwrap_or(true))
                    .map(|r| {
                        r.reported = Some(Instant::now());
                        let age = r.since.elapsed().as_secs_f64();
                        (
                            format!("{} {} ({age:.0}s)", r.method, r.path),
                            format!("{} {} ({age:.0}s)", r.method, route_family(&r.path)),
                        )
                    })
                    .collect()
            };
            let runtime_report = starved && starved_since.is_some_and(|s| s.elapsed() < Duration::from_secs(3));
            if due.is_empty() && !runtime_report {
                continue;
            }
            // The whole report and its open summary (#283), built side by side.
            let head = |full: bool| {
                let what = if due.is_empty() {
                    "the API runtime stopped running tasks".to_string()
                } else {
                    let list: Vec<&str> = due.iter().map(|(w, o)| if full { w.as_str() } else { o.as_str() }).collect();
                    format!("{} request(s) stalled: {}", due.len(), list.join(", "))
                };
                let mut h = format!("API watchdog at +{:.0}s: {what}\n", start_instant().elapsed().as_secs_f64());
                if starved {
                    h.push_str(&format!(
                        "the API runtime has not run its heartbeat for {:.1}s: a worker is blocked synchronously\n",
                        beat_age as f64 / 1000.0
                    ));
                }
                h.push_str(&in_flight_view(STALL_AFTER, full));
                h.push_str(&locks(&state));
                h.push_str(&crate::drive::flushgate::summary_view(Duration::from_secs(60), full));
                h
            };
            let mut rep = head(true);
            let open = head(false);
            let capture = last_capture.map(|t| t.elapsed() >= CAPTURE_EVERY).unwrap_or(true);
            if capture {
                last_capture = Some(Instant::now());
                if !starved {
                    rep.push_str("\n");
                    rep.push_str(&handle.block_on(task_dump(Duration::from_secs(5))));
                }
                rep.push_str("\n");
                rep.push_str(&threads());
            }
            // To the log in one piece, and kept for /debug/stalls.
            tracing::warn!("{rep}");
            eprintln!("{rep}");
            let mut r = reports().lock().unwrap_or_else(|e| e.into_inner());
            if r.len() >= KEEP_REPORTS {
                r.pop_front();
            }
            r.push_back(Report { open, full: rep });
        }
    });
    if let Err(e) = spawned {
        tracing::error!("API watchdog not started: {e}");
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    /// #283: the open view keeps the API prefix and the resource, nothing
    /// after it.
    #[test]
    fn a_route_family_names_no_volume_name_or_tag() {
        assert_eq!(route_family("/api/v1/volumes/3f1c/clone"), "/api/v1/volumes/…");
        assert_eq!(route_family("/api/v1/volumes"), "/api/v1/volumes");
        assert_eq!(route_family("/v1/volumes/x/attach"), "/v1/volumes/…");
        assert_eq!(route_family("/api/v1/synonyms/boothost/server3/claim"), "/api/v1/synonyms/…");
        assert_eq!(route_family("/apis/storage.storm.io/v1/volumes/x"), "/apis/storage.storm.io/v1/volumes/…");
        assert_eq!(route_family("/serve/v1/exports/e1"), "/serve/v1/exports/…");
        assert_eq!(route_family("/debug/stalls"), "/debug/stalls");
        assert_eq!(route_family("/"), "/");
    }

    /// A remote slab's flushes are given by transport in the open view.
    #[test]
    fn a_remote_device_is_given_by_its_transport() {
        crate::drive::flushgate::record_for_test("nvme-tcp://10.0.0.9:4420/nqn.secret:host:server3?nsid=4", Duration::from_millis(3));
        let open = crate::drive::flushgate::summary_view(Duration::from_secs(60), false);
        assert!(!open.contains("nqn.secret"), "{open}");
        assert!(open.contains("nvme-tcp:// (remote)"), "{open}");
        let full = crate::drive::flushgate::summary_view(Duration::from_secs(60), true);
        assert!(full.contains("nqn.secret:host:server3"), "{full}");
    }
}

#[cfg(all(test, tokio_unstable, target_os = "linux"))]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll, Waker};

    /// An I/O whose completion arrives from another thread. `complete`
    /// marks it done without waking (a poll sees it); `wake` wakes its task.
    #[derive(Clone, Default)]
    struct Io(Arc<(AtomicBool, Mutex<Option<Waker>>)>);
    impl Io {
        fn complete(&self) {
            self.0 .0.store(true, Ordering::SeqCst);
        }
        fn wake(&self) {
            if let Some(w) = self.0 .1.lock().unwrap().take() {
                w.wake();
            }
        }
    }
    impl Future for Io {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 .0.load(Ordering::SeqCst) {
                return Poll::Ready(());
            }
            *self.0 .1.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// #334: a ublk device's current-thread runtime. An I/O task (A) holds a
    /// tokio Mutex that an older task (B) waits on; A's I/O completes
    /// underneath, and the watchdog's dump comes before A is woken. Tracing
    /// polls every task, oldest first: B (still waiting, idle again), then A,
    /// which finds its I/O done, releases the Mutex and wakes B — a wake that
    /// re-enters the scheduler while the dump holds its core: `RefCell
    /// already borrowed` (tokio current_thread/mod.rs:723, the Dell's line).
    /// The panic cuts A short and loses B's wake: the I/O never finishes and
    /// its waiters wait for ever, the Dell's hung API. With nothing traced,
    /// A finishes when its I/O wakes it, and B gets the Mutex.
    #[test]
    fn a_task_dump_leaves_a_current_thread_runtime_running() {
        let io = Io::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<tokio::runtime::Handle>();
        let (said, heard) = std::sync::mpsc::channel::<&'static str>();
        let (ready_tx, ready) = std::sync::mpsc::channel::<()>();
        let (io2, stop2) = (io.clone(), stop.clone());
        let device = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tx.send(rt.handle().clone()).unwrap();
            rt.block_on(async move {
                let m = Arc::new(tokio::sync::Mutex::new(()));
                let go = Arc::new(tokio::sync::Notify::new());
                // B, spawned first (older): waits until A holds the Mutex.
                let (m2, s2, go2) = (m.clone(), said.clone(), go.clone());
                tokio::spawn(async move {
                    go2.notified().await;
                    let _g = m2.lock().await;
                    s2.send("waiter woke").unwrap();
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
                // A: takes the Mutex, waits on its I/O.
                let (m1, s1) = (m.clone(), said.clone());
                tokio::spawn(async move {
                    let g = m1.lock_owned().await;
                    io2.await;
                    drop(g); // wakes B, on this runtime
                    s1.send("io finished").unwrap();
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
                // B now waits on the Mutex A holds.
                go.notify_one();
                for _ in 0..3 {
                    tokio::task::yield_now().await;
                }
                ready_tx.send(()).unwrap();
                while !stop2.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        });
        let h = rx.recv().unwrap();
        ready.recv().unwrap();
        register_runtime("ublk-adopt-test (#334)", h.clone());

        io.complete(); // A's I/O done underneath; A not woken yet
        let api = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let out = api.block_on(task_dump(Duration::from_secs(5)));
        io.wake(); // now its completion wakes it, as io_uring's would

        let mut got = Vec::new();
        while let Ok(m) = heard.recv_timeout(Duration::from_millis(1500)) {
            got.push(m);
            if got.len() == 2 {
                break;
            }
        }
        assert_eq!(got, vec!["io finished", "waiter woke"], "the I/O and its waiter must go on after a dump:\n{out}");
        assert!(out.contains("not dumped: a current-thread runtime"), "{out}");
        assert!(!device.is_finished());

        stop.store(true, Ordering::SeqCst);
        device.join().expect("the device's runtime ends cleanly");
        runtimes().lock().unwrap().retain(|(n, _)| !n.starts_with("ublk-adopt-test"));
    }
}
