//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

use std::sync::Arc;

use clap::Parser;

use stormblock::drive::{self, BlockDevice};
use stormblock::drive::slab::{Slab, DEFAULT_SLOT_SIZE as SLAB_SLOT_SIZE};
use stormblock::boot_iscsi::{BootDiskLayout, IscsiBootManager};
use stormblock::placement::topology::StorageTier;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};
use stormblock::target::{self, reactor::{ReactorConfig, ReactorPool}};
use stormblock::mgmt::{self, AppState, ArrayInfo, DriveInfo};
use stormblock::mgmt::config::{StormBlockConfig, parse_size};
#[cfg(feature = "cluster")]
use stormblock::cluster;

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

    /// iSCSI listen address (default: 0.0.0.0:3260)
    #[cfg(feature = "iscsi")]
    #[arg(long, default_value = "0.0.0.0:3260")]
    iscsi_addr: String,

    /// iSCSI target name (IQN)
    #[cfg(feature = "iscsi")]
    #[arg(long, default_value = "iqn.2024.io.stormblock:default")]
    iscsi_target_name: String,

    /// CHAP username for iSCSI authentication
    #[cfg(feature = "iscsi")]
    #[arg(long)]
    chap_user: Option<String>,

    /// CHAP secret for iSCSI authentication
    #[cfg(feature = "iscsi")]
    #[arg(long)]
    chap_secret: Option<String>,

    /// Disable iSCSI target
    #[cfg(feature = "iscsi")]
    #[arg(long)]
    no_iscsi: bool,

    /// NVMe-oF/TCP listen address (default: 0.0.0.0:4420)
    #[cfg(feature = "nvmeof")]
    #[arg(long, default_value = "0.0.0.0:4420")]
    nvmeof_addr: String,

    /// NVMe-oF subsystem NQN
    #[cfg(feature = "nvmeof")]
    #[arg(long, default_value = "nqn.2024.io.stormblock:default")]
    nvmeof_nqn: String,

    /// Disable NVMe-oF/TCP target
    #[cfg(feature = "nvmeof")]
    #[arg(long)]
    no_nvmeof: bool,

    /// Number of reactor cores (0 = auto-detect)
    #[arg(long, default_value = "0")]
    reactor_cores: usize,

    /// Directory for persisting volume metadata (enables restart recovery)
    #[arg(long)]
    data_dir: Option<String>,

    /// Subcommand (slab, ublk, migrate)
    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Slab extent store management
    Slab {
        #[command(subcommand)]
        action: SlabAction,
    },
    /// Export a volume via ublk to the local kernel (/dev/ublkbN)
    Ublk {
        /// Volume UUID to export
        #[arg(long)]
        volume: String,
        /// Number of I/O queues (default: 1)
        #[arg(long, default_value = "1")]
        queues: u16,
    },
    /// Live migrate from iSCSI to local disk
    Migrate {
        /// Path to local disk for migration target
        #[arg(long)]
        local_disk: String,
        /// Slab tier for the local device
        #[arg(long, default_value = "hot")]
        tier: String,
    },
    /// Boot from iSCSI — create partitioned disk with ublk devices
    BootIscsi {
        /// iSCSI target portal (IP address)
        #[arg(long)]
        portal: String,
        /// iSCSI target port (default: 3260)
        #[arg(long, default_value = "3260")]
        port: u16,
        /// iSCSI target IQN
        #[arg(long)]
        iqn: String,
        /// Partition layout (format: name:size,... e.g. esp:256M,boot:512M,root:6G,swap:1G,home:rest)
        #[arg(long)]
        layout: String,
        /// Export each partition as /dev/ublkbN (requires Linux 6.0+ with ublk_drv loaded)
        #[arg(long)]
        ublk: bool,
    },
    /// Migrate boot volumes from iSCSI slab to local disk
    MigrateBoot {
        /// iSCSI target portal (IP address)
        #[arg(long)]
        source_portal: String,
        /// iSCSI target port (default: 3260)
        #[arg(long, default_value = "3260")]
        source_port: u16,
        /// iSCSI target IQN
        #[arg(long)]
        source_iqn: String,
        /// Local device path to migrate to
        #[arg(long)]
        target_device: String,
        /// Target device tier (default: hot)
        #[arg(long, default_value = "hot")]
        target_tier: String,
    },
}

