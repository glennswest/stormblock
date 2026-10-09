//! Retries for every call that leaves this process (#359).
//!
//! Owner: "a single timeout must never kill us, and the bug is the missing
//! retry". One helper, so every remote call retries the same way:
//!
//! * **Bounded:** at most [`Policy::attempts`] tries, never past the
//!   whole-operation [`Policy::deadline`].
//! * **Backoff with jitter:** [`Policy::base`] doubling to [`Policy::max_delay`],
//!   each wait drawn from the upper half of its step, so many callers that
//!   failed together do not retry together.
//! * **Only what is worth retrying:** the caller's classifier says whether a
//!   failure is [`Class::Transient`] (a timeout, a refused or reset
//!   connection, a 5xx, a 408 or 429) or [`Class::Permanent`] (a real
//!   answer: a 4xx, a validation error), which fails at once.
//! * **Said:** a call that needed more than one attempt logs how many and how
//!   long ("succeeded on attempt 3 after 4.1 s"); one that gives up logs the
//!   attempts, the time and the last error. A flaky dependency shows in the
//!   log instead of hiding behind a retry.
//! * **Classified for the caller:** [`Failure`] says whether it gave up on
//!   infrastructure (unreachable, timed out) or got a real error, so a caller
//!   or a test runner does not report one as the other.
//!
//! Only idempotent operations go through it, or ones made safe to repeat
//! (a read, a write of the same bytes at the same place, a PUT, a GET, a
//! check-then-act). A call that must not be retried says why where it is.

use std::future::Future;
use std::time::{Duration, Instant};

/// How a call is retried.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Tries in all, the first included.
    pub attempts: u32,
    /// The first wait.
    pub base: Duration,
    /// The longest wait.
    pub max_delay: Duration,
    /// No attempt starts past this, counted from the first.
    pub deadline: Duration,
}

impl Policy {
    /// A call to a service on the network (HTTP to another component, the
    /// forge, the apiserver): 5 tries, 200 ms doubling to 5 s, 60 s in all.
    pub const NETWORK: Policy = Policy {
        attempts: 5,
        base: Duration::from_millis(200),
        max_delay: Duration::from_secs(5),
        deadline: Duration::from_secs(60),
    };
    /// A call a request is waiting on (an access review behind an API call):
    /// 3 tries, 100 ms doubling to 1 s, 10 s in all.
    pub const QUICK: Policy = Policy {
        attempts: 3,
        base: Duration::from_millis(100),
        max_delay: Duration::from_secs(1),
        deadline: Duration::from_secs(10),
    };
    /// One block I/O over the network (NVMe/TCP, iSCSI): 3 tries, a reconnect
    /// between, 100 ms doubling to 1 s. Each attempt carries its own I/O
    /// timeout; this bounds the whole at about three of them.
    pub const BLOCK_IO: Policy = Policy {
        attempts: 3,
        base: Duration::from_millis(100),
        max_delay: Duration::from_secs(1),
        deadline: Duration::from_secs(120),
    };
    /// A large transfer that resumes (an image download): 8 tries, 1 s
    /// doubling to 30 s, 30 min in all.
    pub const TRANSFER: Policy = Policy {
        attempts: 8,
        base: Duration::from_secs(1),
        max_delay: Duration::from_secs(30),
        deadline: Duration::from_secs(30 * 60),
    };

    /// The wait after attempt `n` (1-based) failed: the doubled step, drawn
    /// from its upper half.
    pub fn delay(&self, n: u32) -> Duration {
        let step = self.base.saturating_mul(1u32 << (n.saturating_sub(1)).min(16)).min(self.max_delay);
        let half = step / 2;
        half + Duration::from_nanos(jitter(half.as_nanos() as u64))
    }
}

/// A pseudo-random number below `bound` (0 when `bound` is 0): enough to
/// spread retries, no dependency for it.
fn jitter(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    // xorshift64*
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound
}

/// What a failure is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Infrastructure: try again.
    Transient,
    /// A real answer: fail now.
    Permanent,
}

/// Why a retried call failed.
#[derive(Debug)]
pub struct Failure<E> {
    pub error: E,
    pub class: Class,
    pub attempts: u32,
    pub elapsed: Duration,
}

impl<E> Failure<E> {
    /// It gave up on infrastructure (unreachable, timed out), not a real error.
    pub fn is_infrastructure(&self) -> bool {
        self.class == Class::Transient
    }
}

