//! Background RAID rebuild and scrub with rate limiting.
//!
//! Rebuilds a replacement drive by reading surviving members and
//! reconstructing the missing data. Rate-limited to avoid starving
//! foreground I/O.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Progress tracking for an ongoing rebuild.
#[derive(Debug)]
pub struct RebuildProgress {
    /// Total number of stripes to rebuild.
    pub total_stripes: u64,
    /// Number of stripes completed so far.
    pub completed_stripes: AtomicU64,
    /// Set to true to cancel the rebuild.
    pub cancelled: AtomicBool,
}

impl RebuildProgress {
    pub fn new(total_stripes: u64) -> Arc<Self> {
        Arc::new(RebuildProgress {
            total_stripes,
            completed_stripes: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        })
    }

    /// Percentage complete (0.0 to 100.0).
    pub fn percent(&self) -> f64 {
        if self.total_stripes == 0 {
            return 100.0;
        }
        let done = self.completed_stripes.load(Ordering::Relaxed);
        (done as f64 / self.total_stripes as f64) * 100.0
    }

    /// Number of stripes completed.
    pub fn completed(&self) -> u64 {
        self.completed_stripes.load(Ordering::Relaxed)
    }

    /// Cancel the rebuild.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Check if rebuild has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Advance progress by one stripe.
    pub fn advance(&self) {
        self.completed_stripes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Configuration for the rebuild rate limiter.
#[derive(Debug, Clone)]
pub struct RebuildConfig {
    /// Maximum number of stripes to rebuild per second (0 = unlimited).
    pub max_stripes_per_sec: u64,
    /// Size of each stripe in bytes.
    pub stripe_size: u64,
}

impl Default for RebuildConfig {
    fn default() -> Self {
        RebuildConfig {
            max_stripes_per_sec: 1000,
            stripe_size: 65536,
        }
    }
}

impl RebuildConfig {
    /// Delay between stripes to stay under the rate limit.
    pub fn inter_stripe_delay(&self) -> std::time::Duration {
        if self.max_stripes_per_sec == 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_micros(1_000_000 / self.max_stripes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_tracking() {
        let progress = RebuildProgress::new(100);
        assert_eq!(progress.percent(), 0.0);
        assert_eq!(progress.completed(), 0);

        progress.advance();
        progress.advance();
        assert_eq!(progress.completed(), 2);
        assert!((progress.percent() - 2.0).abs() < 0.01);
    }

    #[test]
    fn progress_cancel() {
        let progress = RebuildProgress::new(100);
        assert!(!progress.is_cancelled());
        progress.cancel();
        assert!(progress.is_cancelled());
    }

    #[test]
    fn progress_zero_stripes() {
        let progress = RebuildProgress::new(0);
        assert_eq!(progress.percent(), 100.0);
    }

    #[test]
    fn rebuild_config_defaults() {
        let cfg = RebuildConfig::default();
        assert_eq!(cfg.max_stripes_per_sec, 1000);
        let delay = cfg.inter_stripe_delay();
        assert_eq!(delay, std::time::Duration::from_micros(1000));
    }

    #[test]
    fn unlimited_rate() {
        let cfg = RebuildConfig {
            max_stripes_per_sec: 0,
            stripe_size: 65536,
        };
        assert_eq!(cfg.inter_stripe_delay(), std::time::Duration::ZERO);
    }
}