#[derive(clap::Subcommand)]
enum SlabAction {
    /// Format a device as a Slab
    Format {
        /// Device path to format
        device: String,
        /// Storage tier (hot, warm, cool, cold)
        #[arg(long, default_value = "hot")]
        tier: String,
    },
    /// List slabs on specified devices
    List {
        /// Device paths to scan
        devices: Vec<String>,
    },
    /// Show slab details and slot usage
    Info {
        /// Device path of the slab
        device: String,
    },
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

    // Load and merge configuration
    let mut config = StormBlockConfig::load(&cli.config)?;
    let cli_volumes: Vec<(String, u64)> = cli.volumes.iter()
        .map(|v| (v.name.clone(), v.size))
        .collect();
    config.merge_cli(
        &cli.device,
        cli.raid,
        cli.stripe_kb,
        &cli_volumes,
        #[cfg(feature = "iscsi")]
        Some(&cli.iscsi_addr),
        #[cfg(feature = "iscsi")]
        Some(&cli.iscsi_target_name),
        #[cfg(feature = "iscsi")]
        cli.chap_user.as_deref(),
        #[cfg(feature = "iscsi")]
        cli.chap_secret.as_deref(),
        #[cfg(feature = "nvmeof")]
        Some(&cli.nvmeof_addr),
        #[cfg(feature = "nvmeof")]
        Some(&cli.nvmeof_nqn),
        cli.reactor_cores,
    );
    config.validate()?;

    // Handle subcommands
    if let Some(cmd) = &cli.command {
        match cmd {
            SubCommand::Slab { action } => {
                return handle_slab_command(action).await;
            }
            SubCommand::Ublk { volume: _, queues: _ } => {
                tracing::info!("ublk export mode — requires running storage engine");
                tracing::info!("Use the REST API POST /api/v1/exports to configure ublk exports");
                tracing::info!("Requires Linux 6.0+ with ublk_drv module loaded");
                return Ok(());
            }
            SubCommand::Migrate { local_disk, tier } => {
                tracing::info!("Migration mode: target={}, tier={}", local_disk, tier);
                tracing::info!("Migration requires a running StormBlock instance.");
                tracing::info!("Use the REST API POST /api/v1/volumes/{{id}}/migrate to trigger migration.");
                return Ok(());
            }
            SubCommand::BootIscsi { portal, port, iqn, layout, ublk } => {
                return handle_boot_iscsi(portal, *port, iqn, layout, *ublk).await;
            }
            SubCommand::MigrateBoot { source_portal, source_port, source_iqn, target_device, target_tier } => {
                return handle_migrate_boot(source_portal, *source_port, source_iqn, target_device, target_tier).await;
            }
        }
    }

    // Initialize metrics
    mgmt::metrics::init_metrics();
    mgmt::metrics::register_metrics();

    // Build shared state
    let data_dir = cli.data_dir.as_deref()
        .or(config.management.data_dir.as_deref());
    let volume_manager = match data_dir {
        Some(dir) => {
            tracing::info!("Volume metadata persistence enabled: {dir}");
            VolumeManager::with_data_dir(DEFAULT_EXTENT_SIZE, dir.into())?
        }
        None => VolumeManager::new(DEFAULT_EXTENT_SIZE),
    };
    let slab_registry = volume_manager.registry().clone();
    let gem = volume_manager.gem().clone();
    let mut state = Arc::new(AppState::new(config.clone(), volume_manager, slab_registry, gem));

    // Collect device paths from config
    let device_paths: Vec<String> = config.drives.iter()
        .map(|d| d.path.clone())
        .collect();

    // Collect the first volume device for target export
    let mut export_device: Option<Arc<dyn BlockDevice>> = None;

