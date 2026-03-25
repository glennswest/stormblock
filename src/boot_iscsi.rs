//! iSCSI boot disk orchestrator — multi-volume partitioned disk on iSCSI backing.
//!
//! Creates a complete Linux drive layout where each partition is an independent
//! `ThinVolume` backed by slab slots on a remote iSCSI device. Each volume gets
//! its own ublk device (`/dev/ublkbN`) for the kernel to use.
//!
//! Partition layout (example 10 GB disk):
//! | ESP  | boot | root     | swap | home    |
//! | 256M | 512M | 6G       | 1G   | ~1.6G   |
//! | vfat | ext4 | ext4     | swap | ext4    |
//!
//! All volumes are dynamically resizable via the StormBlock volume API.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::drive::iscsi_dev::IscsiDevice;
use crate::drive::slab::{Slab, SlabId, DEFAULT_SLOT_SIZE};
use crate::drive::slab_registry::SlabRegistry;
use crate::drive::BlockDevice;
use crate::placement::topology::StorageTier;
use crate::volume::extent::VolumeId;
use crate::volume::gem::GlobalExtentMap;
use crate::volume::thin::{ThinVolume, ThinVolumeHandle, PlacementPolicy, VolumePurpose};

/// A single boot partition definition.
#[derive(Debug, Clone)]
pub struct BootPartition {
    /// Partition name (e.g., "esp", "boot", "root", "swap", "home").
    pub name: String,
    /// Size in bytes. 0 = use remaining space.
    pub size: u64,
    /// Filesystem type (e.g., "vfat", "ext4", "swap").
    pub fs_type: String,
    /// Mount point (e.g., "/boot/efi", "/boot", "/", "swap", "/home").
    pub mount_point: String,
}

/// Boot disk layout — ordered list of partitions.
#[derive(Debug, Clone)]
pub struct BootDiskLayout {
    pub partitions: Vec<BootPartition>,
}

impl BootDiskLayout {
    /// Parse a layout string like "esp:256M,boot:512M,root:6G,swap:1G,home:rest"
    /// into a `BootDiskLayout`.
    pub fn parse(layout_str: &str) -> Result<Self, String> {
        let mut partitions = Vec::new();

        for part_spec in layout_str.split(',') {
            let part_spec = part_spec.trim();
            let parts: Vec<&str> = part_spec.splitn(2, ':').collect();
            if parts.len() != 2 {
                return Err(format!(
                    "invalid partition spec '{part_spec}': expected name:size"
                ));
            }

            let name = parts[0].to_string();
            let size_str = parts[1];

            let (size, fs_type, mount_point) = match name.as_str() {
                "esp" => {
                    let size = parse_partition_size(size_str)?;
                    (size, "vfat".to_string(), "/boot/efi".to_string())
                }
                "boot" => {
                    let size = parse_partition_size(size_str)?;
                    (size, "ext4".to_string(), "/boot".to_string())
                }
                "root" => {
                    let size = parse_partition_size(size_str)?;
                    (size, "ext4".to_string(), "/".to_string())
                }
                "swap" => {
                    let size = parse_partition_size(size_str)?;
                    (size, "swap".to_string(), "swap".to_string())
                }
                "home" => {
                    let size = if size_str == "rest" || size_str == "0" {
                        0 // remaining space
                    } else {
                        parse_partition_size(size_str)?
                    };
                    (size, "ext4".to_string(), "/home".to_string())
                }
                _ => {
                    let size = if size_str == "rest" || size_str == "0" {
                        0
                    } else {
                        parse_partition_size(size_str)?
                    };
                    (size, "ext4".to_string(), format!("/{name}"))
                }
            };

            partitions.push(BootPartition {
                name,
                size,
                fs_type,
                mount_point,
            });
        }

        if partitions.is_empty() {
            return Err("no partitions specified".to_string());
        }

        Ok(BootDiskLayout { partitions })
    }

    /// Calculate actual partition sizes, resolving "rest" (size=0) to fill remaining space.
    pub fn resolve_sizes(&mut self, total_capacity: u64) -> Result<(), String> {
        let fixed_total: u64 = self.partitions.iter().map(|p| p.size).sum();
        let rest_count = self.partitions.iter().filter(|p| p.size == 0).count();

        if rest_count > 1 {
            return Err("only one partition can use 'rest' size".to_string());
        }

        if fixed_total > total_capacity {
            return Err(format!(
                "partition sizes ({}) exceed disk capacity ({})",
                human_size(fixed_total),
                human_size(total_capacity)
            ));
        }

        if rest_count == 1 {
            let remaining = total_capacity - fixed_total;
            for part in &mut self.partitions {
                if part.size == 0 {
                    part.size = remaining;
                    break;
                }
            }
        }

        // Validate all sizes are non-zero
        for part in &self.partitions {
            if part.size == 0 {
                return Err(format!("partition '{}' has zero size", part.name));
            }
        }

        Ok(())
    }
}

