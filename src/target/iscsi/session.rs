//! iSCSI session and connection state — TSIH allocation, sequence number tracking.
//!
//! Reference: RFC 7143 §7

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

/// Target Session Identifying Handle — unique per session.
pub type Tsih = u16;

/// Negotiated session parameters after login.
#[derive(Debug, Clone)]
pub struct SessionParams {
    pub initiator_name: String,
    pub target_name: String,
    pub max_recv_data_segment_length: u32,
    pub max_burst_length: u32,
    pub first_burst_length: u32,
    pub initial_r2t: bool,
    pub immediate_data: bool,
    pub header_digest: bool,
    pub data_digest: bool,
    pub max_connections: u32,
    pub max_outstanding_r2t: u32,
    /// True when the initiator declared `SessionType=Discovery`. Such a
    /// session only does SendTargets — it never addresses a LUN.
    pub discovery_session: bool,
}

impl Default for SessionParams {
    fn default() -> Self {
        SessionParams {
            initiator_name: String::new(),
            target_name: String::new(),
            max_recv_data_segment_length: 8192,
            max_burst_length: 262144,
            first_burst_length: 65536,
            initial_r2t: true,
            immediate_data: true,
            header_digest: false,
            data_digest: false,
            max_connections: 1,
            max_outstanding_r2t: 1,
            discovery_session: false,
        }
    }
}

/// Per-connection state tracking CmdSN/StatSN windows.
pub struct ConnectionState {
    pub cid: u16,
    pub stat_sn: AtomicU32,
    pub exp_cmd_sn: AtomicU32,
    pub max_cmd_sn: AtomicU32,
}

impl ConnectionState {
    pub fn new(cid: u16) -> Self {
        Self::with_sns(cid, 1, 1)
    }

    /// Create a connection continuing the sequence numbers negotiated during
    /// login: `stat_sn` is the StatSN for the first full-feature response,
    /// `exp_cmd_sn` the CmdSN expected from the initiator's first command.
    pub fn with_sns(cid: u16, stat_sn: u32, exp_cmd_sn: u32) -> Self {
        ConnectionState {
            cid,
            stat_sn: AtomicU32::new(stat_sn),
            exp_cmd_sn: AtomicU32::new(exp_cmd_sn),
            max_cmd_sn: AtomicU32::new(exp_cmd_sn.wrapping_add(31)), // window of 32 commands
        }
    }

    pub fn next_stat_sn(&self) -> u32 {
        self.stat_sn.fetch_add(1, Ordering::Relaxed)
    }

    pub fn advance_cmd_sn(&self, cmd_sn: u32) {
        // Advance ExpCmdSN if this is the expected command
        let exp = self.exp_cmd_sn.load(Ordering::Relaxed);
        if cmd_sn == exp {
            self.exp_cmd_sn.store(exp.wrapping_add(1), Ordering::Relaxed);
            self.max_cmd_sn.store(exp.wrapping_add(32), Ordering::Relaxed);
        }
    }
}

/// A serialisable view of one active session, for the management API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub tsih: Tsih,
    /// Initiator session ID, hex — identifies the initiator across reconnects.
    pub isid: String,
    pub initiator_name: String,
    pub target_name: String,
    /// A discovery session only does SendTargets; it never holds a LUN open.
    pub discovery: bool,
    pub connections: usize,
}

/// An active iSCSI session (may have multiple connections per RFC 7143 §7).
pub struct Session {
    pub tsih: Tsih,
    pub isid: [u8; 6],
    pub params: SessionParams,
    pub connections: RwLock<HashMap<u16, Arc<ConnectionState>>>,
}

impl Session {
    /// Register a new connection for this session.
    pub async fn add_connection(&self, cid: u16) -> Arc<ConnectionState> {
        let conn = Arc::new(ConnectionState::new(cid));
        self.connections.write().await.insert(cid, conn.clone());
        conn
    }

    /// Register an already-built connection, so the full-feature phase can
    /// keep the sequence numbers it seeded from login while still being
    /// visible in the session's connection count.
    pub async fn register_connection(&self, cid: u16, conn: Arc<ConnectionState>) {
        self.connections.write().await.insert(cid, conn);
    }

    /// Remove a connection from this session.
    pub async fn remove_connection(&self, cid: u16) {
        self.connections.write().await.remove(&cid);
    }

    /// Get the connection count.
    pub async fn connection_count(&self) -> usize {
        self.connections.read().await.len()
    }
}