impl<E: std::fmt::Display> std::fmt::Display for Failure<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.class {
            Class::Transient => write!(
                f,
                "gave up after {} attempt(s) / {:.1}s (infrastructure): {}",
                self.attempts,
                self.elapsed.as_secs_f64(),
                self.error
            ),
            Class::Permanent if self.attempts > 1 => {
                write!(f, "{} (on attempt {}, not retried: a real answer)", self.error, self.attempts)
            }
            Class::Permanent => write!(f, "{}", self.error),
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for Failure<E> {}

/// Run `op` under `policy`. `what` names the call in the log; `classify`
/// says whether a failure is worth another attempt.
pub async fn with_backoff<T, E, F, Fut>(
    what: &str,
    policy: Policy,
    classify: impl Fn(&E) -> Class,
    op: F,
) -> Result<T, Failure<E>>
where
    E: std::fmt::Display,
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    run(what, policy, classify, None::<fn() -> u64>, op).await
}

/// [`with_backoff`] for a transfer that resumes (#125): an attempt that
/// moved `progress` forward before it failed starts the count and the
/// deadline over. A long download over a flaky link fails only when it stops
/// getting anywhere, not after so many drops or so long in all: 3 GiB at
/// 0.7 MB/s takes longer than any fixed deadline a stuck one should get.
pub async fn with_backoff_progress<T, E, F, Fut>(
    what: &str,
    policy: Policy,
    classify: impl Fn(&E) -> Class,
    progress: impl Fn() -> u64,
    op: F,
) -> Result<T, Failure<E>>
where
    E: std::fmt::Display,
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    run(what, policy, classify, Some(progress), op).await
}

