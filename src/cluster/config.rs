//! Cluster configuration parsed from [cluster] section in stormblock.toml.

use std::path::{Path, PathBuf};

use serde::{Serialize, Deserialize};

/// Cluster configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Enable cluster mode.
    pub enabled: bool,
    /// Directory for Raft state (log, vote, snapshots, node_id).
    pub data_dir: String,
    /// Seed node addresses for initial cluster join (e.g. ["10.0.0.1:9090"]).
    pub seed_nodes: Vec<String>,
    /// Heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// Heartbeat timeout in milliseconds (suspect after this many ms without response).
    pub heartbeat_timeout_ms: u64,
    /// Replication mode: "sync" or "async".
    pub replication_mode: String,
    /// Number of replicas for each volume (including the primary).
    pub replication_factor: usize,
    /// Enable TLS for inter-node cluster RPCs (Raft, heartbeat, join).
    /// When true, all cluster HTTP clients use HTTPS.
    /// The server side shares the management API's TLS cert/key.
    pub tls_enabled: bool,
    /// Path to CA certificate PEM file for verifying peer node certificates.
    /// Required when tls_enabled is true. If not set, system roots are used.
    pub tls_ca_cert: Option<String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            enabled: false,
            data_dir: "/var/lib/stormblock/raft".to_string(),
            seed_nodes: Vec::new(),
            heartbeat_interval_ms: 1000,
            heartbeat_timeout_ms: 5000,
            replication_mode: "async".to_string(),
            replication_factor: 2,
            tls_enabled: false,
            tls_ca_cert: None,
        }
    }
}

impl ClusterConfig {
    /// Path to the node_id persistence file.
    pub fn node_id_path(&self) -> PathBuf {
        Path::new(&self.data_dir).join("node_id")
    }

    /// Path to the Raft log file.
    pub fn raft_log_path(&self) -> PathBuf {
        Path::new(&self.data_dir).join("raft-log")
    }

    /// Path to the Raft vote file.
    pub fn vote_path(&self) -> PathBuf {
        Path::new(&self.data_dir).join("raft-vote")
    }

    /// Path to the membership store JSON file.
    pub fn membership_path(&self) -> PathBuf {
        Path::new(&self.data_dir).join("membership.json")
    }

    /// Path to the snapshot file.
    pub fn snapshot_path(&self) -> PathBuf {
        Path::new(&self.data_dir).join("raft-snapshot")
    }

