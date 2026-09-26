//! What a run reports: one JSON object per test on stdout (and in
//! `/results/results.jsonl`), a summary line last, and the exit code —
//! 0 all passed, 1 a test failed, 2 the run could not happen.

use std::io::Write;
use std::time::Instant;

use serde_json::json;

/// Why a test did not pass.
pub enum Why {
    Fail(String),
    Skip(String),
}

impl From<String> for Why {
    fn from(s: String) -> Self {
        Why::Fail(s)
    }
}

impl From<&str> for Why {
    fn from(s: &str) -> Self {
        Why::Fail(s.to_string())
    }
}

/// A test's result: `Ok(detail)` passes.
pub type Outcome = Result<String, Why>;

/// Fail unless `cond`.
pub fn ensure(cond: bool, why: impl Into<String>) -> Result<(), Why> {
    if cond {
        Ok(())
    } else {
        Err(Why::Fail(why.into()))
    }
}

#[derive(Default)]
pub struct Report {
    pub pass: u32,
    pub fail: u32,
    pub skip: u32,
    file: Option<std::fs::File>,
}

impl Report {
    pub fn new(results: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(results);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(results.join("results.jsonl"))
            .ok();
        Report { file, ..Default::default() }
    }

    fn line(&mut self, v: serde_json::Value) {
        let s = v.to_string();
        println!("{s}");
        let _ = std::io::stdout().flush();
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    }

    pub fn record(&mut self, test: &str, outcome: &Outcome, ms: u128) {
        let (status, detail) = match outcome {
            Ok(d) => {
                self.pass += 1;
                ("pass", d.as_str())
            }
            Err(Why::Fail(d)) => {
                self.fail += 1;
                ("fail", d.as_str())
            }
            Err(Why::Skip(d)) => {
                self.skip += 1;
                ("skip", d.as_str())
            }
        };
        self.line(json!({ "test": test, "status": status, "ms": ms as u64, "detail": detail }));
    }

    /// Run one test, time it, and record it.
    pub async fn run<F>(&mut self, test: &str, f: F) -> bool
    where
        F: std::future::Future<Output = Outcome>,
    {
        let t = Instant::now();
        let outcome = f.await;
        let ok = outcome.is_ok();
        self.record(test, &outcome, t.elapsed().as_millis());
        ok
    }

    /// Record a metric line (the long suite's per-wave numbers). Not a test.
    pub fn metric(&mut self, v: serde_json::Value) {
        self.line(json!({ "metric": v }));
    }

    pub fn summary(&mut self) {
        let (p, f, s) = (self.pass, self.fail, self.skip);
        self.line(json!({ "summary": { "pass": p, "fail": f, "skip": s } }));
    }
}
