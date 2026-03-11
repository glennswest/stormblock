//! Per-core I/O reactor — spawns single-threaded tokio runtimes per CPU core.
//!
//! Each reactor is a dedicated thread running a `current_thread` tokio runtime.
//! On Linux, threads are pinned to specific CPU cores via `sched_setaffinity`.
//! Connections are dispatched round-robin across reactors.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use tokio::sync::mpsc;

type BoxFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Configuration for the reactor pool.
#[derive(Debug, Clone)]
pub struct ReactorConfig {
    /// Number of reactor cores (0 = auto-detect).
    pub core_count: usize,
    /// Whether to pin threads to CPU cores (Linux only, ignored on macOS).
    pub pin_cores: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for ReactorConfig {
    fn default() -> Self {
        ReactorConfig {
            core_count: 0,
            pin_cores: cfg!(target_os = "linux"),
        }
    }
}

/// A pool of per-core single-threaded tokio runtimes.
pub struct ReactorPool {
    senders: Vec<mpsc::UnboundedSender<BoxFuture>>,
    _threads: Vec<thread::JoinHandle<()>>,
    next: AtomicUsize,
}

impl ReactorPool {
    /// Spawn a pool of reactor threads.
    pub fn new(config: &ReactorConfig) -> Self {
        let count = if config.core_count == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            config.core_count
        };

        let mut senders = Vec::with_capacity(count);
        let mut threads = Vec::with_capacity(count);

        for core_id in 0..count {
            let (tx, mut rx) = mpsc::unbounded_channel::<BoxFuture>();
            let pin = config.pin_cores;

            let handle = thread::Builder::new()
                .name(format!("reactor-{core_id}"))
                .spawn(move || {
                    if pin {
                        pin_to_core(core_id);
                    }

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to create reactor runtime");

                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        while let Some(fut) = rx.recv().await {
                            tokio::task::spawn_local(fut);
                        }
                    });
                })
                .expect("failed to spawn reactor thread");

            senders.push(tx);
            threads.push(handle);
        }

        tracing::info!("Reactor pool started: {} cores, pin={}", count, config.pin_cores);

        ReactorPool {
            senders,
            _threads: threads,
            next: AtomicUsize::new(0),
        }
    }

    /// Dispatch a future to the next reactor (round-robin).
    pub fn dispatch<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        let _ = self.senders[idx].send(Box::pin(fut));
    }

    /// Number of reactor cores.
    pub fn core_count(&self) -> usize {
        self.senders.len()
    }
}

/// Pin the current thread to a specific CPU core (Linux only).
#[cfg(target_os = "linux")]
fn pin_to_core(core_id: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let ret = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if ret != 0 {
            tracing::warn!("Failed to pin reactor thread to core {core_id}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_core(_core_id: usize) {
    // CPU pinning not supported on this platform
}

/// Shared reference to a reactor pool, used by target servers.
pub type ReactorPoolRef = Arc<ReactorPool>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn reactor_pool_dispatch() {
        let config = ReactorConfig {
            core_count: 2,
            pin_cores: false,
        };
        let pool = ReactorPool::new(&config);
        assert_eq!(pool.core_count(), 2);

        let counter = Arc::new(AtomicU32::new(0));

        for _ in 0..4 {
            let c = counter.clone();
            pool.dispatch(async move {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        // Give reactors time to process
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(counter.load(Ordering::Relaxed), 4);
    }
}