async fn run<T, E, F, Fut, P>(
    what: &str,
    policy: Policy,
    classify: impl Fn(&E) -> Class,
    progress: Option<P>,
    mut op: F,
) -> Result<T, Failure<E>>
where
    E: std::fmt::Display,
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    P: Fn() -> u64,
{
    let first = Instant::now();
    let mut start = first;
    let mut attempt = 0u32;
    let mut total = 0u32;
    loop {
        attempt += 1;
        total += 1;
        let before = progress.as_ref().map(|p| p());
        match op(total).await {
            Ok(v) => {
                if total > 1 {
                    tracing::info!(
                        "retry: {what} succeeded on attempt {total} after {:.1}s",
                        first.elapsed().as_secs_f64()
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                let class = classify(&e);
                if let (Some(p), Some(b)) = (progress.as_ref(), before) {
                    if class == Class::Transient && p() > b {
                        // It got somewhere: the retries start over (#125).
                        tracing::info!("retry: {what} attempt {total} failed after progress ({e}); retries start over");
                        attempt = 1;
                        start = Instant::now();
                    }
                }
                let elapsed = start.elapsed();
                let wait = policy.delay(attempt);
                let out_of_time = elapsed + wait >= policy.deadline;
                if class == Class::Permanent || attempt >= policy.attempts || out_of_time {
                    if class == Class::Transient {
                        tracing::warn!(
                            "retry: {what} gave up after {attempt} attempt(s) / {:.1}s: {e}",
                            elapsed.as_secs_f64()
                        );
                    }
                    return Err(Failure { error: e, class, attempts: total, elapsed: first.elapsed() });
                }
                tracing::info!(
                    "retry: {what} attempt {total} failed ({e}); again in {:.2}s",
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// An I/O error's class: the network or a device that did not answer is
/// infrastructure; anything else is the answer.
pub fn classify_io(e: &std::io::Error) -> Class {
    use std::io::ErrorKind::*;
    match e.kind() {
        TimedOut | ConnectionRefused | ConnectionReset | ConnectionAborted | NotConnected | BrokenPipe
        | Interrupted | UnexpectedEof | WouldBlock | AddrNotAvailable | NetworkUnreachable | HostUnreachable
        | NetworkDown => Class::Transient,
        _ => Class::Permanent,
    }
}

/// An HTTP status's class: 408, 429 and 5xx are worth another attempt; any
/// other answer is the answer.
pub fn classify_status(status: u16) -> Class {
    if status == 408 || status == 429 || (500..600).contains(&status) {
        Class::Transient
    } else {
        Class::Permanent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn quick(attempts: u32) -> Policy {
        Policy {
            attempts,
            base: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            deadline: Duration::from_secs(10),
        }
    }

    #[tokio::test]
    async fn a_call_that_fails_twice_then_succeeds_succeeds_on_attempt_three() {
        let n = AtomicU32::new(0);
        let r: Result<u32, Failure<String>> = with_backoff(
            "test",
            quick(5),
            |_| Class::Transient,
            |a| {
                let k = n.fetch_add(1, Ordering::SeqCst);
                async move { if k < 2 { Err(format!("down {a}")) } else { Ok(a) } }
            },
        )
        .await;
        assert_eq!(r.unwrap(), 3);
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_real_answer_is_not_retried() {
        let n = AtomicU32::new(0);
        let r: Result<(), Failure<u16>> = with_backoff(
            "test",
            quick(5),
            |s| classify_status(*s),
            |_| {
                n.fetch_add(1, Ordering::SeqCst);
                async { Err(404u16) }
            },
        )
        .await;
        let f = r.unwrap_err();
        assert_eq!((f.attempts, f.class), (1, Class::Permanent));
        assert!(!f.is_infrastructure());
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn infrastructure_that_stays_down_gives_up_bounded_and_says_so() {
        let r: Result<(), Failure<std::io::Error>> = with_backoff(
            "test",
            quick(4),
            classify_io,
            |_| async { Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer")) },
        )
        .await;
        let f = r.unwrap_err();
        assert_eq!(f.attempts, 4);
        assert!(f.is_infrastructure());
        assert!(f.to_string().contains("gave up after 4 attempt(s)"), "{f}");
    }

    #[tokio::test]
    async fn the_deadline_caps_the_attempts() {
        let p = Policy {
            attempts: 100,
            base: Duration::from_millis(20),
            max_delay: Duration::from_millis(20),
            deadline: Duration::from_millis(70),
        };
        let r: Result<(), Failure<String>> =
            with_backoff("test", p, |_| Class::Transient, |_| async { Err("down".to_string()) }).await;
        let f = r.unwrap_err();
        assert!(f.attempts < 10, "stopped by the deadline, not the count: {}", f.attempts);
    }

    /// #125: a transfer that keeps getting somewhere is not given up on for
    /// the number of drops; one that stops getting anywhere is.
    #[tokio::test]
    async fn progress_starts_the_retries_over() {
        let moved = AtomicU32::new(0);
        let calls = AtomicU32::new(0);
        // 3 attempts allowed, 10 drops each after some progress, then done.
        let r: Result<u32, Failure<String>> = with_backoff_progress(
            "test",
            quick(3),
            |_| Class::Transient,
            || moved.load(Ordering::SeqCst) as u64,
            |_| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                moved.fetch_add(1, Ordering::SeqCst);
                async move { if n < 10 { Err("dropped".to_string()) } else { Ok(n) } }
            },
        )
        .await;
        assert_eq!(r.unwrap(), 10, "ten drops, each after progress, then it finished");
        // No progress: the limit holds.
        let calls = AtomicU32::new(0);
        let r: Result<(), Failure<String>> = with_backoff_progress(
            "test",
            quick(3),
            |_| Class::Transient,
            || 0,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("dropped".to_string()) }
            },
        )
        .await;
        assert_eq!(r.unwrap_err().attempts, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn statuses_and_errors_are_classified() {
        for s in [408u16, 429, 500, 502, 503, 504] {
            assert_eq!(classify_status(s), Class::Transient, "{s}");
        }
        for s in [400u16, 401, 403, 404, 409, 412, 422] {
            assert_eq!(classify_status(s), Class::Permanent, "{s}");
        }
        assert_eq!(classify_io(&std::io::Error::from(std::io::ErrorKind::ConnectionReset)), Class::Transient);
        assert_eq!(classify_io(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)), Class::Permanent);
    }

    #[test]
    fn the_delay_grows_and_is_capped() {
        let p = Policy { attempts: 9, base: Duration::from_millis(100), max_delay: Duration::from_secs(1), deadline: Duration::from_secs(60) };
        assert!(p.delay(1) >= Duration::from_millis(50) && p.delay(1) <= Duration::from_millis(100));
        assert!(p.delay(8) <= Duration::from_secs(1) && p.delay(8) >= Duration::from_millis(500));
    }
}
