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
//! * **`GET /debug/tasks`**: every async task of every runtime the engine
//!   runs (the API's, each adopted ublk device's), with the `.await` it is
//!   parked on — a tokio task dump. Needs `--cfg tokio_unstable` (set in
//!   `.cargo/config.toml`); a build without it says so.
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
            let h2 = h.clone();
            // Dumped on its own runtime: a current-thread runtime is dumped
            // by a task it runs, never from outside.
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