    /// Load or create a persistent node ID.
    /// Reads u64 from `{data_dir}/node_id`, or generates one from UUID and persists it.
    pub fn load_or_create_node_id(&self) -> anyhow::Result<u64> {
        let path = self.node_id_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            let id: u64 = contents.trim().parse()
                .map_err(|e| anyhow::anyhow!("invalid node_id in {}: {e}", path.display()))?;
            return Ok(id);
        }
        // Generate from UUID — take lower 64 bits
        let uuid = uuid::Uuid::new_v4();
        let id = u64::from_le_bytes(uuid.as_bytes()[..8].try_into().unwrap());
        // Ensure data_dir exists
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::write(&path, id.to_string())?;
        Ok(id)
    }

    /// URL scheme for cluster HTTP calls ("https" if TLS enabled, "http" otherwise).
    pub fn url_scheme(&self) -> &str {
        if self.tls_enabled { "https" } else { "http" }
    }

    /// Build a reqwest HTTP client configured for cluster TLS (if enabled).
    pub fn build_http_client(&self) -> anyhow::Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10));

        if self.tls_enabled {
            if let Some(ca_path) = &self.tls_ca_cert {
                let ca_pem = std::fs::read(ca_path)
                    .map_err(|e| anyhow::anyhow!("failed to read cluster CA cert '{}': {e}", ca_path))?;
                let ca_cert = reqwest::Certificate::from_pem(&ca_pem)
                    .map_err(|e| anyhow::anyhow!("failed to parse cluster CA cert: {e}"))?;
                builder = builder.add_root_certificate(ca_cert);
            }
            builder = builder.use_rustls_tls();
        }

        builder.build()
            .map_err(|e| anyhow::anyhow!("failed to build cluster HTTP client: {e}"))
    }

    /// Whether this is a sync replication cluster.
    pub fn is_sync_replication(&self) -> bool {
        self.replication_mode == "sync"
    }

    /// Number of missed heartbeats before marking a node suspect.
    pub fn suspect_threshold(&self) -> u64 {
        self.heartbeat_timeout_ms / self.heartbeat_interval_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = ClusterConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.data_dir, "/var/lib/stormblock/raft");
        assert!(cfg.seed_nodes.is_empty());
        assert_eq!(cfg.heartbeat_interval_ms, 1000);
        assert_eq!(cfg.heartbeat_timeout_ms, 5000);
        assert_eq!(cfg.replication_mode, "async");
        assert_eq!(cfg.replication_factor, 2);
    }

    #[test]
    fn node_id_persistence() {
        let dir = std::env::temp_dir().join("stormblock-cluster-test-nodeid");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = ClusterConfig {
            data_dir: dir.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let id1 = cfg.load_or_create_node_id().unwrap();
        let id2 = cfg.load_or_create_node_id().unwrap();
        assert_eq!(id1, id2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suspect_threshold_calc() {
        let cfg = ClusterConfig {
            heartbeat_interval_ms: 1000,
            heartbeat_timeout_ms: 5000,
            ..Default::default()
        };
        assert_eq!(cfg.suspect_threshold(), 5);
    }

    #[test]
    fn parse_toml_cluster_section() {
        let toml_str = r#"
enabled = true
data_dir = "/data/raft"
seed_nodes = ["10.0.0.1:9090", "10.0.0.2:9090"]
heartbeat_interval_ms = 500
heartbeat_timeout_ms = 3000
replication_mode = "sync"
replication_factor = 3
"#;
        let cfg: ClusterConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.data_dir, "/data/raft");
        assert_eq!(cfg.seed_nodes.len(), 2);
        assert_eq!(cfg.heartbeat_interval_ms, 500);
        assert!(cfg.is_sync_replication());
        assert_eq!(cfg.replication_factor, 3);
    }

    #[test]
    fn tls_defaults_disabled() {
        let cfg = ClusterConfig::default();
        assert!(!cfg.tls_enabled);
        assert!(cfg.tls_ca_cert.is_none());
        assert_eq!(cfg.url_scheme(), "http");
    }

    #[test]
    fn tls_url_scheme() {
        let mut cfg = ClusterConfig::default();
        assert_eq!(cfg.url_scheme(), "http");
        cfg.tls_enabled = true;
        assert_eq!(cfg.url_scheme(), "https");
    }

    #[test]
    fn tls_build_client_no_tls() {
        let cfg = ClusterConfig::default();
        let client = cfg.build_http_client();
        assert!(client.is_ok());
    }

    #[test]
    fn tls_build_client_with_tls_no_ca() {
        let cfg = ClusterConfig {
            tls_enabled: true,
            tls_ca_cert: None,
            ..Default::default()
        };
        // Should succeed — uses system root CAs
        let client = cfg.build_http_client();
        assert!(client.is_ok());
    }

    #[test]
    fn tls_build_client_with_missing_ca() {
        let cfg = ClusterConfig {
            tls_enabled: true,
            tls_ca_cert: Some("/nonexistent/ca.pem".to_string()),
            ..Default::default()
        };
        let client = cfg.build_http_client();
        assert!(client.is_err());
    }

    #[test]
    fn parse_toml_with_tls() {
        let toml_str = r#"
enabled = true
data_dir = "/data/raft"
seed_nodes = ["10.0.0.1:9090"]
tls_enabled = true
tls_ca_cert = "/etc/stormblock/ca.pem"
"#;
        let cfg: ClusterConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.tls_enabled);
        assert_eq!(cfg.tls_ca_cert.as_deref(), Some("/etc/stormblock/ca.pem"));
        assert_eq!(cfg.url_scheme(), "https");
    }
}