/// A provisioned boot partition — volume + metadata.
pub struct ProvisionedPartition {
    /// Partition name.
    pub name: String,
    /// Volume ID in the GEM.
    pub volume_id: VolumeId,
    /// Volume handle for I/O.
    pub handle: Arc<ThinVolumeHandle>,
    /// Size in bytes.
    pub size: u64,
    /// Filesystem type.
    pub fs_type: String,
    /// Mount point.
    pub mount_point: String,
}

/// Result of provisioning a boot disk.
pub struct BootDiskResult {
    /// The iSCSI device used as backing store.
    pub iscsi_device: Arc<IscsiDevice>,
    /// The slab ID on the iSCSI device.
    pub slab_id: SlabId,
    /// Provisioned partitions with their volumes.
    pub partitions: Vec<ProvisionedPartition>,
    /// Shared slab registry.
    pub registry: Arc<Mutex<SlabRegistry>>,
    /// Shared Global Extent Map.
    pub gem: Arc<Mutex<GlobalExtentMap>>,
}

/// Orchestrates creating a multi-volume partitioned disk on an iSCSI backing device.
pub struct IscsiBootManager {
    registry: Arc<Mutex<SlabRegistry>>,
    gem: Arc<Mutex<GlobalExtentMap>>,
}

impl IscsiBootManager {
    /// Create a new boot manager with fresh registry and GEM.
    pub fn new() -> Self {
        IscsiBootManager {
            registry: Arc::new(Mutex::new(SlabRegistry::new())),
            gem: Arc::new(Mutex::new(GlobalExtentMap::new())),
        }
    }

    /// Create a boot manager with existing registry and GEM.
    pub fn with_state(
        registry: Arc<Mutex<SlabRegistry>>,
        gem: Arc<Mutex<GlobalExtentMap>>,
    ) -> Self {
        IscsiBootManager { registry, gem }
    }

    /// Provision a boot disk: connect to iSCSI, format slab, create volumes.
    ///
    /// Returns `BootDiskResult` containing the iSCSI device, slab ID, and
    /// provisioned partition volumes ready for ublk export.
    pub async fn provision(
        &self,
        portal: &str,
        port: u16,
        iqn: &str,
        mut layout: BootDiskLayout,
    ) -> Result<BootDiskResult, BootError> {
        // 1. Connect to iSCSI target
        tracing::info!("Connecting to iSCSI target {portal}:{port} {iqn}");
        let iscsi = IscsiDevice::connect(portal, port, iqn)
            .await
            .map_err(|e| BootError::Connection(e.to_string()))?;

        let capacity = iscsi.capacity_bytes();
        tracing::info!(
            "iSCSI device ready: {} ({:.1} GB)",
            iscsi.id(),
            capacity as f64 / (1024.0 * 1024.0 * 1024.0)
        );

        let iscsi = Arc::new(iscsi);

        // 2. Resolve partition sizes
        layout
            .resolve_sizes(capacity)
            .map_err(BootError::Layout)?;

        // 3. Format iSCSI device as a Slab (Cool tier — remote storage)
        tracing::info!("Formatting iSCSI device as Cool-tier slab");
        let slab = Slab::format(iscsi.clone() as Arc<dyn BlockDevice>, DEFAULT_SLOT_SIZE, StorageTier::Cool)
            .await
            .map_err(|e| BootError::SlabFormat(e.to_string()))?;
        let slab_id = slab.slab_id();
        tracing::info!(
            "Slab {} formatted: {} slots ({:.1} GB usable)",
            slab_id,
            slab.total_slots(),
            slab.total_slots() as f64 * DEFAULT_SLOT_SIZE as f64 / (1024.0 * 1024.0 * 1024.0)
        );

        // 4. Register slab
        {
            let mut reg = self.registry.lock().await;
            reg.add(slab);
        }

        // 5. Create ThinVolume for each partition
        let mut partitions = Vec::new();
        let placement = PlacementPolicy {
            preferred_tier: StorageTier::Cool,
            tier_fallback: vec![StorageTier::Warm, StorageTier::Hot, StorageTier::Cold],
        };

        for part in &layout.partitions {
            tracing::info!(
                "Creating volume '{}': {} ({}) at {}",
                part.name,
                human_size(part.size),
                part.fs_type,
                part.mount_point
            );

            let mut vol = ThinVolume::new(
                part.name.clone(),
                part.size,
                DEFAULT_SLOT_SIZE,
            );
            vol.purpose = match part.name.as_str() {
                "esp" | "boot" => VolumePurpose::Boot,
                _ => VolumePurpose::Partition,
            };

            let vol_id = vol.id();
            let handle = Arc::new(ThinVolumeHandle::new(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                placement.clone(),
            ));

            partitions.push(ProvisionedPartition {
                name: part.name.clone(),
                volume_id: vol_id,
                handle,
                size: part.size,
                fs_type: part.fs_type.clone(),
                mount_point: part.mount_point.clone(),
            });
        }

        tracing::info!(
            "Boot disk provisioned: {} partitions on slab {}",
            partitions.len(),
            slab_id
        );
        for part in &partitions {
            tracing::info!(
                "  {} ({}): {} → {} [{}]",
                part.name,
                part.volume_id,
                human_size(part.size),
                part.mount_point,
                part.fs_type
            );
        }

        Ok(BootDiskResult {
            iscsi_device: iscsi,
            slab_id,
            partitions,
            registry: self.registry.clone(),
            gem: self.gem.clone(),
        })
    }
}

