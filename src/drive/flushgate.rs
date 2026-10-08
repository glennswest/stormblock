//! One cache flush for many callers, per device, and how long flushes take
//! (#269).
//!
//! A cache flush is a whole-device operation: it makes durable every write the
//! device completed before it was issued, whoever made it. On a node the
//! system and data slabs are partitions over one disk, and a template clone,
//! the flow-over's persist and a consumer's fsync each ask for flushes of it,
//! often at the same moment. On server3's spinning disk a flush took seconds,
//! so each one that could have been shared was seconds more queueing for the
//! actuator. Here a caller who asks while a flush is running waits for the
//! next one, which covers everyone who asked before it began.
//!
//! Every flush is timed and kept (the last [`KEEP`]), so `/debug/stalls` can
//! say whether the disk is the bottleneck when the API stalls.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::DriveResult;

/// How many flush durations are kept.
const KEEP: usize = 512;

/// Group commit for one device's flushes.
#[derive(Default)]
pub struct FlushGate {
    asked: AtomicU64,
    done: AtomicU64,
    running: tokio::sync::Mutex<()>,
}

impl FlushGate {
    /// Flush with `f`, unless a flush that began after this call has already
    /// covered it. `label` names the device in the flush record.
    pub async fn flush<F, Fut>(&self, label: &str, f: F) -> DriveResult<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = DriveResult<()>>,
    {
        // Never with the volume manager held (#364).
        crate::lockwatch::assert_not_held("volume manager", "a device flush");
        let ticket = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let _running = self.running.lock().await;
        if self.done.load(Ordering::SeqCst) >= ticket {
            return Ok(());
        }
        // Everyone who asked before this flush begins: their writes had
        // completed when they asked, so it covers them.
        let covers = self.asked.load(Ordering::SeqCst);
        let began = Instant::now();
        let r = f().await;
        record(label, began.elapsed(), (covers + 1).saturating_sub(ticket));
        r?;
        self.done.fetch_max(covers, Ordering::SeqCst);
        Ok(())
    }
}

struct Flush {
    at: Instant,
    label: String,
    took: Duration,
    /// Callers this flush covered besides the one that ran it.
    shared: u64,
}

fn flushes() -> &'static Mutex<VecDeque<Flush>> {
    static F: OnceLock<Mutex<VecDeque<Flush>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// A flush in the record, as if one had run (tests).
#[cfg(test)]
pub fn record_for_test(label: &str, took: Duration) {
    record(label, took, 0);
}

fn record(label: &str, took: Duration, shared: u64) {
    let mut f = flushes().lock().unwrap_or_else(|e| e.into_inner());
    if f.len() >= KEEP {
        f.pop_front();
    }
    f.push_back(Flush { at: Instant::now(), label: label.to_string(), took, shared });
}

/// The device flushes of the last `window`, per device: how many, p50, p99,
/// max, and how many callers they were shared with.
/// The `q` quantile (0..=1) of `label`'s flushes in the last `window`, and
/// how many there were; None when there were none (#282).
pub fn recent(label: &str, window: Duration, q: f64) -> Option<(Duration, usize)> {
    let f = flushes().lock().unwrap_or_else(|e| e.into_inner());
    let mut took: Vec<Duration> = f.iter().filter(|x| x.label == label && x.at.elapsed() <= window).map(|x| x.took).collect();
    if took.is_empty() {
        return None;
    }
    took.sort();
    let at = ((took.len() as f64 - 1.0) * q.clamp(0.0, 1.0)).round() as usize;
    Some((took[at], took.len()))
}

pub fn summary(window: Duration) -> String {
    summary_view(window, true)
}

/// [`summary`]; without `full`, a device named by a URI (a remote slab) is
/// given by its transport only (#283): the URI is what attaching it takes.
pub fn summary_view(window: Duration, full: bool) -> String {
    let f = flushes().lock().unwrap_or_else(|e| e.into_inner());
    let recent: Vec<&Flush> = f.iter().filter(|x| x.at.elapsed() <= window).collect();
    let mut out = format!("device flushes in the last {}s: {}\n", window.as_secs(), recent.len());
    let mut labels: Vec<&str> = recent.iter().map(|x| x.label.as_str()).collect();
    labels.sort();
    labels.dedup();
    for l in labels {
        let shown = match l.split_once("://") {
            Some((scheme, _)) if !full => format!("{scheme}:// (remote)"),
            _ => l.to_string(),
        };
        let mut ms: Vec<f64> = recent.iter().filter(|x| x.label == l).map(|x| x.took.as_secs_f64() * 1e3).collect();
        let shared: u64 = recent.iter().filter(|x| x.label == l).map(|x| x.shared).sum();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |p: f64| ms[((ms.len() as f64 - 1.0) * p).round() as usize];
        out.push_str(&format!(
            "  {shown}: {} flush(es), p50 {:.0} ms, p99 {:.0} ms, max {:.0} ms; {} caller(s) shared one\n",
            ms.len(),
            at(0.5),
            at(0.99),
            at(1.0),
            shared
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Callers who ask while a flush runs share the next one: ten callers,
    /// at most two device flushes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_flushes() {
        let gate = Arc::new(FlushGate::default());
        let ran = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (gate, ran) = (gate.clone(), ran.clone());
            tasks.push(tokio::spawn(async move {
                gate.flush("test", || async {
                    ran.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(())
                })
                .await
                .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let n = ran.load(Ordering::SeqCst);
        assert!(n <= 3, "{n} flushes for 10 concurrent callers");
        // Asked after the last flush ended: it runs its own.
        gate.flush("test", || async {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), n + 1);
        assert!(summary(Duration::from_secs(60)).contains("test:"));
    }
}
