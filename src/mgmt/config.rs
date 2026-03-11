//! Configuration parsing (stormblock.toml).

use std::net::SocketAddr;
use std::path::Path;

use serde::{Serialize, Deserialize};

use crate::raid::RaidLevel;

/// Top-level configuration parsed from stormblock.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StormBlockConfig {
    pub management: ManagementConfig,
    #[serde(default)]
    pub drives: Vec<DriveConfig>,
    #[serde(default)]
    pub arrays: Vec<ArrayConfig>,
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
    #[cfg(feature = "iscsi")]
    pub iscsi: Option<IscsiExportConfig>,
    #[cfg(feature = "nvmeof")]
    pub nvmeof: Option<NvmeofExportConfig>,
    pub reactor: ReactorCfg,
    #[cfg(feature = "cluster")]
    #[serde(default)]
    pub cluster: crate::cluster::config::ClusterConfig,
    #[serde(default)]
    pub stormfs: crate::stormfs::StormFsConfig,
}

#[allow(clippy::derivable_impls)]
impl Default for StormBlockConfig {
    fn default() -> Self {
        StormBlockConfig {
            management: ManagementConfig::default(),
            drives: Vec::new(),
            arrays: Vec::new(),
            volumes: Vec::new(),
            #[cfg(feature = "iscsi")]
            iscsi: None,
            #[cfg(feature = "nvmeof")]
            nvmeof: None,
            reactor: ReactorCfg::default(),
            #[cfg(feature = "cluster")]
            cluster: crate::cluster::config::ClusterConfig::default(),
            stormfs: crate::stormfs::StormFsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagementConfig {
    pub listen_addr: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub data_dir: Option<String>,
}

impl Default for ManagementConfig {
    fn default() -> Self {
        ManagementConfig {
            listen_addr: "0.0.0.0:9090".to_string(),
            tls_cert: None,
            tls_key: None,
            data_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrayConfig {
    pub name: String,
    pub level: RaidLevel,
    pub drives: Vec<String>,
    #[serde(default = "default_stripe_kb")]
    pub stripe_kb: u64,
}

fn default_stripe_kb() -> u64 {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub name: String,
    pub size: String,
    pub array: String,
}

#[cfg(feature = "iscsi")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IscsiExportConfig {
    #[serde(default = "default_iscsi_addr")]
    pub listen_addr: String,
    #[serde(default = "default_iscsi_target_name")]
    pub target_name: String,
    pub chap_user: Option<String>,
    pub chap_secret: Option<String>,
}

#[cfg(feature = "iscsi")]
fn default_iscsi_addr() -> String {
    "0.0.0.0:3260".to_string()
}

#[cfg(feature = "iscsi")]
fn default_iscsi_target_name() -> String {
    "iqn.2024.io.stormblock:default".to_string()
}

#[cfg(feature = "nvmeof")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeofExportConfig {
    #[serde(default = "default_nvmeof_addr")]
    pub listen_addr: String,
    #[serde(default = "default_nvmeof_nqn")]
    pub nqn: String,
}

#[cfg(feature = "nvmeof")]
fn default_nvmeof_addr() -> String {
    "0.0.0.0:4420".to_string()
}

#[cfg(feature = "nvmeof")]
fn default_nvmeof_nqn() -> String {
    "nqn.2024.io.stormblock:default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReactorCfg {
    pub cores: usize,
    pub pin_cores: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for ReactorCfg {
    fn default() -> Self {
        ReactorCfg {
            cores: 0,
            pin_cores: cfg!(target_os = "linux"),
        }
    }
}

impl StormBlockConfig {
    /// Load configuration from a TOML file. Returns default if file doesn't exist.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        if !Path::new(path).exists() {
            tracing::info!("Config file not found at {path}, using defaults");
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)?;
        let config: StormBlockConfig = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Merge CLI arguments into this config (CLI takes precedence).
    #[allow(clippy::too_many_arguments)]
    pub fn merge_cli(
        &mut self,
        devices: &[String],
        raid_level: Option<RaidLevel>,
        stripe_kb: u64,
        volumes: &[(String, u64)],
        #[cfg(feature = "iscsi")] iscsi_addr: Option<&str>,
        #[cfg(feature = "iscsi")] iscsi_target_name: Option<&str>,
        #[cfg(feature = "iscsi")] chap_user: Option<&str>,
        #[cfg(feature = "iscsi")] chap_secret: Option<&str>,
        #[cfg(feature = "nvmeof")] nvmeof_addr: Option<&str>,
        #[cfg(feature = "nvmeof")] nvmeof_nqn: Option<&str>,
        reactor_cores: usize,
    ) {
        // CLI devices override config drives
        if !devices.is_empty() {
            self.drives = devices.iter()
                .map(|p| DriveConfig { path: p.clone() })
                .collect();
        }

        // CLI RAID overrides config arrays
        if let Some(level) = raid_level {
            let drive_paths: Vec<String> = self.drives.iter()
                .map(|d| d.path.clone())
                .collect();
            self.arrays = vec![ArrayConfig {
                name: "cli-array".to_string(),
                level,
                drives: drive_paths,
                stripe_kb,
            }];
        }

        // CLI volumes override config volumes
        if !volumes.is_empty() {
            self.volumes = volumes.iter()
                .map(|(name, size)| VolumeConfig {
                    name: name.clone(),
                    size: size.to_string(),
                    array: "cli-array".to_string(),
                })
                .collect();
        }

        // iSCSI CLI overrides
        #[cfg(feature = "iscsi")]
        if iscsi_addr.is_some() || chap_user.is_some() {
            let existing = self.iscsi.take().unwrap_or(IscsiExportConfig {
                listen_addr: default_iscsi_addr(),
                target_name: default_iscsi_target_name(),
                chap_user: None,
                chap_secret: None,
            });
            self.iscsi = Some(IscsiExportConfig {
                listen_addr: iscsi_addr.unwrap_or(&existing.listen_addr).to_string(),
                target_name: iscsi_target_name.unwrap_or(&existing.target_name).to_string(),
                chap_user: chap_user.map(|s| s.to_string()).or(existing.chap_user),
                chap_secret: chap_secret.map(|s| s.to_string()).or(existing.chap_secret),
            });
        }

        // NVMe-oF CLI overrides
        #[cfg(feature = "nvmeof")]
        if nvmeof_addr.is_some() || nvmeof_nqn.is_some() {
            let existing = self.nvmeof.take().unwrap_or(NvmeofExportConfig {
                listen_addr: default_nvmeof_addr(),
                nqn: default_nvmeof_nqn(),
            });
            self.nvmeof = Some(NvmeofExportConfig {
                listen_addr: nvmeof_addr.unwrap_or(&existing.listen_addr).to_string(),
                nqn: nvmeof_nqn.unwrap_or(&existing.nqn).to_string(),
            });
        }

        if reactor_cores > 0 {
            self.reactor.cores = reactor_cores;
        }
    }

    /// Validate configuration values.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Check management listen address parses
        self.management.listen_addr.parse::<SocketAddr>()
            .map_err(|e| anyhow::anyhow!("invalid management listen_addr '{}': {e}", self.management.listen_addr))?;

        // Validate TLS config: both cert and key must be provided together
        match (&self.management.tls_cert, &self.management.tls_key) {
            (Some(cert), Some(key)) => {
                if !Path::new(cert).exists() {
                    anyhow::bail!("TLS cert file not found: {cert}");
                }
                if !Path::new(key).exists() {
                    anyhow::bail!("TLS key file not found: {key}");
                }
            }
            (Some(_), None) => anyhow::bail!("tls_cert requires tls_key to also be set"),
            (None, Some(_)) => anyhow::bail!("tls_key requires tls_cert to also be set"),
            (None, None) => {} // No TLS, fine
        }

        // Check for port conflicts
        let mgmt_port = self.management.listen_addr.parse::<SocketAddr>()
            .map(|a| a.port())
            .unwrap_or(9090);

        #[cfg(feature = "iscsi")]
        if let Some(ref iscsi) = self.iscsi {
            let port = iscsi.listen_addr.parse::<SocketAddr>()
                .map_err(|e| anyhow::anyhow!("invalid iSCSI listen_addr: {e}"))?
                .port();
            if port == mgmt_port {
                anyhow::bail!("iSCSI port {port} conflicts with management port");
            }
            // CHAP: both user and secret must be set together
            if iscsi.chap_user.is_some() != iscsi.chap_secret.is_some() {
                anyhow::bail!("CHAP requires both chap_user and chap_secret");
            }
        }

        #[cfg(feature = "nvmeof")]
        if let Some(ref nvmeof) = self.nvmeof {
            let port = nvmeof.listen_addr.parse::<SocketAddr>()
                .map_err(|e| anyhow::anyhow!("invalid NVMe-oF listen_addr: {e}"))?
                .port();
            if port == mgmt_port {
                anyhow::bail!("NVMe-oF port {port} conflicts with management port");
            }
        }

        // Validate volume sizes
        for vol in &self.volumes {
            parse_size(&vol.size)
                .map_err(|e| anyhow::anyhow!("invalid volume size '{}': {e}", vol.size))?;
        }

        // Validate cluster TLS config
        #[cfg(feature = "cluster")]
        if self.cluster.enabled && self.cluster.tls_enabled {
            // Cluster TLS requires management TLS (they share the same server)
            if self.management.tls_cert.is_none() || self.management.tls_key.is_none() {
                anyhow::bail!(
                    "cluster.tls_enabled requires management TLS (tls_cert + tls_key) \
                     since cluster RPCs share the management API server"
                );
            }
            // Validate CA cert path if specified
            if let Some(ca_path) = &self.cluster.tls_ca_cert {
                if !Path::new(ca_path).exists() {
                    anyhow::bail!("cluster TLS CA cert file not found: {ca_path}");
                }
            }
        }

        // Validate StormFS config
        if self.stormfs.enabled {
            if self.stormfs.metadata_url.is_empty() {
                anyhow::bail!("stormfs.metadata_url is required when stormfs.enabled = true");
            }
            if self.stormfs.advertise_addr.is_empty() {
                anyhow::bail!("stormfs.advertise_addr is required when stormfs.enabled = true");
            }
        }

        // Array stripe sizes should be reasonable
        for arr in &self.arrays {
            if arr.stripe_kb < 4 || arr.stripe_kb > 4096 {
                anyhow::bail!("stripe_kb {} out of range [4..4096]", arr.stripe_kb);
            }
            if arr.drives.is_empty() {
                anyhow::bail!("array '{}' has no drives", arr.name);
            }
        }

        Ok(())
    }
}

/// Parse a human-readable size string into bytes.
/// Supports T, G, M, K suffixes (base-1024).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('T') {
        (n, 1024u64 * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1024u64)
    } else {
        (s, 1u64)
    };
    let num: u64 = num_str.trim().parse()
        .map_err(|_| format!("invalid size number: '{num_str}'"))?;
    Ok(num * multiplier)
}

/// Format bytes as a human-readable size string.
pub fn human_size(bytes: u64) -> String {
    const TB: u64 = 1024 * 1024 * 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("4K").unwrap(), 4096);
        assert_eq!(parse_size("64M").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2T").unwrap(), 2 * 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_with_whitespace() {
        assert_eq!(parse_size("  100G  ").unwrap(), 100 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_invalid() {
        assert!(parse_size("abc").is_err());
        assert!(parse_size("G").is_err());
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn default_config() {
        let cfg = StormBlockConfig::default();
        assert_eq!(cfg.management.listen_addr, "0.0.0.0:9090");
        assert!(cfg.drives.is_empty());
        assert!(cfg.arrays.is_empty());
        assert!(cfg.volumes.is_empty());
    }

    #[test]
    fn parse_toml_config() {
        let toml_str = r#"
[management]
listen_addr = "127.0.0.1:9091"

[[drives]]
path = "/dev/sda"

[[drives]]
path = "/dev/sdb"

[[arrays]]
name = "data"
level = "Raid1"
drives = ["/dev/sda", "/dev/sdb"]
stripe_kb = 128

[[volumes]]
name = "vol0"
size = "100G"
array = "data"

[reactor]
cores = 4
pin_cores = false
"#;
        let cfg: StormBlockConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.management.listen_addr, "127.0.0.1:9091");
        assert_eq!(cfg.drives.len(), 2);
        assert_eq!(cfg.arrays.len(), 1);
        assert_eq!(cfg.arrays[0].level, RaidLevel::Raid1);
        assert_eq!(cfg.arrays[0].stripe_kb, 128);
        assert_eq!(cfg.volumes.len(), 1);
        assert_eq!(cfg.reactor.cores, 4);
    }

    #[test]
    fn validate_port_conflict() {
        let mut cfg = StormBlockConfig::default();
        cfg.management.listen_addr = "0.0.0.0:3260".to_string();
        #[cfg(feature = "iscsi")]
        {
            cfg.iscsi = Some(IscsiExportConfig {
                listen_addr: "0.0.0.0:3260".to_string(),
                target_name: "iqn.2024.io.test:t1".to_string(),
                chap_user: None,
                chap_secret: None,
            });
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn validate_bad_stripe() {
        let mut cfg = StormBlockConfig::default();
        cfg.arrays.push(ArrayConfig {
            name: "bad".to_string(),
            level: RaidLevel::Raid5,
            drives: vec!["/dev/sda".to_string()],
            stripe_kb: 2, // too small
        });
        assert!(cfg.validate().is_err());
    }
}
