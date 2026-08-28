//! Heartbeat — periodic health pings between cluster peers.
//!
//! # Why the round is concurrent
//!
//! A round used to probe peers one at a time, awaiting each response before
//! starting the next. That costs `N × RTT` per node, and — far worse for a
//! failure detector — it means **one hung peer stalls every peer behind it in
//! the list**. The condition the detector exists to notice is precisely the one
//! that makes it slowest, and a healthy node's detection latency degrades in
//! proportion to how many unhealthy ones happen to sort before it (#41).
//!
//! Probes now go out together, bounded by a semaphore so a large fleet does not
//! open a thousand sockets at once, and each carries its own deadline rather
//! than inheriting the HTTP client's (10 s — ten heartbeat intervals). A round
//! costs about one RTT plus the slowest *permitted* probe, and a dead peer costs
//! one probe timeout regardless of where it sits in the list.
//!
//! This is still `O(N²)` requests fleet-wide per interval; that is what a gossip
//! failure detector fixes, and it is tracked separately (#42).

use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, Deserialize};
use tokio::sync::{RwLock, Semaphore};

use super::membership::{MembershipStore, NodeInfo};

/// Heartbeat request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub capacity_bytes: u64,
    pub drives_count: usize,
    pub arrays_count: usize,
    pub volumes_count: usize,
}

/// Heartbeat response payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub node_id: u64,
    pub hostname: String,
    pub status: String,
}

/// How a round is paced.
#[derive(Debug, Clone)]
pub struct HeartbeatTuning {
    /// How often a round starts.
    pub interval: Duration,
    /// How long any one probe may take. A round is bounded by this rather than
    /// by the HTTP client's timeout, so an unreachable peer costs one deadline
    /// and not a tenth of a minute.
    pub probe_timeout: Duration,
    /// How many probes may be in flight at once. A cap rather than a target:
    /// small clusters never reach it, and a 1000-node fleet does not open 1000
    /// sockets in the same millisecond.
    pub max_in_flight: usize,
}

impl HeartbeatTuning {
    /// Sensible pacing for a given interval: a probe gets the whole interval to
    /// answer, with a floor for the sub-second intervals a small cluster uses.
    ///
    /// The suspicion thresholds are counted in missed rounds, so a probe that
    /// outlives its interval would let rounds overlap and make "missed" mean
    /// something else.
    pub fn for_interval(interval: Duration) -> Self {
        HeartbeatTuning {
            interval,
            probe_timeout: interval.max(Duration::from_millis(500)),
            max_in_flight: 64,
        }
    }
}

/// What one probe came back as.
struct Probe {
    peer_id: u64,
    peer_addr: String,
    /// `None` for any failure — refused, non-2xx, unparseable, or timed out.
    /// The failure detector does not distinguish; the log line does.
    response: Option<HeartbeatResponse>,
    failure: Option<String>,
}

/// Probe every peer at once and collect what came back.
///
/// Takes no locks: the membership store is read before the round and written
/// after it, so a slow round never holds the lock the API and Raft also want.
async fn probe_peers(
    peers: Vec<(u64, String)>,
    req: Arc<HeartbeatRequest>,
    client: crate::http::Client,
    url_scheme: Arc<String>,
    tuning: &HeartbeatTuning,
) -> Vec<Probe> {
    let permits = Arc::new(Semaphore::new(tuning.max_in_flight.max(1)));
    let timeout = tuning.probe_timeout;
    let mut set = tokio::task::JoinSet::new();

    for (peer_id, peer_addr) in peers {
        let permits = permits.clone();
        let req = req.clone();
        let client = client.clone();
        let scheme = url_scheme.clone();
        set.spawn(async move {
            // If the semaphore is closed there is nothing to probe with.
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    return Probe {
                        peer_id,
                        peer_addr,
                        response: None,
                        failure: Some("heartbeat pool closed".to_string()),
                    }
                }
            };
            let url = format!("{scheme}://{peer_addr}/api/v1/cluster/heartbeat");
            let attempt = async {
                let resp = client.post(&url).json(req.as_ref()).send().await?;
                if !resp.status().is_success() {
                    return Err(HeartbeatProbeError::Status(resp.status().as_u16()));
                }
                resp.json::<HeartbeatResponse>().await.map_err(HeartbeatProbeError::from)
            };
            match tokio::time::timeout(timeout, attempt).await {
                Ok(Ok(response)) => Probe { peer_id, peer_addr, response: Some(response), failure: None },
                Ok(Err(e)) => Probe {
                    peer_id,
                    peer_addr,
                    response: None,
                    failure: Some(e.to_string()),
                },
                Err(_) => Probe {
                    peer_id,
                    peer_addr,
                    response: None,
                    failure: Some(format!("no answer within {timeout:?}")),
                },
            }
        });
    }

    let mut probes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(p) => probes.push(p),
            // A probe task that panicked is a failed probe, not a lost round.
            Err(e) => tracing::warn!("heartbeat probe task failed: {e}"),
        }
    }
    probes
}

