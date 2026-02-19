//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

mod drive;
mod raid;
mod volume;
mod target;
mod mgmt;

use std::sync::Arc;

use clap::Parser;

use drive::BlockDevice;
use raid::{RaidArray, RaidLevel};
use volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

#[derive(Parser)]
#[command(name = "stormblock", version, about = "Pure Rust block storage engine")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/stormblock/stormblock.toml")]
    config: String,

    /// Device paths to open (overrides config file)
    #[arg(short, long)]
    device: Vec<String>,

    /// Create a RAID array from the specified devices
    #[arg(long, value_parser = parse_raid_level)]
    raid: Option<RaidLevel>,

    /// Stripe size in KB for RAID 5/6/10 (default: 64)
    #[arg(long, default_value = "64")]
    stripe_kb: u64,

    /// Create thin volumes (format: name:size, e.g. data:100G)
    #[arg(long = "volume", value_parser = parse_volume_spec)]
    volumes: Vec<VolumeSpec>,
}

#[derive(Debug, Clone)]
struct VolumeSpec {
    name: String,
    size: u64,
}

fn parse_volume_spec(s: &str) -> Result<VolumeSpec, String> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err("format: name:size (e.g. data:100G)".into());
    }
    let name = parts[0].to_string();
    let size = parse_size(parts[1])?;
    Ok(VolumeSpec { name, size })
}

fn parse_size(s: &str) -> Result<u64, String> {
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
    let num: u64 = num_str.parse()
        .map_err(|_| format!("invalid size number: '{num_str}'"))?;
    Ok(num * multiplier)
}

fn parse_raid_level(s: &str) -> Result<RaidLevel, String> {
    match s {
        "1" | "raid1" | "mirror" => Ok(RaidLevel::Raid1),
        "5" | "raid5" => Ok(RaidLevel::Raid5),
        "6" | "raid6" => Ok(RaidLevel::Raid6),
        "10" | "raid10" => Ok(RaidLevel::Raid10),
        _ => Err(format!("unknown RAID level '{s}' (use 1, 5, 6, or 10)")),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    tracing::info!("StormBlock starting, config: {}", cli.config);

    // Phase 1: Open drives
    if !cli.device.is_empty() {
        let results = drive::open_drives(&cli.device).await;
        let mut drives: Vec<Arc<dyn BlockDevice>> = Vec::new();
        for (path, result) in results {
            match result {
                Ok(dev) => {
                    tracing::info!(
                        "Opened {} ({}) — {} bytes, block_size={}, type={}",
                        path,
                        dev.id(),
                        dev.capacity_bytes(),
                        dev.block_size(),
                        dev.device_type(),
                    );
                    drives.push(Arc::from(dev));
                }
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                }
            }
        }
        tracing::info!("{} drive(s) ready", drives.len());

        // Phase 2: Create RAID array if requested
        if let Some(level) = cli.raid {
            let stripe_size = cli.stripe_kb * 1024;
            tracing::info!(
                "Creating {} array with {} members, stripe_size={}KB",
                level, drives.len(), cli.stripe_kb,
            );

            match RaidArray::create(level, drives, Some(stripe_size)).await {
                Ok(array) => {
                    tracing::info!(
                        "{} array {} ready — capacity={} bytes ({:.1} GB), members={}, stripe={}KB",
                        array.level(),
                        array.array_id(),
                        array.capacity_bytes(),
                        array.capacity_bytes() as f64 / (1024.0 * 1024.0 * 1024.0),
                        array.member_count(),
                        array.stripe_size() / 1024,
                    );
                    for (idx, state) in array.member_states() {
                        tracing::info!("  member {idx}: {state}");
                    }

                    // Phase 3: Create volumes if requested
                    if !cli.volumes.is_empty() {
                        let array_id = array.array_id();
                        let backing: Arc<dyn BlockDevice> = Arc::new(array);

                        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
                        mgr.add_backing_device(array_id, backing).await;

                        for spec in &cli.volumes {
                            match mgr.create_volume(&spec.name, spec.size, array_id) {
                                Ok(vol_id) => {
                                    tracing::info!(
                                        "Volume '{}' ({}) created — virtual={} bytes ({:.1} GB)",
                                        spec.name, vol_id, spec.size,
                                        spec.size as f64 / (1024.0 * 1024.0 * 1024.0),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create volume '{}': {e}", spec.name);
                                }
                            }
                        }

                        let vols = mgr.list_volumes().await;
                        tracing::info!("{} volume(s) ready:", vols.len());
                        for (id, name, vsize, allocated) in &vols {
                            tracing::info!(
                                "  {} ({}) — virtual={:.1} GB, allocated={:.1} MB",
                                name, id,
                                *vsize as f64 / (1024.0 * 1024.0 * 1024.0),
                                *allocated as f64 / (1024.0 * 1024.0),
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create RAID array: {e}");
                    return Err(e.into());
                }
            }
        }
    } else {
        tracing::info!("No devices specified (use -d /path/to/device)");
    }

    // TODO: Phase 4+ implementation
    // 1. Parse config
    // 2. Start NVMe-oF/TCP target (:4420)
    // 3. Start iSCSI target (:3260)
    // 4. Start management API (:8443)

    Ok(())
}