    // Phase 1: Open drives
    if !device_paths.is_empty() {
        let results = drive::open_drives(&device_paths).await;
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
                    let arc_dev: Arc<dyn BlockDevice> = Arc::from(dev);
                    // Register in state
                    {
                        let mut state_drives = state.drives.write().await;
                        state_drives.push(DriveInfo {
                            device: arc_dev.clone(),
                            path: path.clone(),
                        });
                    }
                    drives.push(arc_dev);
                }
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                }
            }
        }
        tracing::info!("{} drive(s) ready", drives.len());
        metrics::gauge!("stormblock_drives_total").set(drives.len() as f64);
        metrics::gauge!("stormblock_capacity_bytes").set(
            drives.iter().map(|d| d.capacity_bytes() as f64).sum::<f64>()
        );

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
                    for (idx, member_state) in array.member_states() {
                        tracing::info!("  member {idx}: {member_state}");
                    }

                    let array_id = array.array_id();
                    let array_level = array.level();
                    let array_member_count = array.member_count();
                    let array_capacity = array.capacity_bytes();
                    let array_stripe = array.stripe_size();

                    // Phase 3: Create volumes if requested
                    if !cli.volumes.is_empty() {
                        let arc_array = Arc::new(array);
                        let backing: Arc<dyn BlockDevice> = arc_array.clone();

                        // Register array in state + volume manager
                        {
                            let mut vm = state.volume_manager.lock().await;
                            vm.add_backing_device(array_id, backing).await;
                        }
                        {
                            let mut state_arrays = state.arrays.write().await;
                            state_arrays.insert(array_id, ArrayInfo {
                                array: arc_array,
                                level: array_level,
                                member_count: array_member_count,
                                capacity_bytes: array_capacity,
                                stripe_size: array_stripe,
                            });
                        }

                        // Try restoring persisted volumes first
                        let mut restored = false;
                        {
                            let mut vm = state.volume_manager.lock().await;
                            match vm.restore().await {
                                Ok(()) => {
                                    let existing = vm.list_volumes().await;
                                    if !existing.is_empty() {
                                        restored = true;
                                        tracing::info!("Restored {} volume(s) from metadata", existing.len());
                                        for (id, name, vsize, allocated) in &existing {
                                            if export_device.is_none() {
                                                export_device = vm.get_volume(id);
                                            }
                                            let _ = (name, vsize, allocated); // logged by restore()
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Volume restore failed: {e}, creating from config");
                                }
                            }
                        }

                        if !restored {
                            for spec in &cli.volumes {
                                let mut vm = state.volume_manager.lock().await;
                                match vm.create_volume(&spec.name, spec.size, array_id).await {
                                    Ok(vol_id) => {
                                        tracing::info!(
                                            "Volume '{}' ({}) created — virtual={} bytes ({:.1} GB)",
                                            spec.name, vol_id, spec.size,
                                            spec.size as f64 / (1024.0 * 1024.0 * 1024.0),
                                        );
                                        // Export the first volume via target protocols
                                        if export_device.is_none() {
                                            export_device = vm.get_volume(&vol_id);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to create volume '{}': {e}", spec.name);
                                    }
                                }
                            }
                        }

                        let vm = state.volume_manager.lock().await;
                        let vols = vm.list_volumes().await;
                        tracing::info!("{} volume(s) ready:", vols.len());
                        for (id, name, vsize, allocated) in &vols {
                            tracing::info!(
                                "  {} ({}) — virtual={:.1} GB, allocated={:.1} MB",
                                name, id,
                                *vsize as f64 / (1024.0 * 1024.0 * 1024.0),
                                *allocated as f64 / (1024.0 * 1024.0),
                            );
                        }
                        metrics::gauge!("stormblock_volumes_total").set(vols.len() as f64);
                    } else {
                        // No volumes specified — export the raw array
                        export_device = Some(Arc::new(array));
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create RAID array: {e}");
                    return Err(e.into());
                }
            }
        } else if drives.len() == 1 {
            // Single drive, no RAID — export directly
            export_device = Some(drives.into_iter().next().unwrap());
        }
    } else {
        tracing::info!("No devices specified (use -d /path/to/device)");
    }

    // Phase 6: Start cluster engine (if enabled)
    #[cfg(feature = "cluster")]
    if config.cluster.enabled {
        match cluster::ClusterManager::new(config.cluster.clone(), &state).await {
            Ok(mut cluster_mgr) => {
                if let Err(e) = cluster_mgr.start(&state).await {
                    tracing::error!("Cluster start failed: {e}");
                } else {
                    // Store cluster manager in AppState
                    // SAFETY: we have the only Arc reference at this point
                    let state_mut = Arc::get_mut(&mut state)
                        .expect("AppState has multiple references before cluster init");
                    state_mut.cluster = Some(Arc::new(cluster_mgr));
                    tracing::info!("Cluster engine started");
                }
            }
            Err(e) => {
                tracing::error!("Cluster init failed: {e}");
            }
        }
    }

    // Phase 5: Start management API
    tokio::spawn({
        let state = state.clone();
        async move {
            if let Err(e) = mgmt::start_management_server(state).await {
                tracing::error!("Management API error: {e}");
            }
        }
    });

    // StormFS registration (announce volumes to StormFS metadata cluster)
    let _stormfs_handle = if config.stormfs.enabled {
        tracing::info!(
            "StormFS registration enabled — metadata: {}, interval: {}s",
            config.stormfs.metadata_url,
            config.stormfs.heartbeat_secs,
        );
        let reg = stormblock::stormfs::StormFsRegistration::new(config.stormfs.clone());
        Some(reg.start(state.clone()))
    } else {
        None
    };

    // Phase 4: Start target protocols
    if let Some(device) = export_device {
        let reactor_config = ReactorConfig {
            core_count: cli.reactor_cores,
            pin_cores: cfg!(target_os = "linux"),
        };
        let reactor = ReactorPool::new(&reactor_config);

        // Start iSCSI target
        #[cfg(feature = "iscsi")]
        if !cli.no_iscsi {
            let chap = match (&cli.chap_user, &cli.chap_secret) {
                (Some(user), Some(secret)) => Some(target::iscsi::chap::ChapConfig {
                    username: user.clone(),
                    secret: secret.clone(),
                }),
                _ => None,
            };

            let iscsi_config = target::iscsi::IscsiConfig {
                listen_addr: cli.iscsi_addr.parse()
                    .expect("invalid iSCSI listen address"),
                target_name: cli.iscsi_target_name.clone(),
                chap,
                max_sessions: 64,
            };
            let mut iscsi = target::iscsi::IscsiTarget::new(iscsi_config);
            iscsi.add_lun(0, device.clone());
            let iscsi = Arc::new(iscsi);
            let iscsi_reactor = &reactor;
            tokio::spawn({
                let iscsi = iscsi.clone();
                let _reactor_cores = iscsi_reactor.core_count();
                async move {
                    if let Err(e) = iscsi.run(&ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false })).await {
                        tracing::error!("iSCSI target error: {e}");
                    }
                }
            });
        }

        // Start NVMe-oF/TCP target
        #[cfg(feature = "nvmeof")]
        if !cli.no_nvmeof {
            let nvmeof_config = target::nvmeof::NvmeofConfig {
                listen_addr: cli.nvmeof_addr.parse()
                    .expect("invalid NVMe-oF listen address"),
                nqn: cli.nvmeof_nqn.clone(),
                ..Default::default()
            };
            let mut nvmeof = target::nvmeof::NvmeofTarget::new(nvmeof_config);
            nvmeof.add_namespace(1, device.clone());
            let nvmeof = Arc::new(nvmeof);
            tokio::spawn({
                let nvmeof = nvmeof.clone();
                async move {
                    if let Err(e) = nvmeof.run(&ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false })).await {
                        tracing::error!("NVMe-oF target error: {e}");
                    }
                }
            });
        }

        tracing::info!("StormBlock ready, waiting for connections (Ctrl+C to stop)");
        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutting down...");
        {
            let vm = state.volume_manager.lock().await;
            vm.persist().await;
        }
        #[cfg(feature = "cluster")]
        if let Some(ref _cluster_mgr) = state.cluster {
            // Cluster manager shutdown requires &mut — use Arc::try_unwrap or just log
            tracing::info!("Cluster shutdown initiated");
        }
        drop(reactor);
    } else {
        tracing::info!("No device to export — management API still running on {}",
            config.management.listen_addr);
        tracing::info!("Press Ctrl+C to stop");
        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutting down...");
        {
            let vm = state.volume_manager.lock().await;
            vm.persist().await;
        }
    }

    Ok(())
}

