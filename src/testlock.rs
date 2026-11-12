//! Locks for the lib's tests that share process-wide state (#585).
//!
//! `cargo test` runs the lib's tests on threads of one process. A flow-over
//! reads `STORMBLOCK_FLOW_*` from the environment, paces itself on the
//! latency recorder (`fglat`) and counts hot slots (`flowhot`); the NIC
//! planner reads `STORMBLOCK_IFADDRS` / `STORMBLOCK_SYSFS_NET`. Tests that
//! set or depend on these take one of these locks, so each runs alone and
//! starts from a clean slate. (nextest gives every test its own process;
//! `cargo test` does not.)

use tokio::sync::{Mutex, MutexGuard};

static FLOW: Mutex<()> = Mutex::const_new(());
static NET: Mutex<()> = Mutex::const_new(());

/// Every knob a flow-over reads from the environment.
const FLOW_ENV: &[&str] = &[
    "STORMBLOCK_FLOW_WINDOW",
    "STORMBLOCK_FLOW_BATCH",
    "STORMBLOCK_FLOW_BOOT_GRACE_SECS",
    "STORMBLOCK_FLOW_COALESCE",
    "STORMBLOCK_FLOW_FG_FLUSH_MS",
    "STORMBLOCK_FLOW_FG_IO_MS",
    "STORMBLOCK_FLOW_FG_RATIO",
    "STORMBLOCK_FLOW_INSTALL_GRACE_SECS",
    "STORMBLOCK_FLOW_OVERLAP",
    "STORMBLOCK_FLOW_PACE",
    "STORMBLOCK_FLOW_PARALLEL",
    "STORMBLOCK_FLOW_PERSIST_SECS",
    "STORMBLOCK_FLOW_READBACK",
    "STORMBLOCK_FLOW_RUN_MB",
];

fn flow_reset() {
    for k in FLOW_ENV {
        std::env::remove_var(k);
    }
    crate::fglat::clear_for_test();
    crate::flowhot::clear_for_test();
    crate::flowprogress::FLOW.clear_for_test();
}

/// One flow-over test at a time, each from a clean slate.
pub(crate) async fn flow() -> MutexGuard<'static, ()> {
    let g = FLOW.lock().await;
    flow_reset();
    g
}

/// [`flow`] for a test that is not async.
pub(crate) fn flow_blocking() -> MutexGuard<'static, ()> {
    let g = FLOW.blocking_lock();
    flow_reset();
    g
}

/// One test of the NIC environment at a time.
pub(crate) async fn net() -> MutexGuard<'static, ()> {
    let g = NET.lock().await;
    net_reset();
    g
}

/// Nothing configured: no storage interfaces, nothing skipped, the NIC
/// environment unset (a config test's `apply_networks` sets the first two).
fn net_reset() {
    for k in ["STORMBLOCK_IFADDRS", "STORMBLOCK_SYSFS_NET", "STORMBLOCK_PATH_SKIP", "STORMBLOCK_STORAGE_INTERFACE"] {
        std::env::remove_var(k);
    }
    crate::netif::set_path_sources(Vec::new());
    crate::netif::set_path_skip(Vec::new());
}

/// [`net`] for a test that is not async.
pub(crate) fn net_blocking() -> MutexGuard<'static, ()> {
    let g = NET.blocking_lock();
    net_reset();
    g
}
