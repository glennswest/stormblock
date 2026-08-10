//! Node auto-discovery and cluster membership.
//!
//! Nodes announce themselves on a UDP multicast group and listen for their
//! peers, so a cluster forms without a seed list or per-node config. Each
//! beacon carries the sender's cluster identity, which means one network can
//! host several independent clusters plus nodes that belong to none yet —
//! those show up as joinable rather than being silently adopted.
//!
//! This is deliberately separate from `src/cluster/` (Raft consensus and
//! replication, an optional feature): discovery has to work on the MikroTik
//! profile too, and the `/v1` placement view depends on it.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Multicast group and port for beacons. Administratively scoped (239/8), so
/// it stays on the local network.
pub const DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
pub const DISCOVERY_PORT: u16 = 7447;

/// Wire format version, so a future change can be recognised rather than
/// silently misparsed.
const BEACON_VERSION: u32 = 1;

/// What a node broadcasts about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beacon {
    pub version: u32,
    pub node_name: String,
    /// Where this node's management API can be reached, `host:port`.
    pub mgmt_addr: String,
    /// The cluster this node belongs to, if any. `None` means unclustered and
    /// available to join.
    pub cluster_id: Option<Uuid>,
    pub cluster_name: Option<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Engine version, so a mixed-version cluster is visible in the UI.
    pub engine_version: String,
}

/// A peer we have heard from.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredNode {
    #[serde(flatten)]
    pub beacon: Beacon,
    /// Seconds since the last beacon.
    pub age_secs: u64,
    /// True once beacons stop arriving — kept visible in the UI, but excluded
    /// from placement.
    pub stale: bool,
}

/// This node's cluster identity, persisted so it survives a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterIdentity {
    pub cluster_id: Option<Uuid>,
    pub cluster_name: Option<String>,
}

impl ClusterIdentity {
    pub fn is_clustered(&self) -> bool {
        self.cluster_id.is_some()
    }
}

/// Peer table plus this node's cluster identity.
pub struct Discovery {
    node_name: String,
    mgmt_addr: String,
    identity: RwLock<ClusterIdentity>,
    peers: RwLock<HashMap<String, (Beacon, Instant)>>,
    identity_path: Option<PathBuf>,
    /// A peer unheard from for longer than this is treated as gone.
    stale_after: Duration,
}

impl Discovery {
    pub fn new(
        node_name: String,
        mgmt_addr: String,
        data_dir: Option<PathBuf>,
        stale_after: Duration,
    ) -> Self {
        let identity_path = data_dir.map(|d| d.join("cluster_identity.json"));
        let identity = identity_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<ClusterIdentity>(&b).ok())
            .unwrap_or_default();

        if let Some(name) = &identity.cluster_name {
            tracing::info!("node '{node_name}' belongs to cluster '{name}'");
        } else {
            tracing::info!("node '{node_name}' is unclustered");
        }