fn parse_tier(s: &str) -> Result<StorageTier, String> {
    match s.to_lowercase().as_str() {
        "hot" => Ok(StorageTier::Hot),
        "warm" => Ok(StorageTier::Warm),
        "cool" => Ok(StorageTier::Cool),
        "cold" => Ok(StorageTier::Cold),
        _ => Err(format!("unknown tier '{s}' (use hot, warm, cool, cold)")),
    }
}

async fn handle_slab_command(action: &SlabAction) -> anyhow::Result<()> {
    match action {
        SlabAction::Format { device, tier } => {
            let tier = parse_tier(tier)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let dev = Arc::new(
                stormblock::drive::filedev::FileDevice::open(device).await?
            ) as Arc<dyn BlockDevice>;
            let slab = Slab::format(dev, SLAB_SLOT_SIZE, tier).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Slab formatted: {}", slab.slab_id());
            println!("  tier: {}", slab.tier());
            println!("  slot size: {} bytes", slab.slot_size());
            println!("  total slots: {}", slab.total_slots());
            println!("  capacity: {}", stormblock::mgmt::config::human_size(
                slab.total_slots() * slab.slot_size()));
        }
        SlabAction::List { devices } => {
            for device in devices {
                match stormblock::drive::filedev::FileDevice::open(device).await {
                    Ok(dev) => {
                        let dev = Arc::new(dev) as Arc<dyn BlockDevice>;
                        match Slab::open(dev).await {
                            Ok(slab) => {
                                println!("{}: slab {} (tier={}, {} slots, {} free)",
                                    device, slab.slab_id(), slab.tier(),
                                    slab.total_slots(), slab.free_slots());
                            }
                            Err(e) => {
                                println!("{}: not a slab ({e})", device);
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: cannot open ({e})", device);
                    }
                }
            }
        }
        SlabAction::Info { device } => {
            let dev = Arc::new(
                stormblock::drive::filedev::FileDevice::open(device).await?
            ) as Arc<dyn BlockDevice>;
            let slab = Slab::open(dev).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Slab {}", slab.slab_id());
            println!("  tier: {}", slab.tier());
            println!("  slot size: {} bytes", slab.slot_size());
            println!("  total slots: {}", slab.total_slots());
            println!("  free slots: {}", slab.free_slots());
            println!("  allocated slots: {}", slab.allocated_slots());
            println!("  capacity: {}", stormblock::mgmt::config::human_size(
                slab.total_slots() * slab.slot_size()));
            println!("  free: {}", stormblock::mgmt::config::human_size(
                slab.free_slots() * slab.slot_size()));
        }
    }
    Ok(())
}

async fn handle_boot_iscsi(
    portal: &str,
    port: u16,
    iqn: &str,
    layout_str: &str,
    ublk: bool,
) -> anyhow::Result<()> {
    let layout = BootDiskLayout::parse(layout_str)
        .map_err(|e| anyhow::anyhow!("layout parse error: {e}"))?;

    println!("Boot-from-iSCSI: {}:{} target={}", portal, port, iqn);
    println!("Partition layout:");
    for part in &layout.partitions {
        let size_str = if part.size == 0 { "rest".to_string() } else {
            stormblock::mgmt::config::human_size(part.size)
        };
        println!("  {} ({}) — {} at {}", part.name, part.fs_type, size_str, part.mount_point);
    }

    let mgr = IscsiBootManager::new();
    let result = mgr.provision(portal, port, iqn, layout).await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("\nBoot disk provisioned on slab {}", result.slab_id);
    println!("Backing: iSCSI {}:{}/{}", portal, port, iqn);
    println!("\nPartitions:");
    for part in &result.partitions {
        println!(
            "  {:6} {:>10}  {}  {} (vol={})",
            part.name,
            stormblock::mgmt::config::human_size(part.size),
            part.fs_type,
            part.mount_point,
            part.volume_id,
        );
    }

    // Export partitions via ublk if requested (Linux only)
    #[cfg(target_os = "linux")]
    if ublk {
        use stormblock::drive::ublk::UblkServer;

        println!("\nStarting ublk export for {} partitions...", result.partitions.len());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut ublk_threads = Vec::new();

        for (i, part) in result.partitions.iter().enumerate() {
            let server = UblkServer::new(part.handle.clone() as Arc<dyn BlockDevice>)
                .with_dev_id(i as u32);
            let rx = shutdown_rx.clone();
            let name = part.name.clone();
            // UblkServer::run() holds raw pointers (not Send), so run on a
            // dedicated OS thread with its own tokio runtime.
            let thread = std::thread::Builder::new()
                .name(format!("ublk-boot-{i}"))
                .spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create ublk tokio runtime");
                    rt.block_on(async move {
                        match server.run(rx).await {
                            Ok(()) => tracing::info!("ublk#{i} ({name}) stopped"),
                            Err(e) => tracing::error!("ublk#{i} ({name}) error: {e}"),
                        }
                    });
                })
                .expect("failed to spawn ublk thread");
            ublk_threads.push(thread);
            println!("  /dev/ublkb{i} ← {} ({}, {})", part.name,
                stormblock::mgmt::config::human_size(part.size), part.fs_type);
        }

        println!("\nublk devices ready. Press Ctrl+C to stop.");
        tokio::signal::ctrl_c().await?;
        println!("Shutting down...");

        // Signal all ublk servers to stop
        let _ = shutdown_tx.send(true);
        for t in ublk_threads {
            let _ = t.join();
        }
    }

    #[cfg(not(target_os = "linux"))]
    if ublk {
        eprintln!("Error: --ublk requires Linux 6.0+ with ublk_drv module loaded");
        std::process::exit(1);
    }

    if !ublk {
        println!("\nVolumes ready for ublk export.");
        println!("On Linux, each volume can be exported as /dev/ublkbN:");
        for (i, part) in result.partitions.iter().enumerate() {
            println!("  /dev/ublkb{i} ← {} ({}, {})", part.name,
                stormblock::mgmt::config::human_size(part.size), part.fs_type);
        }

        // Keep running until Ctrl+C
        println!("\nPress Ctrl+C to stop");
        tokio::signal::ctrl_c().await?;
        println!("Shutting down...");
    }

    // Disconnect iSCSI
    if let Err(e) = result.iscsi_device.disconnect().await {
        tracing::warn!("iSCSI disconnect: {e}");
    }

    Ok(())
}

