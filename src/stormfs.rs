//! StormFS registration — announce volumes to StormFS metadata cluster.
//!
//! StormBlock nodes register their exported volumes with a StormFS metadata
//! server so StormFS can consume them as backing storage for distributed files.
//! Registration is periodic (heartbeat-style) so StormFS detects node departures.

use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, Deserialize};

use crate::mgmt::AppState;

/// Configuration for StormFS registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StormFsConfig {
    /// Enable StormFS registration.
    pub enabled: bool,
    /// StormFS metadata server URL (e.g., "http://stormfs-meta:8500").
    pub metadata_url: String,
    /// Registration heartbeat interval in seconds.
    pub heartbeat_secs: u64,
    /// This node's advertised address for StormFS to reach back.
    pub advertise_addr: String,
}

impl Default for StormFsConfig {
    fn default() -> Self {
        StormFsConfig {
            enabled: false,
            metadata_url: String::new(),
            heartbeat_secs: 30,
            advertise_addr: String::new(),
        }
    }
}

/// Volume announcement sent to StormFS metadata server.
#[derive(Debug, Serialize)]
struct VolumeAnnouncement {
    node_addr: String,
    hostname: String,
    volumes: Vec<VolumeInfo>,
}

/// Per-volume info in a registration announcement.
#[derive(Debug, Serialize)]
struct VolumeInfo {
    id: String,
    name: String,
    capacity_bytes: u64,
    allocated_bytes: u64,
    protocols: Vec<String>,
}

/// Registration response from StormFS metadata server.
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    #[serde(default)]
    accepted: bool,
    #[serde(default)]
    message: String,
}

/// StormFS registration client.
pub struct StormFsRegistration {
    config: StormFsConfig,
    client: reqwest::Client,
}

impl StormFsRegistration {
    /// Create a new StormFS registration client.
    pub fn new(config: StormFsConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        StormFsRegistration { config, client }
    }

    /// Start the periodic registration loop.
    pub fn start(self, state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.heartbeat_secs);
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.register(&state).await {
                    tracing::warn!("StormFS registration failed: {e}");
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Send a single registration announcement to StormFS.
    async fn register(&self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let hostname = gethostname()
            .unwrap_or_else(|| "unknown".to_string());

        // Collect volume info
        let vm = state.volume_manager.lock().await;
        let vol_list = vm.list_volumes().await;
        let volumes: Vec<VolumeInfo> = vol_list.iter().map(|(id, name, capacity, allocated)| {
            let mut protocols = Vec::new();
            #[cfg(feature = "iscsi")]
            protocols.push("iscsi".to_string());
            #[cfg(feature = "nvmeof")]
            protocols.push("nvmeof".to_string());
            VolumeInfo {
                id: id.to_string(),
                name: name.clone(),
                capacity_bytes: *capacity,
                allocated_bytes: *allocated,
                protocols,
            }
        }).collect();
        drop(vm);

        let announcement = VolumeAnnouncement {
            node_addr: self.config.advertise_addr.clone(),
            hostname,
            volumes,
        };

        let url = format!("{}/api/v1/storage/register", self.config.metadata_url.trim_end_matches('/'));
        let resp = self.client
            .post(&url)
            .json(&announcement)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("StormFS metadata unreachable: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("StormFS registration rejected ({status}): {body}");
        }

        let reg_resp: RegistrationResponse = resp.json().await
            .unwrap_or(RegistrationResponse { accepted: true, message: String::new() });

        if reg_resp.accepted {
            tracing::debug!("StormFS registration accepted");
        } else {
            tracing::warn!("StormFS registration not accepted: {}", reg_resp.message);
        }

        Ok(())
    }

    /// Send a deregistration to StormFS on shutdown.
    pub async fn deregister(&self) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/v1/storage/deregister",
            self.config.metadata_url.trim_end_matches('/')
        );
        let _ = self.client
            .post(&url)
            .json(&serde_json::json!({
                "node_addr": self.config.advertise_addr,
            }))
            .send()
            .await;
        tracing::info!("StormFS deregistration sent");
        Ok(())
    }
}

/// Get the system hostname.
fn gethostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut i8, buf.len()) };
        if ret == 0 {
            let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8(buf[..nul].to_vec()).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_disabled() {
        let cfg = StormFsConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.metadata_url.is_empty());
        assert_eq!(cfg.heartbeat_secs, 30);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = StormFsConfig {
            enabled: true,
            metadata_url: "http://stormfs:8500".to_string(),
            heartbeat_secs: 60,
            advertise_addr: "10.0.0.1:9090".to_string(),
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: StormFsConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.metadata_url, "http://stormfs:8500");
        assert_eq!(parsed.heartbeat_secs, 60);
    }
}