/// Registry of active iSCSI sessions.
pub struct SessionRegistry {
    sessions: RwLock<HashMap<Tsih, Arc<Session>>>,
    next_tsih: AtomicU16,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        SessionRegistry {
            sessions: RwLock::new(HashMap::new()),
            next_tsih: AtomicU16::new(1),
        }
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a TSIH and register a new session.
    pub async fn create_session(&self, isid: [u8; 6], params: SessionParams) -> Arc<Session> {
        let tsih = self.next_tsih.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(Session {
            tsih,
            isid,
            params,
            connections: RwLock::new(HashMap::new()),
        });
        self.sessions.write().await.insert(tsih, session.clone());
        session
    }

    /// Find an existing session by ISID for multi-connection login.
    pub async fn find_by_isid(&self, isid: &[u8; 6]) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.values().find(|s| &s.isid == isid).cloned()
    }

    /// Look up a session by TSIH.
    pub async fn get_session(&self, tsih: Tsih) -> Option<Arc<Session>> {
        self.sessions.read().await.get(&tsih).cloned()
    }

    /// Remove a session.
    pub async fn remove_session(&self, tsih: Tsih) {
        self.sessions.write().await.remove(&tsih);
    }

    /// Number of active sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Point-in-time view of every active session.
    ///
    /// Consumers need this to know when a LUN is safe to withdraw: without it
    /// a teardown has to guess with a drain timer and can pull an export out
    /// from under a live mount.
    pub async fn snapshot(&self) -> Vec<SessionInfo> {
        let sessions: Vec<Arc<Session>> =
            self.sessions.read().await.values().cloned().collect();

        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            out.push(SessionInfo {
                tsih: s.tsih,
                isid: format!(
                    "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    s.isid[0], s.isid[1], s.isid[2], s.isid[3], s.isid[4], s.isid[5]
                ),
                initiator_name: s.params.initiator_name.clone(),
                target_name: s.params.target_name.clone(),
                discovery: s.params.discovery_session,
                connections: s.connection_count().await,
            });
        }
        out.sort_by_key(|s| s.tsih);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Consumers need to distinguish sessions that hold a LUN open from
    /// discovery sessions, which never address one — withdrawing an export on
    /// a discovery-only count would pull it from under a live mount (#29).
    #[tokio::test]
    async fn snapshot_reports_sessions_and_separates_discovery() {
        let registry = SessionRegistry::new();
        assert!(registry.snapshot().await.is_empty());

        let normal = SessionParams {
            initiator_name: "iqn.1994-05.com.redhat:host1".into(),
            target_name: "iqn.2024.io.stormblock:vol1".into(),
            ..Default::default()
        };
        let disc = SessionParams {
            initiator_name: "iqn.1994-05.com.redhat:host1".into(),
            discovery_session: true,
            ..Default::default()
        };
        let s1 = registry.create_session([0x40, 0, 0, 0, 0, 1], normal).await;
        registry.create_session([0x40, 0, 0, 0, 0, 2], disc).await;
        s1.add_connection(0).await;

        let snap = registry.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(registry.session_count().await, 2);

        let active: Vec<_> = snap.iter().filter(|s| !s.discovery).collect();
        assert_eq!(active.len(), 1, "only the normal session holds the target");
        assert_eq!(active[0].initiator_name, "iqn.1994-05.com.redhat:host1");
        assert_eq!(active[0].target_name, "iqn.2024.io.stormblock:vol1");
        assert_eq!(active[0].connections, 1);
        assert_eq!(active[0].isid.len(), 12, "ISID rendered as hex");

        // Once it logs out, nothing is holding the export open.
        registry.remove_session(s1.tsih).await;
        let snap = registry.snapshot().await;
        assert_eq!(snap.iter().filter(|s| !s.discovery).count(), 0);
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let registry = SessionRegistry::new();

        let params = SessionParams {
            initiator_name: "iqn.2024.com.test:init".into(),
            target_name: "iqn.2024.com.stormblock:disk1".into(),
            ..Default::default()
        };
        let session = registry.create_session([0x40, 0, 0, 0, 0, 1], params).await;
        let tsih = session.tsih;
        assert!(tsih > 0);

        let found = registry.get_session(tsih).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().params.initiator_name, "iqn.2024.com.test:init");

        registry.remove_session(tsih).await;
        assert!(registry.get_session(tsih).await.is_none());
    }

    #[test]
    fn connection_state_sn_tracking() {
        let conn = ConnectionState::new(1);
        assert_eq!(conn.next_stat_sn(), 1);
        assert_eq!(conn.next_stat_sn(), 2);
        assert_eq!(conn.next_stat_sn(), 3);

        conn.advance_cmd_sn(1);
        assert_eq!(conn.exp_cmd_sn.load(Ordering::Relaxed), 2);
    }
}
