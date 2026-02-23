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
        ConnectionState {
            cid,
            stat_sn: AtomicU32::new(1),
            exp_cmd_sn: AtomicU32::new(1),
            max_cmd_sn: AtomicU32::new(32), // window of 32 commands
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

impl SessionRegistry {
    pub fn new() -> Self {
        SessionRegistry {
            sessions: RwLock::new(HashMap::new()),
            next_tsih: AtomicU16::new(1),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
