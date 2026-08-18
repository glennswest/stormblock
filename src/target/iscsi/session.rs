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
/// How many commands the initiator may have outstanding at once.
const CMD_WINDOW: u32 = 32;

/// The command-sequence window, which belongs to the **session** and not to
/// any one connection (RFC 7143 §4.2.2.1).
///
/// This is the part of MC/S that has to be right before anything else can be.
/// CmdSN is assigned by the initiator per *session* and may arrive on any
/// connection of it; StatSN is per *connection*. Tracking ExpCmdSN per
/// connection — as this did while only one connection was ever allowed — makes
/// each connection advertise its own command window, so an initiator with two
/// connections is told two different things about one session's flow control
/// and its commands are acknowledged out of order.
///
/// Shared by `Arc` rather than looked up through the session, so the response
/// path stays a couple of atomic loads with no lock.
#[derive(Debug)]
pub struct CmdSnWindow {
    exp_cmd_sn: AtomicU32,
    max_cmd_sn: AtomicU32,
}

impl CmdSnWindow {
    pub fn new(exp_cmd_sn: u32) -> Self {
        CmdSnWindow {
            exp_cmd_sn: AtomicU32::new(exp_cmd_sn),
            max_cmd_sn: AtomicU32::new(exp_cmd_sn.wrapping_add(CMD_WINDOW - 1)),
        }
    }

    pub fn exp_cmd_sn(&self) -> u32 {
        self.exp_cmd_sn.load(Ordering::Relaxed)
    }

    pub fn max_cmd_sn(&self) -> u32 {
        self.max_cmd_sn.load(Ordering::Relaxed)
    }

    /// Seed the window to where login left it, before any command arrives.
    pub fn advance_to(&self, exp_cmd_sn: u32) {
        self.exp_cmd_sn.store(exp_cmd_sn, Ordering::Relaxed);
        self.max_cmd_sn.store(exp_cmd_sn.wrapping_add(CMD_WINDOW - 1), Ordering::Relaxed);
    }

    /// Acknowledge a command. Only the expected one advances the window, so a
    /// command that arrives early on a second connection waits for its
    /// predecessor rather than opening a hole in the sequence.
    ///
    /// `compare_exchange` rather than load-then-store: with MC/S two
    /// connections can acknowledge concurrently, and a read-modify-write that
    /// is not atomic would let the window advance twice for one command.
    pub fn advance(&self, cmd_sn: u32) {
        let exp = self.exp_cmd_sn.load(Ordering::Relaxed);
        if cmd_sn != exp {
            return;
        }
        if self
            .exp_cmd_sn
            .compare_exchange(exp, exp.wrapping_add(1), Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.max_cmd_sn.store(exp.wrapping_add(CMD_WINDOW), Ordering::Relaxed);
        }
    }
}

pub struct ConnectionState {
    pub cid: u16,
    pub stat_sn: AtomicU32,
    /// The session's command window, shared with every other connection on it.
    pub cmd_sn: Arc<CmdSnWindow>,
}

impl ConnectionState {
    pub fn new(cid: u16) -> Self {
        Self::with_sns(cid, 1, Arc::new(CmdSnWindow::new(1)))
    }

    /// Create a connection continuing the sequence numbers negotiated during
    /// login: `stat_sn` is the StatSN for the first full-feature response, and
    /// `cmd_sn` is the session's window — the same one every connection of the
    /// session shares.
    pub fn with_sns(cid: u16, stat_sn: u32, cmd_sn: Arc<CmdSnWindow>) -> Self {
        ConnectionState { cid, stat_sn: AtomicU32::new(stat_sn), cmd_sn }
    }

    pub fn next_stat_sn(&self) -> u32 {
        self.stat_sn.fetch_add(1, Ordering::Relaxed)
    }

    pub fn exp_cmd_sn(&self) -> u32 {
        self.cmd_sn.exp_cmd_sn()
    }

    pub fn max_cmd_sn(&self) -> u32 {
        self.cmd_sn.max_cmd_sn()
    }

