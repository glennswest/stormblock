//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

use std::sync::Arc;

use clap::Parser;

use stormblock::drive::{self, BlockDevice};
use stormblock::drive::slab::{Slab, DEFAULT_SLOT_SIZE as SLAB_SLOT_SIZE};
#[cfg(feature = "iscsi")]
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
    /// Build and inspect disk images and ISOs made of pallets
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Pallets — sealed, versioned sets of images, several per drive
    Pallet {
        /// Drives to work with (files or /dev nodes; a file is a drive like
        /// any other). Repeat for several.
        #[arg(long = "drive", global = true)]
        drives: Vec<String>,
        #[command(subcommand)]
        action: PalletAction,
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
    #[cfg(feature = "iscsi")]
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
    /// Take over the ublk devices an earlier server created, without the
    /// block devices ever disappearing
    ///
    /// The handover the boot needs: the engine the initramfs started cannot be
    /// restarted — `switch_root` deleted the filesystem its binary came from —
    /// so the long-term owner has to be a process that lives in a golden and
    /// can be supervised. This is how it takes the devices on.
    AdoptUblk {
        /// Slab device or file path(s), as `boot-local` was given them
        #[arg(long, required = true)]
        slab: Vec<String>,
        /// Volume to serve on each device, in ublk device order: the first is
        /// `/dev/ublkb0`, and so on. Must match what the previous server had.
        #[arg(long = "volume", required = true)]
        volumes: Vec<String>,
        /// Metadata directory, if the slab does not carry its own
        #[arg(long)]
        meta: Option<String>,
        /// Also serve the management API here (e.g. `127.0.0.1:9090`).
        ///
        /// The process that holds the slab is the engine, and it is the only
        /// one that can be: the slab has a single writer, so a second process
        /// cannot open it to answer for it. Without this the node serves its
        /// root and nothing can ask it for a volume, a template or a clone —
        /// which reads, from the other side, as connection refused.
        #[arg(long)]
        api: Option<String>,
        /// Where the API keeps the state that is not the slab's — templates,
        /// the /v1 record, the wiring table. The slab carries its own volume
        /// metadata, but nothing else has anywhere to live, and without this
        /// a template minted now is gone at the next boot.
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Boot from a local slab — attach an existing slab + metadata
    /// non-destructively and export the boot volume as /dev/ublkb0
    BootLocal {
        /// Slab device or file path(s) (e.g. root.slab). Paired with the
        /// array records in volumes.dat in order.
        #[arg(long, required = true)]
        slab: Vec<String>,
        /// Metadata directory containing volumes.dat (default: "meta" next
        /// to the first slab)
        #[arg(long)]
        meta: Option<String>,
        /// Boot volume, by UUID or name (overrides --boot-config)
        #[arg(long)]
        volume: Option<String>,
        /// initramfs handoff config ([boot] volume = "...")
        #[arg(long, default_value = "/etc/stormblock/boot.toml")]
        boot_config: String,
        /// Also export this volume (UUID or name) as /dev/ublkb1
        #[arg(long)]
        image_store: Option<String>,
        /// Also export a writable volume (UUID or name), one per flag, at the
        /// next /dev/ublkb index after root (and image-store). Order is
        /// preserved so the caller can map each to its mount point. Used for
        /// stormcos's thin /var and /var/lib/containers volumes.
        #[arg(long = "writable")]
        writable: Vec<String>,
        /// After root is up, migrate the slab to this local disk in the
        /// background (zeroboot flow-over)
        #[arg(long)]
        local_disk: Option<String>,
        /// Tier for the --local-disk destination slab
        #[arg(long, default_value = "hot")]
        local_tier: String,
        /// Validate the artifact and resolve the boot volume, then exit
        /// without exporting (no ublk needed)
        #[arg(long)]
        check: bool,
    },
    /// Migrate boot volumes from iSCSI slab to local disk
    #[cfg(feature = "iscsi")]
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

#[derive(clap::Subcommand)]
enum ImageAction {
    /// Build an image from a TOML spec
    Build {
        /// Path to the image spec
        #[arg(long, default_value = "image.toml")]
        spec: String,
        /// Output path. The format is taken from its extension unless
        /// --format says otherwise
        #[arg(long)]
        out: String,
        /// raw, qcow2, vhd, vmdk, iso
        #[arg(long)]
        format: Option<String>,
        /// Keep the intermediate raw image beside a converted one
        #[arg(long)]
        keep_raw: bool,
    },
    /// Convert an existing raw image to another format
    Convert {
        /// Raw image to read
        #[arg(long = "in")]
        input: String,
        #[arg(long)]
        out: String,
        #[arg(long)]
        format: Option<String>,
        /// ISO only: carry the slab too. It is empty in a fresh image, so it
        /// is left out unless asked for
        #[arg(long)]
        include_slab: bool,
    },
    /// Show an image's partitions and the pallets in it
    Inspect {
        /// Image file (raw or ISO)
        image: String,
    },
    /// List the formats this build can write
    Formats,
}

#[derive(clap::Subcommand)]
enum PalletAction {
    /// Write a fresh GPT so a drive can carry pallets
    InitGpt {
        /// Drive to initialize (path or index into --drive)
        drive: String,
        /// Overwrite an existing table
        #[arg(long)]
        force: bool,
    },
    /// List every pallet on every drive
    List {
        /// Only this kind (boot, system, kernel, kube, app, runtime, data)
        #[arg(long)]
        kind: Option<String>,
    },
    /// Show a pallet and its members
    Info {
        /// Pallet UUID
        id: String,
    },
    /// What is selected, what could take over, what will not be used
    Status {
        #[arg(long)]
        kind: Option<String>,
    },
    /// The order a boot-time consumer would try them in
    Chain {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Check a pallet and every member it claims
    Verify {
        /// Pallet UUID, or `all`
        id: String,
    },
    /// Publish a new pallet from files on disk
    Publish {
        /// Pallet name (max 40 bytes; must match the partition name)
        #[arg(long)]
        name: String,
        /// Kind: boot, system, kernel, kube, app, runtime, data
        #[arg(long, default_value = "unspecified")]
        kind: String,
        /// Human-readable version, e.g. 6.12.0-200.fc41
        #[arg(long, default_value = "")]
        label: String,
        /// A member, as name:role:kind:path (repeat)
        #[arg(long = "member", required = true)]
        members: Vec<String>,
        /// Drive to land on (path or index); defaults to the first
        #[arg(long = "on")]
        drive: Option<String>,
        /// Partition size, e.g. 512M. Defaults to fitting the content
        #[arg(long)]
        size: Option<String>,
        /// Verify and select it in one step
        #[arg(long)]
        activate: bool,
    },
    /// Make a pallet the one its consumers select
    Activate { id: String },
    /// Record that a pallet booted and is good
    Successful { id: String },
    /// Select the pallet below the active one
    Rollback {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Copy a pallet to another drive, keeping the original
    Copy {
        id: String,
        /// Destination drive (path or index)
        #[arg(long)]
        to: String,
    },
    /// Move a pallet to another drive, identity and all
    Move {
        id: String,
        #[arg(long)]
        to: String,
    },
    /// Add members to a pallet, publishing it as a new version
    ///
    /// A sealed pallet is never edited in place — that is what sealing is —
    /// so this publishes a new version carrying the existing members plus
    /// the new ones. The old version stays until it is pruned.
    AddMember {
        /// Pallet UUID
        id: String,
        /// A member, as name:role:kind:path (repeat)
        #[arg(long = "member", required = true)]
        members: Vec<String>,
        /// Land the new version on this drive (path or index)
        #[arg(long = "on")]
        drive: Option<String>,
        /// Make the new version the one consumers select
        #[arg(long)]
        activate: bool,
    },
    /// Drop members from a pallet, publishing it as a new version
    RemoveMember {
        /// Pallet UUID
        id: String,
        /// Member name (repeat)
        #[arg(long = "member", required = true)]
        members: Vec<String>,
        #[arg(long = "on")]
        drive: Option<String>,
        #[arg(long)]
        activate: bool,
    },
    /// Copy one member into another pallet, as a new version of the destination
    CopyMember {
        /// Source pallet UUID
        id: String,
        /// Member name
        member: String,
        /// Destination pallet UUID
        #[arg(long)]
        into: String,
    },
    /// Move one member into another pallet, as a new version of each
    MoveMember {
        /// Source pallet UUID
        id: String,
        /// Member name
        member: String,
        /// Destination pallet UUID
        #[arg(long)]
        into: String,
    },
    /// Set the read-only bit
    ReadOnly {
        id: String,
        #[arg(long)]
        value: bool,
        #[arg(long)]
        force: bool,
    },
    /// Set the sealed bit
    Sealed {
        id: String,
        #[arg(long)]
        value: bool,
    },
    /// Remove a pallet's GPT entry
    Delete {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// Keep the newest N versions of a name (never fewer than 2)
    Prune {
        name: String,
        #[arg(long, default_value = "2")]
        keep: usize,
    },
    /// Convert a drive onto another: everything on the source becomes
    /// partitioned pallets on the destination
    Convert {
        /// Source drive (path or index)
        #[arg(long)]
        from: String,
        /// Destination drive (path or index)
        #[arg(long)]
        to: String,
        /// Copy instead of moving — leave every pallet on the source too
        #[arg(long)]
        keep_source: bool,
        /// Give the source a fresh empty table afterwards, so it can carry
        /// pallets. Destructive, and skipped if anything failed to convert
        #[arg(long)]
        reinit_source: bool,
    },
    /// Migrate a whole-drive pallet onto a partitioned drive
    Adopt {
        /// Drive holding the whole-drive pallet
        #[arg(long)]
        from: String,
        /// Partitioned destination drive
        #[arg(long)]
        to: String,
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
    // RUST_LOG controls verbosity (e.g. RUST_LOG=stormblock=debug for
    // per-PDU iSCSI tracing); defaults to info when unset.
    // Logs on stderr, so a subcommand's stdout is only its answer — `pallet
    // list | awk` has to be usable.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
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
            SubCommand::Image { action } => {
                return handle_image_command(action).await;
            }
            SubCommand::Pallet { drives, action } => {
                return handle_pallet_command(drives, action).await;
            }
            SubCommand::Ublk { volume: _, queues: _ } => {
                tracing::info!("ublk export mode — requires running storage engine");
                tracing::info!("For local-slab boot use: stormblock boot-local --slab <path> --volume <id>");
                tracing::info!("Requires Linux 6.0+ with ublk_drv module loaded");
                return Ok(());
            }
            SubCommand::AdoptUblk { slab, volumes, meta, api, data_dir } => {
                return handle_adopt_ublk(
                    slab, volumes, meta.as_deref(), api.as_deref(), data_dir.as_deref(),
                ).await;
            }
            SubCommand::BootLocal {
                slab, meta, volume, boot_config, image_store, writable, local_disk, local_tier, check,
            } => {
                return handle_boot_local(
                    slab,
                    meta.as_deref(),
                    volume.as_deref(),
                    boot_config,
                    image_store.as_deref(),
                    writable,
                    local_disk.as_deref(),
                    local_tier,
                    *check,
                ).await;
            }
            SubCommand::Migrate { local_disk, tier } => {
                tracing::info!("Migration mode: target={}, tier={}", local_disk, tier);
                tracing::info!("Migration requires a running StormBlock instance.");
                tracing::info!("Use the REST API POST /api/v1/volumes/{{id}}/migrate to trigger migration.");
                return Ok(());
            }
            #[cfg(feature = "iscsi")]
            SubCommand::BootIscsi { portal, port, iqn, layout, ublk } => {
                return handle_boot_iscsi(portal, *port, iqn, layout, *ublk).await;
            }
            #[cfg(feature = "iscsi")]
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

    // Node/cluster discovery. Attached before the targets start so peers see
    // this node as soon as it is serving.
    if !config.management.discovery_disabled {
        let node_name = state.local_node_name();
        let mgmt_addr = {
            let host = config.management.resolve_advertised_host(
                config.management.listen_addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(""),
            );
            let port = config.management.listen_addr
                .rsplit_once(':').map(|(_, p)| p).unwrap_or("9090");
            format!("{host}:{port}")
        };
        let disc = Arc::new(mgmt::discovery::Discovery::new(
            node_name,
            mgmt_addr,
            config.management.data_dir.as_ref().map(std::path::PathBuf::from),
            std::time::Duration::from_secs(config.management.peer_stale_secs.max(1)),
        ));
        if let Some(s) = Arc::get_mut(&mut state) {
            s.discovery = Some(disc.clone());
        }
        mgmt::discovery::spawn(
            disc,
            state.clone(),
            std::time::Duration::from_secs(config.management.beacon_secs.max(1)),
        );
    }

    // Background extent collector. Reclaims slab slots no volume maps —
    // capacity that is otherwise unrecoverable without reformatting the slab,
    // since a slab with allocated slots refuses deletion.
    if config.gc.enabled {
        let last = stormblock::volume::gc::spawn(
            state.gem.clone(),
            state.slab_registry.clone(),
            config.gc.clone(),
        );
        if let Some(s) = Arc::get_mut(&mut state) {
            s.last_gc = Some(last);
        }
    }

    // Pool pressure watcher. Thin volumes overcommit, so physical space runs
    // out while every volume still reports free virtual space — nothing else
    // notices until writes start failing (#18).
    if config.pressure.enabled {
        if config.pressure.sources.is_empty() {
            tracing::warn!(
                "pool pressure watching is enabled with no growth sources — pressure will be \
                 reported but nothing can be done about it"
            );
        }
        let status = stormblock::volume::pressure::spawn(
            config.pressure.clone(),
            state.slab_registry.clone(),
            DEFAULT_EXTENT_SIZE,
        );
        if let Some(s) = Arc::get_mut(&mut state) {
            s.pool_pressure = Some(status);
        }
    }

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
    let reactor_config = ReactorConfig {
        core_count: cli.reactor_cores,
        pin_cores: cfg!(target_os = "linux"),
    };
    // One pool shared by both targets, kept alive for the process lifetime —
    // the accept loops run in spawned tasks and dispatch onto it.
    let reactor = Arc::new(ReactorPool::new(&reactor_config));
    tracing::info!(
        "Target connections dispatch across {} reactor core(s)",
        reactor.core_count()
    );

    // Start iSCSI target (always, even with no initial device — LUNs can be added via REST)
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
            max_connections: config
                .iscsi
                .as_ref()
                .map(|c| c.max_connections)
                .unwrap_or(4),
        };
        let iscsi = target::iscsi::IscsiTarget::new(iscsi_config);

        // If we have a device, add it as LUN 0 (preserves existing behavior)
        if let Some(ref device) = export_device {
            iscsi.add_lun(0, device.clone()).await;
        }

        let iscsi = Arc::new(iscsi);

        // Load declarative LUNs from config
        for lun_cfg in &config.luns {
            let dev: Arc<dyn BlockDevice> = if let Some(ref size_str) = lun_cfg.size {
                match parse_size(size_str) {
                    Ok(sz) => match drive::filedev::FileDevice::open_with_capacity(&lun_cfg.path, sz).await {
                        Ok(d) => Arc::new(d),
                        Err(e) => {
                            tracing::error!("Failed to open LUN {} ({}): {e}", lun_cfg.id, lun_cfg.path);
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Invalid size for LUN {}: {e}", lun_cfg.id);
                        continue;
                    }
                }
            } else {
                match drive::open_one_drive(&lun_cfg.path).await {
                    Ok(d) => Arc::from(d),
                    Err(e) => {
                        tracing::error!("Failed to open LUN {} ({}): {e}", lun_cfg.id, lun_cfg.path);
                        continue;
                    }
                }
            };
            iscsi.add_lun_dynamic(lun_cfg.id, dev.clone(), lun_cfg.readonly).await;
            tracing::info!("LUN {} loaded from config: {} ({}{})",
                lun_cfg.id, lun_cfg.path,
                mgmt::config::human_size(dev.capacity_bytes()),
                if lun_cfg.readonly { ", readonly" } else { "" },
            );
        }

        // Store in AppState for REST API access
        {
            let mut target_guard = state.iscsi_target.write().await;
            *target_guard = Some(iscsi.clone());
        }

        // Re-open LUNs created through the API in a previous run (#22). Config
        // LUNs above are declarative and re-added each boot; these are not.
        mgmt::api::luns::restore_luns(&state).await;

        let reactor_for_iscsi = reactor.clone();
        tokio::spawn({
            let iscsi = iscsi.clone();
            async move {
                if let Err(e) = iscsi.run(&reactor_for_iscsi).await {
                    tracing::error!("iSCSI target error: {e}");
                }
            }
        });
    }

    // Start NVMe-oF/TCP target (only if we have a device to export)
    #[cfg(feature = "nvmeof")]
    if !cli.no_nvmeof {
        if let Some(ref device) = export_device {
            let listen_addr: std::net::SocketAddr = cli.nvmeof_addr.parse()
                .expect("invalid NVMe-oF listen address");
            // Report a routable address in the discovery log page — a wildcard
            // listen address is useless to a remote initiator (#26).
            let advertised_addr = config.management
                .advertised_host()
                .and_then(|h| format!("{h}:{}", listen_addr.port()).parse().ok());
            let nvmeof_config = target::nvmeof::NvmeofConfig {
                listen_addr,
                nqn: cli.nvmeof_nqn.clone(),
                advertised_addr,
                ..Default::default()
            };
            let mut nvmeof = target::nvmeof::NvmeofTarget::new(nvmeof_config);
            nvmeof.add_namespace(1, device.clone());
            let nvmeof = Arc::new(nvmeof);

            // Store in AppState so the export API can add namespaces at
            // runtime instead of parking them until the next restart (#26).
            {
                let mut guard = state.nvmeof_target.write().await;
                *guard = Some(nvmeof.clone());
            }
            let reactor_for_nvmeof = reactor.clone();
            tokio::spawn({
                let nvmeof = nvmeof.clone();
                async move {
                    if let Err(e) = nvmeof.run(&reactor_for_nvmeof).await {
                        tracing::error!("NVMe-oF target error: {e}");
                    }
                }
            });
        }
    }

    // Phase 5: the serving surface (#60).
    //
    // `docs/layering.md` puts this in layer 2 — what it takes to serve volumes
    // to something — so the stock binary mounts it rather than leaving each
    // profile to remember. A consumer that runs against a RouterOS node and an
    // x86 one can then rely on `/serve/v1` being there instead of probing for
    // it.
    //
    // Built here, after the targets, for two reasons: the reactor pool it runs
    // per-export portals on exists by now, and so does the shared iSCSI target
    // it reports LUN counts from. The management API is started after it, so
    // the router sees the context rather than racing it.
    // A build without NVMe-oF still has to answer "which interface do the
    // per-export portals bind?", and the answer is the same one it would have
    // been — the range is allocated the same way whichever transport wires it.
    #[cfg(feature = "nvmeof")]
    let nvmeof_bind = cli.nvmeof_addr.clone();
    #[cfg(not(feature = "nvmeof"))]
    let nvmeof_bind = "0.0.0.0:4420".to_string();
    #[cfg(feature = "iscsi")]
    let iscsi_bind = cli.iscsi_addr.clone();
    #[cfg(not(feature = "iscsi"))]
    let iscsi_bind = "0.0.0.0:3260".to_string();

    match config.serve_config(&iscsi_bind, &nvmeof_bind) {
        Ok(serve_cfg) => {
            if let Err(e) = std::fs::create_dir_all(&serve_cfg.data_dir) {
                tracing::error!(
                    "not serving /serve/v1: cannot create {} ({e}) — the wiring table has to \
                     survive a restart",
                    serve_cfg.data_dir
                );
            } else {
                #[cfg(feature = "iscsi")]
                let shared_iscsi = state.iscsi_target.read().await.clone();

                let wiring = stormblock::serve::wiring::WiringTable::load(&serve_cfg.data_dir);
                let status = Arc::new(stormblock::serve::status::MkStatus::new());
                tracing::info!(
                    "Serving /serve/v1 — advertising {}, portals {}..{}, state in {}",
                    serve_cfg.advertise_addr,
                    serve_cfg.portal_base,
                    serve_cfg.portal_base.saturating_add(serve_cfg.portal_span),
                    serve_cfg.data_dir,
                );
                let reconcile_secs = serve_cfg.reconcile_secs;
                let reap_secs = serve_cfg.reap_secs;
                let ctx = Arc::new(stormblock::serve::ctx::ServeContext::new(
                    serve_cfg,
                    state.clone(),
                    status,
                    #[cfg(feature = "iscsi")]
                    shared_iscsi,
                    reactor.clone(),
                    wiring,
                ));
                if state.serve.set(ctx.clone()).is_err() {
                    tracing::error!("serving context was already set — not starting a second one");
                } else {
                    tokio::spawn(stormblock::serve::reconcile::run(ctx.clone()));
                    tracing::debug!("export reconciler running every {reconcile_secs}s");
                    if reap_secs > 0 {
                        tokio::spawn(stormblock::serve::reap::run(ctx));
                        tracing::debug!("template reaper running every {reap_secs}s");
                    }
                }
            }
        }
        // Not an error: a node that is not meant to serve, or has nowhere to
        // keep the wiring table, is a legitimate configuration. But it is
        // never silent — a consumer getting 404s from /serve/v1 has to be able
        // to find out why from this node's log.
        Err(why) => tracing::warn!("not serving /serve/v1: {why}"),
    }

    // Phase 6: Start management API. Last, so the router it builds sees
    // everything above it.
    tokio::spawn({
        let state = state.clone();
        async move {
            if let Err(e) = mgmt::start_management_server(state).await {
                tracing::error!("Management API error: {e}");
            }
        }
    });

    if export_device.is_some() {
        tracing::info!("StormBlock ready, waiting for connections (Ctrl+C to stop)");
    } else {
        tracing::info!("No device to export — LUNs can be added via REST API POST /api/v1/luns");
        tracing::info!("Management API running on {}, press Ctrl+C to stop", config.management.listen_addr);
    }

    // SIGINT (Ctrl+C) and SIGTERM (systemctl stop) both shut down gracefully.
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");
    {
        let vm = state.volume_manager.lock().await;
        vm.persist().await;
    }
    #[cfg(feature = "cluster")]
    if let Some(ref _cluster_mgr) = state.cluster {
        tracing::info!("Cluster shutdown initiated");
    }
    drop(reactor);

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

// ---------------------------------------------------------------- pallets

/// Open the drives a pallet command works over. A file is a drive here — same
/// GPT, same partitions — which is what makes an image assembled on a laptop
/// and a disk in a node the same thing.
async fn pallet_store(drives: &[String]) -> anyhow::Result<stormblock::pallet::PalletStore> {
    let mut store = stormblock::pallet::PalletStore::default();
    for path in drives {
        let dev = stormblock::drive::open_one_drive(path)
            .await
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        store.add_drive(path.clone(), Arc::from(dev));
    }
    Ok(store)
}

fn parse_member_spec(s: &str) -> anyhow::Result<(String, String, String, String)> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [name, role, kind, path] => Ok((
            name.to_string(),
            role.to_string(),
            kind.to_string(),
            path.to_string(),
        )),
        [name, role, path] => Ok((
            name.to_string(),
            role.to_string(),
            role.to_string(),
            path.to_string(),
        )),
        _ => Err(anyhow::anyhow!(
            "member must be name:role:kind:path (or name:role:path), got '{s}'"
        )),
    }
}

fn print_pallet(p: &stormblock::pallet::PalletLocation) {
    let where_ = if p.is_whole_drive() {
        format!("{} (whole drive, no GPT)", p.drive)
    } else {
        format!("{}#{}", p.drive, p.entry_index)
    };
    let state = if p.is_readable() { "" } else { " UNREADABLE" };
    println!(
        "{}  {} v{} [{}] {:<10} pri={} tries={} {}{}{}{}",
        p.id,
        p.name,
        p.version,
        p.kind,
        where_,
        p.attributes.priority,
        p.attributes.tries_left,
        if p.attributes.successful { "good " } else { "" },
        if p.attributes.sealed { "sealed " } else { "" },
        if p.attributes.read_only { "ro" } else { "rw" },
        state,
    );
}

/// Pallet errors carry their own explanation; this just changes the type.
fn pe<T>(r: Result<T, stormblock::pallet::PalletError>) -> anyhow::Result<T> {
    r.map_err(|err| anyhow::anyhow!("{err}"))
}

// ----------------------------------------------------------------- images

async fn handle_image_command(action: &ImageAction) -> anyhow::Result<()> {
    use std::path::{Path, PathBuf};
    use stormblock::image::{ImageBuilder, ImageFormat, ImageSpec};

    let ie = |e: stormblock::image::ImageError| anyhow::anyhow!("{e}");
    let resolve = |out: &str, want: &Option<String>| -> anyhow::Result<ImageFormat> {
        match want {
            Some(f) => ImageFormat::parse(f)
                .ok_or_else(|| anyhow::anyhow!("unknown image format '{f}'")),
            None => Ok(ImageFormat::from_path(Path::new(out)).unwrap_or(ImageFormat::Raw)),
        }
    };

    match action {
        ImageAction::Formats => {
            for f in ImageFormat::ALL {
                println!("{:<6} .{}", f.as_str(), f.extension());
            }
        }
        ImageAction::Build { spec, out, format, keep_raw } => {
            let format = resolve(out, format)?;
            let spec_dir = Path::new(spec).parent().map(PathBuf::from);
            let image_spec = ImageSpec::load(spec).await.map_err(ie)?;
            // Paths in a spec are relative to the spec, which is what anyone
            // editing one expects.
            if let Some(dir) = spec_dir.filter(|d| !d.as_os_str().is_empty()) {
                std::env::set_current_dir(&dir)
                    .map_err(|e| anyhow::anyhow!("cannot enter {}: {e}", dir.display()))?;
            }
            let out_path = PathBuf::from(out);
            let raw_path = if format == ImageFormat::Raw {
                out_path.clone()
            } else {
                out_path.with_extension("raw.img")
            };

            let report = ImageBuilder::new(image_spec).build(&raw_path).await.map_err(ie)?;
            println!(
                "{} — {} in {} partitions, GPT in {}-byte LBAs",
                raw_path.display(),
                stormblock::mgmt::config::human_size(report.size_bytes),
                report.partitions.len(),
                report.block_size
            );
            // Firmware parses the GPT using the *media's* block size, and does
            // not probe for it the way `Gpt::read` does. A 512-LBA image
            // written to a 4Kn drive puts the header where firmware will not
            // look, and the symptom is a disk that simply does not boot — so
            // say which one was written whenever the image is meant to.
            if report.block_size == 512 && report.partitions.iter().any(|p| p.kind == "esp") {
                println!(
                    "  note: bootable image at 512-byte LBAs. A 4Kn target needs \
                     `block_size = 4096` in the spec, or firmware will not find the GPT."
                );
            }
            for p in &report.partitions {
                println!(
                    "  {:<14} {:>10} at {:<12} {}",
                    p.kind,
                    stormblock::mgmt::config::human_size(p.size_bytes),
                    stormblock::mgmt::config::human_size(p.start_bytes),
                    match (&p.pallet_id, p.verified) {
                        (Some(id), Some(true)) => format!("{} v{} verified", id, p.pallet_version.unwrap_or(0)),
                        (Some(id), _) => format!("{id} NOT VERIFIED"),
                        _ => p.name.clone(),
                    }
                );
                for v in &p.volumes {
                    println!(
                        "      {:<12} {:>10} {:>10} mapped  {}",
                        v.name,
                        stormblock::mgmt::config::human_size(v.size_bytes),
                        stormblock::mgmt::config::human_size(v.allocated_bytes),
                        match v.clone_of {
                            Some(g) => format!("clone of {g}"),
                            None => "golden".to_string(),
                        }
                    );
                }
            }

            if format != ImageFormat::Raw {
                stormblock::image::formats::convert(&raw_path, &out_path, format)
                    .await
                    .map_err(ie)?;
                let len = tokio::fs::metadata(&out_path).await?.len();
                println!(
                    "{} — {} ({})",
                    out_path.display(),
                    stormblock::mgmt::config::human_size(len),
                    format
                );
                if !keep_raw {
                    tokio::fs::remove_file(&raw_path).await.ok();
                }
            }
        }
        ImageAction::Convert { input, out, format, include_slab } => {
            let format = resolve(out, format)?;
            if format == ImageFormat::Iso {
                stormblock::image::iso::from_image_with(
                    Path::new(input),
                    Path::new(out),
                    stormblock::image::iso::IsoOptions { include_slab: *include_slab },
                )
                .await
                .map_err(ie)?;
            } else {
                stormblock::image::formats::convert(Path::new(input), Path::new(out), format)
                    .await
                    .map_err(ie)?;
            }
            let len = tokio::fs::metadata(out).await?.len();
            println!("{out} — {} ({format})", stormblock::mgmt::config::human_size(len));
        }
        ImageAction::Inspect { image } => {
            let path = Path::new(image);
            let gpt = stormblock::image::build::table_of(path).await.map_err(ie)?;
            println!(
                "{image}: GPT in {}-byte LBAs{}",
                gpt.block_size,
                if gpt.recovered_from_backup { " (read from the backup)" } else { "" }
            );
            for (i, e) in gpt.partitions() {
                println!(
                    "  {i:>3}  {:<20} {:>10} at {:<12} {}",
                    e.name,
                    stormblock::mgmt::config::human_size(e.size_bytes(gpt.block_size)),
                    stormblock::mgmt::config::human_size(e.start_bytes(gpt.block_size)),
                    if e.is_pallet() { "pallet" } else { "" }
                );
            }
            for p in stormblock::image::build::pallets_in(path).await.map_err(ie)? {
                println!(
                    "  pallet {} {} v{} [{}] {} — {} member(s){}",
                    p.id,
                    p.name,
                    p.version,
                    p.kind,
                    p.version_label,
                    p.member_count,
                    if p.is_readable() { "" } else { " UNREADABLE" }
                );
            }
            for s in stormblock::image::build::slabs_in(path).await.map_err(ie)? {
                println!(
                    "  slab {} — {} slots of {}, {} free{}",
                    s.name,
                    s.total_slots,
                    stormblock::mgmt::config::human_size(s.slot_size),
                    s.free_slots,
                    if s.self_describing { "" } else { " (keeps no volume metadata)" }
                );
                for v in &s.volumes {
                    println!(
                        "    volume {:<24} {:>10} {:>10} mapped  {}",
                        v.name,
                        stormblock::mgmt::config::human_size(v.size_bytes),
                        stormblock::mgmt::config::human_size(v.allocated_bytes),
                        v.id
                    );
                }
            }
        }
    }
    Ok(())
}

async fn handle_pallet_command(drives: &[String], action: &PalletAction) -> anyhow::Result<()> {
    use stormblock::pallet::format::{parse_pallet_kind, MemberExt};
    use stormblock::pallet::manager::{PublishSpec, RecomposeSpec};
    use stormblock::pallet::{PalletBrowser, PalletManager};

    if drives.is_empty() {
        anyhow::bail!("no drives given: pass --drive <path> at least once");
    }
    let store = pallet_store(drives).await?;
    let mgr = PalletManager::new(store.clone());
    let kind_of = |k: &Option<String>| k.as_deref().map(parse_pallet_kind);
    let id_of = |s: &str| {
        uuid::Uuid::parse_str(s).map_err(|_| anyhow::anyhow!("'{s}' is not a pallet UUID"))
    };

    match action {
        PalletAction::InitGpt { drive, force } => {
            let idx = pe(store.drive_index_of(drive))?;
            pe(mgr.init_gpt(idx, *force).await)?;
            println!("{drive}: GPT written (primary and backup)");
        }
        PalletAction::List { kind } => {
            let kind = kind_of(kind);
            let all = mgr.list().await;
            let shown: Vec<_> =
                all.iter().filter(|p| kind.is_none() || Some(p.kind) == kind).collect();
            if shown.is_empty() {
                println!("no pallets on {} drive(s)", drives.len());
            }
            for p in shown {
                print_pallet(p);
            }
        }
        PalletAction::Info { id } => {
            let loc = pe(mgr.get(id_of(id)?).await)?;
            print_pallet(&loc);
            println!("  label: {}", loc.version_label);
            println!(
                "  partition: start {} bytes, size {}, used {}",
                loc.start_bytes,
                stormblock::mgmt::config::human_size(loc.size_bytes),
                stormblock::mgmt::config::human_size(loc.used_bytes),
            );
            match mgr.store().open(&loc).await {
                Ok(p) => {
                    for m in p.members() {
                        println!(
                            "  member {:<20} role={:<12} kind={:<10} {:>10}  {}",
                            m.name(),
                            m.role(),
                            m.kind,
                            stormblock::mgmt::config::human_size(m.byte_len),
                            &m.digest_hex()[..16],
                        );
                    }
                }
                Err(err) => println!("  manifest unreadable: {err}"),
            }
        }
        PalletAction::Status { kind } => {
            let s = mgr.status(kind_of(kind)).await;
            match &s.active {
                Some(a) => {
                    print!("active:    ");
                    print_pallet(a);
                }
                None => println!("active:    none"),
            }
            for p in s.available.iter().filter(|p| Some(p.id) != s.active.as_ref().map(|a| a.id)) {
                print!("available: ");
                print_pallet(p);
            }
            for f in &s.failed {
                print!("failed:    ");
                print_pallet(&f.location);
                println!("           {}", f.reason);
            }
        }
        PalletAction::Chain { kind } => {
            let browser = PalletBrowser::new(store.clone());
            for (i, p) in browser.chain(kind_of(kind)).await.iter().enumerate() {
                print!("{}. ", i + 1);
                print_pallet(p);
            }
        }
        PalletAction::Verify { id } => {
            let targets = if id == "all" {
                mgr.list().await.into_iter().map(|p| p.id).collect::<Vec<_>>()
            } else {
                vec![id_of(id)?]
            };
            let mut bad = 0;
            for t in targets {
                let r = pe(mgr.verify(t).await)?;
                println!(
                    "{} {} v{}: {}",
                    r.id,
                    r.name,
                    r.version,
                    if r.ok { "ok".to_string() } else { format!("FAILED — {}", r.reason.clone().unwrap_or_default()) }
                );
                for m in &r.members {
                    println!(
                        "    {:<20} {}",
                        m.name,
                        if m.ok { "ok".into() } else { format!("FAILED — {}", m.reason.clone().unwrap_or_default()) }
                    );
                }
                if !r.ok {
                    bad += 1;
                }
            }
            if bad > 0 {
                anyhow::bail!("{bad} pallet(s) failed verification");
            }
        }
        PalletAction::Publish { name, kind, label, members, drive, size, activate } => {
            let mut spec = PublishSpec::new(name.clone(), parse_pallet_kind(kind));
            spec.version_label = label.clone();
            spec.activate = *activate;
            if let Some(d) = drive {
                spec.drive = Some(pe(store.drive_index_of(d))?);
            }
            if let Some(sz) = size {
                spec.size_bytes = Some(parse_size(sz).map_err(|m| anyhow::anyhow!("{m}"))?);
            }
            for m in members {
                let (name, role, kind, path) = parse_member_spec(m)?;
                spec.members.push(pe(stormblock::pallet::manager::file_member(
                    name,
                    role,
                    stormblock::pallet::parse_member_kind(&kind),
                    path,
                )
                .await)?);
            }
            let loc = pe(mgr.publish(spec).await)?;
            println!("published and verified:");
            print_pallet(&loc);
        }
        PalletAction::Activate { id } => {
            let loc = pe(mgr.activate(id_of(id)?).await)?;
            print!("active: ");
            print_pallet(&loc);
        }
        PalletAction::Successful { id } => {
            let loc = pe(mgr.mark_successful(id_of(id)?).await)?;
            print!("confirmed good: ");
            print_pallet(&loc);
        }
        PalletAction::Rollback { kind } => {
            let loc = pe(mgr.rollback(kind_of(kind)).await)?;
            print!("rolled back to: ");
            print_pallet(&loc);
        }
        PalletAction::Copy { id, to } => {
            let dest = pe(store.drive_index_of(to))?;
            let loc = pe(mgr.copy_pallet(id_of(id)?, dest).await)?;
            print!("copied: ");
            print_pallet(&loc);
        }
        PalletAction::Move { id, to } => {
            let dest = pe(store.drive_index_of(to))?;
            let loc = pe(mgr.move_pallet(id_of(id)?, dest).await)?;
            print!("moved: ");
            print_pallet(&loc);
        }
        PalletAction::AddMember { id, members, drive, activate } => {
            let mut add = Vec::new();
            for m in members {
                let (name, role, kind, path) = parse_member_spec(m)?;
                add.push(pe(stormblock::pallet::manager::file_member(
                    name,
                    role,
                    stormblock::pallet::parse_member_kind(&kind),
                    path,
                )
                .await)?);
            }
            let on = match drive {
                Some(d) => Some(pe(store.drive_index_of(d))?),
                None => None,
            };
            let loc = pe(mgr
                .recompose(
                    id_of(id)?,
                    RecomposeSpec { add, drive: on, activate: *activate, ..Default::default() },
                )
                .await)?;
            print!("new version: ");
            print_pallet(&loc);
            println!("(the previous version is untouched — prune it when you are ready)");
        }
        PalletAction::RemoveMember { id, members, drive, activate } => {
            let on = match drive {
                Some(d) => Some(pe(store.drive_index_of(d))?),
                None => None,
            };
            let loc = pe(mgr
                .recompose(
                    id_of(id)?,
                    RecomposeSpec {
                        remove: members.clone(),
                        drive: on,
                        activate: *activate,
                        ..Default::default()
                    },
                )
                .await)?;
            print!("new version: ");
            print_pallet(&loc);
        }
        PalletAction::CopyMember { id, member, into } => {
            let loc = pe(mgr.copy_member(id_of(id)?, member, id_of(into)?, false).await)?;
            print!("destination: ");
            print_pallet(&loc);
            println!("(a new version of the destination; the source is unchanged)");
        }
        PalletAction::MoveMember { id, member, into } => {
            let (dest, src) = pe(mgr.move_member(id_of(id)?, member, id_of(into)?, false).await)?;
            print!("destination: ");
            print_pallet(&dest);
            print!("source:      ");
            print_pallet(&src);
            println!("(both are new versions; the originals are untouched)");
        }
        PalletAction::ReadOnly { id, value, force } => {
            let loc = pe(mgr.set_read_only(id_of(id)?, *value, *force).await)?;
            print_pallet(&loc);
        }
        PalletAction::Sealed { id, value } => {
            let loc = pe(mgr.set_sealed(id_of(id)?, *value).await)?;
            print_pallet(&loc);
        }
        PalletAction::Delete { id, force } => {
            let loc = pe(mgr.delete(id_of(id)?, *force).await)?;
            println!("removed {} ({} v{})", loc.id, loc.name, loc.version);
        }
        PalletAction::Prune { name, keep } => {
            let removed = pe(mgr.prune(name, *keep).await)?;
            for p in &removed {
                println!("pruned {} ({} v{})", p.id, p.name, p.version);
            }
            println!("{} removed, keeping the newest {}", removed.len(), (*keep).max(2));
        }
        PalletAction::Convert { from, to, keep_source, reinit_source } => {
            let (f, t) = (pe(store.drive_index_of(from))?, pe(store.drive_index_of(to))?);
            let report = pe(mgr
                .convert_drive(
                    f,
                    t,
                    stormblock::pallet::ConvertOptions {
                        remove_source: !*keep_source,
                        init_destination: true,
                        reinit_source: *reinit_source,
                    },
                )
                .await)?;
            println!("{} -> {}", report.source, report.destination);
            for p in &report.converted {
                print!("  converted: ");
                print_pallet(p);
            }
            for (p, why) in &report.skipped {
                print!("  SKIPPED:   ");
                print_pallet(p);
                println!("             {why}");
            }
            println!(
                "{} converted, {} removed from the source{}",
                report.converted.len(),
                report.removed_from_source,
                if report.source_reinitialized { ", source reinitialized" } else { "" }
            );
            if let Some(note) = &report.note {
                println!("note: {note}");
            }
            if !report.skipped.is_empty() {
                anyhow::bail!("{} pallet(s) did not convert", report.skipped.len());
            }
        }
        PalletAction::Adopt { from, to } => {
            let (f, t) = (pe(store.drive_index_of(from))?, pe(store.drive_index_of(to))?);
            let loc = pe(mgr.adopt_whole_drive(f, t).await)?;
            print!("adopted: ");
            print_pallet(&loc);
            println!("the source drive can now be subdivided: pallet init-gpt {from} --force");
        }
    }
    Ok(())
}

#[cfg(feature = "iscsi")]
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

/// boot.toml handoff dropped into the initramfs by `BootManager::initramfs_config`.
#[derive(serde::Deserialize)]
struct BootToml {
    boot: BootTomlSection,
}

#[derive(serde::Deserialize)]
struct BootTomlSection {
    volume: String,
    #[serde(default)]
    #[allow(dead_code)]
    server: Option<String>,
}

/// Resolve a volume selector (UUID or name) against restored metadata.
async fn resolve_boot_volume(
    mgr: &VolumeManager,
    selector: &str,
) -> anyhow::Result<stormblock::volume::VolumeId> {
    use stormblock::volume::VolumeId;
    if let Ok(u) = uuid::Uuid::parse_str(selector) {
        let id = VolumeId(u);
        if mgr.get_volume(&id).is_some() {
            return Ok(id);
        }
    }
    for (id, name, _, _) in mgr.list_volumes().await {
        if name == selector {
            return Ok(id);
        }
    }
    anyhow::bail!(
        "volume '{selector}' not found in slab metadata (have: {})",
        mgr.list_volumes()
            .await
            .iter()
            .map(|(id, name, _, _)| format!("{name}={}", id.0))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Open the slabs, find the volume metadata, and restore what it describes.
///
/// Shared by every path that attaches to an existing node's storage —
/// `boot-local` at boot and `adopt-ublk` at handover — because they need
/// exactly the same three things and disagreeing about any of them would mean
/// the two halves of a handover had different ideas of what the node holds.
async fn open_slabs_and_restore(
    slab_paths: &[String],
    meta: Option<&str>,
) -> anyhow::Result<VolumeManager> {
    use std::path::{Path, PathBuf};
    use stormblock::volume::MetadataStore;

    // 1. Open the slabs. A slab formatted by `image build` carries its own
    //    volumes.dat, so opening it is also how the metadata is found — an
    //    image has no filesystem to keep one in, and the "meta" directory
    //    beside `/dev/sda4` is `/dev/meta`, which is nothing (#62).
    for path in slab_paths {
        // FileDevice::open would create a missing path as an empty file and
        // die later with a misleading "bad slab magic" — name the real
        // problem (storage driver not loaded / wrong device) instead (#14).
        if !Path::new(path).exists() {
            anyhow::bail!(
                "slab device {path} does not exist — storage driver not loaded or wrong path?"
            );
        }
    }
    let mut slabs = Vec::with_capacity(slab_paths.len());
    for path in slab_paths {
        let dev = stormblock::drive::filedev::FileDevice::open(path).await?;
        let slab = Slab::open(Arc::new(dev))
            .await
            .map_err(|e| anyhow::anyhow!("open slab {path}: {e}"))?;
        slabs.push(slab);
    }

    // 2. Metadata: an explicit --meta wins, then the slab's own copy, then
    //    the "meta" directory beside the first slab.
    let meta_dir: PathBuf = match meta {
        Some(m) => PathBuf::from(m),
        None => Path::new(&slab_paths[0])
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("meta"),
    };
    let embedded = match meta {
        Some(_) => None,
        None => slabs[0]
            .read_metadata()
            .await
            .map_err(|e| anyhow::anyhow!("read slab metadata from {}: {e}", slab_paths[0]))?,
    };
    let (metadata, source) = match embedded {
        Some(bytes) => (
            MetadataStore::decode(&bytes)?,
            format!("slab {}", slab_paths[0]),
        ),
        None => {
            let store = MetadataStore::new(meta_dir.clone())?;
            if !store.exists() {
                anyhow::bail!(
                    "no volume metadata: slab {} carries none and there is no volumes.dat in {}",
                    slab_paths[0],
                    meta_dir.display()
                );
            }
            (store.load()?, meta_dir.display().to_string())
        }
    };
    println!("Volume metadata from {source}");
    if metadata.arrays.is_empty() {
        anyhow::bail!("metadata from {source} records no arrays");
    }
    if slab_paths.len() > metadata.arrays.len() {
        anyhow::bail!(
            "{} slab path(s) given but metadata records only {} array(s)",
            slab_paths.len(),
            metadata.arrays.len()
        );
    }

    // 3. Attach the slabs non-destructively (no reformat) and restore volumes.
    //    Runtime changes go back where the metadata came from.
    let mut mgr = if source.starts_with("slab ") {
        VolumeManager::new(metadata.extent_size)
    } else {
        VolumeManager::with_data_dir(metadata.extent_size, meta_dir.clone())?
    };
    let mut metadata_slab = None;
    for ((path, rec), slab) in slab_paths.iter().zip(&metadata.arrays).zip(slabs) {
        if metadata_slab.is_none() && slab.has_metadata_region() {
            metadata_slab = Some(slab.slab_id());
        }
        mgr.attach_slab(rec.array_id, slab)
            .await
            .map_err(|e| anyhow::anyhow!("attach slab {path}: {e}"))?;
        println!("Attached slab {path} (array {})", rec.array_id);
    }
    if let Some(id) = metadata_slab {
        mgr.persist_to_slab(id);
    }
    mgr.restore().await?;

    Ok(mgr)
}

/// adopt-ublk: take over the ublk devices an earlier server created.
///
/// The handover the boot needs. The engine the initramfs started owns the slab
/// and serves root, and it can never be restarted: `switch_root` deleted the
/// filesystem its binary came from, so `/proc/<pid>/exe` reads `(deleted)` and
/// nothing on the node could exec it again. That makes the one process the
/// root filesystem depends on unrepeatable — a failure with no recovery path
/// rather than one with a slow recovery path.
///
/// So the long-term owner is a process that lives in a golden, can be
/// upgraded, and can be put back by PID 1 when it dies. It takes over here.
///
/// **The order matters and the caller owns it.** The previous server must be
/// stopped before this runs: `START_USER_RECOVERY` is the kernel refusing to
/// have two servers, not a way to have them briefly. The block device itself
/// never goes away, so a filesystem mounted on it stays mounted throughout,
/// and `UBLK_F_USER_RECOVERY_REISSUE` hands this server the I/O that was in
/// flight rather than failing it.
///
/// The slab needs no handover of its own: `Slab::open` reads the header and
/// the slot table from disk and derives the free bitmap, so the on-disk state
/// *is* the allocator. Opening it here, after the old engine has stopped, is
/// the whole transfer.
#[cfg(target_os = "linux")]
async fn handle_adopt_ublk(
    slab_paths: &[String],
    volumes: &[String],
    meta: Option<&str>,
    api: Option<&str>,
    data_dir: Option<&str>,
) -> anyhow::Result<()> {
    use stormblock::drive::ublk::UblkServer;

    let mut mgr = open_slabs_and_restore(slab_paths, meta).await?;

    // Resolve every volume before adopting anything. A name that does not
    // resolve should cost nothing — half-adopting a set of devices leaves the
    // node with some queues served and some not, which is worse than not
    // starting.
    let mut serving: Vec<(u32, String, Arc<dyn BlockDevice>)> = Vec::new();
    for (i, selector) in volumes.iter().enumerate() {
        let id = resolve_boot_volume(&mgr, selector).await?;
        let name = mgr
            .get_volume_handle(&id)
            .expect("resolved volume exists")
            .name()
            .await;
        let dev = mgr.get_volume(&id).expect("resolved volume exists");
        serving.push((i as u32, name, dev));
    }

    for (dev_id, name, dev) in &serving {
        println!(
            "  adopting /dev/ublkb{dev_id} ← {name} ({})",
            stormblock::mgmt::config::human_size(dev.capacity_bytes())
        );
    }

    // Lock this process into RAM before anything else.
    //
    // The engine is about to stop the server that is exporting **its own
    // root**. Between that moment and the end of recovery there is no backing
    // store for this binary: a page fault on code not yet resident would wait
    // for a device this process is on its way to serving, and wait forever.
    // Locking first makes the window survivable — the pages cannot be
    // reclaimed while it is open.
    //
    // Best effort: a node where mlockall is refused still works, it is simply
    // relying on those pages happening to stay resident.
    // SAFETY: mlockall takes flags and touches nothing of ours.
    let locked = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } == 0;
    if locked {
        tracing::info!("adopt: locked into memory for the handover");
    } else {
        tracing::warn!(
            "adopt: could not lock memory ({}) — the handover relies on this \
             binary's pages staying resident",
            std::io::Error::last_os_error()
        );
    }

    // The incumbent stands down before anything is adopted. The kernel runs
    // one server per device, so this is the handover's first step rather than
    // an afterthought — and the kernel is asked who the incumbent is, because
    // it is the only party that actually knows.
    let dev_ids: Vec<u32> = serving.iter().map(|(id, ..)| *id).collect();
    stormblock::drive::ublk::stand_down(&dev_ids, std::time::Duration::from_secs(15))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut threads = Vec::new();
    for (dev_id, name, dev) in serving {
        let rx = shutdown_rx.clone();
        let thread = std::thread::Builder::new()
            .name(format!("ublk-adopt-{dev_id}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                let server = UblkServer::new(dev).adopting(dev_id);
                if let Err(e) = rt.block_on(server.run(rx)) {
                    tracing::error!("ublk adopt {dev_id} ({name}): {e}");
                }
            })?;
        threads.push(thread);
    }

    // Report what is actually being served, not what was attempted.
    //
    // Every adopt runs on its own thread and an adopt that fails does so
    // early, before serving; a thread still alive after the settle is one that
    // reached its I/O loop. Counting the spawns instead announced "Adopted 4
    // device(s)" in the same breath as four errors saying none of them had
    // been — the sort of report that sends the next session looking in the
    // wrong place.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let live = threads.iter().filter(|t| !t.is_finished()).count();
    if live == 0 {
        let _ = shutdown_tx.send(true);
        for t in threads {
            let _ = t.join();
        }
        anyhow::bail!(
            "adopted none of {} device(s) — the errors above are the reason; the root \
             filesystem is still served by whoever had it before this ran",
            dev_ids.len()
        );
    }
    if live < threads.len() {
        tracing::warn!(
            "adopted {live} of {} device(s); the rest are named in the errors above",
            threads.len()
        );
    }
    println!("Adopted {live} device(s). Serving until Ctrl+C.");

    // The management API, in this process, over the manager that owns the
    // slab. There is nowhere else to put it: one writer per volume means a
    // second process cannot open the same slab to answer on its behalf, so an
    // engine that is serving its node's root and not answering questions about
    // it is an engine that is half here.
    if let Some(addr) = api {
        let mut config = stormblock::mgmt::config::StormBlockConfig::default();
        config.management.listen_addr = addr.to_string();
        // The metadata the manager was restored from is where anything the API
        // creates has to persist to, or a template minted now is gone at the
        // next boot.
        config.management.data_dir = data_dir.map(|d| d.to_string());
        if let Some(d) = data_dir {
            std::fs::create_dir_all(d)
                .map_err(|e| anyhow::anyhow!("cannot create API state directory {d}: {e}"))?;
        }
        let slab_registry = mgr.registry().clone();
        let gem = mgr.gem().clone();
        let state = Arc::new(AppState::new(config, mgr, slab_registry, gem));
        tokio::spawn(async move {
            if let Err(e) = mgmt::start_management_server(state).await {
                tracing::error!("management API error: {e}");
            }
        });
        println!("  management API on {addr}");
    }

    tokio::signal::ctrl_c().await?;
    let _ = shutdown_tx.send(true);
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn handle_adopt_ublk(
    _slab_paths: &[String],
    _volumes: &[String],
    _meta: Option<&str>,
    _api: Option<&str>,
    _data_dir: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::bail!("ublk is Linux-only")
}

/// boot-local: attach an existing local slab (no reformat, no repartition),
/// restore volume metadata, export the boot volume as /dev/ublkb0.
/// The local-slab → ublk-root path stormcos boots through (issue #12).
#[allow(clippy::too_many_arguments)]
async fn handle_boot_local(
    slab_paths: &[String],
    meta: Option<&str>,
    volume: Option<&str>,
    boot_config: &str,
    image_store: Option<&str>,
    writable: &[String],
    local_disk: Option<&str>,
    local_tier: &str,
    check: bool,
) -> anyhow::Result<()> {
    use std::path::{Path, PathBuf};
    use stormblock::volume::MetadataStore;

    let mut mgr = open_slabs_and_restore(slab_paths, meta).await?;

    // 3. Resolve the boot volume: --volume wins, else boot.toml.
    let selector = match volume {
        Some(v) => v.to_string(),
        None => {
            let raw = std::fs::read_to_string(boot_config).map_err(|e| {
                anyhow::anyhow!(
                    "no --volume given and cannot read {boot_config}: {e}"
                )
            })?;
            let parsed: BootToml = toml::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("parse {boot_config}: {e}"))?;
            parsed.boot.volume
        }
    };
    let root_id = resolve_boot_volume(&mgr, &selector).await?;
    let root_name = mgr
        .get_volume_handle(&root_id)
        .expect("resolved volume exists")
        .name()
        .await;

    let mut exports: Vec<(u32, String, Arc<dyn BlockDevice>)> = vec![(
        0,
        root_name.clone(),
        mgr.get_volume(&root_id).expect("resolved volume exists"),
    )];
    if let Some(sel) = image_store {
        let img_id = resolve_boot_volume(&mgr, sel).await?;
        let img_name = mgr
            .get_volume_handle(&img_id)
            .expect("resolved volume exists")
            .name()
            .await;
        exports.push((1, img_name, mgr.get_volume(&img_id).expect("resolved volume exists")));
    }

    // Writable thin volumes (var, containers) at the next indices after root
    // (0) and image-store (1). Order preserved so the caller maps each ublk
    // device to its mount point.
    let mut next_dev = exports.len() as u32;
    for sel in writable {
        let wid = resolve_boot_volume(&mgr, sel).await?;
        let wname = mgr
            .get_volume_handle(&wid)
            .expect("resolved volume exists")
            .name()
            .await;
        exports.push((next_dev, wname, mgr.get_volume(&wid).expect("resolved volume exists")));
        next_dev += 1;
    }

    println!("Boot volume: {root_name} ({})", root_id.0);
    for (dev_id, name, dev) in &exports {
        println!(
            "  /dev/ublkb{dev_id} ← {} ({})",
            name,
            stormblock::mgmt::config::human_size(dev.capacity_bytes())
        );
    }

    if check {
        println!("boot-local check OK");
        return Ok(());
    }

    // 4. Optional zeroboot flow-over: migrate extents to a local disk in the
    //    background, one extent per lock cycle so root I/O keeps flowing.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    if let Some(disk) = local_disk {
        let tier = parse_tier(local_tier).map_err(|e| anyhow::anyhow!("{e}"))?;
        let source_slabs: Vec<_> = {
            let reg = mgr.registry().read().await;
            reg.iter().map(|(id, _)| *id).collect()
        };
        let dest_dev: Arc<dyn BlockDevice> =
            Arc::new(stormblock::drive::filedev::FileDevice::open(disk).await?);
        let dest_slab = Slab::format(dest_dev, mgr.slot_size(), tier)
            .await
            .map_err(|e| anyhow::anyhow!("format local disk {disk}: {e}"))?;
        let dest_id = dest_slab.slab_id();
        mgr.registry().write().await.add(dest_slab);
        println!("Flow-over: migrating to local slab {dest_id} on {disk} in background");

        let gem_arc = mgr.gem().clone();
        let reg_arc = mgr.registry().clone();
        let mut flow_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let engine = stormblock::placement::PlacementEngine::new();
            let mut moved = 0u64;
            let mut failed = 0u64;
            for source in source_slabs {
                loop {
                    if *flow_shutdown.borrow_and_update() {
                        return;
                    }
                    // One extent per lock cycle: ublk I/O interleaves between
                    // iterations instead of stalling for the whole migration.
                    let mut gem = gem_arc.write().await;
                    let mut reg = reg_arc.write().await;
                    let Some((vol, vext, _)) = gem.slab_extents(source).into_iter().next()
                    else {
                        break;
                    };
                    match engine
                        .migrate_extent(&mut gem, &mut reg, vol, vext, Some(dest_id))
                        .await
                    {
                        Ok(_) => moved += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::error!("flow-over: extent {vol:?}/{vext}: {e}");
                            if failed > 16 {
                                tracing::error!("flow-over: aborting after repeated failures");
                                return;
                            }
                        }
                    }
                }
            }
            tracing::info!("flow-over complete: {moved} extent(s) migrated, {failed} failed");
            println!("Flow-over complete: {moved} extent(s) now on local disk");
        });
    }

    // 5. Export via ublk (Linux 6.0+ with ublk_drv).
    #[cfg(target_os = "linux")]
    {
        use stormblock::drive::ublk::UblkServer;

        let (done_tx, mut done_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u32, Result<(), String>)>();
        let total = exports.len();
        let mut ublk_threads = Vec::new();
        for (dev_id, name, dev) in exports {
            // Recoverable, always, on the boot path. The process creating
            // these devices is the one the initramfs started, and
            // `switch_root` deletes the filesystem its binary came from — so
            // it can never be restarted, by anything, for the life of the
            // boot. Without this flag the engine serving root is a single
            // point of failure with no recovery path at all; with it, another
            // process can take the devices over, and stormpump can put the
            // engine back if it dies.
            //
            // The flag is fixed at creation, so this is the only moment it can
            // be asked for.
            let server = UblkServer::new(dev).with_dev_id(dev_id).recoverable(true);
            let rx = shutdown_rx.clone();
            let done = done_tx.clone();
            // UblkServer::run() holds raw pointers (not Send), so run on a
            // dedicated OS thread with its own tokio runtime.
            let thread = std::thread::Builder::new()
                .name(format!("ublk-local-{dev_id}"))
                .spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create ublk tokio runtime");
                    rt.block_on(async move {
                        let res = server.run(rx).await;
                        match &res {
                            Ok(()) => tracing::info!("ublk#{dev_id} ({name}) stopped"),
                            Err(e) => tracing::error!("ublk#{dev_id} ({name}) error: {e}"),
                        }
                        let _ = done.send((dev_id, res.map_err(|e| e.to_string())));
                    });
                })
                .expect("failed to spawn ublk thread");
            ublk_threads.push(thread);
        }
        drop(done_tx);

        // Serve until Ctrl+C/SIGTERM — but if the root export (dev 0) dies,
        // or every server exits, fail instead of hanging the boot forever.
        println!("\nublk devices starting. Press Ctrl+C to stop.");
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut finished = 0usize;
        let mut fatal: Option<String> = None;
        loop {
            tokio::select! {
                r = tokio::signal::ctrl_c() => { r?; break; }
                _ = sigterm.recv() => break,
                msg = done_rx.recv() => match msg {
                    Some((dev_id, res)) => {
                        finished += 1;
                        if let Err(e) = res {
                            if dev_id == 0 {
                                fatal = Some(format!("root export /dev/ublkb0 failed: {e}"));
                                break;
                            }
                            eprintln!("WARNING: /dev/ublkb{dev_id} export failed: {e}");
                        }
                        if finished == total {
                            fatal = Some("all ublk exports exited".to_string());
                            break;
                        }
                    }
                    None => { fatal = Some("all ublk exports exited".to_string()); break; }
                }
            }
        }
        println!("Shutting down...");
        let _ = shutdown_tx.send(true);
        for t in ublk_threads {
            let _ = t.join();
        }
        // Capture extent maps mutated while serving (COW allocations) so
        // snapshots stay bootable across the next reattach (#13).
        mgr.persist().await;
        if let Some(msg) = fatal {
            anyhow::bail!("{msg} — is ublk_drv loaded (Linux 6.0+)?");
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = exports;
        let _ = shutdown_tx;
        anyhow::bail!("boot-local ublk export requires Linux 6.0+ with ublk_drv loaded");
    }

    #[cfg(target_os = "linux")]
    Ok(())
}

#[cfg(feature = "iscsi")]
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
