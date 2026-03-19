//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

use std::sync::Arc;

use clap::Parser;

use stormblock::drive::{self, BlockDevice};
use stormblock::drive::pool::DiskPool;
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

    /// Subcommand (pool, nbd, migrate)
    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// DiskPool management
    Pool {
        #[command(subcommand)]
        action: PoolAction,
    },
    /// Export a volume via NBD to the local kernel
    Nbd {
        /// Volume UUID to export
        #[arg(long)]
        volume: String,
        /// NBD listen address (default: 127.0.0.1:10809)
        #[arg(long, default_value = "127.0.0.1:10809")]
        listen: String,
    },
    /// Live migrate from iSCSI to local disk
    Migrate {
        /// Path to local disk for migration target
        #[arg(long)]
        local_disk: String,
        /// VDrive label for the local copy
        #[arg(long, default_value = "root")]
        label: String,
    },
}

#[derive(clap::Subcommand)]
enum PoolAction {
    /// Format a device as a DiskPool
    Format {
        /// Device path to format
        device: String,
    },
    /// List pools on specified devices
    List {
        /// Device paths to scan
        devices: Vec<String>,
    },
    /// List VDrives in a pool
    Vdrives {
        /// Device path of the pool
        device: String,
    },
    /// Create a VDrive in a pool
    CreateVdrive {
        /// Device path of the pool
        device: String,
        /// Label for the VDrive
        #[arg(long)]
        label: String,
        /// Size of the VDrive (e.g. 100G)
        #[arg(long)]
        size: String,
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
            SubCommand::Pool { action } => {
                return handle_pool_command(action).await;
            }
            SubCommand::Nbd { volume: _, listen: _ } => {
                tracing::info!("NBD export mode — requires running storage engine");
                tracing::info!("Use the REST API POST /api/v1/exports to configure NBD exports");
                return Ok(());
            }
            SubCommand::Migrate { local_disk, label } => {
                tracing::info!("Migration mode: target={}, label={}", local_disk, label);
                tracing::info!("Migration requires a running StormBlock instance with an active RAID 1 volume.");
                tracing::info!("Use the REST API POST /api/v1/volumes/{{id}}/migrate to trigger migration.");
                return Ok(());
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
    let mut state = Arc::new(AppState::new(config.clone(), volume_manager));

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

async fn handle_pool_command(action: &PoolAction) -> anyhow::Result<()> {
    match action {
        PoolAction::Format { device } => {
            let dev = Arc::new(
                stormblock::drive::filedev::FileDevice::open(device).await?
            ) as Arc<dyn BlockDevice>;
            let pool = DiskPool::format(dev, device).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Pool formatted: {}", pool.pool_uuid());
            println!("  capacity: {} bytes", pool.total_capacity());
            println!("  data offset: {} bytes", pool.data_offset());
        }
        PoolAction::List { devices } => {
            for device in devices {
                match stormblock::drive::filedev::FileDevice::open(device).await {
                    Ok(dev) => {
                        let dev = Arc::new(dev) as Arc<dyn BlockDevice>;
                        match DiskPool::open(dev, device).await {
                            Ok(pool) => {
                                println!("{}: pool {} ({} VDrives, {} free)",
                                    device, pool.pool_uuid(), pool.vdrive_count(),
                                    stormblock::mgmt::config::human_size(pool.free_space()));
                            }
                            Err(e) => {
                                println!("{}: not a pool ({e})", device);
                            }
                        }
                    }
                    Err(e) => {
                        println!("{}: cannot open ({e})", device);
                    }
                }
            }
        }
        PoolAction::Vdrives { device } => {
            let dev = Arc::new(
                stormblock::drive::filedev::FileDevice::open(device).await?
            ) as Arc<dyn BlockDevice>;
            let pool = DiskPool::open(dev, device).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Pool {} — {} VDrives:", pool.pool_uuid(), pool.vdrive_count());
            for entry in pool.list_vdrives() {
                println!("  {} [{}] {} — {} ({:?})",
                    entry.uuid, entry.label,
                    stormblock::mgmt::config::human_size(entry.size),
                    stormblock::mgmt::config::human_size(entry.start_offset),
                    entry.state);
            }
        }
        PoolAction::CreateVdrive { device, label, size } => {
            let sz = parse_size(size)
                .map_err(|e| anyhow::anyhow!("invalid size: {e}"))?;
            let dev = Arc::new(
                stormblock::drive::filedev::FileDevice::open(device).await?
            ) as Arc<dyn BlockDevice>;
            let mut pool = DiskPool::open(dev, device).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let entry = pool.create_vdrive(sz, label).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("VDrive created: {}", entry.uuid);
            println!("  label: {}", entry.label);
            println!("  size: {} bytes", entry.size);
            println!("  offset: {}", entry.start_offset);
        }
    }
    Ok(())
}
