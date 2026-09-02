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
    #[serde(default)]
    pub luns: Vec<LunConfig>,
    #[cfg(feature = "iscsi")]
    pub iscsi: Option<IscsiExportConfig>,
    #[cfg(feature = "nvmeof")]
    pub nvmeof: Option<NvmeofExportConfig>,
    pub reactor: ReactorCfg,
    #[serde(default)]
    pub boot: Option<BootConfig>,
    #[cfg(feature = "cluster")]
    #[serde(default)]
    pub cluster: crate::cluster::config::ClusterConfig,
    #[serde(default)]
    pub stormfs: crate::stormfs::StormFsConfig,
    #[serde(default)]
    pub gc: GcConfig,
    /// Grow the pool when it comes under physical pressure (#18).
    #[serde(default)]
    pub pressure: crate::volume::pressure::PressureConfig,
    /// The serving surface — `/serve/v1` (#60).
    #[serde(default)]
    pub serve: ServeSection,
}

/// Where the stock binary gets its serving parameters from.
///
/// `serve::config::ServeConfig` says what serving needs to know and
/// deliberately says nothing about where the values come from — that is a
/// profile's job. This section is the *stock* profile: the answers a
/// single-node server gives when nobody has configured anything, which is the
/// situation `docs/layering.md` describes as the job rather than a choice.
///
/// Every field is an override. Leaving one unset takes the serving default
/// from `ServeConfig::default()`, or derives it from the management config
/// where a derived value is better than a constant — the advertise address
/// and the data directory both work that way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeSection {
    /// Mount `/serve/v1` at all. On by default: a layer-2 surface that only
    /// some profiles serve is the situation this exists to end (#60).
    pub enabled: bool,
    /// Where the export and wiring tables live. Defaults to
    /// `<management.data_dir>/serve`.
    ///
    /// **Serving is skipped when neither is set.** The wiring table pins LUN
    /// and port assignments across restarts, and without somewhere durable to
    /// keep it a restart can hand a LUN a consumer is already attached to
    /// over to a different volume. Refusing to serve is the safe answer, and
    /// the log says so.
    pub data_dir: Option<String>,
    /// What consumers are told to attach to. Defaults to
    /// `management.advertised_addr`, then the management listen host, then
    /// loopback.
    pub advertise_addr: Option<String>,
    /// Serve the legacy shared iSCSI target. Off unless set, matching
    /// `ServeConfig`: NVMe-TCP is the transport.
    pub iscsi_enabled: Option<bool>,
    /// First port of the per-export portal range, and how many it holds.
    pub portal_base: Option<u16>,
    pub portal_span: Option<u16>,
    pub iqn: Option<String>,
    pub iqn_prefix: Option<String>,
    pub nqn: Option<String>,
    pub nqn_prefix: Option<String>,
    pub drain_grace_secs: Option<u64>,
    pub reconcile_secs: Option<u64>,
    pub orphan_export_grace_secs: Option<u64>,
    pub reap_secs: Option<u64>,
    pub reap_apply: Option<bool>,
    pub reap_min_age_secs: Option<u64>,
    pub reap_max_per_pass: Option<usize>,
}

impl Default for ServeSection {
    fn default() -> Self {
        ServeSection {
            enabled: true,
            data_dir: None,
            advertise_addr: None,
            iscsi_enabled: None,
            portal_base: None,
            portal_span: None,
            iqn: None,
            iqn_prefix: None,
            nqn: None,
            nqn_prefix: None,
            drain_grace_secs: None,
            reconcile_secs: None,
            orphan_export_grace_secs: None,
            reap_secs: None,
            reap_apply: None,
            reap_min_age_secs: None,
            reap_max_per_pass: None,
        }
    }
}

/// Why the serving surface is not being mounted.
///
/// Returned rather than logged in place so the caller decides how loudly to
/// say it — and so it can be tested without capturing logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeSkipped {
    /// `serve.enabled = false`.
    Disabled,
    /// Nowhere durable to keep the wiring table.
    NoDataDir,
}

impl std::fmt::Display for ServeSkipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeSkipped::Disabled => write!(f, "serve.enabled is false"),
            ServeSkipped::NoDataDir => write!(
                f,
                "neither serve.data_dir nor management.data_dir is set, and the wiring table \
                 has to survive a restart: it pins which LUN and which port each volume was \
                 given, so without it a restart can hand a LUN a consumer is already attached \
                 to over to a different volume"
            ),
        }
    }
}