/// Why a probe did not produce a response.
#[derive(Debug)]
enum HeartbeatProbeError {
    Http(crate::http::Error),
    Status(u16),
}

impl From<crate::http::Error> for HeartbeatProbeError {
    fn from(e: crate::http::Error) -> Self {
        HeartbeatProbeError::Http(e)
    }
}

impl std::fmt::Display for HeartbeatProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeartbeatProbeError::Http(e) => write!(f, "{e}"),
            HeartbeatProbeError::Status(s) => write!(f, "HTTP {s}"),
        }
    }
}

/// Run one round: probe every peer concurrently, then apply the outcome to the
/// membership store under a single write lock.
///
/// Returns how long the round took, which is also what the metric records.
async fn heartbeat_round(
    local: &NodeInfo,
    membership: &Arc<RwLock<MembershipStore>>,
    client: &crate::http::Client,
    url_scheme: &Arc<String>,
    tuning: &HeartbeatTuning,
) -> Duration {
    let started = std::time::Instant::now();

    let peers: Vec<(u64, String)> = {
        let store = membership.read().await;
        store
            .list_nodes()
            .iter()
            .filter(|(info, _)| info.node_id != local.node_id)
            .map(|(info, _)| (info.node_id, info.mgmt_addr.clone()))
            .collect()
    };
    if peers.is_empty() {
        return started.elapsed();
    }

    let req = Arc::new(HeartbeatRequest {
        node_id: local.node_id,
        hostname: local.hostname.clone(),
        mgmt_addr: local.mgmt_addr.clone(),
        capacity_bytes: local.capacity_bytes,
        drives_count: local.drives_count,
        arrays_count: local.arrays_count,
        volumes_count: local.volumes_count,
    });

    let probes =
        probe_peers(peers, req, client.clone(), url_scheme.clone(), tuning).await;

    // One write lock for the whole round rather than one per peer: the round is
    // now concurrent, so N acquisitions would be N chances to interleave with a
    // membership change mid-round.
    let mut store = membership.write().await;
    let (mut ok, mut failed) = (0u64, 0u64);
    for probe in probes {
        match probe.response {
            Some(hb) => {
                store.heartbeat_success(
                    probe.peer_id,
                    NodeInfo {
                        node_id: probe.peer_id,
                        hostname: hb.hostname,
                        mgmt_addr: probe.peer_addr,
                        // Filled in from the peer's own heartbeat to us.
                        capacity_bytes: 0,
                        drives_count: 0,
                        arrays_count: 0,
                        volumes_count: 0,
                    },
                );
                ok += 1;
            }
            None => {
                store.heartbeat_failure(probe.peer_id);
                failed += 1;
                tracing::warn!(
                    "heartbeat to node {} ({}) failed: {}",
                    probe.peer_id,
                    probe.peer_addr,
                    probe.failure.as_deref().unwrap_or("unknown")
                );
            }
        }
    }
    drop(store);

    if ok > 0 {
        metrics::counter!("stormblock_cluster_heartbeat_success_total").increment(ok);
    }
    if failed > 0 {
        metrics::counter!("stormblock_cluster_heartbeat_failures_total").increment(failed);
    }
    started.elapsed()
}