impl Default for IscsiBootManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors during boot disk provisioning.
#[derive(Debug)]
pub enum BootError {
    Connection(String),
    SlabFormat(String),
    Layout(String),
    Volume(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Connection(e) => write!(f, "iSCSI connection failed: {e}"),
            BootError::SlabFormat(e) => write!(f, "slab format failed: {e}"),
            BootError::Layout(e) => write!(f, "layout error: {e}"),
            BootError::Volume(e) => write!(f, "volume error: {e}"),
        }
    }
}

impl std::error::Error for BootError {}

/// Parse a size string like "256M", "6G", "512M", "1G" into bytes.
fn parse_partition_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s == "rest" || s == "0" {
        return Ok(0);
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('G') {
        (n, 1024 * 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('g') {
        (n, 1024 * 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1024u64)
    } else {
        // Assume bytes
        (s, 1u64)
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size number: '{num_str}'"))?;
    Ok(num * multiplier)
}

/// Format bytes as human-readable size string.
fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_layout_basic() {
        let layout =
            BootDiskLayout::parse("esp:256M,boot:512M,root:6G,swap:1G,home:rest").unwrap();
        assert_eq!(layout.partitions.len(), 5);

        assert_eq!(layout.partitions[0].name, "esp");
        assert_eq!(layout.partitions[0].size, 256 * 1024 * 1024);
        assert_eq!(layout.partitions[0].fs_type, "vfat");
        assert_eq!(layout.partitions[0].mount_point, "/boot/efi");

        assert_eq!(layout.partitions[1].name, "boot");
        assert_eq!(layout.partitions[1].size, 512 * 1024 * 1024);

        assert_eq!(layout.partitions[2].name, "root");
        assert_eq!(layout.partitions[2].size, 6 * 1024 * 1024 * 1024);

        assert_eq!(layout.partitions[3].name, "swap");
        assert_eq!(layout.partitions[3].size, 1024 * 1024 * 1024);

        assert_eq!(layout.partitions[4].name, "home");
        assert_eq!(layout.partitions[4].size, 0); // rest
    }

    #[test]
    fn resolve_sizes_rest() {
        let mut layout =
            BootDiskLayout::parse("esp:256M,boot:512M,root:6G,swap:1G,home:rest").unwrap();
        let total = 10 * 1024 * 1024 * 1024u64; // 10 GB
        layout.resolve_sizes(total).unwrap();

        let fixed = 256 * 1024 * 1024 + 512 * 1024 * 1024 + 6 * 1024 * 1024 * 1024
            + 1024 * 1024 * 1024u64;
        let expected_home = total - fixed;
        assert_eq!(layout.partitions[4].size, expected_home);
    }

    #[test]
    fn resolve_sizes_overflow() {
        let mut layout =
            BootDiskLayout::parse("esp:256M,boot:512M,root:12G,swap:1G").unwrap();
        let total = 10 * 1024 * 1024 * 1024u64;
        let result = layout.resolve_sizes(total);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceed"));
    }

    #[test]
    fn parse_partition_sizes() {
        assert_eq!(parse_partition_size("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_partition_size("6G").unwrap(), 6 * 1024 * 1024 * 1024);
        assert_eq!(parse_partition_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_partition_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_partition_size("4096").unwrap(), 4096);
        assert_eq!(parse_partition_size("rest").unwrap(), 0);
    }

    #[test]
    fn parse_layout_empty_rejected() {
        let result = BootDiskLayout::parse("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_layout_invalid_spec() {
        let result = BootDiskLayout::parse("badspec");
        assert!(result.is_err());
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(256 * 1024 * 1024), "256.0 MB");
        assert_eq!(human_size(512 * 1024), "512.0 KB");
        assert_eq!(human_size(100), "100 B");
    }

    #[test]
    fn multiple_rest_rejected() {
        let mut layout =
            BootDiskLayout::parse("root:rest,home:rest").unwrap();
        let result = layout.resolve_sizes(10 * 1024 * 1024 * 1024);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("only one"));
    }
}