impl StormBlockConfig {
    /// Build the serving parameters for the stock binary, or say why not.
    ///
    /// `iscsi_bind` and `nvmeof_bind` come from wherever the targets are
    /// actually being told to listen — CLI flag or config — because the
    /// portal range binds the same interface as the shared NVMe-oF portal so
    /// that one firewall rule covers it.
    pub fn serve_config(
        &self,
        iscsi_bind: &str,
        nvmeof_bind: &str,
    ) -> Result<crate::serve::config::ServeConfig, ServeSkipped> {
        use crate::serve::config::ServeConfig;

        if !self.serve.enabled {
            return Err(ServeSkipped::Disabled);
        }

        let data_dir = match (&self.serve.data_dir, &self.management.data_dir) {
            (Some(d), _) => d.clone(),
            (None, Some(d)) => std::path::Path::new(d)
                .join("serve")
                .to_string_lossy()
                .to_string(),
            (None, None) => return Err(ServeSkipped::NoDataDir),
        };

        let listen_host = nvmeof_bind.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
        let advertise_addr = self
            .serve
            .advertise_addr
            .clone()
            .unwrap_or_else(|| self.management.resolve_advertised_host(listen_host));

        let d = ServeConfig::default();
        Ok(ServeConfig {
            data_dir,
            advertise_addr,
            iscsi_enabled: self.serve.iscsi_enabled.unwrap_or(d.iscsi_enabled),
            iscsi_bind: iscsi_bind.to_string(),
            nvmeof_bind: nvmeof_bind.to_string(),
            iqn: self.serve.iqn.clone().unwrap_or(d.iqn),
            iqn_prefix: self.serve.iqn_prefix.clone().unwrap_or(d.iqn_prefix),
            nqn: self.serve.nqn.clone().unwrap_or(d.nqn),
            nqn_prefix: self.serve.nqn_prefix.clone().unwrap_or(d.nqn_prefix),
            portal_base: self.serve.portal_base.unwrap_or(d.portal_base),
            portal_span: self.serve.portal_span.unwrap_or(d.portal_span),
            drain_grace_secs: self.serve.drain_grace_secs.unwrap_or(d.drain_grace_secs),
            reconcile_secs: self.serve.reconcile_secs.unwrap_or(d.reconcile_secs),
            orphan_export_grace_secs: self
                .serve
                .orphan_export_grace_secs
                .unwrap_or(d.orphan_export_grace_secs),
            reap_secs: self.serve.reap_secs.unwrap_or(d.reap_secs),
            reap_apply: self.serve.reap_apply.unwrap_or(d.reap_apply),
            reap_min_age_secs: self.serve.reap_min_age_secs.unwrap_or(d.reap_min_age_secs),
            reap_max_per_pass: self.serve.reap_max_per_pass.unwrap_or(d.reap_max_per_pass),
        })
    }
}

