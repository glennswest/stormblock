//! Who is connected right now, per target (#292).
//!
//! The configured side of an export — which host NQN a subsystem admits, which
//! LUNs a target carries — says who *may* attach. A guard that refuses to
//! release a storage node, or to turn its storage off, while another node uses
//! a volume on it (stormcluster#15/#21) needs who *is* attached, and so does
//! the registry's clone reaper (stormblock-registry#63). This is that list:
//! every NVMe/TCP connection bound to a subsystem by its Connect, and every
//! iSCSI connection in a normal (not discovery) session, registered when it
//! is made and gone when it ends.
//!
//! Keyed by the target's name (subsystem NQN, target IQN), process-wide, as
//! the NSID high-water marks are: the shared target, the per-host subsystems,
//! the serve listener and per-volume portals all register here, and the
//! volume listing maps an attachment to its target by name.
//!
//! A host connected to a subsystem reaches every namespace in it (an iSCSI
//! session every LUN of its target), so a volume's remote hosts are the hosts
//! connected to the target that serves it. That is what the target can
//! honestly say; it does not track which namespace a host has read.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

struct Conn {
    protocol: &'static str,
    target: String,
    host: String,
    addr: SocketAddr,
    since: u64,
}

static NEXT: AtomicU64 = AtomicU64::new(1);
static CONNS: Mutex<Option<HashMap<u64, Conn>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<u64, Conn>) -> R) -> R {
    let mut g = CONNS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// One connection's place in the list, removed when dropped.
pub struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        with(|m| m.remove(&self.0));
    }
}

/// Record a live connection: `protocol` is `nvme-tcp` or `iscsi`, `target`
/// the subsystem NQN or target IQN it reached, `host` the host NQN or
/// initiator IQN it named.
pub fn register(protocol: &'static str, target: &str, host: &str, addr: SocketAddr) -> Registered {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let since = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    with(|m| {
        m.insert(id, Conn { protocol, target: target.to_string(), host: host.to_string(), addr, since })
    });
    Registered(id)
}

/// A remote host attached to a target: its connections from one address
/// counted together (an NVMe/TCP host opens an admin queue and several I/O
/// queues, each its own TCP connection).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Remote {
    /// Host NQN (NVMe/TCP) or initiator IQN (iSCSI).
    pub host: String,
    /// The host's address, without the port.
    pub addr: String,
    /// `nvme-tcp` or `iscsi`.
    pub protocol: &'static str,
    /// The subsystem NQN or target IQN it is connected to.
    pub target: String,
    /// Live connections from it.
    pub connections: usize,
    /// When the oldest of them was made, seconds since the epoch.
    pub since: u64,
}

/// Every host connected to `target` right now, sorted.
pub fn on(target: &str) -> Vec<Remote> {
    grouped(Some(target))
}

/// Every host connected to any target of this process right now, sorted.
pub fn all() -> Vec<Remote> {
    grouped(None)
}

fn grouped(target: Option<&str>) -> Vec<Remote> {
    let mut by: HashMap<(String, String, String), Remote> = HashMap::new();
    with(|m| {
        for c in m.values().filter(|c| target.is_none_or(|t| c.target == t)) {
            let key = (c.target.clone(), c.host.clone(), c.addr.ip().to_string());
            let r = by.entry(key.clone()).or_insert_with(|| Remote {
                host: key.1,
                addr: key.2,
                protocol: c.protocol,
                target: c.target.clone(),
                connections: 0,
                since: c.since,
            });
            r.connections += 1;
            r.since = r.since.min(c.since);
        }
    });
    let mut out: Vec<Remote> = by.into_values().collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_listed_while_connected_and_its_queues_are_counted_together() {
        let t = "nqn.test:remote-292";
        let a: SocketAddr = "10.0.0.5:40001".parse().unwrap();
        let b: SocketAddr = "10.0.0.5:40002".parse().unwrap();
        let c: SocketAddr = "10.0.0.9:40003".parse().unwrap();
        let q1 = register("nvme-tcp", t, "nqn.host:b2", a);
        let q2 = register("nvme-tcp", t, "nqn.host:b2", b);
        let other = register("nvme-tcp", t, "nqn.host:b3", c);
        let _elsewhere = register("nvme-tcp", "nqn.test:another-292", "nqn.host:b4", c);
        let list = on(t);
        assert_eq!(list.len(), 2, "{list:?}");
        assert_eq!(list[0].host, "nqn.host:b2");
        assert_eq!(list[0].addr, "10.0.0.5");
        assert_eq!(list[0].connections, 2);
        drop(other);
        assert_eq!(on(t).len(), 1);
        drop(q1);
        drop(q2);
        assert!(on(t).is_empty(), "gone once every connection has ended");
        assert!(all().iter().any(|r| r.host == "nqn.host:b4"), "all() spans every target");
    }
}