async fn handle_migrate_boot(
    source_portal: &str,
    source_port: u16,
    source_iqn: &str,
    target_device: &str,
    target_tier: &str,
) -> anyhow::Result<()> {
    let tier = parse_tier(target_tier)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("Boot migration: iSCSI {}:{}/{} → {}", source_portal, source_port, source_iqn, target_device);

    // 1. Connect to iSCSI source and open the existing slab
    let iscsi = stormblock::drive::iscsi_dev::IscsiDevice::connect(source_portal, source_port, source_iqn)
        .await
        .map_err(|e| anyhow::anyhow!("iSCSI connect failed: {e}"))?;
    let iscsi_dev = Arc::new(iscsi) as Arc<dyn BlockDevice>;

    // Open existing slab on iSCSI device
    let source_slab = Slab::open(iscsi_dev).await
        .map_err(|e| anyhow::anyhow!("failed to open slab on iSCSI device: {e}"))?;
    let source_slab_id = source_slab.slab_id();

    println!("Source slab: {} ({} slots, {} allocated)", source_slab_id,
        source_slab.total_slots(), source_slab.allocated_slots());

    // 2. Open local target device
    let local_dev = Arc::new(
        stormblock::drive::filedev::FileDevice::open(target_device).await?
    ) as Arc<dyn BlockDevice>;

    // 3. Build registry + GEM from source slab
    let mut registry = stormblock::drive::slab_registry::SlabRegistry::new();
    let gem = stormblock::volume::gem::GlobalExtentMap::rebuild_from_slabs(
        std::iter::once((&source_slab_id, &source_slab))
    );
    registry.add(source_slab);

    println!("GEM rebuilt: {} extents across {} volumes",
        gem.total_extents(), gem.volume_count());

    // 4. Migrate via placement engine
    let engine = stormblock::placement::PlacementEngine::new();
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let mut gem = gem;
    let result = stormblock::migrate::migrate_to_slab(
        &mut gem, &mut registry, &engine,
        source_slab_id, local_dev, tier, SLAB_SLOT_SIZE,
        &rx,
    ).await.map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

    println!("\nMigration complete:");
    println!("  Source slab: {}", result.source_slab);
    println!("  Dest slab:   {}", result.dest_slab);
    println!("  Migrated:    {} extents", result.migrated);
    println!("  Failed:      {} extents", result.failed);

    if result.failed > 0 {
        anyhow::bail!("{} extents failed to migrate", result.failed);
    }

    println!("\nAll data migrated to local device. Boot volumes now on {}", target_device);

    Ok(())
}