/// Start the heartbeat background task.
/// Periodically pings all known peers and updates their health status.
pub fn start_heartbeat(
    local_info: NodeInfo,
    membership: Arc<RwLock<MembershipStore>>,
    tuning: HeartbeatTuning,
    membership_path: std::path::PathBuf,
    client: crate::http::Client,
    url_scheme: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let url_scheme = Arc::new(url_scheme);
        let mut ticker = tokio::time::interval(tuning.interval);
        // A round that overruns its interval must not queue up rounds behind
        // it; skipping to the next tick keeps the cadence the thresholds count.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let elapsed =
                heartbeat_round(&local_info, &membership, &client, &url_scheme, &tuning).await;
            metrics::histogram!("stormblock_cluster_heartbeat_round_seconds")
                .record(elapsed.as_secs_f64());
            if elapsed > tuning.interval {
                tracing::warn!(
                    "heartbeat round took {elapsed:?}, longer than the {:?} interval",
                    tuning.interval
                );
            }

            {
                let store = membership.read().await;
                metrics::gauge!("stormblock_cluster_nodes_total").set(store.node_count() as f64);
                metrics::gauge!("stormblock_cluster_nodes_online").set(store.online_count() as f64);
                if let Err(e) = store.persist(&membership_path) {
                    tracing::warn!("failed to persist membership: {e}");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, addr: &str) -> NodeInfo {
        NodeInfo {
            node_id: id,
            hostname: format!("node-{id}"),
            mgmt_addr: addr.to_string(),
            capacity_bytes: 0,
            drives_count: 0,
            arrays_count: 0,
            volumes_count: 0,
        }
    }

    /// A peer that answers, and a peer that never does.
    ///
    /// `hang` accepts the connection and then holds it open, which is what a
    /// wedged node looks like from here — worse than a refused connection,
    /// because nothing comes back to end the wait early.
    async fn responder(hang: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                if hang {
                    // Hold the connection and answer nothing, forever.
                    tokio::spawn(async move {
                        let mut sink = Vec::new();
                        let _ = tokio::io::AsyncReadExt::read_buf(&mut sock, &mut sink).await;
                        std::future::pending::<()>().await;
                    });
                    continue;
                }
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let body = serde_json::json!({
                        "node_id": 0,
                        "hostname": "peer",
                        "status": "ok"
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        addr
    }

    /// One hung peer must not delay the peers behind it (#41).
    ///
    /// The hung peer is listed *first*, which under the old sequential round
    /// meant every healthy peer waited out its timeout before being probed at
    /// all. The round is now bounded by one probe timeout, not by their sum.
    #[tokio::test]
    async fn a_hung_peer_does_not_stall_the_healthy_ones() {
        let hung = responder(true).await;
        let mut healthy = Vec::new();
        for _ in 0..4 {
            healthy.push(responder(false).await);
        }

        let mut store = MembershipStore::new(2, 4);
        store.add_node(node(1, "127.0.0.1:1"));
        store.add_node(node(2, &hung));
        for (i, addr) in healthy.iter().enumerate() {
            store.add_node(node(3 + i as u64, addr));
        }
        let membership = Arc::new(RwLock::new(store));

        let tuning = HeartbeatTuning {
            interval: Duration::from_millis(1000),
            probe_timeout: Duration::from_millis(400),
            max_in_flight: 64,
        };
        let client = crate::http::Client::builder()
            // The client's own timeout is deliberately far longer than the
            // probe deadline, which is the situation in production: the round
            // must be bounded by its own pacing, not by the client's.
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let started = std::time::Instant::now();
        let took = heartbeat_round(
            &node(1, "127.0.0.1:1"),
            &membership,
            &client,
            &Arc::new("http".to_string()),
            &tuning,
        )
        .await;
        let wall = started.elapsed();

        // Sequentially this would be one 400 ms timeout plus four round trips;
        // concurrently it is the timeout and nothing else worth counting.
        assert!(
            wall < Duration::from_millis(1200),
            "round took {wall:?} — a hung peer is still stalling the others"
        );
        assert!(took <= wall);

        let store = membership.read().await;
        for id in 3..7u64 {
            assert_eq!(
                store.missed_heartbeats(id),
                Some(0),
                "healthy peer {id} was recorded as failing"
            );
        }
        assert_eq!(
            store.missed_heartbeats(2),
            Some(1),
            "the hung peer is the one that failed"
        );
    }

    /// A round with no peers is free, and does not touch the store.
    #[tokio::test]
    async fn an_empty_round_costs_nothing() {
        let mut store = MembershipStore::new(2, 4);
        store.add_node(node(1, "127.0.0.1:1"));
        let membership = Arc::new(RwLock::new(store));
        let took = heartbeat_round(
            &node(1, "127.0.0.1:1"),
            &membership,
            &crate::http::Client::new(),
            &Arc::new("http".to_string()),
            &HeartbeatTuning::for_interval(Duration::from_millis(100)),
        )
        .await;
        assert!(took < Duration::from_millis(50), "{took:?}");
    }

    /// The probe deadline follows the interval, with a floor for the sub-second
    /// intervals a small cluster runs — a probe that outlives its interval
    /// would make "missed rounds" mean something other than what the suspicion
    /// thresholds count.
    #[test]
    fn probe_deadline_tracks_the_interval() {
        let fast = HeartbeatTuning::for_interval(Duration::from_millis(100));
        assert_eq!(fast.probe_timeout, Duration::from_millis(500));
        let slow = HeartbeatTuning::for_interval(Duration::from_secs(3));
        assert_eq!(slow.probe_timeout, Duration::from_secs(3));
    }
}