/// Background extent garbage collection.
///
/// Reclaims slab slots no volume maps. Leaks should not happen, but when
/// accounting between the extent map and a slot table diverges the space is
/// otherwise unrecoverable without reformatting the slab.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GcConfig {
    /// Run the collector periodically. On by default: the failure it recovers
    /// from is silent and cumulative, and a pass over an unleaked node costs
    /// one scan of the slot tables.
    pub enabled: bool,
    /// Seconds between passes (default 600).
    pub interval_secs: u64,
    /// Require an orphan to be seen by two consecutive passes before its data
    /// is freed. Costs one interval of delay; buys a second independent check
    /// that nothing references the slot.
    pub confirm_passes: bool,
    /// Most slots one pass may free, bounding how long it holds the registry
    /// lock on a badly leaked node (default 4096).
    pub max_reclaim_per_pass: usize,
    /// Find and report orphans, but never free anything.
    pub dry_run: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        GcConfig {
            enabled: true,
            interval_secs: 600,
            confirm_passes: true,
            max_reclaim_per_pass: 4096,
            dry_run: false,
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for StormBlockConfig {
    fn default() -> Self {
        StormBlockConfig {
            management: ManagementConfig::default(),
            drives: Vec::new(),
            arrays: Vec::new(),
            volumes: Vec::new(),
            luns: Vec::new(),
            #[cfg(feature = "iscsi")]
            iscsi: None,
            #[cfg(feature = "nvmeof")]
            nvmeof: None,
            reactor: ReactorCfg::default(),
            boot: None,
            #[cfg(feature = "cluster")]
            cluster: crate::cluster::config::ClusterConfig::default(),
            stormfs: crate::stormfs::StormFsConfig::default(),
            gc: GcConfig::default(),
            pressure: crate::volume::pressure::PressureConfig::default(),
            serve: ServeSection::default(),
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
    /// Optional bearer token required on /v1 requests (Authorization: Bearer).
    pub api_token: Option<String>,
    /// This node's name in the /v1 surface. Falls back to $STORMBLOCK_NODE,
    /// then $HOSTNAME, then "localhost".
    pub node_name: Option<String>,
    /// Topology labels (zone, rack, ...) reported for this node via
    /// GET /v1/nodes/capacity.
    pub topology: std::collections::BTreeMap<String, String>,
    /// Turn off node auto-discovery. Peers are found over UDP multicast with
    /// no seed list; disable only where multicast is unwanted, accepting that
    /// the node then sees no peers and cannot place replicas.
    #[serde(default)]
    pub discovery_disabled: bool,
    /// How often to announce this node, in seconds (default 5).
    #[serde(default = "default_beacon_secs")]
    pub beacon_secs: u64,
    /// Treat a peer as gone after this long without a beacon (default 30s),
    /// so volumes stop being placed on a node that has fallen silent.
    #[serde(default = "default_peer_stale_secs")]
    pub peer_stale_secs: u64,
    /// Address remote consumers should use to reach this node's targets.
    ///
    /// Target listen addresses are usually wildcards (`0.0.0.0:4420`), which
    /// tell a caller nothing, and falling back to loopback is useless to a
    /// remote initiator. Set this to the node's routable address (host or
    /// `host:port`) and `/v1/.../attach` plus the NVMe-oF discovery log page
    /// report it instead. Falls back to `$STORMBLOCK_ADVERTISED_ADDR`.
    pub advertised_addr: Option<String>,
    /// Offer the ublk transport when a volume is attached on this same node.
    /// The consumer then gets a local `/dev/ublkbN` with no NVMe-oF/TCP round
    /// trip. Requires Linux 6.0+ with `ublk_drv`; when that is missing the
    /// engine falls back to nvme-tcp on its own.
    ///
    /// **On by default**, and it was off. Every guard that makes a local
    /// attach safe is already checked at the call site — the volume must be
    /// backed here, and the request must name this node — so the flag was
    /// guarding nothing that those did not, and its absence produced
    /// `409 Conflict: ublk is a local device …, or ublk_transport is off`,
    /// which reads as a transport problem on a node already serving 39 ublk
    /// devices. Set it to `false` to force every attach through nvme-tcp.
    #[serde(default = "yes")]
    pub ublk_transport: bool,
}

/// serde needs a function for a default of `true`.
fn yes() -> bool {
    true
}

impl Default for ManagementConfig {
    fn default() -> Self {
        ManagementConfig {
            listen_addr: "0.0.0.0:9090".to_string(),
            tls_cert: None,
            tls_key: None,
            data_dir: None,
            api_token: None,
            node_name: None,
            topology: std::collections::BTreeMap::new(),
            discovery_disabled: false,
            beacon_secs: default_beacon_secs(),
            peer_stale_secs: default_peer_stale_secs(),
            advertised_addr: None,
            ublk_transport: true,
        }
    }
}

fn default_beacon_secs() -> u64 { 5 }
fn default_peer_stale_secs() -> u64 { 30 }

/// True for a listen host that names no concrete address a peer could dial.
fn is_wildcard_host(host: &str) -> bool {
    matches!(host, "" | "0.0.0.0" | "::" | "[::]" | "*")
}

impl ManagementConfig {
    /// Host part of the configured advertised address, if any.
    ///
    /// Accepts either a bare host (`10.0.0.5`) or `host:port` — the port is
    /// ignored here because each protocol supplies its own.
    pub fn advertised_host(&self) -> Option<String> {
        let raw = self
            .advertised_addr
            .clone()
            .or_else(|| std::env::var("STORMBLOCK_ADVERTISED_ADDR").ok())?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        // Bracketed IPv6 literal, optionally with a port.
        if let Some(rest) = raw.strip_prefix('[') {
            let host = rest.split(']').next().unwrap_or_default();
            return (!host.is_empty()).then(|| host.to_string());
        }
        // host:port only when there is exactly one colon (bare IPv6 has more).
        let host = match raw.split_once(':') {
            Some((h, _)) if raw.matches(':').count() == 1 => h,
            _ => raw,
        };
        (!is_wildcard_host(host)).then(|| host.to_string())
    }

    /// Resolve the host a remote consumer should dial for a target listening
    /// on `listen_host`.
    ///
    /// Preference order: explicit `advertised_addr`, the target's own listen
    /// host when it is concrete, the management listen host when it is
    /// concrete, then loopback as a last resort.
    pub fn resolve_advertised_host(&self, listen_host: &str) -> String {
        if let Some(h) = self.advertised_host() {
            return h;
        }
        if !is_wildcard_host(listen_host) {
            return listen_host.to_string();
        }
        let mgmt_host = self
            .listen_addr
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or("");
        if !is_wildcard_host(mgmt_host) {
            return mgmt_host.to_string();
        }
        "127.0.0.1".to_string()
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
    /// `mirror:2`, `raid5:4+1`, … — see `POST /api/v1/volumes`. Absent means
    /// one copy.
    #[serde(default)]
    pub redundancy: Option<String>,
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
    /// Most connections one session may carry — MC/S (#31). Default 4.
    ///
    /// Negotiation takes the lower of this and what the initiator asks for, so
    /// raising it cannot affect a consumer that does not want more than one.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_export_drives() -> bool {
    true
}

#[cfg(feature = "iscsi")]
fn default_max_connections() -> u32 {
    4
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
    /// Publish each configured drive as a raw namespace.
    ///
    /// True where the drives *are* what this node serves — an appliance
    /// holding one image per drive, which is what the file-per-image layout
    /// looks like. **False where the drives are this engine's storage pool**:
    /// publishing the pool raw hands every initiator a second, unmanaged
    /// writer into slabs the engine is allocating from, beside the volume
    /// exports that are the intended door.
    #[serde(default = "default_export_drives")]
    pub export_drives: bool,
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
                    redundancy: None,
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
                max_connections: default_max_connections(),
            });
            self.iscsi = Some(IscsiExportConfig {
                listen_addr: iscsi_addr.unwrap_or(&existing.listen_addr).to_string(),
                target_name: iscsi_target_name.unwrap_or(&existing.target_name).to_string(),
                chap_user: chap_user.map(|s| s.to_string()).or(existing.chap_user),
                chap_secret: chap_secret.map(|s| s.to_string()).or(existing.chap_secret),
                max_connections: existing.max_connections,
            });
        }

        // NVMe-oF CLI overrides
        #[cfg(feature = "nvmeof")]
        if nvmeof_addr.is_some() || nvmeof_nqn.is_some() {
            let existing = self.nvmeof.take().unwrap_or(NvmeofExportConfig {
                listen_addr: default_nvmeof_addr(),
                nqn: default_nvmeof_nqn(),
                export_drives: default_export_drives(),
            });
            // `..existing` rather than a fresh struct: rebuilding it field by
            // field silently dropped everything the overrides did not mention,
            // which is why a `[nvmeof]` setting in the config file appeared to
            // do nothing whenever the command line touched this section at all.
            self.nvmeof = Some(NvmeofExportConfig {
                listen_addr: nvmeof_addr.unwrap_or(&existing.listen_addr).to_string(),
                nqn: nvmeof_nqn.unwrap_or(&existing.nqn).to_string(),
                ..existing
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

/// Configuration for a declarative LUN (loaded from stormblock.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LunConfig {
    /// LUN ID (0-255).
    pub id: u64,
    /// Path to backing file or block device.
    pub path: String,
    /// Size for file-backed LUNs (creates/extends file). Ignored for block devices.
    pub size: Option<String>,
    /// Read-only LUN (default: false).
    #[serde(default)]
    pub readonly: bool,
}

/// Configuration for the boot volume manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootConfig {
    /// Directory for boot volume templates.
    #[serde(default = "default_templates_dir")]
    pub templates_dir: String,
    /// StormBlock server address for iPXE scripts.
    #[serde(default)]
    pub server_addr: String,
}

fn default_templates_dir() -> String {
    "/var/lib/stormblock/templates".to_string()
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

    fn mgmt_with(advertised: Option<&str>, listen: &str) -> ManagementConfig {
        ManagementConfig {
            listen_addr: listen.to_string(),
            advertised_addr: advertised.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn advertised_host_parses_forms() {
        assert_eq!(
            mgmt_with(Some("10.0.0.5"), "0.0.0.0:9090").advertised_host().as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            mgmt_with(Some("10.0.0.5:4420"), "0.0.0.0:9090").advertised_host().as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            mgmt_with(Some("[fd00::1]:4420"), "0.0.0.0:9090").advertised_host().as_deref(),
            Some("fd00::1")
        );
        // A bare IPv6 literal has more than one colon and keeps all of it.
        assert_eq!(
            mgmt_with(Some("fd00::1"), "0.0.0.0:9090").advertised_host().as_deref(),
            Some("fd00::1")
        );
        // Wildcards and blanks advertise nothing.
        assert!(mgmt_with(Some("0.0.0.0"), "0.0.0.0:9090").advertised_host().is_none());
        assert!(mgmt_with(Some("  "), "0.0.0.0:9090").advertised_host().is_none());
    }

    #[test]
    fn resolve_advertised_host_preference_order() {
        // Explicit advertised address wins over everything.
        let c = mgmt_with(Some("203.0.113.7"), "192.168.1.10:9090");
        assert_eq!(c.resolve_advertised_host("10.1.1.1"), "203.0.113.7");

        // No advertised address: a concrete target listen host is used as-is.
        let c = mgmt_with(None, "0.0.0.0:9090");
        assert_eq!(c.resolve_advertised_host("10.1.1.1"), "10.1.1.1");

        // Wildcard target listen host falls back to a concrete mgmt host.
        let c = mgmt_with(None, "192.168.1.10:9090");
        assert_eq!(c.resolve_advertised_host("0.0.0.0"), "192.168.1.10");

        // Everything wildcard: loopback is the last resort.
        let c = mgmt_with(None, "0.0.0.0:9090");
        assert_eq!(c.resolve_advertised_host("0.0.0.0"), "127.0.0.1");
    }

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
                max_connections: default_max_connections(),
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

    // ---- the serving surface (#60) -------------------------------------

    fn cfg_with_data_dir(dir: &str) -> StormBlockConfig {
        let mut c = StormBlockConfig::default();
        c.management.data_dir = Some(dir.to_string());
        c
    }

    /// The point of #60: a node that configured nothing about serving still
    /// serves, because serving volumes is the job rather than a choice.
    #[test]
    fn serving_is_on_by_default_once_there_is_somewhere_to_keep_state() {
        let cfg = cfg_with_data_dir("/var/lib/stormblock");
        let s = cfg
            .serve_config("0.0.0.0:3260", "0.0.0.0:4420")
            .expect("a node with a data dir serves");

        assert_eq!(s.data_dir, "/var/lib/stormblock/serve");
        assert_eq!(s.portal_base, ServeConfigDefaults::portal_base());
        assert!(!s.iscsi_enabled, "NVMe-TCP is the transport");
        assert_eq!(s.nvmeof_bind, "0.0.0.0:4420");
        assert_eq!(s.iscsi_bind, "0.0.0.0:3260");
    }

    /// Without anywhere durable the wiring table cannot survive a restart,
    /// and a restart could then hand a LUN a consumer is attached to over to
    /// a different volume. Refusing is the safe answer.
    #[test]
    fn no_data_dir_anywhere_means_no_serving() {
        let cfg = StormBlockConfig::default();
        let err = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap_err();
        assert_eq!(err, ServeSkipped::NoDataDir);
        assert!(
            err.to_string().contains("survive a restart"),
            "the reason has to be actionable: {err}"
        );
    }

    #[test]
    fn serving_can_be_turned_off_outright() {
        let mut cfg = cfg_with_data_dir("/var/lib/stormblock");
        cfg.serve.enabled = false;
        assert_eq!(
            cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap_err(),
            ServeSkipped::Disabled
        );
    }

    /// A consumer told to attach to 0.0.0.0 cannot. The advertise address
    /// falls back through the same ladder the NVMe-oF discovery log uses.
    #[test]
    fn the_advertise_address_is_never_a_wildcard_when_anything_better_exists() {
        // Explicit wins.
        let mut cfg = cfg_with_data_dir("/d");
        cfg.management.advertised_addr = Some("10.0.0.5".to_string());
        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.advertise_addr, "10.0.0.5");

        // Then the target's own listen host.
        let cfg = cfg_with_data_dir("/d");
        let s = cfg.serve_config("0.0.0.0:3260", "10.0.0.6:4420").unwrap();
        assert_eq!(s.advertise_addr, "10.0.0.6");

        // Then the management listen host.
        let mut cfg = cfg_with_data_dir("/d");
        cfg.management.listen_addr = "10.0.0.7:8080".to_string();
        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.advertise_addr, "10.0.0.7");

        // Loopback only when there is genuinely nothing better — honest for a
        // single-node server, and visible in the startup log either way.
        let cfg = cfg_with_data_dir("/d");
        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.advertise_addr, "127.0.0.1");
    }

    /// `serve.advertise_addr` overrides even an explicit management one: a
    /// node can be reached at a different address for data than for control.
    #[test]
    fn serve_advertise_addr_overrides_the_management_one() {
        let mut cfg = cfg_with_data_dir("/d");
        cfg.management.advertised_addr = Some("10.0.0.5".to_string());
        cfg.serve.advertise_addr = Some("192.168.1.9".to_string());
        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.advertise_addr, "192.168.1.9");
    }

    #[test]
    fn every_serving_parameter_can_be_overridden() {
        let mut cfg = cfg_with_data_dir("/d");
        cfg.serve.data_dir = Some("/srv/state".to_string());
        cfg.serve.iscsi_enabled = Some(true);
        cfg.serve.portal_base = Some(9000);
        cfg.serve.portal_span = Some(16);
        cfg.serve.drain_grace_secs = Some(7);
        cfg.serve.reap_secs = Some(0);
        cfg.serve.reap_apply = Some(false);
        cfg.serve.nqn_prefix = Some("nqn.2026-08.example".to_string());

        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.data_dir, "/srv/state");
        assert!(s.iscsi_enabled);
        assert_eq!(s.portal_base, 9000);
        assert_eq!(s.portal_span, 16);
        assert_eq!(s.drain_grace_secs, 7);
        assert_eq!(s.reap_secs, 0, "0 disables the reaper");
        assert!(!s.reap_apply);
        assert_eq!(s.nqn_prefix, "nqn.2026-08.example");
    }

    /// A config file with no `[serve]` section at all must still parse and
    /// still serve — the whole point is that nobody has to know about it.
    #[test]
    fn a_config_without_a_serve_section_still_serves() {
        let toml_str = r#"
[management]
listen_addr = "0.0.0.0:8080"
data_dir = "/var/lib/stormblock"
"#;
        let cfg: StormBlockConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.serve.enabled);
        assert!(cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").is_ok());
    }

    #[test]
    fn a_serve_section_round_trips() {
        let toml_str = r#"
[management]
data_dir = "/var/lib/stormblock"

[serve]
enabled = true
advertise_addr = "10.0.0.5"
portal_base = 9000
"#;
        let cfg: StormBlockConfig = toml::from_str(toml_str).unwrap();
        let s = cfg.serve_config("0.0.0.0:3260", "0.0.0.0:4420").unwrap();
        assert_eq!(s.advertise_addr, "10.0.0.5");
        assert_eq!(s.portal_base, 9000);
    }

    /// Reaches into `ServeConfig::default()` so the assertions above track it
    /// rather than restating constants that could drift apart from it.
    struct ServeConfigDefaults;
    impl ServeConfigDefaults {
        fn portal_base() -> u16 {
            crate::serve::config::ServeConfig::default().portal_base
        }
    }
}

#[cfg(test)]
mod ublk_default_tests {
    use super::*;

    /// The default that cost a node its VM disks: off, on a machine already
    /// serving 39 ublk devices, reported as a transport conflict.
    #[test]
    fn a_local_attach_is_offered_ublk_by_default() {
        assert!(ManagementConfig::default().ublk_transport);
        // And a config that says otherwise is still obeyed — the flag is the
        // door to forcing every attach through the network.
        let off: StormBlockConfig =
            toml::from_str("[management]\nublk_transport = false\n").unwrap();
        assert!(!off.management.ublk_transport);
        // A config that does not mention it gets the new default rather than
        // serde's `bool` default of false.
        let quiet: StormBlockConfig = toml::from_str("[management]\n").unwrap();
        assert!(quiet.management.ublk_transport);
    }
}
