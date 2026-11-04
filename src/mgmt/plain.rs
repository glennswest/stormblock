//! Who still calls the management API in plain (#525, stormcos#374).
//!
//! While callers move to TLS one golden at a time, the exit test is that this
//! count stops moving: `tls_mode = "optional"` is only safe to end when nobody
//! is left, and without the count nobody can tell who is. Every request served
//! in plain on a listener with TLS configured is counted by caller address and
//! route, whatever the mode: with the pair not there yet (#483) a caller that
//! has moved fails its handshake, so a plain request is an unmoved caller then
//! too.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;

/// Distinct caller + route pairs kept apart. Anyone who can reach the port
/// can make up a path, and a label is memory for as long as the process runs;
/// past this they are counted together as `other`, and said once.
const MAX_PAIRS: usize = 1024;

/// The counter `/metrics` serves.
pub const PLAIN_REQUESTS: &str = "stormblock_mgmt_plain_requests_total";

pub struct PlainTally {
    seen: Mutex<HashSet<(String, String)>>,
    audit: Option<PathBuf>,
}

impl PlainTally {
    pub fn new(audit: Option<PathBuf>) -> Self {
        PlainTally { seen: Mutex::new(HashSet::new()), audit }
    }

    /// One request served in plain. Only the route's family: a ticket, a
    /// volume's name or a machine's tag is no label (#283).
    pub fn note(&self, peer: IpAddr, method: &str, path: &str) {
        let pair = (peer.to_canonical().to_string(), route_label(method, path));
        let (labels, first) = self.admit(pair);
        metrics::counter!(PLAIN_REQUESTS, "caller" => labels.0.clone(), "route" => labels.1.clone()).increment(1);
        if first {
            self.say(&labels.0, &labels.1);
        }
    }

    /// The labels to count under, and whether this is their first request.
    fn admit(&self, pair: (String, String)) -> ((String, String), bool) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(&pair) {
            return (pair, false);
        }
        if seen.len() < MAX_PAIRS {
            seen.insert(pair.clone());
            return (pair, true);
        }
        let other = ("other".to_string(), "other".to_string());
        let first = seen.insert(other.clone());
        (other, first)
    }

    /// Off the runtime: on a node the audit log is on a volume this engine
    /// serves, and a request must not wait on it.
    fn say(&self, caller: &str, route: &str) {
        tracing::warn!("management API: first plain request from {caller}: {route} (#525)");
        let Some(path) = self.audit.clone() else { return };
        let (method, family) = route.split_once(' ').unwrap_or(("", route));
        let rec = super::kubeauth::AuditRecord {
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            who: caller.to_string(),
            method: method.to_string(),
            path: family.to_string(),
            resource: "management-api".to_string(),
            verb: "plain-request".to_string(),
            target: None,
            decision: "served-plain".to_string(),
            reason: Some("first plain request from this caller and route".to_string()),
            status: None,
        };
        tokio::task::spawn_blocking(move || super::kubeauth::append_audit(&path, &rec));
    }
}

/// `GET /api/v1/volumes/…`. The path's first segment can be anything a caller
/// makes up, so only what a Prometheus label can carry as it is.
fn route_label(method: &str, path: &str) -> String {
    let family: String = super::debug::route_family(path)
        .chars()
        .map(|c| if c == '"' || c == '\\' || c.is_control() { '_' } else { c })
        .take(80)
        .collect();
    format!("{method} {family}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(i: usize) -> (String, String) {
        (format!("10.0.0.{i}"), "GET /x".to_string())
    }

    #[test]
    fn a_route_label_names_no_id_and_carries_no_quote() {
        assert_eq!(route_label("GET", "/api/v1/volumes/3f1c/clone"), "GET /api/v1/volumes/…");
        assert_eq!(route_label("POST", "/api/v1/synonyms/boothost/server3/claim"), "POST /api/v1/synonyms/…");
        assert_eq!(route_label("GET", "/a\"b\\c"), "GET /a_b_c");
        assert!(route_label("GET", &format!("/{}", "x".repeat(4096))).len() < 100);
    }

    #[test]
    fn each_pair_is_first_once_and_the_table_stops_growing() {
        let t = PlainTally::new(None);
        assert_eq!(t.admit(pair(1)), (pair(1), true));
        assert_eq!(t.admit(pair(1)), (pair(1), false), "a pair is new once");
        for i in 2..=MAX_PAIRS {
            assert!(t.admit(pair(i)).1);
        }
        let other = ("other".to_string(), "other".to_string());
        assert_eq!(t.admit(pair(MAX_PAIRS + 1)), (other.clone(), true), "past the bound: other, said once");
        assert_eq!(t.admit(pair(MAX_PAIRS + 2)), (other, false));
        assert_eq!(t.admit(pair(1)), (pair(1), false), "one already kept stays itself");
        assert!(t.seen.lock().unwrap().len() <= MAX_PAIRS + 1);
    }
}