    pub fn advance_cmd_sn(&self, cmd_sn: u32) {
        self.cmd_sn.advance(cmd_sn);
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
    /// One command window for the whole session, handed to every connection
    /// that joins it.
    pub cmd_sn: Arc<CmdSnWindow>,
}

impl Session {
    /// Register a new connection for this session, sharing its command window.
    pub async fn add_connection(&self, cid: u16) -> Arc<ConnectionState> {
        self.add_connection_with_stat_sn(cid, 1).await
    }

    /// Register a new connection continuing from a StatSN login left off at.
    pub async fn add_connection_with_stat_sn(
        &self,
        cid: u16,
        stat_sn: u32,
    ) -> Arc<ConnectionState> {
        let conn = Arc::new(ConnectionState::with_sns(cid, stat_sn, self.cmd_sn.clone()));
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
            cmd_sn: Arc::new(CmdSnWindow::new(1)),
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
        assert_eq!(conn.exp_cmd_sn(), 2);
    }

    /// The MC/S invariant: StatSN is per connection, CmdSN is per **session**
    /// (RFC 7143 §4.2.2.1). Two connections on one session must advertise one
    /// command window, or the initiator is told two different things about its
    /// own flow control (#31).
    #[tokio::test]
    async fn cmd_sn_is_session_wide_and_stat_sn_is_not() {
        let registry = SessionRegistry::new();
        let session = registry.create_session([1, 2, 3, 4, 5, 6], SessionParams::default()).await;

        let a = session.add_connection(0).await;
        let b = session.add_connection(1).await;
        assert_eq!(session.connection_count().await, 2);

        // StatSN runs independently per connection.
        assert_eq!(a.next_stat_sn(), 1);
        assert_eq!(a.next_stat_sn(), 2);
        assert_eq!(b.next_stat_sn(), 1, "connection b keeps its own StatSN");

        // CmdSN does not: a command acknowledged on one connection moves the
        // window both of them advertise.
        assert_eq!(a.exp_cmd_sn(), 1);
        assert_eq!(b.exp_cmd_sn(), 1);
        a.advance_cmd_sn(1);
        assert_eq!(a.exp_cmd_sn(), 2);
        assert_eq!(b.exp_cmd_sn(), 2, "connection b did not see the session advance");
        assert_eq!(a.max_cmd_sn(), b.max_cmd_sn(), "one window, one answer");

        // And the next one can be acknowledged on the other connection, which
        // is the whole point of having two.
        b.advance_cmd_sn(2);
        assert_eq!(a.exp_cmd_sn(), 3);

        // A command that arrives out of order does not open a hole.
        b.advance_cmd_sn(9);
        assert_eq!(a.exp_cmd_sn(), 3, "an early command must wait for its predecessor");
    }

    /// A connection joining later shares the window as it stands, not a fresh
    /// one — otherwise the second path would rewind the session's flow control.
    #[tokio::test]
    async fn a_late_connection_joins_the_window_in_progress() {
        let registry = SessionRegistry::new();
        let session = registry.create_session([9; 6], SessionParams::default()).await;
        let first = session.add_connection(0).await;
        for sn in 1..=5 {
            first.advance_cmd_sn(sn);
        }
        assert_eq!(first.exp_cmd_sn(), 6);

        let late = session.add_connection(1).await;
        assert_eq!(late.exp_cmd_sn(), 6, "the new connection rewound the session");
        assert_eq!(late.next_stat_sn(), 1, "but its own StatSN starts fresh");
    }

    /// One connection closing leaves the session, and its siblings, alone.
    #[tokio::test]
    async fn removing_one_connection_keeps_the_session() {
        let registry = SessionRegistry::new();
        let session = registry.create_session([7; 6], SessionParams::default()).await;
        session.add_connection(0).await;
        let survivor = session.add_connection(1).await;

        session.remove_connection(0).await;
        assert_eq!(session.connection_count().await, 1);
        assert!(registry.get_session(session.tsih).await.is_some());

        // The survivor still drives the session's window.
        survivor.advance_cmd_sn(1);
        assert_eq!(survivor.exp_cmd_sn(), 2);
    }
}
