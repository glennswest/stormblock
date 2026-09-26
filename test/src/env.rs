//! What the runner hands the container (stormcentral docs/test-standard.md).

use std::path::PathBuf;
use std::time::Duration;

pub struct Env {
    pub suite: String,
    /// The node's address, for the checks of the node's own engine.
    pub node: Option<String>,
    pub run_id: String,
    pub timeout: Duration,
    /// Where results and the in-pod engine's files go.
    pub results: PathBuf,
    /// The stormblock binary of the commit under test.
    pub bin: PathBuf,
    /// A token for the node's engine, when the runner has one. The node's
    /// API is closed by default (v17) and a Job is given none, so without it
    /// the authenticated node checks are skipped, never passed.
    pub node_token: Option<String>,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

impl Env {
    pub fn read(suite: String) -> Env {
        let default_timeout = match suite.as_str() {
            "short" => 120,
            "medium" => 1800,
            _ => 8 * 3600,
        };
        let bin = var("STORMBLOCK_BIN").map(PathBuf::from).unwrap_or_else(|| {
            // Beside the test binary (a cargo target dir), else the image's.
            let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("stormblock")));
            match beside {
                Some(p) if p.exists() => p,
                _ => PathBuf::from("/stormblock"),
            }
        });
        Env {
            suite,
            node: var("STORM_NODE"),
            run_id: var("STORM_RUN_ID").unwrap_or_else(|| format!("local-{}", std::process::id())),
            timeout: Duration::from_secs(var("STORM_TIMEOUT").and_then(|t| t.parse().ok()).unwrap_or(default_timeout)),
            results: PathBuf::from(var("STORM_RESULTS").unwrap_or_else(|| "/results".into())),
            bin,
            node_token: var("STORM_STORMBLOCK_TOKEN"),
        }
    }
}
