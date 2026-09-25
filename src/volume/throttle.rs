//! A byte-rate budget shared by background work that competes with live I/O
//! (#146): every rebuild on the node draws from one bucket, so ten volumes
//! rebuilding at once take no more of the drives than one would.
//!
//! Zero means unlimited. The rate can be changed while work is running; a
//! waiter already sleeping finishes its current wait at the old rate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Throttle {
    /// Bytes per second; 0 = unlimited.
    rate: AtomicU64,
    /// When the bucket was last refilled, and what it held then (may be
    /// negative: a take larger than the balance is paid by waiting).
    state: tokio::sync::Mutex<(Instant, f64)>,
    taken: AtomicU64,
}

impl Throttle {
    pub fn new(bytes_per_sec: u64) -> Self {
        Throttle {
            rate: AtomicU64::new(bytes_per_sec),
            state: tokio::sync::Mutex::new((Instant::now(), bytes_per_sec as f64)),
            taken: AtomicU64::new(0),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Bytes taken through this bucket so far.
    pub fn taken(&self) -> u64 {
        self.taken.load(Ordering::Relaxed)
    }

    /// Take `bytes` from the budget, waiting until the rate allows it. The
    /// bucket holds at most one second's worth, so an idle period does not
    /// bank a burst that lands on live I/O all at once.
    pub async fn take(&self, bytes: u64) {
        self.taken.fetch_add(bytes, Ordering::Relaxed);
        let rate = self.rate();
        if rate == 0 {
            return;
        }
        // Waiters queue on the lock (tokio's mutex is fair), so the budget is
        // shared in arrival order rather than by whoever polls first.
        let mut st = self.state.lock().await;
        let now = Instant::now();
        let refill = now.duration_since(st.0).as_secs_f64() * rate as f64;
        st.1 = (st.1 + refill).min(rate as f64) - bytes as f64;
        st.0 = now;
        if st.1 < 0.0 {
            let wait = Duration::from_secs_f64(-st.1 / rate as f64);
            tokio::time::sleep(wait).await;
            st.0 = Instant::now();
            st.1 = 0.0;
        }
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::unlimited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_rate_holds_takes_to_it() {
        // 1 MiB/s, starting with a full bucket: the first MiB is free, the
        // next half MiB costs half a second.
        let t = Throttle::new(1 << 20);
        let start = Instant::now();
        t.take(1 << 20).await;
        t.take(1 << 19).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(450), "{took:?}");
        assert!(took < Duration::from_millis(1500), "{took:?}");
        assert_eq!(t.taken(), (1 << 20) + (1 << 19));
    }

    #[tokio::test]
    async fn zero_is_unlimited_and_the_rate_can_change() {
        let t = Throttle::unlimited();
        let start = Instant::now();
        for _ in 0..1000 {
            t.take(1 << 30).await;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        t.set_rate(4096);
        assert_eq!(t.rate(), 4096);
    }
}