        Discovery {
            node_name,
            mgmt_addr,
            identity: RwLock::new(identity),
            peers: RwLock::new(HashMap::new()),
            identity_path,
            stale_after,
        }
    }

    pub async fn identity(&self) -> ClusterIdentity {
        self.identity.read().await.clone()
    }

    /// Form a new cluster with this node as its founding member.
    pub async fn create_cluster(&self, name: &str) -> ClusterIdentity {
        let ident = ClusterIdentity {
            cluster_id: Some(Uuid::new_v4()),
            cluster_name: Some(name.to_string()),
        };
        self.set_identity(ident.clone()).await;
        tracing::info!("created cluster '{name}' ({})", ident.cluster_id.unwrap());
        ident
    }

    /// Adopt another cluster's identity.
    ///
    /// Joining is a local decision recorded on this node — the beacon then
    /// advertises the new identity and peers pick it up on their next sweep.
    /// There is no remote approval step, which keeps a join from depending on
    /// any one node being reachable.
    pub async fn join_cluster(&self, cluster_id: Uuid, name: &str) -> ClusterIdentity {
        let ident = ClusterIdentity {
            cluster_id: Some(cluster_id),
            cluster_name: Some(name.to_string()),
        };
        self.set_identity(ident.clone()).await;
        tracing::info!("joined cluster '{name}' ({cluster_id})");
        ident
    }

    /// Leave the current cluster, becoming unclustered and joinable again.
    pub async fn leave_cluster(&self) {
        let previous = self.identity.read().await.cluster_name.clone();
        self.set_identity(ClusterIdentity::default()).await;
        if let Some(name) = previous {
            tracing::info!("left cluster '{name}'");
        }
    }

    async fn set_identity(&self, ident: ClusterIdentity) {
        *self.identity.write().await = ident.clone();
        if let Some(path) = &self.identity_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_vec_pretty(&ident) {
                Ok(bytes) => {
                    let tmp = path.with_extension("json.tmp");
                    if std::fs::write(&tmp, bytes)
                        .and_then(|_| std::fs::rename(&tmp, path))
                        .is_err()
                    {
                        tracing::warn!("failed to persist cluster identity");
                    }
                }
                Err(e) => tracing::warn!("failed to serialize cluster identity: {e}"),
            }
        }
    }

    /// Every node we have heard from, newest information first.
    pub async fn nodes(&self) -> Vec<DiscoveredNode> {
        let peers = self.peers.read().await;
        let mut out: Vec<DiscoveredNode> = peers
            .values()
            .map(|(b, seen)| DiscoveredNode {
                beacon: b.clone(),
                age_secs: seen.elapsed().as_secs(),
                stale: seen.elapsed() > self.stale_after,
            })
            .collect();
        out.sort_by(|a, b| a.beacon.node_name.cmp(&b.beacon.node_name));
        out
    }

    /// Live peers sharing this node's cluster.
    ///
    /// This is what placement consults, so it excludes stale peers and anyone
    /// in another cluster. An unclustered node has no peers by definition —
    /// adopting strangers because they happen to share a network is exactly
    /// what the cluster id prevents.
    pub async fn cluster_peers(&self) -> Vec<Beacon> {
        let Some(cid) = self.identity.read().await.cluster_id else {
            return Vec::new();
        };
        self.peers
            .read()
            .await
            .values()
            .filter(|(b, seen)| b.cluster_id == Some(cid) && seen.elapsed() <= self.stale_after)
            .map(|(b, _)| b.clone())
            .collect()
    }

    async fn record(&self, beacon: Beacon) {
        if beacon.node_name == self.node_name {
            return; // our own announcement, reflected back by the switch
        }
        self.peers
            .write()
            .await
            .insert(beacon.node_name.clone(), (beacon, Instant::now()));
    }

    async fn beacon_now(&self, total_bytes: u64, free_bytes: u64) -> Beacon {
        let ident = self.identity.read().await;
        Beacon {
            version: BEACON_VERSION,
            node_name: self.node_name.clone(),
            mgmt_addr: self.mgmt_addr.clone(),
            cluster_id: ident.cluster_id,
            cluster_name: ident.cluster_name.clone(),
            total_bytes,
            free_bytes,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Start the announce and listen loops.
///
/// Failure to join the multicast group is logged and tolerated rather than
/// fatal: a node that cannot discover peers must still serve its own volumes,
/// and some networks block multicast entirely.
pub fn spawn(
    discovery: Arc<Discovery>,
    state: Arc<crate::mgmt::AppState>,
    announce_every: Duration,
) {
    let listener = discovery.clone();
    tokio::spawn(async move {
        if let Err(e) = listen_loop(listener).await {
            tracing::warn!("discovery listener stopped: {e}");
        }
    });

    tokio::spawn(async move {
        if let Err(e) = announce_loop(discovery, state, announce_every).await {
            tracing::warn!("discovery announcer stopped: {e}");
        }
    });
}

async fn listen_loop(discovery: Arc<Discovery>) -> std::io::Result<()> {
    let socket = bind_multicast()?;
    tracing::info!(
        "discovery listening on {}:{}",
        DISCOVERY_GROUP,
        DISCOVERY_PORT
    );

    let mut buf = vec![0u8; 4096];
    loop {
        let (len, _from) = socket.recv_from(&mut buf).await?;
        match serde_json::from_slice::<Beacon>(&buf[..len]) {
            Ok(b) if b.version == BEACON_VERSION => discovery.record(b).await,
            Ok(b) => tracing::debug!("ignoring beacon version {}", b.version),
            Err(e) => tracing::debug!("malformed beacon: {e}"),
        }
    }
}

async fn announce_loop(
    discovery: Arc<Discovery>,
    state: Arc<crate::mgmt::AppState>,
    every: Duration,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?;
    socket.set_multicast_loop_v4(false)?;
    let dest = SocketAddrV4::new(DISCOVERY_GROUP, DISCOVERY_PORT);

    let mut ticker = tokio::time::interval(every);
    loop {
        ticker.tick().await;
        let (total, free) = local_capacity(&state).await;
        let beacon = discovery.beacon_now(total, free).await;
        match serde_json::to_vec(&beacon) {
            Ok(bytes) => {
                if let Err(e) = socket.send_to(&bytes, dest).await {
                    tracing::debug!("beacon send failed: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to serialize beacon: {e}"),
        }
    }
}

/// Capacity across this node's slabs, as advertised to peers.
async fn local_capacity(state: &crate::mgmt::AppState) -> (u64, u64) {
    let reg = state.slab_registry.read().await;
    let mut total = 0u64;
    let mut free = 0u64;
    for (_, slab) in reg.iter() {
        let slot = slab.slot_size();
        total += slab.total_slots() * slot;
        free += slab.free_slots() * slot;
    }
    (total, free)
}

/// Bind the multicast group with SO_REUSEADDR, so several nodes on one host
/// (and the test suite) can listen at once.
fn bind_multicast() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.bind(&SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT)).into())?;
    sock.join_multicast_v4(&DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    sock.set_nonblocking(true)?;

    UdpSocket::from_std(std::net::UdpSocket::from(sock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("sb-disc-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn disc(name: &str, dir: Option<PathBuf>) -> Discovery {
        Discovery::new(name.into(), "127.0.0.1:9090".into(), dir, Duration::from_secs(30))
    }

    #[tokio::test]
    async fn starts_unclustered_and_creating_assigns_an_identity() {
        let d = disc("n1", None);
        assert!(!d.identity().await.is_clustered());

        let ident = d.create_cluster("prod").await;
        assert!(ident.is_clustered());
        assert_eq!(ident.cluster_name.as_deref(), Some("prod"));
        assert!(d.identity().await.is_clustered());
    }

    /// Cluster membership must survive a restart, or a reboot would silently
    /// drop a node out of its cluster.
    #[tokio::test]
    async fn identity_is_persisted_and_reloaded() {
        let dir = ident_dir();
        let id = {
            let d = disc("n1", Some(dir.clone()));
            d.create_cluster("prod").await.cluster_id.unwrap()
        };

        let reloaded = disc("n1", Some(dir.clone()));
        let ident = reloaded.identity().await;
        assert_eq!(ident.cluster_id, Some(id), "identity must survive restart");
        assert_eq!(ident.cluster_name.as_deref(), Some("prod"));

        reloaded.leave_cluster().await;
        let after = disc("n1", Some(dir.clone()));
        assert!(!after.identity().await.is_clustered(), "leaving must persist too");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Several clusters share one network, so placement must only ever see
    /// peers from its own — and never a node that has not joined one.
    #[tokio::test]
    async fn cluster_peers_are_filtered_by_cluster() {
        let d = disc("n1", None);
        let ident = d.create_cluster("prod").await;
        let other = Uuid::new_v4();

        let mk = |name: &str, cid: Option<Uuid>, cname: Option<&str>| Beacon {
            version: BEACON_VERSION,
            node_name: name.into(),
            mgmt_addr: "127.0.0.1:9090".into(),
            cluster_id: cid,
            cluster_name: cname.map(|s| s.into()),
            total_bytes: 100,
            free_bytes: 50,
            engine_version: "test".into(),
        };

        d.record(mk("same", ident.cluster_id, Some("prod"))).await;
        d.record(mk("other", Some(other), Some("staging"))).await;
        d.record(mk("loner", None, None)).await;

        let peers = d.cluster_peers().await;
        assert_eq!(peers.len(), 1, "only same-cluster peers count");
        assert_eq!(peers[0].node_name, "same");

        // All three remain visible for the UI to offer as join targets.
        assert_eq!(d.nodes().await.len(), 3);
    }

    /// A node's own beacon comes back off the network; recording it would make
    /// it its own peer and inflate placement.
    #[tokio::test]
    async fn own_beacon_is_ignored() {
        let d = disc("n1", None);
        d.create_cluster("prod").await;
        let self_beacon = d.beacon_now(10, 5).await;
        d.record(self_beacon).await;
        assert!(d.nodes().await.is_empty());
        assert!(d.cluster_peers().await.is_empty());
    }

    #[tokio::test]
    async fn unclustered_node_has_no_peers() {
        let d = disc("n1", None);
        let mk = Beacon {
            version: BEACON_VERSION,
            node_name: "other".into(),
            mgmt_addr: "127.0.0.1:9090".into(),
            cluster_id: Some(Uuid::new_v4()),
            cluster_name: Some("prod".into()),
            total_bytes: 100,
            free_bytes: 50,
            engine_version: "test".into(),
        };
        d.record(mk).await;
        assert!(
            d.cluster_peers().await.is_empty(),
            "an unclustered node must not adopt peers it merely shares a network with"
        );
        assert_eq!(d.nodes().await.len(), 1, "but it can still see them to join");
    }

    #[tokio::test]
    async fn stale_peers_drop_out_of_placement_but_stay_visible() {
        let d = Discovery::new(
            "n1".into(),
            "127.0.0.1:9090".into(),
            None,
            Duration::from_millis(50),
        );
        let ident = d.create_cluster("prod").await;
        d.record(Beacon {
            version: BEACON_VERSION,
            node_name: "peer".into(),
            mgmt_addr: "127.0.0.1:9091".into(),
            cluster_id: ident.cluster_id,
            cluster_name: Some("prod".into()),
            total_bytes: 100,
            free_bytes: 50,
            engine_version: "test".into(),
        })
        .await;

        assert_eq!(d.cluster_peers().await.len(), 1);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(d.cluster_peers().await.is_empty(), "stale peer must not take placements");
        let nodes = d.nodes().await;
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].stale, "but it stays visible, marked stale");
    }
}
