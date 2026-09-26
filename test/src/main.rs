//! stormblock's test container (#139), per stormcentral
//! `docs/test-standard.md`: `/test short|medium|long`.
//!
//! Exit 0 when every test passed, 1 when one failed, 2 when the run could not
//! happen. One JSON object per test on stdout, a summary last.

mod engine;
mod env;
mod flows;
mod long;
mod medium;
mod node;
mod report;
mod short;

use std::time::Duration;

#[tokio::main]
async fn main() {
    let suite = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("STORM_SUITE").ok())
        .unwrap_or_else(|| "short".into());
    if !["short", "medium", "long"].contains(&suite.as_str()) {
        eprintln!("usage: test short|medium|long");
        std::process::exit(2);
    }
    let env = env::Env::read(suite.clone());
    let mut r = report::Report::new(&env.results);
    let limit = env.timeout + Duration::from_secs(30);
    let result = match suite.as_str() {
        "short" => tokio::time::timeout(limit, short::run(&env, &mut r)).await,
        "medium" => tokio::time::timeout(limit, medium::run(&env, &mut r)).await,
        _ => tokio::time::timeout(limit, long::run(&env, &mut r)).await,
    };
    let code = match result {
        Ok(Ok(())) => {
            if r.fail > 0 {
                1
            } else {
                0
            }
        }
        Ok(Err(e)) => {
            eprintln!("could not run: {e}");
            2
        }
        Err(_) => {
            r.record("suite-timeout", &Err(report::Why::Fail(format!("{suite} ran past {}s", env.timeout.as_secs()))), 0);
            1
        }
    };
    r.summary();
    // A run that ends with the engine killed leaves its files; the run's
    // namespace (and the pod's emptyDir) go with it.
    std::process::exit(code);
}
