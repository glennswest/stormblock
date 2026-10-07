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
//! `/api/v1/health`: the supervisor asking holds no node token.

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

fn reports() -> &'static Mutex<VecDeque<String>> {
    static R: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
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
        .route("/debug/threads", get(|| async { text(threads()) }))
        .route("/debug/locks", get(locks_route))
        .with_state(state)
}

fn text(body: String) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

async fn stalls() -> Response {
    let mut out = in_flight_report(Duration::ZERO);
    out.push_str(&crate::drive::flushgate::summary(Duration::from_secs(300)));
    let r = reports().lock().unwrap_or_else(|e| e.into_inner());
    out.push_str(&format!("\n{} watchdog report(s) kept, newest last\n", r.len()));
    for rep in r.iter() {
        out.push_str("\n");
        out.push_str(rep);
    }
    text(out)
}

async fn tasks() -> Response {
    text(task_dump(Duration::from_secs(10)).await)
}

async fn locks_route(State(state): State<Arc<AppState>>) -> Response {
    text(locks(&state))
}

/// Requests in flight longer than `over`, oldest first.
fn in_flight_report(over: Duration) -> String {
    let m = inflight().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<&InFlight> = m.values().filter(|r| r.since.elapsed() >= over).collect();
    v.sort_by_key(|r| r.since);
    let mut out = format!("{} API request(s) in flight", m.len());
    if over > Duration::ZERO {
        out.push_str(&format!(", {} waiting over {}s", v.len(), over.as_secs()));
    }
    out.push('\n');
    for r in v {
        out.push_str(&format!("  {:>7.1}s  {} {}\n", r.since.elapsed().as_secs_f64(), r.method, r.path));
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
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let beat_age = since_start_ms().saturating_sub(HEARTBEAT.load(Ordering::Relaxed));
            let starved = beat_age > 5_000;
            if starved && starved_since.is_none() {
                starved_since = Some(Instant::now());
            } else if !starved {
                starved_since = None;
            }
            // Which stalls to report now.
            let due: Vec<String> = {
                let mut m = inflight().lock().unwrap_or_else(|e| e.into_inner());
                m.values_mut()
                    .filter(|r| r.since.elapsed() >= STALL_AFTER)
                    .filter(|r| r.reported.map(|t| t.elapsed() >= REPORT_AGAIN).unwrap_or(true))
                    .map(|r| {
                        r.reported = Some(Instant::now());
                        format!("{} {} ({:.0}s)", r.method, r.path, r.since.elapsed().as_secs_f64())
                    })
                    .collect()
            };
            let runtime_report = starved && starved_since.is_some_and(|s| s.elapsed() < Duration::from_secs(3));
            if due.is_empty() && !runtime_report {
                continue;
            }
            let mut rep = format!(
                "API watchdog at +{:.0}s: {}\n",
                start_instant().elapsed().as_secs_f64(),
                if due.is_empty() { "the API runtime stopped running tasks".to_string() } else { format!("{} request(s) stalled: {}", due.len(), due.join(", ")) }
            );
            if starved {
                rep.push_str(&format!(
                    "the API runtime has not run its heartbeat for {:.1}s: a worker is blocked synchronously\n",
                    beat_age as f64 / 1000.0
                ));
            }
            rep.push_str(&in_flight_report(STALL_AFTER));
            rep.push_str(&locks(&state));
            rep.push_str(&crate::drive::flushgate::summary(Duration::from_secs(60)));
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
            r.push_back(rep);
        }
    });
    if let Err(e) = spawned {
        tracing::error!("API watchdog not started: {e}");
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
