//! The `stormblock` command line (moved from `main.rs`, #209).
//!
//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

use std::sync::Arc;

use clap::Parser;

use crate::drive::{self, BlockDevice};
use crate::drive::slab::{Slab, SlabRole, DEFAULT_SLOT_SIZE as SLAB_SLOT_SIZE};
#[cfg(feature = "iscsi")]
use crate::boot_iscsi::{BootDiskLayout, IscsiBootManager};
use crate::placement::topology::StorageTier;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};
use crate::volume::VolumeManager;
use crate::target::{self, reactor::{ReactorConfig, ReactorPool}};
use crate::mgmt::{self, AppState, DriveInfo};
use crate::mgmt::config::{StormBlockConfig, parse_size};
#[cfg(feature = "cluster")]
use crate::cluster;

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
    /// Collect everything needed to debug this node into one directory.
    ///
    /// The bundle someone can send you when the node is not the one in front
    /// of you: what the kernel saw, what the storage layer thinks it has, and
    /// the contents of the log volumes. Read-only throughout — a diagnostic
    /// that can change what it is diagnosing is not one.
    MustGather {
        /// Slab device, partition or image file to read. Repeatable. With
        /// none, the slabs this node is serving are used.
        #[arg(long)]
        slab: Vec<String>,
        /// Metadata directory, if the slab does not carry its own.
        #[arg(long)]
        meta: Option<String>,
        /// Where to write the bundle.
        #[arg(long, default_value = "/tmp/stormblock-must-gather")]
        out: String,
        /// Also copy the contents of these volumes, by name. Repeatable.
        /// Volumes whose name contains "log" or "data" are included anyway.
        #[arg(long = "volume")]
        volumes: Vec<String>,
        /// Skip volume contents — inventory and node state only.
        #[arg(long)]
        no_contents: bool,
        /// Largest file to copy out of a volume, in MB.
        #[arg(long, default_value = "32")]
        max_file_mb: u64,
    },
    /// Build a golden filesystem image from a tar archive.
    ///
    /// The conversion a node's build has always needed and has been doing with
    /// `mkfs`, a loop mount and `tar -x` as root. Every piece of it already
    /// exists here — the same code the registry uses to lay an image's layers
    /// into a volume — and none of it needs a mount, a loop device or
    /// privileges: the filesystem is written directly through the ext4 writer.
    ///
    ///   podman export "$cid" | stormblock golden --out fedora.img --size 512M
    Golden {
        /// Where to write the image.
        #[arg(long)]
        out: String,
        /// How big to make it, e.g. `512M`, `2G`.
        #[arg(long)]
        size: String,
        /// Filesystem label. Defaults to the output file's stem.
        #[arg(long)]
        label: Option<String>,
        /// Archive to unpack, or `-` for standard input. Repeatable, applied
        /// in order — which is how a container image's layers go on.
        #[arg(long = "tar")]
        tars: Vec<String>,
        /// Honour OCI whiteouts (`.wh.` entries) while unpacking. On by
        /// default, because layers are the usual source and a flattened export
        /// simply has none.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        whiteouts: bool,
        /// Format it as what it is: a filesystem nothing will ever write to.
        ///
        /// A content golden is sealed at build time and mounted read-only for
        /// the life of the image; a workload that needs to write takes a PVC.
        /// So it needs no journal — a journal exists to make a write
        /// survivable, and there are no writes — and none of the 5% of blocks
        /// ext4 reserves for root to recover a full filesystem. Both are pure
        /// overhead in every clone, on every node, for ever.
        ///
        /// Not the default, and deliberately: a blank template is cloned and
        /// then written, and one of those without a journal is a data loss
        /// waiting for a power cut.
        #[arg(long = "read-only")]
        read_only: bool,
        /// Check the result before writing it out. A golden that does not
        /// check out is one every clone of it inherits.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        fsck: bool,
    },
    /// Attach a slab and export (and optionally mount) any volume in it.
    ///
    /// The debugging and rescue door: point it at a disk, an image file or a
    /// partition and look at what is inside without booting the node that
    /// owns it. With no --volume it lists what is there, so finding out and
    /// getting in are the same command.
    Attach {
        /// Slab device, partition or image file. Repeatable.
        #[arg(long, required = true)]
        slab: Vec<String>,
        /// Metadata directory, if the slab does not carry its own.
        #[arg(long)]
        meta: Option<String>,
        /// Volume to export, by name or UUID. Repeatable. With none, the
        /// volumes are listed and nothing is attached.
        #[arg(long = "volume")]
        volumes: Vec<String>,
        /// Export every volume in the slab.
        #[arg(long)]
        all: bool,
        /// Mount each exported volume under this directory, by name.
        #[arg(long)]
        mount: Option<String>,
        /// Read-only: mount `ro`, and refuse anything that would write.
        ///
        /// The right default for looking at a node's disk while something
        /// else may still own it.
        #[arg(long)]
        ro: bool,
        /// Attach even though the kernel says another server is serving this
        /// volume. Two writers on one volume corrupt it silently, so this is
        /// never the first thing to try.
        #[arg(long)]
        force: bool,
    },
    /// Export a volume via ublk to the local kernel (/dev/ublkbN). Not
    /// implemented as a subcommand: it needs a running engine, so use
    /// `POST /api/v1/volumes/{id}/attach` (or `attach --slab` offline).
    Ublk {
        /// Volume UUID to export
        #[arg(long)]
        volume: String,
        /// Number of I/O queues (default: 1)
        #[arg(long, default_value = "1")]
        queues: u16,
    },
    /// Live migrate from iSCSI to local disk. Not implemented as a
    /// subcommand: a boot volume flows over with `boot-local --local-disk`,
    /// and a running engine moves volumes with `/api/v1/moves` or
    /// `/api/v1/volumes/{id}/tier`.
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
        /// Slab device or file path(s), as `boot-local` was given them.
        ///
        /// Optional: the server being taken over wrote down which slabs it
        /// opened, and that record is the default.
        #[arg(long)]
        slab: Vec<String>,
        /// Volume to serve on each device, in ublk device order: the first is
        /// `/dev/ublkb0`, and so on. Must match what the previous server had.
        ///
        /// Optional, and better left out. The incumbent recorded exactly which
        /// volume is behind each device it created, so this is derived rather
        /// than repeated — a list kept by hand in two places is one that
        /// disagrees with itself the first time the node gains a volume, and
        /// standing a server down abandons every device left off it.
        #[arg(long = "volume")]
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
    /// Ask the appliance which image this machine boots, and print somewhere
    /// to attach it from.
    ///
    /// The kernel command line is baked into the image and is therefore the
    /// same on every node that boots it, so it cannot name a per-machine
    /// namespace. The service tag can: it identifies the machine rather than
    /// one of its network cards, and survives a card being swapped. This is
    /// the same `boothost/<tag>` claim the firmware makes one stage earlier —
    /// by the time Linux is up the firmware's block device is gone with the
    /// UEFI that published it, so the node has to ask again in its own right.
    ///
    /// Prints the attach URI on stdout and nothing else, so it can be used
    /// directly as `--slab`:
    ///
    ///   stormblock boot-local --slab "$(stormblock boot-claim --boothost URL)"
    BootClaim {
        /// The appliance, e.g. http://192.168.31.202:9090
        #[arg(long, required = true)]
        boothost: String,
        /// The machine's identity, exactly as the synonym names it.
        ///
        /// Not discovered here. stormbootx reads it from SMBIOS and claims on
        /// it before Linux exists (`smbios.rs`, `registry.rs`); this is that
        /// same string handed down. Two implementations of "who is this
        /// machine" would drift, and the one in firmware is the one that has
        /// been proven on hardware.
        #[arg(long, required = true)]
        tag: String,
        /// Synonym namespace holding the per-machine decision.
        #[arg(long, default_value = "boothost")]
        namespace: String,
        /// How long to keep trying. A node boots faster than the appliance
        /// reboots, and a machine that gives up first needs a human.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        /// Bearer token, for an appliance that requires one (#107). Falls
        /// back to `$STORMBLOCK_API_TOKEN`.
        ///
        /// A claim is a write — it hands this machine a clone and marks it
        /// taken — so on a closed appliance it needs a credential. The kernel
        /// command line is the only place an initramfs has to carry one, and
        /// that means every machine booting that image shares it: it is a
        /// fleet token, worth no more than the boot network it travels on.
        #[arg(long)]
        token: Option<String>,
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
        /// Take --local-disk even though it already carries a data slab,
        /// destroying the identity on it. Never inferred: a policy cannot
        /// decide this, only somebody who knows the drive is spent.
        #[arg(long)]
        local_disk_force: bool,
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
        /// What the slab is for: `system` (goldens, replaced by an image) or
        /// `data` (identity and state, which no install may reformat)
        #[arg(long, default_value = "system")]
        role: String,
        /// Bytes reserved for the slab's own record of what it holds.
        ///
        /// Sized from the device by default, for every role: a slab that
        /// cannot say what is on it can only be read by attaching it. `0`
        /// formats one that deliberately keeps no record of itself.
        #[arg(long)]
        metadata_bytes: Option<u64>,
    },
    /// Grow a node disk's data half into the space after it.
    ///
    /// The data slab is the last partition; when the drive has grown, this
    /// extends the partition to the end and the slab into it, in place. The
    /// boot does the same on its own disk before opening it.
    Grow {
        /// The whole disk, e.g. /dev/sda
        device: String,
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
    /// List the volumes a slab says it holds — offline, read-only (#108).
    ///
    /// The volume records live on the device, in the region the header's
    /// `meta_offset`/`meta_size` name, and this reads them exactly the way
    /// `slab info` reads the header: no daemon, no reactor, no ublk, no root
    /// and nothing attached. It is the check something has to make *before*
    /// it decides whether this slab is one to touch at all — an initramfs
    /// deciding whether to boot from this disk or ask the appliance, and a
    /// disk formatted and never filled passes every other check and boots
    /// nothing.
    ///
    /// Prints positive evidence, in the shape `slab list` uses, so a caller
    /// can require a match rather than infer one from the absence of an
    /// error:
    ///
    ///   /dev/sda2: volume boot-cp-01 (2.1 GB, 540 slots)
    ///
    /// A slab with no metadata region says so rather than reporting no
    /// volumes: "keeps no volume metadata" and "holds no volumes" are
    /// different answers, and only one of them means the disk is empty.
    Volumes {
        /// Device paths, partitions or image files to read
        devices: Vec<String>,
    },
    /// Does a local drive already hold the release on an image? (#236)
    ///
    /// Compares the image's goldens (its sealed volumes, by id) with every
    /// volume the local drive's slabs record. Read-only on both. This is how
    /// the initramfs tells an install from a reboot when the appliance serves
    /// no boot intent: a netboot of a release the local disk holds is a
    /// reboot; of one it does not, an install.
    ///
    /// Exit 0: held. Exit 1: not held. Exit 2: cannot say (one side keeps no
    /// records, or the image has no sealed volume) — neither answer. Exit 3:
    /// the same release, its flow-over cut short (records still place extents
    /// on a slab not on the drive): boot the drive, which finishes it from a
    /// fresh clone (#171) — never a reason to install over it (#258).
    Holds {
        /// The local drive (a disk whose partitions are slabs, or a slab)
        local: String,
        /// The image: a device path, a file or an nvme-tcp:// URI
        image: String,
    },
}

#[derive(clap::Subcommand)]
enum ImageAction {
    /// Build an image from a TOML spec
    Build {
        /// Path to the image spec
        #[arg(long, default_value = "image.toml")]
        spec: String,
        /// The engine holding any `volume:` goldens the spec names.
        ///
        /// A golden that is a sealed volume is read from the appliance rather
        /// than from a file on the build box, so nothing is converted, copied
        /// or duplicated to build an image out of it. Only needed when the
        /// spec actually names one.
        #[arg(long, env = "STORMBLOCK_ENGINE")]
        engine: Option<String>,
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
    /// Lay a node's layout on a drive: boot area, system slab, data slab last.
    /// What `boot-local --local-disk` does to a drive it takes, by hand.
    /// **Destroys what is on the drive**, and refuses one carrying a data
    /// slab — that partition is a node's identity.
    LayNode {
        /// The drive (or an image file standing in for one)
        #[arg(long)]
        disk: String,
        /// GPT block size. Defaults to the drive's logical sector size, which
        /// is what firmware reads the table in; a file follows the device
        #[arg(long)]
        lba: Option<u32>,
        /// Boot area in front of the system slab, e.g. 4G. Defaults by drive
        /// size (4G on 64G and up, 1G on 16G and up, none below)
        #[arg(long)]
        boot_area: Option<String>,
        /// System slab size, e.g. 32G. Defaults by drive size
        #[arg(long)]
        system: Option<String>,
    },
    /// Make an installed disk boot on its own: copy the ESP (stormuefi) and
    /// the boot pallets of the image a node booted into the disk's boot area
    /// (#123). What the engine does after a flow-over, by hand.
    LocalBoot {
        /// The node's disk — a drive carrying the two slabs a flow-over lays
        #[arg(long)]
        disk: String,
        /// The image to take them from: a disk, an image file, or an
        /// `nvme-tcp://` URI (repeat for several)
        #[arg(long = "from", required = true)]
        from: Vec<String>,
    },
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
    redundancy: crate::volume::RedundancyPolicy,
}

fn parse_volume_spec(s: &str) -> Result<VolumeSpec, String> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err("format: name:size[:redundancy] (e.g. data:100G, data:100G:mirror:2)".into());
    }
    let name = parts[0].to_string();
    let size = parse_size(parts[1])?;
    let redundancy = match parts.get(2) {
        Some(r) => crate::volume::RedundancyPolicy::parse(r)?,
        None => Default::default(),
    };
    Ok(VolumeSpec { name, size, redundancy })
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

/// The `stormblock` command: parse the command line and run it. `main.rs`
/// is a wrapper around this, so the CLI is compiled and unit-tested once, as
/// part of the library (#209), not a second time as a separate binary.
pub async fn run() -> anyhow::Result<()> {
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
            SubCommand::MustGather { slab, meta, out, volumes, no_contents, max_file_mb } => {
                return handle_must_gather(
                    slab, meta.as_deref(), out, volumes, *no_contents, *max_file_mb,
                ).await;
            }
            SubCommand::Golden { out, size, label, tars, whiteouts, fsck, read_only } => {
                return handle_golden(out, size, label.as_deref(), tars, *whiteouts, *fsck, *read_only).await;
            }
            SubCommand::Attach { slab, meta, volumes, all, mount, ro, force } => {
                return handle_attach(
                    slab, meta.as_deref(), volumes, *all, mount.as_deref(), *ro, *force,
                ).await;
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
                    &cli.config,
                ).await;
            }
            SubCommand::BootClaim { boothost, tag, namespace, timeout_secs, token } => {
                return handle_boot_claim(boothost, tag, namespace, *timeout_secs, token.as_deref()).await;
            }
            SubCommand::BootLocal {
                slab, meta, volume, boot_config, image_store, writable, local_disk, local_tier,
                local_disk_force, check,
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
                    *local_disk_force,
                    *check,
                ).await;
            }
            SubCommand::Migrate { local_disk, tier } => {
                tracing::info!("Migration mode: target={}, tier={}", local_disk, tier);
                tracing::info!("Migration requires a running StormBlock instance.");
                tracing::info!("A boot volume flows over with `boot-local --local-disk`; a running engine moves volumes with /api/v1/moves or /api/v1/volumes/{{id}}/tier.");
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
    // A volume extent IS a slab slot. The volume layer divides an offset by
    // this to pick an extent and uses the remainder as the offset *within the
    // slot* the slab hands back, so a value larger than the slab's slot size
    // does not mean "bigger extents" — it means every write runs past the end
    // of its own slot and over its neighbours. This was `DEFAULT_EXTENT_SIZE`
    // (4 MiB) against slabs formatted with `DEFAULT_SLOT_SIZE` (1 MiB): extent
    // 0 was written across slots 0-3, extent 1 across 4-7, and the data that
    // did land was overwritten by the next extent's overflow. It read back as
    // whole megabytes of zeros scattered through the volume, and only on the
    // serving path — `boot-local` and `image build` take their extent size
    // from the slab they opened, which is why every image this engine built
    // was correct while everything it served was not.
    let extent_size = drive::slab::DEFAULT_SLOT_SIZE;
    let volume_manager = match data_dir {
        Some(dir) => {
            tracing::info!("Volume metadata persistence enabled: {dir}");
            VolumeManager::with_data_dir(extent_size, dir.into())?
        }
        None => VolumeManager::new(extent_size),
    };
    let slab_registry = volume_manager.registry().clone();
    let gem = volume_manager.gem().clone();
    let mut state = Arc::new(AppState::new(config.clone(), volume_manager, slab_registry, gem));

    // What consumers will be told to dial, said once, before anything can be
    // attached — a derived address is a guess on a multi-homed node.
    mgmt::config::log_advertised_host(
        &config.management,
        cli.nvmeof_addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(""),
    );

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
        let disc = Arc::new(
            mgmt::discovery::Discovery::new(
                node_name,
                mgmt_addr,
                config.management.data_dir.as_ref().map(std::path::PathBuf::from),
                std::time::Duration::from_secs(config.management.peer_stale_secs.max(1)),
            )
            .with_topology(config.management.topology.clone()),
        );
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
        let last = crate::volume::gc::spawn(
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
        let status = crate::volume::pressure::spawn(
            config.pressure.clone(),
            state.slab_registry.clone(),
            extent_size,
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
    // The drives to publish as NVMe namespaces, in config order, when the
    // drives *are* what is served. Empty when a RAID array or a set of
    // volumes is being exported instead — then there is one thing to export
    // and `export_device` is it.
    let mut raw_drive_namespaces: Vec<Arc<dyn BlockDevice>> = Vec::new();

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
                            labels: Default::default(),
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

        // RAID sets first (#252). A drive that is a set's member or a hot
        // spare carries a RAID superblock, not a slab: the storage on it is
        // the set's, found on the set once it is put back together. So those
        // drives are taken out of everything below — the slab scan, `--raid`,
        // the raw namespaces — and nothing formats over them.
        let assembled_sets;
        {
            let (report, claimed) = crate::mgmt::raid_sets::assemble_and_adopt(&state, &drives).await;
            if !report.arrays.is_empty() || !report.spares.is_empty() || !report.refused.is_empty() {
                tracing::info!(
                    "RAID: {} set(s) assembled, {} refused, {} spare(s); {} slab(s) and {} volume(s) on them",
                    report.arrays.len(),
                    report.refused.len(),
                    report.spares.len(),
                    report.slabs_adopted,
                    report.volumes_adopted
                );
            }
            assembled_sets = report.arrays.iter().any(|a| !a.already);
            drives.retain(|d| {
                !claimed.iter().any(|c| std::ptr::addr_eq(Arc::as_ptr(c), Arc::as_ptr(d)))
            });
        }

        // Take on the storage that is already on them. For an appliance whose
        // drives *are* its storage pool this is the difference between coming
        // back up holding what it held and coming back up empty: slabs were
        // only ever registered by an explicit call, so a restart left the pool
        // invisible until someone made one — and the only other way to
        // register a slab is to format it, which is the wrong answer to
        // "where did my volumes go".
        //
        // Non-destructive: it opens what is there and reads it. A drive with
        // no slab on it contributes nothing.
        {
            let mut adopted_slabs = 0usize;
            let mut adopted_volumes = 0usize;
            // Every drive's slabs in one call: a volume's legs can be on
            // several drives, and adopting one drive at a time restored the
            // volume from the first and dropped its legs on the rest — half of
            // a two-drive volume came back after a restart.
            let mut found = Vec::new();
            for dev in &drives {
                found.extend(crate::drive::discover::slabs_in_partitions(dev).await);
            }
            if !found.is_empty() {
                let mut vm = state.volume_manager.lock().await;
                match vm.adopt_slabs(found).await {
                    Ok(r) => {
                        adopted_slabs += r.slabs.len();
                        adopted_volumes += r.volumes.len();
                    }
                    Err(e) => tracing::warn!("adopting the drives' slabs: {e}"),
                }
            }
            if adopted_slabs > 0 {
                tracing::info!(
                    "adopted {adopted_slabs} slab(s) and {adopted_volumes} volume(s) \
                     already on the drives"
                );
            }
        }
        metrics::gauge!("stormblock_drives_total").set(drives.len() as f64);
        metrics::gauge!("stormblock_capacity_bytes").set(
            drives.iter().map(|d| d.capacity_bytes() as f64).sum::<f64>()
        );

        // Phase 2: Create RAID array if requested — unless the drives already
        // carry one, which was assembled above: re-creating it on every start
        // formatted the data away.
        if cli.raid.is_some() && assembled_sets {
            tracing::info!("--raid: the drives already carry a RAID set (assembled above); not creating another");
            let vm = state.volume_manager.lock().await;
            if let Some((id, ..)) = vm.list_volumes().await.first() {
                export_device = vm.get_volume(id);
            }
        } else if let Some(level) = cli.raid {
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

                        // Register array in state + volume manager. The slab
                        // carries its volumes' records, so a restart that
                        // reassembles the array finds them (#252).
                        {
                            let mut vm = state.volume_manager.lock().await;
                            if let Err(e) = vm.add_array_slab(array_id, backing, false).await {
                                tracing::error!("formatting the slab on array {array_id}: {e}");
                            }
                        }
                        let _ = (array_level, array_member_count, array_capacity, array_stripe);
                        crate::mgmt::raid_sets::register(&state, arc_array).await;

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
                                let created = if spec.redundancy.is_none() {
                                    vm.create_volume(&spec.name, spec.size, array_id).await
                                } else {
                                    vm.create_volume_with(
                                        &spec.name,
                                        spec.size,
                                        crate::volume::CreateOptions::redundant(spec.redundancy.clone()),
                                    )
                                    .await
                                };
                                match created {
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
                        let arc_array = Arc::new(array);
                        crate::mgmt::raid_sets::register(&state, arc_array.clone()).await;
                        export_device = Some(arc_array);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create RAID array: {e}");
                    return Err(e.into());
                }
            }
        } else if !drives.is_empty() {
            // No RAID and no volumes: the drives themselves are what this
            // node serves, each as its own namespace. `drives.len() == 1`
            // here used to mean a second drive was exported as nothing at
            // all — invisible to every initiator, with no way to write it
            // except copying a finished file onto the machine.
            raw_drive_namespaces = drives.clone();
            export_device = Some(drives.into_iter().next().unwrap());
        }
    } else {
        tracing::info!("No devices specified (use -d /path/to/device)");
    }

    // Phase 6: Start cluster engine (if enabled)
    //
    // Peers are called with the cluster's shared token (#107), and the
    // heartbeat and Raft clients are built here — before the management
    // server resolves its own token — so name it now.
    #[cfg(feature = "cluster")]
    if config.cluster.enabled {
        if let Some(t) = config.management.api_token.clone().filter(|t| !t.trim().is_empty()) {
            crate::mgmt::auth::set_fleet_token(Some(t));
        }
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
        let reg = crate::stormfs::StormFsRegistration::new(config.stormfs.clone());
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
            // Namespace n is the nth drive in the configuration, from 1. That
            // ordering is the whole contract an initiator has for telling the
            // drives apart, so it is logged rather than left to be inferred.
            if !config.nvmeof.as_ref().map(|n| n.export_drives).unwrap_or(true) {
                // The drives are this engine's storage pool, not what it
                // serves. Publishing them raw beside the volume exports would
                // hand every initiator an unmanaged second writer into slabs
                // the engine allocates from.
                tracing::info!(
                    "NVMe-oF: not publishing {} drive(s) as raw namespaces \
                     (nvmeof.export_drives = false); volume exports only",
                    raw_drive_namespaces.len().max(1),
                );
            } else if raw_drive_namespaces.is_empty() {
                nvmeof.add_namespace(1, device.clone());
            } else {
                for (i, drive) in raw_drive_namespaces.iter().enumerate() {
                    let nsid = i as u32 + 1;
                    tracing::info!(
                        "NVMe-oF namespace {nsid}: {} ({} bytes)",
                        config.drives.get(i).map(|d| d.path.as_str()).unwrap_or("?"),
                        drive.capacity_bytes(),
                    );
                    nvmeof.add_namespace(nsid, drive.clone());
                }
            }
            // Before the exports, because a template is a fact about a
            // volume and an export is a decision about one.
            mgmt::api::fstemplates::adopt_slab_templates(&state).await;
            serve_shared_nvmeof(&state, &reactor, Arc::new(nvmeof)).await;
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

    start_serving(&config, &state, &iscsi_bind, &nvmeof_bind, &reactor).await;

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

    // **Kernel devices first, and signalled before anything is waited on.**
    //
    // A ublk export's queue threads sit in `io_uring_enter` waiting for work.
    // Nothing here used to tell them to stop, so the process exited with the
    // devices still up: the threads stayed in the kernel, and a thread stuck
    // in the kernel cannot be reaped. forge carried a defunct process in its
    // unit's cgroup for four days, and *every* restart after it ended in
    // "failed mode" because systemd found something it could not kill (#105).
    //
    // The lock is bounded too, for the same reason the flush below is: a
    // claim in flight holds this map, and a stop that waits on a lock is a
    // stop that does not happen.
    let ublk = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        state.ublk_exports.lock(),
    )
    .await
    {
        Ok(mut mgr) => {
            let wait = mgr.shutdown_all();
            if !wait.is_empty() {
                tracing::info!("stopping {} ublk export(s)", wait.len());
            }
            wait
        }
        Err(_) => {
            tracing::warn!(
                "ublk exports not signalled: something still holds the export map. \
                 Their devices stay up and their threads with them."
            );
            mgmt::ublk_export::ShutdownWait::none()
        }
    };

    // Bounded, because a stop that waits on a lock is a stop that does not
    // happen.
    //
    // This took the volume manager's mutex and flushed under it. Any task
    // holding that lock — a compose, a reconciler pass, a claim — holds it
    // against the shutdown too, so `systemctl stop` sat for its full timeout
    // and systemd escalated to SIGKILL. A process killed there leaves its
    // io_uring and ublk teardown unrun, and a thread stuck in the kernel
    // cannot be reaped: forge carried an unreapable process in the unit's
    // cgroup for four days, and *every* restart after it ended in "failed
    // mode" because systemd found something it could not kill.
    //
    // Ten seconds is enough for a flush and short enough to be a stop. What
    // is lost by giving up is nothing that is not recoverable: each slab
    // keeps its own copy of the volume record, which is what a node reads at
    // boot and what adoption rebuilds from.
    //
    // Run alongside the ublk teardown rather than after it: they contend for
    // nothing, and a stop's budget is the sum of what it does in series. The
    // whole of this is ~13 s worst case, comfortably inside the unit's
    // `TimeoutStopSec` — which is the number that decides whether systemd
    // sends SIGKILL into the middle of a device teardown.
    let flush = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let vm = state.volume_manager.lock().await;
        vm.persist().await;
    });
    let ublk_count = ublk.len();
    let (flushed, stuck) = tokio::join!(flush, ublk.settle(std::time::Duration::from_secs(10)));
    match flushed {
        Ok(()) => tracing::info!("volume metadata flushed"),
        Err(_) => tracing::warn!(
            "volume metadata not flushed within 10s — something still holds the manager. \
             Each slab's own copy stands, which is what adoption reads."
        ),
    }
    if stuck.is_empty() {
        if ublk_count > 0 {
            tracing::info!("{ublk_count} ublk export(s) stopped");
        }
    } else {
        // Named, because this is the log line that says why the next restart
        // ends in failed mode.
        tracing::warn!(
            "ublk teardown unfinished after 10s for {} — exiting anyway; \
             a thread of this process may be left in the kernel",
            stuck.join(", ")
        );
    }
    #[cfg(feature = "cluster")]
    if let Some(ref _cluster_mgr) = state.cluster {
        tracing::info!("Cluster shutdown initiated");
    }
    drop(reactor);

    Ok(())
}

/// Wait for ublk export threads to finish their teardown, up to `budget`.
///
/// `JoinHandle::join` has no deadline, and a teardown that wedges in the
/// kernel would hold the whole stop open until systemd sends SIGKILL — which
/// is what leaves a thread stuck and a process that cannot be reaped (#105).
/// So poll, and leave: the process is exiting, and a thread that is not going
/// to finish is not going to finish because we waited longer.
///
/// Returns how many were still running when the budget ran out.
// Every caller is behind `cfg(target_os = "linux")` — ublk is a Linux
// interface — so a macOS build has none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn join_ublk_threads<T>(
    threads: Vec<std::thread::JoinHandle<T>>,
    budget: std::time::Duration,
) -> usize {
    let deadline = std::time::Instant::now() + budget;
    let mut pending = threads;
    loop {
        let (done, still): (Vec<_>, Vec<_>) = pending.into_iter().partition(|t| t.is_finished());
        for t in done {
            let _ = t.join();
        }
        if still.is_empty() {
            return 0;
        }
        if std::time::Instant::now() >= deadline {
            return still.len();
        }
        pending = still;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
        SlabAction::Format { device, tier, role, metadata_bytes } => {
            let tier = parse_tier(tier)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let role = SlabRole::parse(role)
                .ok_or_else(|| anyhow::anyhow!("unknown slab role '{role}': system or data"))?;
            // Formatting is destructive, and a data slab is the one thing on
            // the node nothing can mint again (#88).
            if let Some(what) = data_slab_on(device).await? {
                if role != SlabRole::Data {
                    anyhow::bail!(
                        "refusing to format {device}: {what}. Pass --role data if you mean to \
                         replace it"
                    );
                }
            }
            let dev = (
                open_storage(device).await?
            );
            // Every slab carries its own volume records, whatever its role,
            // and how much room that takes scales with the slots it can hand
            // out — leave it at none and every write to it is acknowledged and
            // lost at the next restart.
            //
            // This reserved a region for `data` alone, and the reasoning was
            // sound as far as it went: a data slab has to outlive whatever
            // formatted it. But `image build` gives *both* roles a region, so
            // a disk formatted here and a disk the image builder laid down
            // were not the same kind of thing. A system slab formatted by this
            // command could not say what was on it: `slab volumes` answered
            // "keeps no volume metadata", the initramfs boot probe could not
            // verify the volume the loader entry names, and the fallback the
            // bounded shutdown flush leans on — each slab keeps its own copy,
            // which is what adoption reads — did not exist for it.
            //
            // `--metadata-bytes 0` is the door out for a slab that
            // deliberately keeps no record of itself.
            let capacity = dev.capacity_bytes();
            let meta = metadata_bytes.unwrap_or_else(|| {
                crate::drive::slab::auto_metadata_bytes(capacity, SLAB_SLOT_SIZE)
            });
            let opts = crate::drive::slab::SlabFormat::new(SLAB_SLOT_SIZE, tier)
                .with_role(role)
                .with_metadata(meta);
            let slab = Slab::format_with(dev, opts).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Slab formatted: {}", slab.slab_id());
            println!("  role: {}", slab.role());
            println!("  tier: {}", slab.tier());
            // Said out loud, because "keeps no volume metadata" from
            // `slab volumes` later is otherwise the first anyone hears of it.
            println!(
                "  own record: {}",
                if slab.has_metadata_region() {
                    crate::mgmt::config::human_size(slab.metadata_capacity())
                } else {
                    "none — this slab cannot say what is on it".to_string()
                }
            );
            println!("  slot size: {} bytes", slab.slot_size());
            println!("  total slots: {}", slab.total_slots());
            println!("  capacity: {}", crate::mgmt::config::human_size(
                slab.total_slots() * slab.slot_size()));
        }
        SlabAction::Grow { device } => {
            // `open` creates a missing path; a typo must not become a file.
            if !std::path::Path::new(device).exists() {
                anyhow::bail!("{device} does not exist");
            }
            let dev: Arc<dyn BlockDevice> =
                open_storage(device).await?;
            match crate::image::local::grow_data_half(dev).await? {
                Some((was, now)) => println!(
                    "{device}: data slab grew from {was} to {now} slots (+{})",
                    crate::mgmt::config::human_size((now - was) * SLAB_SLOT_SIZE)
                ),
                None => println!("{device}: nothing to grow into"),
            }
        }
        SlabAction::List { devices } => {
            for device in devices {
                match inspect_storage(device).await {
                    Ok(dev) => {
                        match Slab::open(dev.clone()).await {
                            Ok(slab) => {
                                println!("{}: slab {} (role={}, tier={}, {} slots, {} free)",
                                    device, slab.slab_id(), slab.role(), slab.tier(),
                                    slab.total_slots(), slab.free_slots());
                            }
                            // A disk whose *partitions* are slabs is a slab
                            // disk, and saying "not a slab" about it is
                            // wrong in the way that matters most.
                            //
                            // The boot path has walked the GPT for a long
                            // time — it is how `rd.stormblock.slab=/dev/sda`
                            // works on a composed disk — and this did not, so
                            // the same drive gave two answers depending on
                            // which asked. The node that flowed over onto its
                            // own disk then read `/dev/sda is not a slab` from
                            // its own probe and went back to the appliance,
                            // with 4399 migrated extents sitting unused on the
                            // drive underneath it.
                            Err(e) => {
                                let found =
                                    crate::drive::discover::slabs_in_partitions(&dev).await;
                                if found.is_empty() {
                                    println!("{}: not a slab ({e})", device);
                                } else {
                                    for f in found {
                                        println!(
                                            "{}: slab {} (role={}, tier={}, {} slots, {} free, \
                                             in {})",
                                            device, f.slab.slab_id(), f.slab.role(),
                                            f.slab.tier(), f.slab.total_slots(),
                                            f.slab.free_slots(), f.label,
                                        );
                                    }
                                }
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
            let dev = (
                inspect_storage(device).await?
            );
            let slab = Slab::open(dev).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Slab {}", slab.slab_id());
            println!("  role: {}", slab.role());
            println!("  tier: {}", slab.tier());
            println!("  slot size: {} bytes", slab.slot_size());
            println!("  total slots: {}", slab.total_slots());
            println!("  free slots: {}", slab.free_slots());
            println!("  allocated slots: {}", slab.allocated_slots());
            println!("  capacity: {}", crate::mgmt::config::human_size(
                slab.total_slots() * slab.slot_size()));
            println!("  free: {}", crate::mgmt::config::human_size(
                slab.free_slots() * slab.slot_size()));
        }
        SlabAction::Holds { local, image } => {
            use crate::image::local::{release_held, ReleaseHeld};
            let open = |p: String| async move {
                inspect_storage(&p).await.map_err(|e| anyhow::anyhow!("cannot open {p}: {e}"))
            };
            let (l, i) = match (open(local.clone()).await, open(image.clone()).await) {
                (Ok(l), Ok(i)) => (l, i),
                (Err(e), _) | (_, Err(e)) => {
                    println!("cannot say: {e}");
                    std::process::exit(2);
                }
            };
            match release_held(&l, &i).await {
                ReleaseHeld::Held { goldens } => {
                    println!("{local} holds the release on {image}: all {goldens} golden(s)");
                }
                ReleaseHeld::NotHeld { goldens, missing } => {
                    println!(
                        "{local} does not hold the release on {image}: {missing} of {goldens} \
                         golden(s) missing"
                    );
                    std::process::exit(1);
                }
                ReleaseHeld::Unfinished { goldens, slabs } => {
                    // The same release, cut short: a power cut during the
                    // install's flow-over. Booting the drive finishes it; an
                    // install over it would destroy what the node wrote
                    // since (#258, 0 of 300 objects on 11.63).
                    println!(
                        "{local} holds the release on {image}, unfinished: all {goldens} golden(s) \
                         are recorded, and extents are still placed on {slabs} slab(s) not on this \
                         drive — booting it finishes the flow-over from a fresh clone"
                    );
                    std::process::exit(3);
                }
                ReleaseHeld::CannotSay(why) => {
                    println!("cannot say whether {local} holds the release on {image}: {why}");
                    std::process::exit(2);
                }
            }
        }
        SlabAction::Volumes { devices } => {
            for device in devices {
                // Read-only, and never created: this is the command something
                // runs on a machine it knows nothing about, before deciding
                // whether that machine's disk is one to touch. The ordinary
                // door creates what it cannot find, so `slab volumes /dev/sdz`
                // made a zero-byte /dev/sdz and called it "not a slab" — true,
                // and not what happened.
                let dev = match inspect_storage(device).await {
                    Ok(d) => d,
                    Err(e) => {
                        println!("{device}: cannot open ({e})");
                        continue;
                    }
                };
                // A whole disk whose partitions are slabs answers for all
                // of them. `rd.stormblock.slab=/dev/sda` names a disk, and
                // this is the command the boot probe runs to decide whether
                // that disk can boot the node — so it has to look where the
                // boot itself looks. It did not, and a node that had just
                // migrated 4399 extents onto its own drive read `/dev/sda is
                // not a slab` from its own probe and went back to the
                // appliance, leaving every one of them unused.
                let slabs: Vec<Slab> = match Slab::open(dev.clone()).await {
                    Ok(s) => vec![s],
                    Err(e) => {
                        let found =
                            crate::drive::discover::slabs_in_partitions(&dev).await;
                        if found.is_empty() {
                            println!("{device}: not a slab ({e})");
                            continue;
                        }
                        found.into_iter().map(|f| f.slab).collect()
                    }
                };
                for slab in slabs {
                // Said apart from "no volumes", because they are different
                // facts: one is a slab that cannot answer and the other is a
                // slab that answered "nothing". A caller that treats them the
                // same boots off an empty disk.
                if !slab.has_metadata_region() {
                    println!("{device}: slab {} keeps no volume metadata", slab.slab_id());
                    continue;
                }
                let bytes = match slab.read_metadata().await {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        // The region is there and no copy has ever been
                        // written: this slab is *empty*, which is a different
                        // answer from "cannot say" and the one #108 was filed
                        // about — a slab formatted and never filled boots
                        // nothing, and reads as fine to everything that only
                        // asks whether a slab is there.
                        println!("{device}: slab {} holds no volumes", slab.slab_id());
                        continue;
                    }
                    Err(e) => {
                        println!("{device}: slab {} metadata unreadable ({e})", slab.slab_id());
                        continue;
                    }
                };
                let meta = match crate::volume::MetadataStore::decode(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        println!("{device}: slab {} metadata will not decode ({e})", slab.slab_id());
                        continue;
                    }
                };
                let mut volumes = meta.volumes;
                volumes.sort_by(|a, b| a.name.cmp(&b.name));
                if volumes.is_empty() {
                    println!("{device}: slab {} holds no volumes", slab.slab_id());
                    continue;
                }
                for v in &volumes {
                    // Slots, not extents: an extent *is* a slot, and slots are
                    // what `slab list` counts, so the two numbers on a screen
                    // are in the same unit.
                    let slots = v.extents.len() as u64;
                    let mut notes = String::new();
                    if v.sealed {
                        notes.push_str(", sealed");
                    }
                    if v.template {
                        notes.push_str(", template");
                    }
                    if let Some(parent) = v.parent {
                        notes.push_str(&format!(", clone of {parent}"));
                    }
                    println!(
                        "{device}: volume {} ({}, {} slots{}) {}",
                        v.name,
                        crate::mgmt::config::human_size(v.virtual_size),
                        slots,
                        notes,
                        v.id
                    );
                }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- pallets

/// Open the drives a pallet command works over. A file is a drive here — same
/// GPT, same partitions — which is what makes an image assembled on a laptop
/// and a disk in a node the same thing.
async fn pallet_store(drives: &[String]) -> anyhow::Result<crate::pallet::PalletStore> {
    let mut store = crate::pallet::PalletStore::default();
    for path in drives {
        let dev = crate::drive::open_one_drive(path)
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

fn print_pallet(p: &crate::pallet::PalletLocation) {
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
fn pe<T>(r: Result<T, crate::pallet::PalletError>) -> anyhow::Result<T> {
    r.map_err(|err| anyhow::anyhow!("{err}"))
}

// ----------------------------------------------------------------- images

async fn handle_image_command(action: &ImageAction) -> anyhow::Result<()> {
    use std::path::{Path, PathBuf};
    use crate::image::{ImageBuilder, ImageFormat, ImageSpec};

    let ie = |e: crate::image::ImageError| anyhow::anyhow!("{e}");
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
        ImageAction::LayNode { disk, lba, boot_area, system } => {
            if let Some(what) = data_slab_on(disk).await? {
                anyhow::bail!("refusing to lay a node layout on {disk}: {what}");
            }
            let dev: Arc<dyn BlockDevice> =
                open_storage(disk).await?;
            let mut layout = crate::image::local::LocalLayout::for_drive(dev.capacity_bytes());
            layout.lba = lba.or_else(|| crate::drive::filedev::logical_sector_size(disk));
            if let Some(b) = boot_area {
                layout.boot_bytes = crate::mgmt::config::parse_size(b).map_err(|e| anyhow::anyhow!(e))?;
            }
            if let Some(sz) = system {
                layout.system_bytes = crate::mgmt::config::parse_size(sz).map_err(|e| anyhow::anyhow!(e))?;
            }
            let laid = crate::image::local::lay_node_slabs(dev, &layout).await.map_err(ie)?;
            println!(
                "{disk}: {}-byte table, boot area {}, system slab {} ({}), data slab {} ({})",
                laid.lba,
                crate::mgmt::config::human_size(layout.boot_bytes),
                laid.system.slab_id(),
                crate::mgmt::config::human_size(laid.system_bytes),
                laid.data.slab_id(),
                crate::mgmt::config::human_size(laid.data_bytes),
            );
        }
        ImageAction::LocalBoot { disk, from } => {
            run_local_boot(disk, from).await?;
        }
        ImageAction::Build { spec, out, format, keep_raw, engine } => {
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

            let report = ImageBuilder::new(image_spec)
                .engine(engine.clone())
                .build(&raw_path)
                .await
                .map_err(ie)?;
            println!(
                "{} — {} in {} partitions, GPT in {}-byte LBAs",
                raw_path.display(),
                crate::mgmt::config::human_size(report.size_bytes),
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
                    crate::mgmt::config::human_size(p.size_bytes),
                    crate::mgmt::config::human_size(p.start_bytes),
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
                        crate::mgmt::config::human_size(v.size_bytes),
                        crate::mgmt::config::human_size(v.allocated_bytes),
                        match v.clone_of {
                            Some(g) => format!("clone of {g}"),
                            None => "golden".to_string(),
                        }
                    );
                }
            }

            if format != ImageFormat::Raw {
                crate::image::formats::convert(&raw_path, &out_path, format)
                    .await
                    .map_err(ie)?;
                let len = tokio::fs::metadata(&out_path).await?.len();
                println!(
                    "{} — {} ({})",
                    out_path.display(),
                    crate::mgmt::config::human_size(len),
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
                crate::image::iso::from_image_with(
                    Path::new(input),
                    Path::new(out),
                    crate::image::iso::IsoOptions { include_slab: *include_slab },
                )
                .await
                .map_err(ie)?;
            } else {
                crate::image::formats::convert(Path::new(input), Path::new(out), format)
                    .await
                    .map_err(ie)?;
            }
            let len = tokio::fs::metadata(out).await?.len();
            println!("{out} — {} ({format})", crate::mgmt::config::human_size(len));
        }
        ImageAction::Inspect { image } => {
            let path = Path::new(image);
            let gpt = crate::image::build::table_of(path).await.map_err(ie)?;
            println!(
                "{image}: GPT in {}-byte LBAs{}",
                gpt.block_size,
                if gpt.recovered_from_backup { " (read from the backup)" } else { "" }
            );
            for (i, e) in gpt.partitions() {
                println!(
                    "  {i:>3}  {:<20} {:>10} at {:<12} {}",
                    e.name,
                    crate::mgmt::config::human_size(e.size_bytes(gpt.block_size)),
                    crate::mgmt::config::human_size(e.start_bytes(gpt.block_size)),
                    if e.is_pallet() { "pallet" } else { "" }
                );
            }
            for p in crate::image::build::pallets_in(path).await.map_err(ie)? {
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
            for s in crate::image::build::slabs_in(path).await.map_err(ie)? {
                println!(
                    "  {} slab {} — {} slots of {}, {} free{}",
                    s.role,
                    s.name,
                    s.total_slots,
                    crate::mgmt::config::human_size(s.slot_size),
                    s.free_slots,
                    if s.self_describing { "" } else { " (keeps no volume metadata)" }
                );
                for v in &s.volumes {
                    println!(
                        "    volume {:<24} {:>10} {:>10} mapped  {}",
                        v.name,
                        crate::mgmt::config::human_size(v.size_bytes),
                        crate::mgmt::config::human_size(v.allocated_bytes),
                        v.id
                    );
                }
            }
        }
    }
    Ok(())
}

async fn handle_pallet_command(drives: &[String], action: &PalletAction) -> anyhow::Result<()> {
    use crate::pallet::format::{parse_pallet_kind, MemberExt};
    use crate::pallet::manager::{PublishSpec, RecomposeSpec};
    use crate::pallet::{PalletBrowser, PalletManager};

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
                crate::mgmt::config::human_size(loc.size_bytes),
                crate::mgmt::config::human_size(loc.used_bytes),
            );
            match mgr.store().open(&loc).await {
                Ok(p) => {
                    for m in p.members() {
                        println!(
                            "  member {:<20} role={:<12} kind={:<10} {:>10}  {}",
                            m.name(),
                            m.role(),
                            m.kind,
                            crate::mgmt::config::human_size(m.byte_len),
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
                spec.members.push(pe(crate::pallet::manager::file_member(
                    name,
                    role,
                    crate::pallet::parse_member_kind(&kind),
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
                add.push(pe(crate::pallet::manager::file_member(
                    name,
                    role,
                    crate::pallet::parse_member_kind(&kind),
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
                    crate::pallet::ConvertOptions {
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
            crate::mgmt::config::human_size(part.size)
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
            crate::mgmt::config::human_size(part.size),
            part.fs_type,
            part.mount_point,
            part.volume_id,
        );
    }

    // Export partitions via ublk if requested (Linux only)
    #[cfg(target_os = "linux")]
    if ublk {
        use crate::drive::ublk::UblkServer;

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
                crate::mgmt::config::human_size(part.size), part.fs_type);
        }

        println!("\nublk devices ready. Press Ctrl+C to stop.");
        tokio::signal::ctrl_c().await?;
        println!("Shutting down...");

        // Signal all ublk servers to stop, and wait — bounded (#105).
        let _ = shutdown_tx.send(true);
        let stuck = join_ublk_threads(ublk_threads, std::time::Duration::from_secs(10));
        if stuck > 0 {
            eprintln!("WARNING: {stuck} ublk export(s) did not finish their teardown");
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
                crate::mgmt::config::human_size(part.size), part.fs_type);
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
/// The volume the engine keeps its own state in.
///
/// A well-known name rather than a flag: every node that has one wants it used,
/// and a node that has not got one carries on without. Never exported and never
/// mounted — the engine reads it in-process with the ext4 library.
const STATE_VOLUME: &str = "stormblock-state";

async fn resolve_boot_volume(
    mgr: &VolumeManager,
    selector: &str,
) -> anyhow::Result<crate::volume::VolumeId> {
    use crate::volume::VolumeId;
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

/// Whether `path` carries a data slab, and what names it.
///
/// Asked of the *device*, never of the path: the answer has to hold when an
/// operator hands over `/dev/sda` and the data slab is `/dev/sda6`, and when
/// they hand over `/dev/sda6` itself. Two independent records say so — the
/// GPT type GUID of the partition, which can be read without opening
/// anything, and the role byte in the slab's own header, which is what a
/// whole-drive slab with no partition table has instead (#88).
///
/// `Ok(None)` means nothing on the device claims to be one. A device that
/// cannot be read at all is not an error here: the caller is about to open it
/// properly and will fail there with a better message.
async fn data_slab_on(path: &str) -> anyhow::Result<Option<String>> {
    use crate::drive::partition::PartitionDevice;

    if !std::path::Path::new(path).exists() {
        return Ok(None);
    }
    let dev: Arc<dyn BlockDevice> =
        match inspect_storage(path).await {
            Ok(d) => d,
            Err(_) => return Ok(None),
        };

    // The device itself, when it is a bare slab rather than a partitioned
    // drive.
    if let Ok(slab) = Slab::open(dev.clone()).await {
        if slab.is_data() {
            return Ok(Some(format!("{path} is itself a data slab ({})", slab.slab_id().0)));
        }
        return Ok(None);
    }

    let Ok(gpt) = crate::pallet::gpt::Gpt::read(&dev).await else {
        return Ok(None);
    };
    let lba = gpt.block_size as u64;
    for (i, e) in gpt.entries.iter().enumerate() {
        if e.first_lba == 0 || e.last_lba < e.first_lba {
            continue;
        }
        let label = if e.name.is_empty() {
            format!("partition {}", i + 1)
        } else {
            format!("partition {} ({})", i + 1, e.name)
        };
        if e.type_guid == crate::image::type_guid::SLAB_DATA {
            return Ok(Some(format!("{path} {label} is typed as a stormblock data slab")));
        }
        // A slab whose GPT entry predates the data type still knows what it
        // is: the header carries the role too, and the two are written
        // together.
        let start = e.first_lba * lba;
        let len = (e.last_lba + 1 - e.first_lba) * lba;
        let Ok(part) = PartitionDevice::new(dev.clone(), start, len) else { continue };
        if let Ok(slab) = Slab::open(Arc::new(part)).await {
            if slab.is_data() {
                return Ok(Some(format!("{path} {label} holds a data slab")));
            }
        }
    }
    Ok(None)
}

/// A slab named as a fabric URI (`nvme-tcp://…`) rather than a local path.
/// stormblock opens one wherever it opens a device path — attaching instead of
/// statting — so a remote root is an ordinary slab.
/// Real storage by path, for writing (#140): a block device is opened
/// `O_DIRECT` as the drive it is and a fabric URI is attached — never a
/// `FileDevice`. A regular file stays one, and may be created: that is an
/// image being made, or a test's scratch disk.
async fn open_storage(path: &str) -> anyhow::Result<Arc<dyn BlockDevice>> {
    if is_fabric_uri(path) || crate::drive::is_block_device(path) {
        return Ok(crate::drive::open_path(path, false).await?);
    }
    Ok(Arc::new(crate::drive::filedev::FileDevice::open(path).await?))
}

/// Storage by path, for looking at (#140): read-only, never created.
async fn inspect_storage(path: &str) -> anyhow::Result<Arc<dyn BlockDevice>> {
    Ok(crate::drive::open_path(path, true).await?)
}

fn is_fabric_uri(path: &str) -> bool {
    path.contains("://")
}

/// Make `disk` boot on its own from the image at `sources` (#123): the ESP
/// and the boot pallets, into the boot area of the node layout. Read-only on
/// every source.
/// Answers whether the disk now boots on its own.
async fn run_local_boot(disk: &str, sources: &[String]) -> anyhow::Result<bool> {
    use crate::image::local_boot::{lay_local_boot, EspOutcome};

    let mut opened: Vec<(String, Arc<dyn BlockDevice>)> = Vec::new();
    for path in sources.iter().filter(|p| p.as_str() != disk) {
        let dev: Arc<dyn BlockDevice> = if is_fabric_uri(path) {
            let spec = crate::drive::nvmeof_dev::NvmeTcpSpec::parse(path)
                .ok_or_else(|| anyhow::anyhow!("malformed nvme-tcp URI: {path}"))?;
            Arc::new(crate::drive::nvmeof_dev::NvmeofDevice::connect(&spec).await?)
        } else {
            inspect_storage(path).await?
        };
        opened.push((path.clone(), dev));
    }
    let dest: Arc<dyn BlockDevice> =
        open_storage(disk).await?;
    let r = lay_local_boot(disk, dest, opened)
        .await
        .map_err(|e| anyhow::anyhow!("local boot on {disk}: {e}"))?;

    match &r.esp {
        EspOutcome::NoSource => println!("Local boot: {disk}: no ESP on the image — nothing to start the kernel with"),
        EspOutcome::Unchanged => println!("Local boot: {disk}: ESP already current"),
        EspOutcome::Copied { bytes } => println!("Local boot: {disk}: ESP copied ({bytes} bytes)"),
        EspOutcome::Rebuilt { from_sector, to_sector, files } => println!(
            "Local boot: {disk}: ESP rebuilt at {to_sector}-byte sectors from {from_sector} ({files} file(s))"
        ),
    }
    for c in &r.copied {
        println!("Local boot: {disk}: boot pallet {c} copied and verified");
    }
    if r.already > 0 {
        println!("Local boot: {disk}: {} boot pallet(s) already present", r.already);
    }
    for g in &r.removed {
        println!("Local boot: {disk}: removed {g}");
    }
    for f in &r.failed {
        println!("Local boot: {disk}: not copied — {f}");
    }
    for (name, version, pri) in &r.ladder {
        println!("Local boot: {disk}: ladder {name} v{version} priority {pri}");
    }
    if r.bootable() {
        println!("Local boot: {disk} boots on its own");
    } else {
        println!("Local boot: {disk} does not boot on its own yet");
    }
    Ok(r.bootable())
}

async fn open_slabs_and_restore(
    slab_paths: &[String],
    meta: Option<&str>,
) -> anyhow::Result<VolumeManager> {
    Ok(open_slabs_resuming(slab_paths, meta, false).await?.0)
}

/// What `open_slabs_resuming` had to fetch from the appliance (#171).
struct Resumed {
    /// The clone's attach URI, as the successor must open it.
    uri: String,
    /// The local slabs the flow-over resumes into.
    system_slab: Option<crate::drive::slab::SlabId>,
    data_slab: Option<crate::drive::slab::SlabId>,
}

/// The vendor GUID of stormbootx's volatile variables (#249).
const STORMBOOT_GUID: &str = "ab361f54-0166-44a4-a088-1ac22e98ab76";

/// This machine's name to the appliance, by the initramfs's rules (#249):
/// `STORMBLOCK_BOOT_TAG` (what `/init` resolved and exported), else the name
/// stormbootx claimed on (`StormBootTag`, a volatile EFI variable), else the
/// SMBIOS serial (a Dell's service tag), else the SMBIOS UUID. The SMBIOS
/// values are a guess: a MicroCloud's blades share one serial, and a resume
/// that claimed by it got another machine's image (#259).
fn machine_tag() -> Option<String> {
    let efivars = std::env::var("STORM_EFIVARS").unwrap_or_else(|_| "/sys/firmware/efi/efivars".into());
    let dmi = std::env::var("STORM_DMI").unwrap_or_else(|_| "/sys/class/dmi/id".into());
    let read = |f: String| std::fs::read_to_string(f).ok();
    machine_tag_from(
        std::env::var("STORMBLOCK_BOOT_TAG").ok(),
        std::fs::read(format!("{efivars}/StormBootTag-{STORMBOOT_GUID}")).ok(),
        read(format!("{dmi}/product_serial")),
        read(format!("{dmi}/product_uuid")),
    )
}

fn machine_tag_from(
    env: Option<String>,
    efivar: Option<Vec<u8>>,
    serial: Option<String>,
    uuid: Option<String>,
) -> Option<String> {
    if let Some(t) = env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Some(t);
    }
    // 4 attribute bytes, then the value: ASCII, no NUL (stormbootx#76). A
    // value outside the name alphabet is not one stormbootx would set — the
    // same check `/init` makes.
    if let Some(raw) = efivar.filter(|r| r.len() > 4) {
        let v: String = String::from_utf8_lossy(&raw[4..])
            .chars()
            .filter(|c| !matches!(c, '\0' | '\n' | '\r' | ' '))
            .collect();
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c)) {
            return Some(v);
        }
    }
    let clean = |s: Option<String>| s.map(|s| s.trim().replace(' ', ""));
    if let Some(serial) = clean(serial) {
        let placeholder = matches!(serial.as_str(), "" | "NotSpecified" | "None" | "Unknown" | "ToBeFilledByO.E.M.")
            || serial.starts_with("Default");
        if !placeholder {
            return Some(serial);
        }
    }
    clean(uuid).filter(|u| !u.is_empty())
}

/// Extents the records place only on slabs not in `have`: no leg left to
/// read them from (#259). Volumes with parity groups are left out — a lost
/// data leg there is reconstructed, and a missing drive is their ordinary
/// degraded state. Returns (volume name, extents) per volume affected.
fn stranded_extents(
    docs: &[Option<crate::volume::metadata::VolumeMetadata>],
    have: &std::collections::HashSet<crate::drive::slab::SlabId>,
) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for d in docs.iter().flatten() {
        for v in &d.volumes {
            if !v.parity.is_empty() {
                continue;
            }
            let n = v
                .extents
                .values()
                .filter(|loc| !loc.legs().any(|l| have.contains(&l.slab_id)))
                .count();
            if n > 0 {
                out.push((v.name.clone(), n));
            }
        }
    }
    out
}

/// Every slab a set of volume records places an extent on.
fn slabs_named(docs: &[Option<crate::volume::metadata::VolumeMetadata>]) -> std::collections::HashSet<crate::drive::slab::SlabId> {
    let mut ids = std::collections::HashSet::new();
    for d in docs.iter().flatten() {
        for v in &d.volumes {
            for loc in v.extents.values() {
                ids.extend(loc.legs().map(|l| l.slab_id));
            }
            for g in v.parity.values() {
                ids.extend(g.legs.iter().map(|l| l.slab_id));
            }
        }
    }
    ids
}

/// Open the slabs and restore — and when the local records place extents on
/// a slab that is not here, fetch it (#171).
///
/// That is a flow-over cut short: the power went while the goldens were still
/// moving from the appliance's clone onto this disk. The next boot claims a
/// *new* clone, so those extents were dropped, the root came up with holes,
/// and the node could not boot again. But every system golden is sealed and a
/// claim is a clone of it, and cloning restamps the disk's GPT, never the
/// slabs inside it: a fresh clone carries the same slabs, by id, holding the
/// same bytes. So claim one, attach its slabs for their data only (their
/// records are the image's and are never read), let the local records map
/// onto them, and hand the flow-over on to finish. Only `boot-local` asks
/// (`resume`), and only when an appliance is named (`STORMBLOCK_BOOTHOST`,
/// which the initramfs exports): a claim releases the machine's earlier
/// clones, which is not something a diagnostic may do.
async fn open_slabs_resuming(
    slab_paths: &[String],
    meta: Option<&str>,
    resume: bool,
) -> anyhow::Result<(VolumeManager, Option<Resumed>)> {
    use std::path::{Path, PathBuf};
    use crate::volume::MetadataStore;

    // 1. Open the slabs. A slab formatted by `image build` carries its own
    //    volumes.dat, so opening it is also how the metadata is found — an
    //    image has no filesystem to keep one in, and the "meta" directory
    //    beside `/dev/sda4` is `/dev/meta`, which is nothing (#62).
    for path in slab_paths {
        // A fabric URI is opened by attaching, not by statting a file — see the
        // scheme dispatch below. Only a local path is required to exist first:
        // FileDevice::open would create a missing path as an empty file and die
        // later with a misleading "bad slab magic", so name the real problem
        // (storage driver not loaded / wrong device) instead (#14).
        if !is_fabric_uri(path) && !Path::new(path).exists() {
            anyhow::bail!(
                "slab device {path} does not exist — storage driver not loaded or wrong path?"
            );
        }
    }
    let mut slabs = Vec::with_capacity(slab_paths.len());
    // The path each slab came from. One whole-disk path can yield several
    // slabs, so this is what the per-slab reporting below zips against —
    // `slab_paths` is no longer 1:1 with `slabs`.
    let mut slab_sources: Vec<String> = Vec::with_capacity(slab_paths.len());
    for path in slab_paths {
        // A slab is on a block device (O_DIRECT, #140), a namespace on the
        // fabric (NvmeofDevice), or — tests and development — a file. The diskless boot hands boot-local an
        // `nvme-tcp://` URI from the appliance claim; attaching it here is what
        // makes a remote root an ordinary slab, exactly as a local one.
        let dev: Arc<dyn BlockDevice> = if is_fabric_uri(path) {
            let spec = crate::drive::nvmeof_dev::NvmeTcpSpec::parse(path)
                .ok_or_else(|| anyhow::anyhow!("malformed nvme-tcp URI: {path}"))?;
            Arc::new(crate::drive::nvmeof_dev::NvmeofDevice::connect(&spec).await?)
        } else {
            let dev = open_storage(path).await?;
            if !crate::drive::is_block_device(path) {
                // Real storage is a block device, opened O_DIRECT (#140). A
                // slab in a regular file goes through the page cache: fine
                // for a test or a laptop, not for a node, and said so.
                println!(
                    "WARNING: {path} is a regular file, not a block device — a slab in a file is \
                     for tests and development only"
                );
                tracing::warn!("slab {path} is on a regular file (tests and development only)");
            }
            dev
        };
        match Slab::open(dev.clone()).await {
            Ok(s) => {
                slabs.push(s);
                slab_sources.push(path.clone());
            }
            // A whole disk, or a disk image, rather than the partition the
            // slab is in. Both are the ordinary thing to be handed — a disk
            // image is what `image build` produces and what someone copies off
            // a node — and requiring the offset to be worked out by hand is
            // how a debugging tool ends up unused. The table says where the
            // partitions are; try each one, and take them all: a system slab
            // and a data slab sit in the same GPT.
            Err(first) => {
                let found: Vec<Slab> = {
                    let discovered =
                        crate::drive::discover::slabs_in_partitions(&dev).await;
                    for f in &discovered {
                        let role = if f.slab.is_data() { "data slab" } else { "slab" };
                        println!("  {path}: {role} found in {}", f.label);
                    }
                    discovered.into_iter().map(|f| f.slab).collect()
                };
                if found.is_empty() {
                    return Err(anyhow::anyhow!("open slab {path}: {first}"));
                }
                for s in found {
                    slabs.push(s);
                    slab_sources.push(path.clone());
                }
            }
        }
    }

    // 2. Metadata: an explicit --meta wins, then each slab's own copy, then
    //    the "meta" directory beside the first slab.
    //
    //    **Each slab's own copy**, plural, because a node's mutable storage
    //    is a system slab and a data slab, and the second one's record has to
    //    survive the first being replaced by an install. A single merged copy
    //    living in one of them would recreate exactly the coupling the split
    //    exists to break (#88). A slab with no copy of its own is the older
    //    arrangement — one document naming every array, positionally — and
    //    still works.
    let meta_dir: PathBuf = match meta {
        Some(m) => PathBuf::from(m),
        None => Path::new(&slab_paths[0])
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("meta"),
    };
    let mut embedded: Vec<Option<crate::volume::metadata::VolumeMetadata>> =
        Vec::with_capacity(slabs.len());
    if meta.is_none() {
        for (path, slab) in slab_sources.iter().zip(&slabs) {
            let doc = match slab
                .read_metadata()
                .await
                .map_err(|e| anyhow::anyhow!("read slab metadata from {path}: {e}"))?
            {
                Some(bytes) => Some(MetadataStore::decode(&bytes)?),
                None => None,
            };
            embedded.push(doc);
        }
    } else {
        embedded.resize_with(slabs.len(), || None);
    }

    // Slabs the records need and that did not open here (#171).
    let mut fetched: Vec<(String, Slab)> = Vec::new();
    let mut resumed_uri: Option<String> = None;
    {
        let opened: std::collections::HashSet<_> = slabs.iter().map(|s| s.slab_id()).collect();
        let mut missing: std::collections::HashSet<_> =
            slabs_named(&embedded).into_iter().filter(|id| !opened.contains(id)).collect();
        if resume && !missing.is_empty() {
            let boothost = std::env::var("STORMBLOCK_BOOTHOST").ok().filter(|b| !b.trim().is_empty());
            // A test (or an operator by hand) can name the source directly.
            let given = std::env::var("STORMBLOCK_RESUME_SOURCE").ok().filter(|b| !b.trim().is_empty());
            let source: Option<String> = match (given, boothost, machine_tag()) {
                (Some(src), _, _) => {
                    println!(
                        "The local records place extents on {} slab(s) not on this machine - a \
                         flow-over cut short. Finishing it from {src}.",
                        missing.len()
                    );
                    Some(src)
                }
                (None, Some(boothost), Some(tag)) => {
                    println!(
                        "The local records place extents on {} slab(s) not on this machine - a \
                         flow-over cut short. Claiming a fresh clone of {tag}'s image from {boothost} \
                         to finish it.",
                        missing.len()
                    );
                    let mut uri = claim_boot_uri(&boothost, &tag, "boothost", 120, None).await?;
                    if std::env::var("STORMBLOCK_HOST_NQN").is_err() && !uri.contains("hostnqn=") {
                        uri.push_str(if uri.contains('?') { "&" } else { "?" });
                        uri.push_str(&format!("hostnqn=nqn.2026-09.lo.storm:host-{tag}"));
                    }
                    Some(uri)
                }
                _ => None,
            };
            let tried = source.clone();
            match source {
                Some(uri) => {
                    let dev: Arc<dyn BlockDevice> = if is_fabric_uri(&uri) {
                        let spec = crate::drive::nvmeof_dev::NvmeTcpSpec::parse(&uri)
                            .ok_or_else(|| anyhow::anyhow!("malformed nvme-tcp URI: {uri}"))?;
                        Arc::new(crate::drive::nvmeof_dev::NvmeofDevice::connect(&spec).await?)
                    } else {
                        open_storage(&uri).await?
                    };
                    let candidates: Vec<Slab> = match Slab::open(dev.clone()).await {
                        Ok(s) => vec![s],
                        Err(_) => crate::drive::discover::slabs_in_partitions(&dev)
                            .await
                            .into_iter()
                            .map(|f| f.slab)
                            .collect(),
                    };
                    for slab in candidates {
                        if missing.remove(&slab.slab_id()) {
                            println!("  {uri}: slab {} - the extents still to move", slab.slab_id().0);
                            fetched.push((uri.clone(), slab));
                        }
                    }
                    if !fetched.is_empty() {
                        resumed_uri = Some(uri);
                    }
                    if !missing.is_empty() {
                        println!(
                            "WARNING: {} slab(s) the records need are not in this machine's image either: {}",
                            missing.len(),
                            missing.iter().map(|m| m.0.to_string()).collect::<Vec<_>>().join(", ")
                        );
                    }
                }
                None => println!(
                    "WARNING: the local records place extents on {} slab(s) not on this machine \
                     (a flow-over cut short?), and no appliance or machine tag is known to fetch \
                     them from",
                    missing.len()
                ),
            }
            // Booting on with those mappings dropped is a root that reads
            // holes: PID 1 died of SIGSEGV on server3 (#259). Stop, and say
            // what is missing and from where it was looked for.
            let mut have = opened.clone();
            have.extend(fetched.iter().map(|(_, s)| s.slab_id()));
            let stranded = stranded_extents(&embedded, &have);
            if !stranded.is_empty() {
                let total: usize = stranded.iter().map(|(_, n)| n).sum();
                let names: Vec<String> =
                    stranded.iter().take(8).map(|(v, n)| format!("{v} ({n})")).collect();
                anyhow::bail!(
                    "refusing to boot: the local records place {total} extent(s) of {} volume(s) \
                     [{}{}] only on slab(s) {} - not on this machine, and not in the image claimed \
                     to finish the flow-over{}. That image is not the one this disk was laid from \
                     (the wrong machine name?), or the flow-over's source is gone (#259)",
                    stranded.len(),
                    names.join(", "),
                    if stranded.len() > 8 { ", ..." } else { "" },
                    missing.iter().map(|m| m.0.to_string()).collect::<Vec<_>>().join(", "),
                    match &tried {
                        Some(t) => format!(" ({t})"),
                        None => " (none was: no appliance or machine name known)".into(),
                    },
                );
            }
        }
    }

    let primary = embedded.iter().position(|d| d.is_some());
    let (extent_size, from_slabs, source) = match primary {
        Some(i) => {
            let carriers: Vec<&str> = slab_sources
                .iter()
                .zip(&embedded)
                .filter(|(_, d)| d.is_some())
                .map(|(p, _)| p.as_str())
                .collect();
            (
                embedded[i].as_ref().unwrap().extent_size,
                true,
                format!("slab(s) {}", carriers.join(", ")),
            )
        }
        None => {
            let store = MetadataStore::new(meta_dir.clone())?;
            if !store.exists() {
                anyhow::bail!(
                    "no volume metadata: none of the slab(s) {} carries any, and there is no volumes.dat in {}",
                    slab_paths.join(", "),
                    meta_dir.display()
                );
            }
            let doc = store.load()?;
            if doc.arrays.is_empty() {
                anyhow::bail!("metadata in {} records no arrays", meta_dir.display());
            }
            if slabs.len() > doc.arrays.len() {
                anyhow::bail!(
                    "{} slab(s) opened but metadata records only {} array(s)",
                    slabs.len(),
                    doc.arrays.len()
                );
            }
            let size = doc.extent_size;
            embedded[0] = Some(doc);
            (size, false, meta_dir.display().to_string())
        }
    };
    println!("Volume metadata from {source}");

    // Every document has to agree on the slot size: it is the unit the extent
    // maps are written in, and two slabs disagreeing about it is not something
    // to average out.
    for (path, doc) in slab_sources.iter().zip(&embedded) {
        if let Some(d) = doc {
            if d.extent_size != extent_size {
                anyhow::bail!(
                    "slab {path} records a {}-byte extent and slab {} records {extent_size}",
                    d.extent_size,
                    slab_sources[primary.unwrap_or(0)]
                );
            }
        }
    }

    // 3. Attach the slabs non-destructively (no reformat) and restore volumes.
    //    Runtime changes go back where the metadata came from.
    let mut mgr = if from_slabs {
        VolumeManager::new(extent_size)
    } else {
        VolumeManager::with_data_dir(extent_size, meta_dir.clone())?
    };
    // Array ids: a slab that describes itself names its own; one that does
    // not falls back to the positional pairing the single-document layout
    // used, taking the next unclaimed record.
    let fallback: Vec<RaidArrayId> = embedded[primary.unwrap_or(0)]
        .as_ref()
        .map(|d| d.arrays.iter().map(|a| a.array_id).collect())
        .unwrap_or_default();
    let mut claimed: Vec<RaidArrayId> = embedded
        .iter()
        .filter_map(|d| d.as_ref().and_then(|d| d.arrays.first()).map(|a| a.array_id))
        .collect();
    let mut metadata_slabs = Vec::new();
    for ((path, slab), doc) in slab_sources.iter().zip(slabs).zip(&embedded) {
        let array_id = match doc.as_ref().and_then(|d| d.arrays.first()) {
            Some(rec) => rec.array_id,
            None => match fallback.iter().find(|a| !claimed.contains(a)) {
                Some(next) => {
                    let next = *next;
                    claimed.push(next);
                    next
                }
                // An empty slab is a new array, not a pairing failure.
                //
                // The positional fallback exists for the old single-document
                // layout, where a slab's record lived in another slab's
                // document — so a slab with no metadata means "find its
                // record over there". A slab that was formatted a second ago
                // and has never held an extent has no record anywhere,
                // because there is nothing to record. Refusing it broke the
                // boot that laid one: `boot-local` formatted a local disk,
                // put it in the handover, and the engine that adopted the
                // devices died on
                //
                //   slab /dev/sda carries no metadata of its own and the
                //   record names no further array to pair it with
                //
                // leaving the node with a login prompt, no engine, and a
                // registry reporting "stormblockmk not ready after 120s".
                //
                // "Has a region of its own, and nothing in it" is the test.
                //
                // Not "no allocated slots": formatting a slab allocates some
                // — the system slab on this drive came up with two the
                // instant it was laid, its own reserved region — so counting
                // them called a brand-new slab occupied and failed the boot a
                // second time. And `read_metadata` returning `None` means the
                // region is empty rather than unreadable, because a region
                // that cannot be decoded is an error, not a `None`.
                //
                // A slab with no metadata region at all is the legacy layout
                // this fallback was written for, and still pairs positionally.
                None if slab.has_metadata_region() => {
                    let fresh = RaidArrayId(uuid::Uuid::new_v4());
                    tracing::info!(
                        "slab {path} is empty and names no array — opening it as a new one \
                         ({fresh})"
                    );
                    claimed.push(fresh);
                    fresh
                }
                None => anyhow::bail!(
                    "slab {path} keeps no metadata region of its own, holds {} allocated \
                     slot(s), and the record names no further array to pair it with — its \
                     extents belong to an array this boot cannot name",
                    slab.allocated_slots()
                ),
            },
        };
        let role = slab.role();
        if slab.has_metadata_region() {
            metadata_slabs.push(slab.slab_id());
        }
        mgr.attach_slab(array_id, slab)
            .await
            .map_err(|e| anyhow::anyhow!("attach slab {path}: {e}"))?;
        println!("Attached {role} slab {path} (array {array_id})");
    }
    if !metadata_slabs.is_empty() {
        mgr.persist_to_slabs(metadata_slabs);
    }
    // Fetched slabs are sources of data only: registered, never read for
    // records nor written with them.
    for (uri, slab) in fetched {
        let role = slab.role();
        mgr.add_slab(slab).await;
        println!("Attached {role} slab {uri} (to finish the flow-over)");
    }
    mgr.restore().await?;

    let resumed = match resumed_uri {
        None => None,
        Some(uri) => {
            let reg = mgr.registry().read().await;
            let local = |data: bool| {
                reg.iter()
                    .find(|(id, s)| s.is_data() == data && mgr.is_metadata_slab(id))
                    .map(|(id, _)| *id)
            };
            Some(Resumed { uri, system_slab: local(false), data_slab: local(true) })
        }
    };
    Ok((mgr, resumed))
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
/// Mount the `/serve/v1` surface over an engine that is already assembled.
///
/// Shared by the two ways this binary becomes a node's engine: the ordinary
/// serve path, and `adopt-ublk`, which takes the devices over from the
/// initramfs and then *is* the engine. Only the first one had it, so a node
/// that booted through a handover answered 404 to every call the registry
/// next door made — while its management API, one port along, was answering
/// perfectly. Layer 2 belongs to the engine, not to one of its entry points.
async fn start_serving(
    config: &crate::mgmt::config::StormBlockConfig,
    state: &Arc<AppState>,
    iscsi_bind: &str,
    nvmeof_bind: &str,
    reactor: &Arc<ReactorPool>,
) {
    match config.serve_config(iscsi_bind, nvmeof_bind) {
        Ok(serve_cfg) => {
            if let Err(e) = std::fs::create_dir_all(&serve_cfg.data_dir) {
                tracing::error!(
                    "not serving /serve/v1: cannot create {} ({e}) — the wiring table has to \
                     survive a restart",
                    serve_cfg.data_dir
                );
                return;
            }
            #[cfg(feature = "iscsi")]
            let shared_iscsi = state.iscsi_target.read().await.clone();

            let wiring = crate::serve::wiring::WiringTable::load(&serve_cfg.data_dir);
            let status = Arc::new(crate::serve::status::MkStatus::new());
            tracing::info!(
                "Serving /serve/v1 — advertising {}, portals {}..{}, state in {}",
                serve_cfg.advertise_addr,
                serve_cfg.portal_base,
                serve_cfg.portal_base.saturating_add(serve_cfg.portal_span),
                serve_cfg.data_dir,
            );
            let reconcile_secs = serve_cfg.reconcile_secs;
            let reap_secs = serve_cfg.reap_secs;
            let ctx = Arc::new(crate::serve::ctx::ServeContext::new(
                serve_cfg,
                state.clone(),
                status,
                #[cfg(feature = "iscsi")]
                shared_iscsi,
                reactor.clone(),
                wiring,
            ));
            // Tell the API how to serve a volume as a subsystem of its own.
            // The settings live in the serve config and the API cannot see it,
            // so publish them: a claim then hands out the address that names
            // the volume rather than a namespace number in a shared subsystem
            // (#98).
            *state.per_volume.write().await = Some(crate::mgmt::PerVolumeServing {
                nqn_prefix: ctx.cfg.nqn_prefix.clone(),
                portal_base: ctx.cfg.portal_base,
                portal_span: ctx.cfg.portal_span,
                reactor: reactor.clone(),
            });
            // Readiness reflects what this engine has actually done.
            //
            // These flags were set by the profile that owned the serving layer
            // before it was promoted into the engine (#60); the fields came
            // across and the code that set them did not. Nothing set them
            // afterwards, so every node reported "slab not open", "volume
            // metadata not restored" and "management API not listening" while
            // demonstrably doing all three — and a registry asking whether the
            // storage was ready was told no, forever.
            //
            // Both are true by construction here: `start_serving` is only
            // reached with a volume manager built over attached slabs, in
            // either of the two ways this binary becomes a node's engine.
            ctx.status.set(&ctx.status.slab_open, true);
            ctx.status.set(&ctx.status.volumes_restored, true);
            // The transport, in the sense this layer means it: portals are
            // bound per export from the range above rather than one listener
            // held open, so what readiness can say is that the node is able to
            // bind them. A portal that then fails to bind surfaces as that
            // export staying pending, which is where it belongs.
            ctx.status.set(&ctx.status.nvmeof_listening, true);

            if state.serve.set(ctx.clone()).is_err() {
                tracing::error!("serving context was already set — not starting a second one");
                return;
            }
            tokio::spawn(crate::serve::reconcile::run(ctx.clone()));
            tracing::debug!("export reconciler running every {reconcile_secs}s");
            if reap_secs > 0 {
                tokio::spawn(crate::serve::reap::run(ctx));
                tracing::debug!("template reaper running every {reap_secs}s");
            }
        }
        // Not an error: a node that is not meant to serve, or has nowhere to
        // keep the wiring table, is a legitimate configuration. But it is
        // never silent — a consumer getting 404s from /serve/v1 has to be able
        // to find out why from this node's log.
        Err(why) => tracing::warn!("not serving /serve/v1: {why}"),
    }
}

/// Every block device this node has, with identity, firmware and health.
///
/// From sysfs where sysfs knows, and from the drive itself where it does not:
/// NVMe endurance and temperature live in a SMART log page reached by an admin
/// command, not in a sysfs file, so a report built only from sysfs silently
/// omits the two numbers most worth having.
#[cfg(target_os = "linux")]
fn collect_devices() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let read = |p: String| -> String {
        std::fs::read_to_string(&p).map(|s| s.trim().to_owned()).unwrap_or_default()
    };

    let Ok(blocks) = std::fs::read_dir("/sys/block") else {
        return "cannot read /sys/block\n".into();
    };
    let mut names: Vec<String> = blocks
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // Virtual devices are this node's own doing and say nothing about its
        // media; ublk especially, since those are volumes we serve.
        .filter(|n| !n.starts_with("loop") && !n.starts_with("ram") && !n.starts_with("ublk"))
        .collect();
    names.sort();

    for n in names {
        let base = format!("/sys/block/{n}");
        let sectors: u64 = read(format!("{base}/size")).parse().unwrap_or(0);
        let bytes = sectors * 512;
        let rotational = read(format!("{base}/queue/rotational"));
        let model = {
            let m = read(format!("{base}/device/model"));
            if m.is_empty() { read(format!("{base}/device/name")) } else { m }
        };
        let _ = writeln!(
            out,
            "{n}: {} {}  {}  {}",
            read(format!("{base}/device/vendor")),
            model,
            crate::mgmt::config::human_size(bytes),
            if rotational == "1" { "rotational" } else { "solid state" },
        );
        for (label, path) in [
            ("serial", format!("{base}/device/serial")),
            ("firmware", format!("{base}/device/firmware_rev")),
            ("firmware", format!("{base}/device/rev")),
            ("wwid", format!("{base}/device/wwid")),
            ("queue depth", format!("{base}/device/queue_depth")),
            ("scheduler", format!("{base}/queue/scheduler")),
            ("logical block", format!("{base}/queue/logical_block_size")),
            ("physical block", format!("{base}/queue/physical_block_size")),
        ] {
            let v = read(path);
            if !v.is_empty() {
                let _ = writeln!(out, "    {label:<16} {v}");
            }
        }
        // Temperature, where the kernel exposes it without an admin command.
        for hw in ["device/hwmon", "device/device/hwmon"] {
            if let Ok(rd) = std::fs::read_dir(format!("{base}/{hw}")) {
                for e in rd.flatten() {
                    let t = read(format!("{}/temp1_input", e.path().display()));
                    if let Ok(milli) = t.parse::<i64>() {
                        let _ = writeln!(out, "    {:<16} {}°C", "temperature", milli / 1000);
                    }
                }
            }
        }
        if n.starts_with("nvme") {
            let ctrl = n.split('n').next().unwrap_or(&n).to_owned();
            let _ = write!(out, "{}", nvme_smart(&format!("/dev/{ctrl}")));
        }
        out.push('\n');
    }
    out
}

/// NVMe SMART / Health Information (log page 0x02), by admin passthrough.
///
/// The numbers here are the ones a sysfs-only report cannot have: endurance
/// used, spare remaining, media errors, unsafe shutdowns. A drive at 95% of
/// its endurance explains a class of behaviour that looks like a software
/// problem right up until someone reads this counter.
#[cfg(target_os = "linux")]
fn nvme_smart(dev: &str) -> String {
    use std::fmt::Write as _;
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    #[derive(Default)]
    struct AdminCmd {
        opcode: u8,
        flags: u8,
        rsvd1: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }
    // _IOWR('N', 0x41, struct nvme_admin_cmd), sizeof == 72.
    // libc's ioctl request type differs by target (c_ulong on glibc,
    // c_int on musl) and has changed across libc releases — keep the raw
    // value and cast at the call site.
    const NVME_IOCTL_ADMIN_CMD: u32 =
        (3u32 << 30) | (72u32 << 16) | ((b'N' as u32) << 8) | 0x41;

    let Ok(f) = std::fs::File::open(dev) else {
        return format!("    (no SMART: cannot open {dev})\n");
    };
    let mut buf = [0u8; 512];
    let mut cmd = AdminCmd {
        opcode: 0x02, // Get Log Page
        nsid: 0xffff_ffff,
        addr: buf.as_mut_ptr() as u64,
        data_len: buf.len() as u32,
        // Log id 0x02, number of dwords - 1 in the top half.
        cdw10: 0x02 | (((buf.len() / 4 - 1) as u32) << 16),
        ..Default::default()
    };
    // SAFETY: an ioctl on a file this process opened, with a buffer it owns.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
    if rc != 0 {
        return format!("    (no SMART from {dev}: {})\n", std::io::Error::last_os_error());
    }

    let u16le = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let u128le = |o: usize| {
        let mut v = [0u8; 16];
        v.copy_from_slice(&buf[o..o + 16]);
        u128::from_le_bytes(v)
    };
    let mut out = String::new();
    // Composite temperature is in kelvin.
    let kelvin = u16le(1);
    let _ = writeln!(out, "    {:<16} {}°C", "temperature", kelvin as i32 - 273);
    let _ = writeln!(out, "    {:<16} {}%", "spare left", buf[3]);
    let _ = writeln!(out, "    {:<16} {}% (endurance consumed)", "wear", buf[5]);
    let _ = writeln!(out, "    {:<16} {}", "critical warning", buf[0]);
    let _ = writeln!(out, "    {:<16} {}", "power-on hours", u128le(128));
    let _ = writeln!(out, "    {:<16} {}", "unsafe shutdowns", u128le(160));
    let _ = writeln!(out, "    {:<16} {}", "media errors", u128le(176));
    let _ = writeln!(out, "    {:<16} {}", "error log entries", u128le(192));
    out
}

#[cfg(not(target_os = "linux"))]
fn collect_devices() -> String {
    "device inventory is read from sysfs and NVMe admin commands, which are Linux-only\n".into()
}

/// `must-gather` — one directory holding everything needed to explain a node.
///
/// Modelled on `oc adm must-gather`, and for the same reason: the node with
/// the problem is rarely the node in front of you, and asking someone to run
/// eleven commands and paste the output loses the one that mattered. This
/// collects what the kernel saw, what the storage layer thinks it has, and the
/// contents of the log volumes, and puts them in one place.
///
/// **Read-only throughout.** A diagnostic that can change what it is
/// diagnosing is not one, so the volumes are mounted `ro` and released again.
#[cfg(target_os = "linux")]
async fn handle_must_gather(
    slab_paths: &[String],
    meta: Option<&str>,
    out: &str,
    extra_volumes: &[String],
    no_contents: bool,
    max_file_mb: u64,
) -> anyhow::Result<()> {
    use std::io::Write;

    let root = std::path::Path::new(out);
    std::fs::create_dir_all(root)?;
    let mut manifest = Vec::<String>::new();

    let write = |name: &str, body: &str| -> anyhow::Result<()> {
        let mut f = std::fs::File::create(root.join(name))?;
        f.write_all(body.as_bytes())?;
        Ok(())
    };
    // A command's output, or the reason there is none. An absent file would
    // leave the reader unable to tell "nothing to report" from "never ran".
    let run = |cmd: &str, args: &[&str]| -> String {
        match std::process::Command::new(cmd).args(args).output() {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                if !o.stderr.is_empty() {
                    s.push_str("\n--- stderr ---\n");
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                }
                s
            }
            Err(e) => format!("({cmd}: {e})\n"),
        }
    };

    // --- the node itself ---
    let mut node = String::new();
    node.push_str(&format!("stormblock {}\n", env!("CARGO_PKG_VERSION")));
    for (label, path) in [
        ("kernel", "/proc/version"),
        ("cmdline", "/proc/cmdline"),
        ("uptime", "/proc/uptime"),
        ("meminfo", "/proc/meminfo"),
        ("mounts", "/proc/mounts"),
        ("modules", "/proc/modules"),
        ("partitions", "/proc/partitions"),
    ] {
        node.push_str(&format!("\n=== {label} ({path}) ===\n"));
        node.push_str(&std::fs::read_to_string(path).unwrap_or_else(|e| format!("({e})\n")));
    }
    write("node.txt", &node)?;
    manifest.push("node.txt — kernel, command line, memory, mounts, modules".into());

    write("dmesg.txt", &run("dmesg", &["-T"]))?;
    manifest.push("dmesg.txt — the kernel's account of this boot".into());

    // --- ublk: the devices, who serves them, what state they are in ---
    let mut ublk = String::new();
    match crate::drive::ublk::devices() {
        Ok(ids) if ids.is_empty() => ublk.push_str("no ublk devices\n"),
        Ok(ids) => {
            for id in ids {
                let pid = crate::drive::ublk::server_pid(id)
                    .ok()
                    .flatten()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into());
                let state = crate::drive::ublk::dev_state(id)
                    .ok()
                    .flatten()
                    .map(|s| match s {
                        0 => "DEAD".to_string(),
                        1 => "LIVE".to_string(),
                        2 => "QUIESCED".to_string(),
                        other => format!("state {other}"),
                    })
                    .unwrap_or_else(|| "?".into());
                ublk.push_str(&format!("/dev/ublkb{id}  server {pid}  {state}\n"));
            }
        }
        Err(e) => ublk.push_str(&format!("(cannot enumerate: {e})\n")),
    }
    write("ublk.txt", &ublk)?;
    manifest.push("ublk.txt — exported devices, their servers and their state".into());

    // --- the node's configuration, as it actually is on disk ---
    //
    // Not as it was meant to be. Half the questions a bundle answers are
    // "what was this node configured to do", and the answer is a file someone
    // edited, a unit that was generated, or a default that was never
    // overridden — and which of those it is only shows in the file itself.
    {
        let dst = root.join("config");
        let mut n = 0;
        for dir in ["/etc/stormblock", "/etc/stormpump", "/etc/sbregistry", "/etc/registry"] {
            let src = std::path::Path::new(dir);
            if src.is_dir() {
                n += copy_tree(src, &dst.join(dir.trim_start_matches('/')), 1024 * 1024)
                    .unwrap_or(0);
            }
        }
        for f in ["/proc/cmdline", "/etc/fstab", "/etc/resolv.conf"] {
            let p = std::path::Path::new(f);
            if p.is_file() {
                let _ = std::fs::create_dir_all(&dst);
                if std::fs::copy(p, dst.join(f.trim_start_matches('/').replace('/', "_"))).is_ok() {
                    n += 1;
                }
            }
        }
        manifest.push(format!("config/ — {n} configuration file(s) as they are on this node"));
    }

    // --- the drives themselves: what they are, and how worn ---
    //
    // A storage node's most useful fact about itself is often the state of its
    // media. Model and firmware because a fault is frequently a firmware
    // revision rather than a drive; wear and temperature because a drive at
    // 95% endurance or 70°C explains a class of behaviour that looks like a
    // software problem right up until someone reads the counter.
    write("devices.txt", &collect_devices())?;
    manifest.push("devices.txt — every drive, its firmware, and its wear and temperature".into());

    // --- what the last crash left behind ---
    //
    // A panic that takes the kernel down cannot be logged by anything running
    // on it: the log service is gone with everything else, the file it was
    // writing may be short by whatever was still in the page cache, and the
    // network stack that would have carried it out is dead. What survives is
    // pstore — the kernel's own crash record, written to firmware-backed
    // storage on the way down and still there on the next boot.
    //
    // So this is the one part of a bundle that is about the *previous* boot,
    // and it is often the only account of the failure anyone will get.
    {
        let pstore = std::path::Path::new("/sys/fs/pstore");
        let mut found = 0;
        if pstore.is_dir() {
            let dst = root.join("crash");
            let _ = std::fs::create_dir_all(&dst);
            if let Ok(entries) = std::fs::read_dir(pstore) {
                for e in entries.flatten() {
                    if std::fs::copy(e.path(), dst.join(e.file_name())).is_ok() {
                        found += 1;
                    }
                }
            }
        }
        if found > 0 {
            manifest.push(format!(
                "crash/ — {found} record(s) the kernel wrote on its way down in a previous boot"
            ));
        } else {
            // Said explicitly, because "no crash directory" and "a crash with
            // nothing recorded" are very different findings and both look like
            // an absent directory.
            write(
                "crash.txt",
                if pstore.is_dir() {
                    "/sys/fs/pstore is mounted and empty: no crash record from a previous boot\n"
                } else {
                    "/sys/fs/pstore is not mounted: this kernel keeps no crash record, so a \
                     panic leaves nothing behind. Mount pstore to change that.\n"
                },
            )?;
            manifest.push("crash.txt — whether this node can record a kernel crash at all".into());
        }
    }

    // --- the handover record, which says what this node was serving ---
    let hpath = std::path::Path::new(crate::drive::handover::DEFAULT_PATH);
    if let Ok(body) = std::fs::read_to_string(hpath) {
        write("handover.json", &body)?;
        manifest.push("handover.json — slabs and volumes the boot handed over".into());
    }

    // --- the supervisor's logs, which are on tmpfs and die with the boot ---
    let logs_src = std::path::Path::new("/run/stormpump/logs");
    if logs_src.is_dir() {
        let dst = root.join("stormpump-logs");
        std::fs::create_dir_all(&dst)?;
        let mut n = 0;
        if let Ok(entries) = std::fs::read_dir(logs_src) {
            for e in entries.flatten() {
                if std::fs::copy(e.path(), dst.join(e.file_name())).is_ok() {
                    n += 1;
                }
            }
        }
        manifest.push(format!("stormpump-logs/ — {n} supervised workload log(s)"));
    }

    // --- the storage layer ---
    //
    // The slabs this node is serving, unless told otherwise. Reading them is
    // the point of the exercise: an inventory is what says whether the volume
    // someone is asking about exists at all.
    let slabs: Vec<String> = if !slab_paths.is_empty() {
        slab_paths.to_vec()
    } else {
        crate::drive::handover::Record::read(hpath)
            .map(|r| r.slabs)
            .unwrap_or_default()
    };

    if slabs.is_empty() {
        write("volumes.txt", "no slab given and none in the handover record\n")?;
        manifest.push("volumes.txt — (no slab to read)".into());
    } else {
        let mgr = open_slabs_and_restore(&slabs, meta).await?;
        let mut names = mgr.list_volumes().await;
        names.sort_by(|a, b| a.1.cmp(&b.1));

        let mut inv = format!("slabs: {}\n\n", slabs.join(", "));
        inv.push_str(&format!("{:<30} {:>10} {:>10}  {}\n", "volume", "size", "mapped", "id"));
        for (id, name, size, used) in &names {
            inv.push_str(&format!(
                "{:<30} {:>10} {:>10}  {id}\n",
                name,
                crate::mgmt::config::human_size(*size),
                crate::mgmt::config::human_size(*used),
            ));
        }
        write("volumes.txt", &inv)?;
        manifest.push(format!("volumes.txt — {} volume(s) in the slab", names.len()));

        if !no_contents {
            // Which volumes to copy out. The name is the only signal available
            // without opening every filesystem, and it is the one the node's
            // own convention already carries: a data container is where a
            // workload keeps what it would otherwise lose.
            let wanted: Vec<_> = names
                .iter()
                .filter(|(_, n, ..)| {
                    let l = n.to_lowercase();
                    (l.contains("log") || l.contains("data") || extra_volumes.contains(n))
                        && !l.ends_with(".golden")
                })
                .collect();

            let gathered = root.join("volumes");
            std::fs::create_dir_all(&gathered)?;
            let mut copied = 0usize;
            for (id, name, ..) in wanted {
                match gather_volume(&mgr, id, name, &gathered, max_file_mb).await {
                    Ok(n) => {
                        copied += 1;
                        manifest.push(format!("volumes/{name}/ — {n} file(s)"));
                    }
                    Err(e) => {
                        manifest.push(format!("volumes/{name}/ — not gathered: {e}"));
                    }
                }
            }
            println!("  gathered {copied} volume(s)");
        }
    }

    // The index. Someone opening this directory should not have to guess what
    // is in it or which file answers their question.
    let mut index = String::from("stormblock must-gather\n\n");
    for line in &manifest {
        index.push_str(&format!("  {line}\n"));
    }
    index.push_str("\nEverything here was read without writing to the node.\n");
    write("README.txt", &index)?;

    println!("{}", index);
    println!("bundle: {}", root.display());
    println!("  tar it with: tar czf must-gather.tar.gz -C {} .", root.display());
    Ok(())
}

/// Copy one volume's files into the bundle, read-only.
#[cfg(target_os = "linux")]
async fn gather_volume(
    mgr: &crate::volume::VolumeManager,
    id: &crate::volume::VolumeId,
    name: &str,
    into: &std::path::Path,
    max_file_mb: u64,
) -> anyhow::Result<usize> {
    use crate::drive::ublk::UblkServer;

    let dev = mgr
        .get_volume(id)
        .ok_or_else(|| anyhow::anyhow!("volume has no device"))?;
    let dev_id = crate::drive::ublk::devices()?
        .into_iter()
        .max()
        .map_or(0, |m| m + 1);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let thread = std::thread::Builder::new()
        .name(format!("gather-{dev_id}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let _ = rt.block_on(UblkServer::new(dev).with_dev_id(dev_id).run(rx));
        })?;

    let released = |tx: tokio::sync::watch::Sender<bool>, t: std::thread::JoinHandle<()>| {
        let _ = tx.send(true);
        let _ = t.join();
    };

    let ids = [dev_id];
    let pending = tokio::task::spawn_blocking(move || {
        crate::drive::ublk::wait_live(&ids, std::time::Duration::from_secs(30))
    })
    .await??;
    if !pending.is_empty() {
        released(tx, thread);
        anyhow::bail!("/dev/ublkb{dev_id} never came up");
    }

    let mnt = std::path::Path::new("/run/stormblock/gather").join(name);
    std::fs::create_dir_all(&mnt)?;
    let fs = match mount_volume(&format!("/dev/ublkb{dev_id}"), &mnt, true) {
        Ok(fs) => fs,
        Err(e) => {
            released(tx, thread);
            return Err(e);
        }
    };
    let _ = fs;

    let dst = into.join(name);
    let n = copy_tree(&mnt, &dst, max_file_mb * 1024 * 1024).unwrap_or(0);

    let c = std::ffi::CString::new(mnt.to_string_lossy().as_ref())?;
    // SAFETY: unmounting a path this process just mounted.
    unsafe { libc::umount(c.as_ptr()) };
    released(tx, thread);
    Ok(n)
}

/// Copy a directory tree, skipping anything too big to be worth sending.
///
/// A must-gather that fills the disk it is written to has made the problem
/// worse, so the cap is real and what it skipped is recorded in place of the
/// file — the reader needs to know a log was there and was too large, which is
/// itself a fact about the node.
#[cfg(target_os = "linux")]
fn copy_tree(from: &std::path::Path, to: &std::path::Path, max_bytes: u64) -> std::io::Result<usize> {
    use std::io::Write;
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for entry in std::fs::read_dir(from)?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if name == "lost+found" {
                continue;
            }
            n += copy_tree(&path, &to.join(&name), max_bytes)?;
        } else if meta.is_file() {
            if meta.len() > max_bytes {
                let mut f = std::fs::File::create(to.join(format!(
                    "{}.skipped",
                    name.to_string_lossy()
                )))?;
                writeln!(f, "{} bytes — over the must-gather limit", meta.len())?;
                continue;
            }
            if std::fs::copy(&path, to.join(&name)).is_ok() {
                n += 1;
            }
        }
    }
    Ok(n)
}

#[cfg(not(target_os = "linux"))]
async fn handle_must_gather(
    _slab_paths: &[String],
    _meta: Option<&str>,
    _out: &str,
    _extra_volumes: &[String],
    _no_contents: bool,
    _max_file_mb: u64,
) -> anyhow::Result<()> {
    anyhow::bail!("must-gather reads volumes through ublk, which is Linux-only")
}

/// `golden` — a filesystem image from a tar, without a mount.
///
/// This is the build step every node image needs, and it has been done with
/// `mkfs.ext4`, a loop mount, `tar -x` and root. All three requirements come
/// from using the kernel to write the filesystem; none of them are necessary,
/// because the ext4 writer here can do it directly — which is exactly how the
/// registry lays a container image's layers into a volume.
async fn handle_golden(
    out: &str,
    size: &str,
    label: Option<&str>,
    tars: &[String],
    whiteouts: bool,
    fsck: bool,
    read_only: bool,
) -> anyhow::Result<()> {
    use crate::fs::ext4::{Ext4Params, FsProfile};

    let bytes = crate::mgmt::config::parse_size(size)
        .map_err(|e| anyhow::anyhow!("--size {size}: {e}"))?;
    let name = label
        .map(|l| l.to_string())
        .or_else(|| {
            std::path::Path::new(out)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "golden".into());

    // A block device is written in place; a file is made fresh.
    //
    // In place because the point of naming a device here is that the golden
    // *is* the volume — attached from the appliance over NVMe/TCP, written
    // once, sealed. A golden that has to be built as a file and then copied
    // into a volume is a second full copy of every byte, and the copy is the
    // thing worth removing: a disk is a map over goldens, so the goldens have
    // to be volumes to be mapped.
    //
    // For a file, unlink first: a golden built over the remains of an older
    // one inherits whatever that one had past the new end.
    let on_device = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(out).map(|m| m.file_type().is_block_device()).unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    let dev: Arc<dyn BlockDevice> = if on_device {
        let dev = open_storage(out).await?;
        let have = dev.capacity_bytes();
        if have < bytes {
            anyhow::bail!(
                "{out} is {have} bytes and --size asks for {bytes}: a golden cannot be \
                 larger than the volume it is written into"
            );
        }
        if have > bytes {
            // Not an error: a volume is often rounded up to a slot boundary.
            // The filesystem is made at --size and the rest is left alone.
            println!("  {name}: {out} is {have} bytes, formatting {bytes}");
        }
        dev
    } else {
        let _ = std::fs::remove_file(out);
        Arc::new(crate::drive::filedev::FileDevice::open_with_capacity(out, bytes).await?)
    };

    let params = Ext4Params {
        profile: FsProfile::Ext4,
        label: name.clone(),
        uuid: uuid::Uuid::new_v4(),
        // Nothing will write to this, so it carries neither the machinery for
        // surviving a write nor the space set aside for recovering from a full
        // filesystem. Both would be inherited by every clone on every node.
        journal: if read_only { Some(false) } else { None },
        reserved_percent: if read_only { 0.0 } else { 5.0 },
        ..Default::default()
    };
    let report = crate::fs::ext4::format(&dev, &params).await?;
    println!(
        "  {name}: {} blocks of {} bytes, {} inodes",
        report.blocks, report.block_size, report.inodes
    );

    let mut files = 0u64;
    for t in tars {
        let src: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if t == "-" {
            Box::new(tokio::io::stdin())
        } else {
            Box::new(tokio::fs::File::open(t).await?)
        };
        // Sniffed from the content, so a caller can hand over .tar or .tar.gz
        // — or a pipe, where there is no name to go on — without saying which.
        let comp = crate::serve::tarfs::parse_compression(None)
            .map_err(|e| anyhow::anyhow!("{t}: {e}"))?;
        let r =
            crate::serve::tarfs::unpack(&dev, src, "/", comp, whiteouts).await?;
        let n = r.files + r.directories + r.symlinks + r.hard_links + r.devices;
        println!(
            "  {name}: {} file(s), {} dir(s), {} link(s) from {t}",
            r.files, r.directories, r.symlinks + r.hard_links
        );
        files += n as u64;
    }

    if fsck {
        let check = crate::fs::ext4::check(&dev).await?;
        if !check.is_clean() {
            anyhow::bail!(
                "{name} does not check out after {files} entries — {} problem(s); \
                 not shipping a golden every clone would inherit",
                check.problems.len()
            );
        }
        println!("  {name}: checks out");
    }

    dev.flush().await?;
    println!(
        "built: {out} ({}, {files} entries)",
        crate::mgmt::config::human_size(bytes)
    );
    Ok(())
}

/// `attach` — open a slab and export, or list, what is in it.
///
/// Everything a node does with its storage happens through a volume it has
/// already opened, which is fine until the node will not boot. Then the disk
/// is a slab full of volumes and there is nothing that can look inside one:
/// not `mount`, which sees an extent store rather than a filesystem, and not
/// the engine, which only opens the volumes its own configuration names. This
/// is the door — the same code paths the boot uses, pointed anywhere.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    slab_paths: &[String],
    meta: Option<&str>,
    volumes: &[String],
    all: bool,
    mount_at: Option<&str>,
    read_only: bool,
    force: bool,
) -> anyhow::Result<()> {
    use crate::drive::ublk::UblkServer;

    let mgr = open_slabs_and_restore(slab_paths, meta).await?;

    // Listing and attaching are the same command, because when a node will not
    // boot the first question is what is on the disk at all, and having to
    // know a volume's name before being allowed to ask is the wrong way round.
    let mut names = mgr.list_volumes().await;
    names.sort_by(|a, b| a.1.cmp(&b.1));

    if volumes.is_empty() && !all {
        println!("{} volume(s) in {}:", names.len(), slab_paths.join(", "));
        for (id, name, size, used) in &names {
            println!(
                "  {:<28} {:>10} {:>10} mapped  {id}",
                name,
                crate::mgmt::config::human_size(*size),
                crate::mgmt::config::human_size(*used),
            );
        }
        println!("\nAttach one with --volume <name>, or all of them with --all.");
        return Ok(());
    }

    let wanted: Vec<(crate::volume::VolumeId, String)> = if all {
        names.iter().map(|(id, n, ..)| (*id, n.clone())).collect()
    } else {
        let mut v = Vec::new();
        for sel in volumes {
            let id = resolve_boot_volume(&mgr, sel).await?;
            let name = names
                .iter()
                .find(|(i, ..)| *i == id)
                .map(|(_, n, ..)| n.clone())
                .unwrap_or_else(|| sel.clone());
            v.push((id, name));
        }
        v
    };

    // Whoever is already serving this volume is still serving it. Two writers
    // on one volume corrupt it, and the corruption is silent — each believes
    // its own copy-on-write mapping — so the check is on by default and the
    // override has to be typed.
    if !force && !read_only {
        let live = crate::drive::ublk::devices()?;
        let mut busy = Vec::new();
        for id in &live {
            if let Some(pid) = crate::drive::ublk::server_pid(*id)? {
                if pid > 0 && pid != std::process::id() as i32 {
                    busy.push(format!("/dev/ublkb{id} (server {pid})"));
                }
            }
        }
        if !busy.is_empty() {
            anyhow::bail!(
                "this node is already serving {} — attaching writable would put two \
                 writers on one volume, which corrupts it silently. Use --ro to look, \
                 or --force if you know the other server is not touching what you want.",
                busy.join(", ")
            );
        }
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut threads = Vec::new();
    let mut attached: Vec<(u32, String)> = Vec::new();
    let base = crate::drive::ublk::devices()?.into_iter().max().map_or(0, |m| m + 1);

    for (i, (id, name)) in wanted.iter().enumerate() {
        let Some(dev) = mgr.get_volume(id) else {
            eprintln!("  {name}: no such volume");
            continue;
        };
        let dev_id = base + i as u32;
        let rx = shutdown_rx.clone();
        let label = name.clone();
        let thread = std::thread::Builder::new()
            .name(format!("ublk-attach-{dev_id}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                let server = UblkServer::new(dev).with_dev_id(dev_id);
                if let Err(e) = rt.block_on(server.run(rx)) {
                    tracing::error!("attach {label} on /dev/ublkb{dev_id}: {e}");
                }
            })?;
        threads.push(thread);
        attached.push((dev_id, name.clone()));
    }

    // Give the devices a moment to appear before anything tries to mount one.
    let ids: Vec<u32> = attached.iter().map(|(d, _)| *d).collect();
    let _ = tokio::task::spawn_blocking({
        let ids = ids.clone();
        move || crate::drive::ublk::wait_live(&ids, std::time::Duration::from_secs(30))
    })
    .await?;

    let mut mounted: Vec<String> = Vec::new();
    for (dev_id, name) in &attached {
        let path = format!("/dev/ublkb{dev_id}");
        match mount_at {
            None => println!("  {name:<28} {path}"),
            Some(dir) => {
                let target = std::path::Path::new(dir).join(name);
                std::fs::create_dir_all(&target)?;
                match mount_volume(&path, &target, read_only) {
                    Ok(fs) => {
                        println!(
                            "  {name:<28} {path} -> {} ({fs}{})",
                            target.display(),
                            if read_only { ", ro" } else { "" }
                        );
                        mounted.push(target.to_string_lossy().into_owned());
                    }
                    // Not fatal, and worth being precise about: a volume that
                    // holds no filesystem is a perfectly good thing to attach,
                    // and the block device is still there to look at.
                    Err(e) => println!("  {name:<28} {path} (not mounted: {e})"),
                }
            }
        }
    }

    println!("\nAttached {}. Ctrl+C to release.", attached.len());
    tokio::signal::ctrl_c().await?;

    for m in mounted.iter().rev() {
        let c = std::ffi::CString::new(m.as_str()).unwrap_or_default();
        // SAFETY: unmounting a path this process mounted.
        if unsafe { libc::umount(c.as_ptr()) } != 0 {
            eprintln!("could not unmount {m}: {}", std::io::Error::last_os_error());
        }
    }
    let _ = shutdown_tx.send(true);
    let stuck = join_ublk_threads(threads, std::time::Duration::from_secs(10));
    if stuck > 0 {
        eprintln!("WARNING: {stuck} ublk export(s) did not finish their teardown");
    }
    Ok(())
}

/// Mount a block device without being told what is on it.
///
/// There is no `blkid` here and no reason to need one: the kernel refuses a
/// filesystem it does not recognise, so trying the handful this node can
/// produce and reporting which one worked is both the probe and the mount.
#[cfg(target_os = "linux")]
fn mount_volume(
    dev: &str,
    target: &std::path::Path,
    read_only: bool,
) -> anyhow::Result<&'static str> {
    let src = std::ffi::CString::new(dev)?;
    let dst = std::ffi::CString::new(target.to_string_lossy().as_ref())?;
    let flags = if read_only { libc::MS_RDONLY } else { 0 };
    let mut last = 0;
    for fs in ["ext4", "erofs", "vfat", "xfs", "ext2"] {
        let t = std::ffi::CString::new(fs)?;
        // SAFETY: all four pointers are valid NUL-terminated strings.
        let rc = unsafe {
            libc::mount(src.as_ptr(), dst.as_ptr(), t.as_ptr(), flags, std::ptr::null())
        };
        if rc == 0 {
            return Ok(fs);
        }
        last = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    }
    anyhow::bail!("no filesystem the kernel recognises ({})", std::io::Error::from_raw_os_error(last))
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    _slab_paths: &[String],
    _meta: Option<&str>,
    _volumes: &[String],
    _all: bool,
    _mount_at: Option<&str>,
    _read_only: bool,
    _force: bool,
) -> anyhow::Result<()> {
    anyhow::bail!("attach exports volumes through ublk, which is Linux-only")
}

/// Whether [`seed_data_half`] runs without being asked.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeedWhen {
    /// The data slab was laid this boot and its records are routed to it.
    Always,
    /// Only with `STORMBLOCK_SEED_DATA` set.
    Asked,
}

/// Take the local disk for this boot: lay it (or update its system half),
/// register its slabs with `mgr` and seed the data half (#118). `boot-local`
/// calls it before exporting anything; a test drives it the same way (#239).
///
/// `Ok(None)`: the disk already holds everything this boot would copy.
#[cfg(target_os = "linux")]
pub(crate) async fn take_local_disk(
    mgr: &mut crate::volume::VolumeManager,
    disk: &str,
    local_tier: &str,
    local_disk_force: bool,
) -> anyhow::Result<Option<crate::drive::handover::FlowOver>> {
    let tier = parse_tier(local_tier).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dest_dev: Arc<dyn BlockDevice> =
        open_storage(disk).await?;
    let mut layout =
        crate::image::local::LocalLayout::for_drive(dest_dev.capacity_bytes());
    layout.slot_size = mgr.slot_size();
    layout.tier = tier;
    // The table in the drive's own sector size: firmware parses a GPT
    // in the medium's block size, and `FileDevice` reports 4096 for
    // every drive (#123). A file has none, and follows the device.
    layout.lba = crate::drive::filedev::logical_sector_size(disk);

    // **A drive that is already this node's is updated, not replaced.**
    //
    // A reinstall is "boot a fresh image and flow over onto the disk
    // the last install used", and that disk carries two things: the
    // goldens, which this boot exists to replace, and the data slab,
    // which holds the node's CA key and its ServiceAccount signing key
    // and cannot be made again. Laying a fresh table destroys the
    // second to refresh the first; refusing the drive leaves the node
    // running from the appliance for the rest of its life. Neither is
    // an install.
    //
    // The partition types say which half is which — that is what they
    // are for (#88) — so the system half is formatted afresh, the data
    // half is opened and left alone, and the node boots normally with
    // the identity it already had. No force, because nothing is
    // destroyed that an install is not meant to destroy.
    if !local_disk_force
        && crate::image::local::node_layout(&dest_dev).await?.is_some()
    {
        // **And if it is already up to date, do nothing at all.**
        //
        // A node that netboots regularly would otherwise reformat its
        // own system half and re-copy every golden on every boot —
        // destroying a working local half to rebuild the same bytes,
        // and running from the appliance for the minutes that takes,
        // each time.
        //
        // By volume id, which is what a migration preserves: the
        // flow-over moves a volume's extents, it does not make a new
        // volume, so a system half that has already had this image
        // flowed onto it holds the same ids. A new build makes new
        // volumes and this comes out false, which is what should
        // happen.
        //
        // Conservative in the direction that costs least: a wrong
        // "not up to date" reformats and re-copies, which is wasteful;
        // a wrong "up to date" leaves the node booting from the
        // appliance. Neither loses anything, and only a superset
        // counts as up to date.
        let want: std::collections::HashSet<uuid::Uuid> = mgr
            .list_volumes()
            .await
            .into_iter()
            .map(|(id, ..)| id.0)
            .collect();
        let have = crate::image::local::system_slab_volumes(&dest_dev)
            .await
            .unwrap_or(None);
        // And only a disk that can boot on its own counts: one laid
        // before local boot existed holds every golden and has no
        // boot area, and the shortcut would leave it that way (#123).
        let boot_ready =
            crate::image::local::boot_ready(&dest_dev, &layout).await;
        if !boot_ready {
            println!(
                "Flow-over: {disk} has no room to boot on its own (no boot area, or a \
                 table firmware cannot read) — laying the system half again"
            );
        }
        if let Some(have) = have.filter(|_| boot_ready) {
            if !want.is_empty() && want.iter().all(|id| have.contains(id)) {
                println!(
                    "Flow-over: {disk} already holds all {} volume(s) this boot would \
                     copy — nothing to do",
                    want.len()
                );
                println!(
                    "Flow-over: leaving it as it stands; the node boots from it next \
                     time, which is what the local-slab probe is for."
                );
                return Ok(None);
            }
            let missing = want.iter().filter(|id| !have.contains(id)).count();
            println!(
                "Flow-over: {disk} holds {} of the {} volume(s) this boot carries; {} \
                 to copy",
                want.len() - missing,
                want.len(),
                missing
            );
        }
        println!(
            "Flow-over: {disk} is already this node's — replacing the system half, \
             keeping the data half"
        );
        let laid = crate::image::local::update_system_slab(dest_dev, &layout)
            .await
            .map_err(|e| anyhow::anyhow!("updating the system slab on {disk}: {e}"))?;
        let data_id = laid.data.slab_id();
        let system_id = laid.system.slab_id();
        println!(
            "Flow-over: {disk} updated — data slab {data_id} kept ({}), system slab \
             {system_id} replaced ({})",
            crate::mgmt::config::human_size(laid.data_bytes),
            crate::mgmt::config::human_size(laid.system_bytes),
        );
        let flow = crate::drive::handover::FlowOver {
            disk: disk.to_string(),
            system_slab: system_id.0.to_string(),
            data_slab: data_id.0.to_string(),
        };
        {
            let mut reg = mgr.registry().write().await;
            reg.add(laid.data);
            reg.add(laid.system);
        }
        // Not seeded by default. The data slab kept here holds this
        // node's records, and adopting them over the fresh clone's
        // volumes of the same names is the upgrade path, which is not
        // built yet. Seeding without it would move extents onto the
        // drive and record them nowhere.
        seed_data_half(&mgr, data_id, disk, SeedWhen::Asked).await?;
        println!(
            "Flow-over: {disk} is laid out and handed to the engine that adopts this boot"
        );
        return Ok(Some(flow));
    }

    // The target is about to be formatted. An operator supplies a path,
    // and a path proves nothing about what is on the device — so ask the
    // device (#88). A reinstall is exactly "boot a fresh image and flow
    // over onto the disk the previous install was on", and that disk is
    // where this node's CA and its ServiceAccount signing key live.
    if let Some(what) = data_slab_on(disk).await? {
        // The override exists because the guard cannot tell a live
        // identity from a dead one.
        //
        // A drive carrying a data slab from an install that was
        // abandoned — interrupted mid-migration, corrupted, replaced
        // — looks exactly like a drive carrying the identity of a
        // node that is running. The guard refuses both, forever, and
        // no sequence of boots recovers the drive: zeroing a header
        // is not something a node does to itself, and every policy
        // the survey offers is still refused right here.
        //
        // `--local-disk-force` is that sequence, and it is
        // deliberately not a policy. `assimilate=any` is a statement
        // about a fleet; this is a statement about one drive that
        // somebody has looked at. It names what it destroys first.
        if !local_disk_force {
            anyhow::bail!(
                "refusing to format {disk} for flow-over: {what}. That partition holds \
                 this node's identity — its CA key and its ServiceAccount signing key — \
                 and nothing can mint it again. Point --local-disk at the system \
                 partition, at a drive that carries no data slab, or pass \
                 --local-disk-force if that identity is spent and you mean to destroy it"
            );
        }
        println!("Flow-over: {what} — destroying it, as --local-disk-force was given.");
        tracing::warn!("flow-over: --local-disk-force overrides the identity guard: {what}");
    }
    // Both halves, each onto a slab of its own role.
    //
    // This formatted the whole device as one slab — which takes
    // `SlabRole`'s default, System — and then drained only the non-data
    // slabs onto it. So the goldens came local and the *writes* did not:
    // every log line, every claim and every byte of `stormcos-state`
    // still landed in a clone on the appliance, for the life of the node.
    // One node can afford that. Twenty write to one appliance.
    //
    // The layout is the image's own, for the image's own reason: an
    // install replaces the system end and leaves the data end alone, and
    // the two are told apart from the partition table (#88).
    let laid = crate::image::local::lay_node_slabs(dest_dev, &layout)
        .await
        .map_err(|e| anyhow::anyhow!("laying slabs on {disk}: {e}"))?;
    let data_id = laid.data.slab_id();
    let system_id = laid.system.slab_id();
    println!(
        "Flow-over: {disk} laid out — data slab {data_id} ({}), system slab {system_id} ({})",
        crate::mgmt::config::human_size(laid.data_bytes),
        crate::mgmt::config::human_size(laid.system_bytes),
    );

    // The system half only, and not from here. **Do not migrate a
    // live data slab, and do not migrate anything from a process
    // that is about to be killed.**
    //
    // Migrating the data slab was tried on hardware and it corrupts:
    // the flow-over moves extents out from under mounted, actively
    // written filesystems, and the data slab is exactly the half
    // being written — logs, state, claims. Within a minute the node
    // reported
    //
    //   EXT4-fs error (device ublkb26): __ext4_find_entry:
    //       checksumming directory block 0
    //   capturing state: no ext2/3/4 superblock found (magic 0x0000)
    //
    // and stormdrive was in a restart loop. Goldens survive it
    // because nothing writes to them; a data volume does not.
    //
    // Migrating the *system* half from here is safe and still wrong,
    // because this process does not live long enough to finish. It is
    // the initramfs engine: twenty-six seconds after it laid these
    // slabs the successor adopted its ublk devices, and `switch_root`
    // had already deleted the filesystem its binary came from. The
    // copy is minutes. Every run of it was killed part-way, leaving a
    // slab that is real, incomplete and unable to boot the node —
    // which is precisely the shape the local-slab probe now has to
    // reject on the next boot.
    //
    // So the long-lived process does the long-running job. This lays
    // the structure, which is fast and bounded, and writes down what
    // it laid; the engine that adopts the devices moves the extents
    // at its leisure and is still there when they land.
    let flow = crate::drive::handover::FlowOver {
        disk: disk.to_string(),
        system_slab: system_id.0.to_string(),
        data_slab: data_id.0.to_string(),
    };
    {
        let mut reg = mgr.registry().write().await;
        reg.add(laid.data);
        reg.add(laid.system);
    }
    // **The records go where the extents go.** The slabs just laid
    // keep metadata of their own, and this manager was only ever
    // writing into the slabs it opened, which are the appliance
    // clone's. So a seeded data half had its extents on the drive and
    // its records on a clone the next boot never attaches, and the
    // engine that adopted the boot died on
    //
    //   Error: volume 'stormcert-data' not found in slab metadata
    //
    // Safe here, and only here: both slabs were formatted a moment
    // ago and hold nothing a persist could overwrite. The update path
    // above keeps a data slab that holds this node's records, and
    // writing this manager's view of it would replace them.
    mgr.keep_metadata_in_first(&[data_id, system_id]);
    seed_data_half(&mgr, data_id, disk, SeedWhen::Always).await?;
    println!(
        "Flow-over: {disk} is laid out and handed to the engine that adopts this boot"
    );
    Ok(Some(flow))
}

/// Put the writable half on the local disk, **now**, before anything is
/// exported.
///
/// The flow-over moves the goldens and deliberately does not move the data
/// slab, because migrating a slab while a filesystem on it is being written
/// corrupts it — tried on hardware, and within a minute the node reported
/// `EXT4-fs error (device ublkb26): __ext4_find_entry: checksumming directory
/// block 0` with its state store's superblock gone. That reasoning is sound
/// and it left a hole: the data half was then never populated at all, so a
/// drive that had flowed over held `stormpump` and every golden and none of
/// `stormcert-data`, `stormcos-state`, `registry-data` or the logs. The node
/// attached its own disk, restored 75 volumes, dropped 5712 extent mappings
/// pointing into the appliance's slabs, and died on
///
/// ```text
/// Error: volume 'stormcert-data' not found in slab metadata
/// ```
///
/// The window where copying it *is* safe is this one. `boot-local` has
/// attached the slabs and resolved the volumes, and it has not exported a
/// single ublk device yet — so nothing is mounted, no filesystem is open, and
/// not one byte has been written to any of these volumes this boot. It is the
/// same argument the migration comment already makes for where the writable
/// volumes belong; this is that place.
///
/// Synchronous on purpose. It is the difference between a node that boots from
/// its own disk next time and one that asks the appliance forever, and it is
/// bounded — the data half is logs and state, not goldens.
///
/// **Only into an empty data half.** A data slab that holds volumes holds this
/// node's identity, and copying over it would destroy a CA key that cannot be
/// minted again. Empty is the whole test, and it is asked of the drive rather
/// than assumed from which code path got here.
#[cfg(target_os = "linux")]
async fn seed_data_half(
    mgr: &crate::volume::VolumeManager,
    dest: crate::drive::slab::SlabId,
    disk: &str,
    when: SeedWhen,
) -> anyhow::Result<()> {
    // Its own handle on the drive. The one the caller had was consumed laying
    // the slabs, and reading a partition table is cheap next to what follows.
    let dev: Arc<dyn BlockDevice> =
        open_storage(disk).await?;
    // **On for a data half laid this boot, and asked-for otherwise.**
    //
    // It was off everywhere, because the records did not survive: the manager
    // persisted its map only to the metadata slabs it chose when it opened,
    // which are the appliance's, and the local slabs were registered
    // afterwards. So the engine that adopted the boot opened the drive,
    // restored 68 volumes, and every data volume was missing:
    //
    //   Error: volume 'stormcert-data' not found in slab metadata
    //     (have: ... every golden and every *-logs, and none of the rest)
    //
    // The fresh-lay path now names the local slabs as metadata slabs, first,
    // before calling this, so the records land beside the extents (#118).
    // Leaving the data half on the appliance is what made every write to a
    // -data volume vanish at the next boot, because the appliance side is a
    // clone that the next boot claims afresh.
    //
    // A data slab kept from an earlier install is different. It holds this
    // node's records, and seeding into it before those are adopted over the
    // fresh clone's volumes of the same names would move extents that nothing
    // records. That is the upgrade path, and until it exists it is `Asked`.
    let asked = std::env::var("STORMBLOCK_SEED_DATA").is_ok();
    let refused = std::env::var("STORMBLOCK_NO_SEED_DATA").is_ok();
    if refused || (when == SeedWhen::Asked && !asked) {
        println!(
            "Flow-over: leaving the data half where it is — writes stay on the appliance{}",
            if refused { " (STORMBLOCK_NO_SEED_DATA)" } else { "" }
        );
        return Ok(());
    }

    // Per volume, not per slab.
    //
    // "Empty, or leave it alone" was too blunt by exactly one case, and it is
    // the case this node was in. The local data half is registered with the
    // engine from the boot that laid it, so ordinary allocation put *some*
    // volumes on it — twenty of them — while the ones the command line mounts
    // stayed on the appliance. A slab-wide test called that occupied and
    // skipped it, and the probe went on refusing the drive for seven missing
    // volumes, boot after boot, with the fix sitting behind a guard that would
    // never open.
    //
    // A volume already on this slab is this node's and is not touched. A
    // volume that is not here cannot be overwritten by being copied here,
    // because there is nothing of it here to overwrite. That is the whole
    // safety argument, and it holds per volume, which is the granularity the
    // danger actually has.
    let have = match crate::image::local::data_slab_volumes(&dev).await {
        Ok(Some(have)) => have,
        // Cannot say. An unanswerable question about identity is answered by
        // doing nothing: the node runs its writes on the appliance, which is
        // slower and is not destructive.
        Ok(None) | Err(_) => {
            println!(
                "Flow-over: cannot read the data half of {disk} - leaving it alone; \
                 writes stay on the appliance"
            );
            return Ok(());
        }
    };
    if !have.is_empty() {
        println!(
            "Flow-over: the data half of {disk} already holds {} volume(s); those stay as they are",
            have.len()
        );
    }

    let sources: Vec<crate::drive::slab::SlabId> = {
        let reg = mgr.registry().read().await;
        reg.iter()
            .filter(|(id, s)| s.is_data() && **id != dest)
            .map(|(id, _)| *id)
            .collect()
    };
    if sources.is_empty() {
        println!("Flow-over: no data slab to copy from; writes stay where they are");
        return Ok(());
    }

    // The work is decided before any of it is done.
    //
    // The old loop asked the map for "an extent still on the source" and
    // repeated until there were none, which cannot express "all but these".
    // Listing first, filtering by volume, then moving what is left says
    // exactly what will happen and lets it be counted before it starts.
    let todo: Vec<(crate::volume::VolumeId, u64)> = {
        let gem = mgr.gem().read().await;
        sources
            .iter()
            .flat_map(|s| gem.slab_extents(*s))
            .filter(|(vol, _, _)| !have.contains(&vol.0))
            .map(|(vol, vext, _)| (vol, vext))
            .collect()
    };
    if todo.is_empty() {
        println!("Flow-over: the data half of {disk} has everything this boot would copy");
        return Ok(());
    }
    // By uuid, because VolumeId is an identity and deliberately not ordered.
    let volumes: std::collections::HashSet<uuid::Uuid> =
        todo.iter().map(|(vol, _)| vol.0).collect();
    let volumes = volumes.len();
    println!(
        "Flow-over: seeding the data half of {disk} - {} volume(s), {} extent(s)",
        volumes,
        todo.len()
    );

    let engine = crate::placement::PlacementEngine::new();
    let started = std::time::Instant::now();
    let (mut moved, mut failed) = (0u64, 0u64);
    // The drain's cadence, for the same reason (see `drain::run`).
    const SEED_PERSIST_EVERY: u32 = 64;
    let mut since_persist = 0u32;
    for (vol, vext) in todo {
        // Under the slot fence, like every move (#239). Nothing is exported
        // yet, so nothing contends for it; it is what a move is made of.
        let Some(leg) = mgr.gem().read().await.lookup(vol, vext).map(|l| l.primary()) else {
            continue;
        };
        // A shared slot moves once, for every map that names it.
        if leg.slab_id == dest {
            moved += 1;
            continue;
        }
        let fence = crate::volume::fence::exclusive(leg).await;
        {
        let mut gem = mgr.gem().write().await;
        let mut reg = mgr.registry().write().await;
        match engine.migrate_leg_fenced(&mut gem, &mut reg, vol, vext, leg.slab_id, Some(dest), &fence).await {
            Ok(_) => moved += 1,
            Err(e) => {
                failed += 1;
                tracing::error!("seeding the data half: extent {vol:?}/{vext}: {e}");
                // Giving up is safe and leaves a data half with holes, which
                // the local-slab probe rejects - the node boots from the
                // appliance rather than from an identity that is missing
                // pieces.
                if failed > 8 {
                    anyhow::bail!(
                        "gave up seeding the data half of {disk} after {failed} failures; \
                         {moved} extent(s) had moved"
                    );
                }
            }
        }
        }
        // Durable map first, then the source slots it no longer names.
        //
        // The locks are dropped above so the persist can take what it needs.
        // Doing this as it goes, rather than only at the end, is what makes an
        // interruption harmless: the most a crash can cost is one batch's
        // worth of leaked slot, and never a volume that points at a slot the
        // slab has already freed — a source is freed only after the map that
        // stopped naming it is durable, and until then the slot table's newer
        // generation is what a restore takes.
        //
        // **In batches, not per extent.** Per extent was 3301 whole-map
        // writes to four metadata slabs, two of them on a spinning drive,
        // each flushed: 382.6 s on the R230, past the boot's 300 s wait for
        // the root, so PID 1 gave up and dropped to a shell one minute before
        // the seeding it was waiting for finished (#118).
        since_persist += 1;
        if since_persist >= SEED_PERSIST_EVERY {
            since_persist = 0;
            mgr.persist().await;
            let mut reg = mgr.registry().write().await;
            engine.release_owed(&mut reg).await;
        }
    }
    mgr.persist().await;
    {
        let mut reg = mgr.registry().write().await;
        engine.release_owed(&mut reg).await;
    }
    println!(
        "Flow-over: data half seeded - {moved} extent(s) onto {disk} in {:.1}s{}",
        started.elapsed().as_secs_f64(),
        if failed > 0 { format!(", {failed} failed") } else { String::new() }
    );
    Ok(())
}

/// Move every extent on `sources` onto `dest`, one per lock cycle, while the
/// volumes on them are mounted and written (#239).
///
/// Each extent is taken under the slot fence: the move waits for the I/O on
/// that slot to finish, keeps new I/O out while it copies and rewrites the
/// maps, and an I/O that looked the slot up meanwhile finds the copy. Without
/// it a write landing after the copy was lost, and a copy-on-write reading
/// the source after it was freed — discarded, so zeros on the appliance's
/// thin clone — wrote those zeros into the clone: `cni-bin`'s root directory
/// on 11.56, which Cilium was filling while this ran.
///
/// The map is made durable (`persist`) before the sources it no longer
/// names are freed, after every extent. The sources are quarantined for new
/// allocations, and stay so once empty. `None` when it gave up: more than 16
/// extents that would not move (the quarantine is lifted).
#[cfg(target_os = "linux")]
/// Quarantine the slabs a flow-over empties: every system slab but the one
/// it fills. Nothing new is placed on them, and a write to an extent still on
/// one goes to a fresh slot on a slab that stays (`ThinVolumeHandle`'s
/// relocate-on-write) rather than in place.
///
/// They are the appliance's per-boot clone. A boot cut short before the
/// flow-over reaches an extent resumes from a fresh, pristine clone, so
/// whatever was written in place there is gone at the next boot — while the
/// copy-on-writes, which already land locally, are kept. A filesystem then
/// reads a directory naming an inode its inode table never got (#239: the
/// first free inodes of cadvisor, stormlb, vmimages, stormvm and stormimds on
/// 11.57; hubble-relay's on 11.50). So this is set as soon as the boot knows
/// a flow-over is coming, in the engine that laid the disk and again in the
/// one that adopts it, before either serves a write.
pub(crate) async fn quarantine_flow_sources(
    mgr: &crate::volume::VolumeManager,
    flow: &crate::drive::handover::FlowOver,
) {
    let Ok(dest) = uuid::Uuid::parse_str(&flow.system_slab).map(crate::drive::slab::SlabId) else {
        return;
    };
    let sources: Vec<_> = {
        let mut reg = mgr.registry().write().await;
        let sources: Vec<_> =
            reg.iter().filter(|(id, s)| !s.is_data() && **id != dest).map(|(id, _)| *id).collect();
        for s in &sources {
            reg.set_quarantined(*s, true);
        }
        sources
    };
    if !sources.is_empty() {
        println!(
            "Flow-over: {} appliance slab(s) quarantined — writes to what is still on them go to {}",
            sources.len(),
            flow.disk
        );
        // And the disk names what is still to come (#258), written now: a
        // power cut before the first extent moves must leave a disk that
        // says what it is missing, not one that looks like somebody else's.
        mgr.record_flow_over(dest, sources);
        mgr.persist().await;
    }
}

/// Put the shared NVMe-oF target in service: the daemon's, and a stormcos
/// node's when its config asks for one (#206).
///
/// Stored in the AppState so the export API can add namespaces at runtime
/// instead of parking them until the next restart (#26), and so a boothost
/// claim can answer with an NVMe/TCP attach. Then who may connect, before
/// anything is served (#210): the shared subsystem admits no host unless the
/// config opens it, and each host subsystem admits its own hosts. Then the
/// exports made through the API in an earlier run: an export is an address
/// something out there has written down (firmware booting over NVMe/TCP has
/// the subsystem and namespace in its configuration), so losing the table on
/// restart stops answering at an address a machine is still dialling.
#[cfg(feature = "nvmeof")]
async fn serve_shared_nvmeof(
    state: &Arc<AppState>,
    reactor: &Arc<target::reactor::ReactorPool>,
    nvmeof: Arc<target::nvmeof::NvmeofTarget>,
) {
    *state.nvmeof_target.write().await = Some(nvmeof.clone());
    mgmt::nvme_hosts::apply_shared_policy(state, &nvmeof);
    mgmt::nvme_hosts::restore(state).await;
    mgmt::api::exports::restore_exports(state).await;
    mgmt::api::v1::restore_nvme_nsids(state).await;
    let reactor = reactor.clone();
    tokio::spawn(async move {
        if let Err(e) = nvmeof.run(&reactor).await {
            tracing::error!("NVMe-oF target error: {e}");
        }
    });
}

/// The shared NVMe-oF target an adopting engine serves when its config has
/// an `[nvmeof]` section (#206): a stormcos node that is also forge (a
/// bastion, stormcos#90) exports goldens and host clones and answers boot
/// claims with something to attach. A node whose config has no `[nvmeof]`
/// opens no NVMe/TCP port, as before. No raw drive namespaces: the slab is
/// the engine's pool, served only through volume exports.
#[cfg(feature = "nvmeof")]
fn adopted_nvmeof_target(config: &mgmt::config::StormBlockConfig) -> anyhow::Result<Option<target::nvmeof::NvmeofTarget>> {
    let Some(section) = config.nvmeof.as_ref() else { return Ok(None) };
    let listen_addr: std::net::SocketAddr = section
        .listen_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("[nvmeof] listen_addr {:?}: {e}", section.listen_addr))?;
    if section.export_drives {
        tracing::info!(
            "NVMe-oF: [nvmeof] export_drives is ignored here — an adopting engine serves volumes, \
             never its slab's drive"
        );
    }
    let advertised_addr = config
        .management
        .advertised_host()
        .and_then(|h| format!("{h}:{}", listen_addr.port()).parse().ok());
    Ok(Some(target::nvmeof::NvmeofTarget::new(target::nvmeof::NvmeofConfig {
        listen_addr,
        nqn: section.nqn.clone(),
        advertised_addr,
        ..Default::default()
    })))
}

/// The longest a flow-over waits between two moves for foreground I/O
/// (#269): a move is a slot read from the appliance and a write and read back
/// on the local disk, tens of milliseconds on a spinning one.
const FLOW_YIELD_MAX: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) async fn flow_system_half<P, F>(
    gem: &Arc<tokio::sync::RwLock<crate::volume::gem::GlobalExtentMap>>,
    registry: &Arc<tokio::sync::RwLock<crate::drive::slab_registry::SlabRegistry>>,
    sources: &[crate::drive::slab::SlabId],
    dest: crate::drive::slab::SlabId,
    persist: P,
    remaining: Option<&std::sync::atomic::AtomicI64>,
) -> Option<(u64, u64)>
where
    P: Fn() -> F,
    F: std::future::Future<Output = ()>,
{
    use crate::placement::PlacementError;
    let engine = crate::placement::PlacementEngine::new();
    let (mut moved, mut failed) = (0u64, 0u64);
    // Looks that found the extent changed under them, in a row. An extent
    // that keeps changing is being written as fast as it can be looked at;
    // past this it counts as one that would not move.
    let mut again = 0u32;
    // Nothing new lands on a source while it is emptied, the way a drain
    // quarantines its drive: a copy-on-write in the meantime takes a slot on
    // the local disk, not one more on the appliance for this loop to chase —
    // or to leave behind once it has finished.
    {
        let mut r = registry.write().await;
        for s in sources {
            r.set_quarantined(*s, true);
        }
    }
    let give_up = || async {
        let mut r = registry.write().await;
        for s in sources {
            r.set_quarantined(*s, false);
        }
    };
    // What is left, for `/api/v1/health` (#260): the extents with a leg on
    // a source, counted from the lists this loop reads anyway. Sources not
    // reached yet keep their count from the start; nothing lands on them
    // meanwhile (quarantined above).
    let mut later: Vec<usize> = {
        let g = gem.read().await;
        sources
            .iter()
            .map(|s| g.slab_extents(*s).iter().filter(|(_, _, loc)| loc.leg_on(*s).is_some()).count())
            .collect()
    };
    let report = |n: usize| {
        if let Some(r) = remaining {
            r.store(n as i64, std::sync::atomic::Ordering::Relaxed);
        }
    };
    report(later.iter().sum());
    let mut foreground = crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed);
    let mut last_move = std::time::Duration::ZERO;
    for (i, &source) in sources.iter().enumerate() {
        later[i] = 0;
        let after: usize = later.iter().sum();
        loop {
            // Which slot, under the map's read lock only: the fence is waited
            // for with no lock held, since an I/O holding it may be waiting
            // for the map.
            let pick = {
                let g = gem.read().await;
                let on_source: Vec<_> = g
                    .slab_extents(source)
                    .into_iter()
                    .filter_map(|(vol, vext, loc)| loc.leg_on(source).map(|leg| (vol, vext, leg)))
                    .collect();
                report(on_source.len() + after);
                on_source.into_iter().next()
            };
            let Some((vol, vext, leg)) = pick else { break };
            // Foreground first (#269): when a volume has been read or written
            // since the last move, give the disk back for as long as that move
            // took (capped) before the next. An idle node moves at full speed.
            if crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed) != foreground {
                tokio::time::sleep(last_move.min(FLOW_YIELD_MAX)).await;
            }
            foreground = crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed);
            let started = std::time::Instant::now();
            let fence = crate::volume::fence::exclusive(leg).await;
            // The copy holds only the fence on this slot; the map and the
            // registry are taken to allocate and to publish (#269). Holding
            // them for the copy stalled every volume's I/O and every API call
            // behind each of the 7528 extents of the Dell's install.
            let res = engine
                .migrate_leg_unlocked(gem, registry, vol, vext, leg, dest, &fence)
                .await;
            drop(fence);
            last_move = started.elapsed();
            match res {
                Ok(_) => {
                    moved += 1;
                    again = 0;
                }
                // The extent changed while the fence was awaited (a
                // copy-on-write took it, a discard freed it): look again.
                Err(PlacementError::Busy { .. } | PlacementError::ExtentNotFound { .. })
                    if again < 64 =>
                {
                    again += 1;
                    continue;
                }
                Err(e) => {
                    again = 0;
                    failed += 1;
                    tracing::error!("flow-over: extent {vol:?}/{vext}: {e}");
                    // A handful of bad extents is a disk worth giving up
                    // on, and giving up leaves the node exactly where it
                    // was: running from the appliance.
                    if failed > 16 {
                        // The node goes on running from the appliance.
                        give_up().await;
                        return None;
                    }
                }
            }
            // The map, then the slots it no longer names. Same order and
            // same reason as the data half: this runs for minutes on a
            // machine that can lose power at any point in them, and a slot
            // table that has run ahead of the map is a volume with a hole in
            // it.
            persist().await;
            let mut r = registry.write().await;
            engine.release_owed(&mut r).await;
        }
    }
    report(0);
    Some((moved, failed))
}

/// Move the goldens onto the disk the boot laid out, in the background.
///
/// Only the system half, and only ever the system half. A data slab is being
/// written the whole time it is mounted — logs, state, claims — and moving its
/// extents out from under a live filesystem corrupted every one of them on the
/// first machine it was tried on.
///
/// "The goldens survive it because nothing writes to them" was only half
/// true. The system half holds clones that are written too (`cni-bin`, the
/// `-logs` volumes), and every clone reads its golden's slots. The corruption
/// was the move racing the I/O, not the writing itself. The slot fence closes
/// that race (#239, see [`flow_system_half`]); the data half still moves only
/// before anything is exported.
///
/// One extent per lock cycle, so root I/O interleaves with the copy instead of
/// stalling behind the whole migration.
#[cfg(target_os = "linux")]
fn spawn_flow_over(
    state: &Arc<AppState>,
    flow: crate::drive::handover::FlowOver,
    then_local_boot: Option<(String, Vec<String>)>,
    install: Option<crate::drive::handover::InstallTicket>,
) {
    use crate::drive::slab::SlabId;

    let Ok(dest) = uuid::Uuid::parse_str(&flow.system_slab).map(SlabId) else {
        tracing::warn!(
            "flow-over: the handover names system slab {} on {}, which is not a uuid — \
             the goldens stay on the appliance",
            flow.system_slab,
            flow.disk
        );
        return;
    };
    let gem_arc = state.gem.clone();
    let reg_arc = state.slab_registry.clone();
    let flow_remaining = state.flow_over_remaining.clone();
    // Weak, so a migration in flight cannot keep the whole engine alive past
    // a shutdown that is trying to end.
    let state_for_persist = Arc::downgrade(state);
    tokio::spawn(async move {
        // Every slab that is not a data slab and is not the destination. On a
        // node that has just adopted, that is the appliance's system slab —
        // the local one is registered too, and migrating it into itself would
        // be a long way of doing nothing.
        let sources: Vec<SlabId> = {
            let reg = reg_arc.read().await;
            reg.iter()
                .filter(|(id, s)| !s.is_data() && **id != dest)
                .map(|(id, _)| *id)
                .collect()
        };
        // The disk boots on its own once it holds everything — and only then.
        // An install the appliance asked for is done at that point, and only
        // at that point (#148): `local` on a disk that cannot boot would
        // leave the machine nothing to boot.
        let local_boot = |job: Option<(String, Vec<String>)>| async move {
            let booted = match job {
                Some((disk, sources)) => match run_local_boot(&disk, &sources).await {
                    Ok(bootable) => bootable,
                    Err(e) => {
                        println!("Local boot: {disk}: {e}");
                        tracing::warn!("local boot on {disk}: {e}");
                        false
                    }
                },
                None => false,
            };
            match install {
                Some(t) if booted => report_installed(t).await,
                Some(t) => println!(
                    "Install: not reported done to {} - the disk does not boot on its own; \
                     the intent stays install",
                    t.boothost
                ),
                None => {}
            }
        };
        if sources.is_empty() {
            flow_remaining.store(0, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("flow-over: nothing left to move onto {}", flow.disk);
            local_boot(then_local_boot).await;
            return;
        }
        println!(
            "Flow-over: moving {} slab(s) onto {} in the background",
            sources.len(),
            flow.disk
        );
        let persist = || async {
            if let Some(state) = state_for_persist.upgrade() {
                state.volume_manager.lock().await.persist().await;
            }
        };
        let Some((moved, failed)) =
            flow_system_half(&gem_arc, &reg_arc, &sources, dest, persist, Some(&*flow_remaining)).await
        else {
            tracing::error!(
                "flow-over: too many failures — abandoning {}; the node keeps running from \
                 the appliance",
                flow.disk
            );
            return;
        };
        tracing::info!("flow-over complete: {moved} extent(s) migrated, {failed} failed");
        println!("Flow-over complete: {moved} extent(s) now on {}", flow.disk);
        if failed == 0 {
            local_boot(then_local_boot).await;
        } else {
            println!(
                "Local boot: {} is left unbootable — {failed} extent(s) did not move, and a \
                 disk that boots into incomplete slabs is worse than one that netboots",
                flow.disk
            );
        }
    });
}

#[cfg(target_os = "linux")]
async fn handle_adopt_ublk(
    slab_paths: &[String],
    volumes: &[String],
    meta: Option<&str>,
    api: Option<&str>,
    data_dir: Option<&str>,
    config_path: &str,
) -> anyhow::Result<()> {
    use crate::drive::ublk::UblkServer;

    // What to adopt: the incumbent's own record, unless told otherwise.
    //
    // The kernel knows the devices exist and who serves them; only the server
    // that created them knows which volume is behind each. It writes that
    // down, so a handover needs no arguments at all — and cannot be given a
    // list that is short by one, which leaves the devices left off it mounted
    // with no server and the node unable to restart the engine, because its
    // own root is among them.
    let record = crate::drive::handover::Record::read(std::path::Path::new(
        crate::drive::handover::DEFAULT_PATH,
    ));

    let from_record = record.as_ref().map(|r| r.volumes_in_device_order());
    let volumes: &[String] = if !volumes.is_empty() {
        if let Some(recorded) = from_record.as_deref() {
            if recorded != volumes {
                // Explicit wins — someone may be recovering a node by hand —
                // but disagreeing with the incumbent is worth saying out loud,
                // because the usual cause is a list that has drifted.
                tracing::warn!(
                    "the volumes given differ from what the previous server recorded \
                     ({} given, {} recorded): using the ones given",
                    volumes.len(),
                    recorded.len()
                );
            }
        }
        volumes
    } else {
        match from_record.as_deref() {
            Some(v) if !v.is_empty() => {
                tracing::info!("adopting {} volume(s) from the handover record", v.len());
                v
            }
            _ => anyhow::bail!(
                "nothing to adopt: no volumes were given and no handover record at {} \
                 — the server being taken over is older than the record, so name its \
                 volumes with --volume, in device order",
                crate::drive::handover::DEFAULT_PATH
            ),
        }
    };

    let slab_paths: &[String] = if !slab_paths.is_empty() {
        slab_paths
    } else {
        match record.as_ref().map(|r| r.slabs.as_slice()) {
            Some(s) if !s.is_empty() => s,
            _ => anyhow::bail!(
                "no slab given and none in the handover record at {}",
                crate::drive::handover::DEFAULT_PATH
            ),
        }
    };
    let meta = meta.or(record.as_ref().and_then(|r| r.meta.as_deref()));

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
    // Device n serves the nth volume of the record: the ids are known
    // without reading anything from the slabs.
    let dev_ids: Vec<u32> = (0..volumes.len() as u32).collect();

    // Refuse a handover that would abandon devices.
    //
    // Standing a server down stops every device that server has, not the ones
    // named here. A list that is short by one leaves that device mounted with
    // nothing behind it, and every I/O to it returns EIO — which is how a node
    // came up having adopted its root and lost its data volume, reporting
    // "Adopted 4 device(s)" and then failing to write to /data.
    //
    // The kernel knows which devices exist and who serves them, so this is
    // checkable before anything is stopped rather than discoverable afterwards.
    let orphans = crate::drive::ublk::also_served_by(&dev_ids)?;
    if !orphans.is_empty() {
        let names: Vec<String> =
            orphans.iter().map(|id| format!("/dev/ublkb{id}")).collect();
        anyhow::bail!(
            "the server being taken over also serves {} — adopting only the {} volume(s) \
             named here would leave {} with no server at all, mounted and returning EIO. \
             Name every volume it serves, in device order.",
            names.join(", "),
            dev_ids.len(),
            if orphans.len() == 1 { "it" } else { "them" }
        );
    }

    // Stand the incumbent down and wait for it to be gone, THEN read the
    // slabs (#171). ublk recovery holds every device's I/O in the gap.
    let (mgr, serving, adopted_ids) = crate::drive::handover::take_over(
        || async {
            let ids = dev_ids.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let pids = crate::drive::ublk::stand_down(&ids, std::time::Duration::from_secs(15))?;
                crate::drive::ublk::wait_exited(&pids, std::time::Duration::from_secs(30));
                Ok(())
            })
            .await??;
            Ok(())
        },
        || async {
            let mut mgr = open_slabs_and_restore(slab_paths, meta).await?;

            // The drive this boot laid keeps the records first, as it does in the
            // engine that laid it (#118). The slabs open in handover order, appliance
            // first, and a volume with no extents yet is recorded in the first
            // metadata slab of its role. A PVC created and not yet written would
            // otherwise exist only on a clone the next boot does not attach.
            if let Some(flow) = record.as_ref().and_then(|r| r.flow_over.as_ref()) {
                let local: Vec<crate::drive::slab::SlabId> = [&flow.data_slab, &flow.system_slab]
                    .into_iter()
                    .filter_map(|s| uuid::Uuid::parse_str(s).ok())
                    .map(crate::drive::slab::SlabId)
                    .filter(|id| mgr.is_metadata_slab(id))
                    .collect();
                if !local.is_empty() {
                    mgr.keep_metadata_in_first(&local);
                }
                // Before anything is served, not when the flow-over starts.
                quarantine_flow_sources(&mgr, flow).await;
            }

            // Resolve every volume before serving any. The incumbent is gone by
            // now, so a name that does not resolve leaves the devices held in
            // recovery with no server — loud, and retryable by running this
            // again — where half-adopting a set would serve some queues and
            // not others.
            let mut serving: Vec<(u32, String, Arc<dyn BlockDevice>)> = Vec::new();
            // Which volume each adopted device is, for the API's "in use" (#138).
            let mut adopted_ids: Vec<(u32, uuid::Uuid)> = Vec::new();
            for (i, selector) in volumes.iter().enumerate() {
                let id = resolve_boot_volume(&mgr, selector).await?;
                adopted_ids.push((i as u32, id.0));
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
                    crate::mgmt::config::human_size(dev.capacity_bytes())
                );
            }
            Ok((mgr, serving, adopted_ids))
        },
    )
    .await?;

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
    // Wait for the kernel to bring them back, not for the threads to look
    // busy. A thread that has not exited has reached its I/O loop; the device
    // is only readable once END_USER_RECOVERY has returned it to LIVE. Between
    // those two moments a read of a filesystem on one of these devices fails,
    // and the first thing this process does next is write to one.
    let not_live = tokio::task::spawn_blocking({
        let ids = dev_ids.clone();
        move || {
            crate::drive::ublk::wait_live(&ids, std::time::Duration::from_secs(30))
        }
    })
    .await??;
    for id in &not_live {
        tracing::error!("/dev/ublkb{id} did not come back after recovery");
    }
    let live = threads.iter().filter(|t| !t.is_finished()).count();
    if live == 0 {
        let _ = shutdown_tx.send(true);
        join_ublk_threads(threads, std::time::Duration::from_secs(10));
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

    // Held out here so the capture on the way down can reach them; set inside
    // the block below, where the volume manager still exists.
    let mut state_store_final: Option<Arc<crate::state::StateStore>> = None;
    let mut data_dir_final: Option<String> = None;

    // The management API, in this process, over the manager that owns the
    // slab. There is nowhere else to put it: one writer per volume means a
    // second process cannot open the same slab to answer on its behalf, so an
    // engine that is serving its node's root and not answering questions about
    // it is an engine that is half here.
    if let Some(addr) = api {
        // The node's own config, not the defaults.
        //
        // The engine ships one — where to listen, where state lives, whether
        // to serve /serve/v1 — and building a default here quietly discarded
        // all of it. The visible symptom was the registry next door getting
        // 404s from /serve/v1 for every template it tried to build, because
        // the serving surface is configured and the configuration was never
        // read. The flags stay, as overrides, because a handover may need to
        // put the API somewhere the file does not say.
        let mut config = match crate::mgmt::config::StormBlockConfig::load(
            &config_path,
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("{config_path}: {e} — using defaults");
                crate::mgmt::config::StormBlockConfig::default()
            }
        };
        config.management.listen_addr = addr.to_string();
        // The metadata the manager was restored from is where anything the API
        // creates has to persist to, or a template minted now is gone at the
        // next boot.
        if data_dir.is_some() {
            config.management.data_dir = data_dir.map(|d| d.to_string());
        }
        let data_dir = config.management.data_dir.clone();
        let data_dir = data_dir.as_deref();
        if let Some(d) = data_dir {
            std::fs::create_dir_all(d)
                .map_err(|e| anyhow::anyhow!("cannot create API state directory {d}: {e}"))?;
        }
        // The engine's own durable state.
        //
        // Its writers — the wiring table, the LUN map, the /v1 epochs, the
        // filesystem templates — go on doing synchronous file I/O into
        // `data_dir`, which on a node is tmpfs: fast, unable to block, and
        // unable to reach any volume this engine is responsible for. That last
        // property is the whole point; a data_dir on a served volume is a
        // cycle, and it wedged this node four seconds into every boot.
        //
        // What makes tmpfs survive a reboot is this volume. It is opened by
        // name, never exported and never mounted — read and written by the
        // ext4 library in-process, the same way a golden is built. See
        // `crate::state`.
        let state_store: Option<Arc<crate::state::StateStore>> = match data_dir {
            Some(dir) => match resolve_boot_volume(&mgr, STATE_VOLUME).await {
                Ok(id) => match mgr.get_volume(&id) {
                    Some(dev) => {
                        let store = Arc::new(
                            crate::state::StateStore::open_volume(dev).await,
                        );
                        match store.restore_into(std::path::Path::new(dir)).await {
                            Ok(0) => tracing::info!(
                                "state volume {STATE_VOLUME} is empty — this node has not written any yet"
                            ),
                            Ok(n) => tracing::info!(
                                "restored {n} state file(s) from volume {STATE_VOLUME}"
                            ),
                            Err(e) => tracing::error!("restoring state: {e}"),
                        }
                        Some(store)
                    }
                    None => None,
                },
                // No such volume: an image built before this, or a deployment
                // that keeps its data_dir somewhere already durable. Neither
                // is an error — say so once and carry on, because a node that
                // refuses to start over where it files its paperwork is worse
                // than one that files it somewhere less permanent.
                Err(_) => {
                    tracing::info!(
                        "no {STATE_VOLUME} volume — engine state stays in {dir} and does not survive a reboot"
                    );
                    None
                }
            },
            None => None,
        };

        state_store_final = state_store.clone();
        data_dir_final = data_dir.map(str::to_owned);

        let slab_registry = mgr.registry().clone();
        let gem = mgr.gem().clone();
        let state = Arc::new(AppState::new(config.clone(), mgr, slab_registry, gem));
        // The boot devices this process now serves are in use, and a volume
        // listing must say so — they were recorded nowhere (#138).
        {
            let mut ublk = state.ublk_exports.lock().await;
            for (dev_id, id) in &adopted_ids {
                ublk.record_adopted(&id.to_string(), format!("/dev/ublkb{dev_id}"));
            }
        }
        // The serving surface too. An engine that took the devices over from
        // the initramfs *is* this node's engine, and layer 2 belongs to the
        // engine rather than to one of the two ways of becoming it.
        let reactor = Arc::new(ReactorPool::new(&ReactorConfig {
            core_count: 0,
            pin_cores: cfg!(target_os = "linux"),
        }));
        // The blanks this node's slabs carry are templates, here too.
        //
        // A node that took its devices over from the initramfs comes up
        // through this path and not through `serve`, so seeding the template
        // store in one of them seeds it on a build box and never on a node —
        // which is exactly what happened: the fix shipped, the node still
        // reported "0 of 5 blank size(s) sealed", and the difference was
        // which of the two ways of becoming this node's engine it had taken.
        mgmt::api::fstemplates::adopt_slab_templates(&state).await;
        // Forge mode (#206): the shared NVMe/TCP target, when the config
        // asks for one. Before `/serve/v1`, as in the daemon.
        #[cfg(feature = "nvmeof")]
        match adopted_nvmeof_target(&config) {
            Ok(Some(nvmeof)) => {
                if let Some(n) = config.nvmeof.as_ref() {
                    println!("  NVMe-oF target on {} ({}), from [nvmeof] in {config_path}", n.listen_addr, n.nqn);
                }
                serve_shared_nvmeof(&state, &reactor, Arc::new(nvmeof)).await;
            }
            Ok(None) => {}
            Err(e) => {
                println!("  NVMe-oF target not started: {e}");
                tracing::error!("NVMe-oF target not started: {e}");
            }
        }
        start_serving(&config, &state, "0.0.0.0:3260", "0.0.0.0:4420", &reactor).await;

        // Finish the flow-over the boot started.
        //
        // The initramfs engine laid the slabs and stopped there, because it
        // had seconds to live and the copy takes minutes; this process is the
        // one that is still here when the extents land. See
        // `drive::handover::FlowOver`.
        //
        // Then make the disk boot on its own (#123) — after the copy, never
        // before it: a disk that boots before its slabs are complete boots
        // into a probe that rejects them.
        let local_boot = record.as_ref().and_then(|r| {
            let disk = r.local_boot.clone()?;
            let sources: Vec<String> =
                r.slabs.iter().filter(|p| **p != disk).cloned().collect();
            Some((disk, sources))
        });
        let install = record.as_ref().and_then(|r| r.install.clone());
        if let Some(flow) = record.as_ref().and_then(|r| r.flow_over.clone()) {
            spawn_flow_over(&state, flow, local_boot, install);
        } else if let Some((disk, sources)) = local_boot {
            tokio::spawn(async move {
                if let Err(e) = run_local_boot(&disk, &sources).await {
                    println!("Local boot: {disk}: {e}");
                    tracing::warn!("local boot on {disk}: {e}");
                }
            });
        }

        // Push the working directory down to the volume, on a timer.
        //
        // Only what changed is written, so a node whose state is not moving
        // writes nothing — which is what makes ten seconds a reasonable
        // interval rather than an expensive one. The interval bounds what a
        // node that stops without being asked can lose.
        if let (Some(store), Some(dir)) = (state_store.clone(), data_dir) {
            let dir = std::path::PathBuf::from(dir);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    if let Err(e) = store.capture_from(&dir).await {
                        tracing::warn!("capturing state: {e}");
                    }
                }
            });
        }

        tokio::spawn(async move {
            if let Err(e) = mgmt::start_management_server(state).await {
                tracing::error!("management API error: {e}");
            }
        });
        println!("  management API on {addr}");
    }

    tokio::signal::ctrl_c().await?;
    // Once more on the way down: a node asked to stop should not lose the last
    // thing it was told.
    if let (Some(store), Some(dir)) = (state_store_final.clone(), data_dir_final.as_deref()) {
        match store.capture_from(std::path::Path::new(dir)).await {
            Ok(n) if n > 0 => tracing::info!("captured {n} state file(s) before stopping"),
            Ok(_) => {}
            Err(e) => tracing::error!("capturing state before stopping: {e}"),
        }
    }
    let _ = shutdown_tx.send(true);
    let stuck = join_ublk_threads(threads, std::time::Duration::from_secs(10));
    if stuck > 0 {
        eprintln!("WARNING: {stuck} ublk export(s) did not finish their teardown");
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
    _config_path: &str,
) -> anyhow::Result<()> {
    anyhow::bail!("ublk is Linux-only")
}

/// boot-local: attach an existing local slab (no reformat, no repartition),
/// restore volume metadata, export the boot volume as /dev/ublkb0.
/// The local-slab → ublk-root path stormcos boots through (issue #12).
#[allow(clippy::too_many_arguments)]
/// Resolve this machine's image and print where to attach it.
///
/// Everything diagnostic goes to stderr so stdout is exactly the URI and
/// nothing else — the caller substitutes it straight into `--slab`.
async fn handle_boot_claim(
    boothost: &str,
    tag: &str,
    namespace: &str,
    timeout_secs: u64,
    token: Option<&str>,
) -> anyhow::Result<()> {
    let uri = claim_boot_uri(boothost, tag, namespace, timeout_secs, token).await?;
    println!("{uri}");
    Ok(())
}

/// Claim this machine's image and answer the attach URI of the clone.
async fn claim_boot_uri(
    boothost: &str,
    tag: &str,
    namespace: &str,
    timeout_secs: u64,
    token: Option<&str>,
) -> anyhow::Result<String> {
    let base = boothost.trim_end_matches('/');
    let base = if base.contains("://") { base.to_string() } else { format!("http://{base}") };
    let url = format!("{base}/api/v1/synonyms/{namespace}/{tag}/claim");
    eprintln!("boot-claim: {url}");

    // The in-house client, not reqwest: the binary deliberately does not carry
    // reqwest, and this runs in the initramfs where the binary is the payload.
    let token = token
        .map(|t| t.to_string())
        .or_else(|| std::env::var("STORMBLOCK_API_TOKEN").ok())
        .filter(|t| !t.trim().is_empty());
    let client = crate::http::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .bearer(token)
        .build()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Retry rather than fail: a node and the appliance it boots from can come
    // back from a power cut together, and whichever loses the race should
    // wait rather than drop someone to an initramfs shell.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut last = String::new();
    loop {
        match client.post(&url).json(&serde_json::json!({})).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
                        anyhow::anyhow!("claim returned {status} but not JSON: {e}: {body}")
                    })?;
                    let uri = v
                        .get("attach")
                        .and_then(|a| a.get("uri"))
                        .and_then(|u| u.as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "claim answered without an attach URI — a volume id alone is not \
                                 bootable: {body}"
                            )
                        })?;
                    if let Some(name) = v.get("volume").and_then(|x| x.get("name")).and_then(|x| x.as_str()) {
                        eprintln!("boot-claim: {tag} -> {name}");
                    }
                    note_install_ticket(&base, &v);
                    return Ok(uri.to_string());
                }
                // A tag nobody has decided for is a fleet decision that has
                // not been made. Say which name was missing: it is the thing
                // an operator has to create.
                if status.as_u16() == 404 {
                    anyhow::bail!(
                        "no image is assigned to this machine: {namespace}/{tag} does not exist \
                         on {base}. Create it with PUT /api/v1/synonyms/{namespace}/{tag}"
                    );
                }
                last = format!("{status}: {body}");
            }
            Err(e) => last = e.to_string(),
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("boot-claim failed after {timeout_secs}s: {last}");
        }
        eprintln!("boot-claim: {last} - retrying");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Leave the install the claim asked for where the initramfs and
/// `boot-local` look (#148): `intent: install` in the reply writes the
/// ticket, anything else removes a stale one. Never fails the claim — a
/// ticket that cannot be written is an install that does not happen, which
/// is the boot this machine would have had before intents existed.
fn note_install_ticket(base: &str, reply: &serde_json::Value) {
    use crate::drive::handover::{InstallTicket, INSTALL_TICKET_PATH};
    let path = std::path::Path::new(INSTALL_TICKET_PATH);
    let stated = reply.get("intent").and_then(|i| i.as_str());
    let intent = stated.unwrap_or("auto");
    eprintln!("boot-claim: intent {intent}{}", if stated.is_none() { " (none stated)" } else { "" });
    note_no_intent(std::path::Path::new(crate::drive::handover::NO_INTENT_PATH), base, stated.is_none());
    let ticket = (intent == "install")
        .then(|| {
            let host = reply.get("host")?.get("name")?.as_str()?;
            let volume = reply.get("volume")?.get("id")?.as_str()?;
            Some(InstallTicket { boothost: base.to_string(), host: host.to_string(), volume: volume.to_string() })
        })
        .flatten();
    match ticket {
        Some(t) => match t.write(path) {
            Ok(()) => eprintln!("boot-claim: install requested for {} - {} written", t.host, path.display()),
            Err(e) => eprintln!("boot-claim: install requested, but {}: {e} - booting as auto", path.display()),
        },
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Note an appliance that stated no intent (#236, see `NO_INTENT_PATH`).
/// Best effort, like the ticket: a marker that cannot be written is a boot
/// that keeps the old behaviour.
fn note_no_intent(path: &std::path::Path, base: &str, none_stated: bool) {
    if none_stated {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, format!("{base}\n")) {
            eprintln!("boot-claim: {}: {e}", path.display());
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Tell the appliance an install it asked for is done (#148): the flow-over
/// finished and the disk boots on its own, so the machine's intent goes back
/// to `local`. Retried for an hour — until it lands, the next power cycle
/// installs again — and never fatal.
async fn report_installed(ticket: crate::drive::handover::InstallTicket) {
    let url = format!(
        "{}/api/v1/synonyms/boothost/{}/installed",
        ticket.boothost.trim_end_matches('/'),
        ticket.host
    );
    let client = match crate::http::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("install report to {url}: {e}");
            return;
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    loop {
        let last = match client.post(&url).json(&serde_json::json!({ "volume": ticket.volume })).send().await {
            Ok(resp) if resp.status().is_success() => {
                println!("Install: reported done to {url}; this machine's boot intent is local again");
                tracing::info!("install reported done to {url}");
                return;
            }
            // Refused — not the clone the install is for, or no such host.
            // Asking again will not change the answer.
            Ok(resp) if (400..500).contains(&resp.status().as_u16()) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                println!("Install: {url} refused the report ({status}: {body})");
                tracing::warn!("install report refused by {url}: {status}: {body}");
                return;
            }
            Ok(resp) => format!("{}", resp.status()),
            Err(e) => e.to_string(),
        };
        if std::time::Instant::now() >= deadline {
            println!(
                "Install: could not report done to {url} ({last}); the intent stays install, \
                 so the next power cycle installs again"
            );
            tracing::error!("install report to {url} gave up: {last}");
            return;
        }
        tracing::warn!("install report to {url}: {last} - retrying");
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn handle_boot_local(
    slab_paths: &[String],
    meta: Option<&str>,
    volume: Option<&str>,
    boot_config: &str,
    image_store: Option<&str>,
    writable: &[String],
    local_disk: Option<&str>,
    local_tier: &str,
    local_disk_force: bool,
    check: bool,
) -> anyhow::Result<()> {
    // A node disk's data half takes any space the drive has gained, before
    // anything is open on it: nothing is mounted, nothing is exported, and a
    // grow is a table rewrite and a header write. Its own failure costs
    // nothing — the node boots on the data half it already had.
    for path in slab_paths {
        if is_fabric_uri(path) || !std::path::Path::new(path).exists() {
            continue;
        }
        let Ok(dev) = open_storage(path).await else {
            continue;
        };
        match crate::image::local::grow_data_half(dev).await {
            Ok(Some((was, now))) => println!(
                "{path}: the data half grew from {was} to {now} slots into the space after it"
            ),
            Ok(None) => {}
            Err(e) => println!("{path}: not growing the data half — {e}"),
        }
    }

    let (mut mgr, resumed) = open_slabs_resuming(slab_paths, meta, true).await?;

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
            crate::mgmt::config::human_size(dev.capacity_bytes())
        );
    }

    if check {
        println!("boot-local check OK");
        return Ok(());
    }

    // 4. Optional zeroboot flow-over: migrate extents to a local disk in the
    //    background, one extent per lock cycle so root I/O keeps flowing.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // What this boot laid down and did not fill, for the handover record
    // below. `None` on a node with no local disk, and on one whose disk was
    // refused — in both cases the successor has nothing to flow into and the
    // node runs from the appliance, which is what it did before any of this.
    let mut laid_flow_over: Option<crate::drive::handover::FlowOver> = None;
    let mut local_boot_disk: Option<String> = None;
    if let Some(disk) = local_disk {
        // Flow-over is an optimisation, and an optimisation may not decide
        // whether a node boots.
        //
        // Every one of these steps can fail for a reason that has nothing to
        // do with the root filesystem: the drive already carries a data slab,
        // it is smaller than a slab needs, it has developed a fault. The root
        // is already attached and serving by this point — from the appliance,
        // which is exactly where it lived before any of this existed. Failing
        // the whole boot for it costs a node that was otherwise fine.
        //
        // It did. `refusing to format /dev/sda for flow-over` propagated out
        // of `boot-local`, so `/dev/ublkb0` was never exported and the boot
        // ended as
        //
        //     FATAL: root device /dev/ublkb0 not found after 30s
        //     Dropping to shell...
        //
        // — a failure that names the root device and says nothing about the
        // local disk, for a node whose root was reachable the whole time.
        let flow_over =
            take_local_disk(&mut mgr, disk, local_tier, local_disk_force).await;
        match flow_over {
            Ok(f) => {
                laid_flow_over = f;
                // Laid, updated or already current: the disk carries this
                // node's layout, and the successor makes it bootable.
                local_boot_disk = Some(disk.to_string());
            }
            // Said plainly, and on the console, because this is the one line
            // that explains why a node that was going to run locally is
            // running from the appliance instead.
            Err(e) => {
                println!("Flow-over: not taking {disk} — {e}");
                println!("Flow-over: the node boots from the appliance, unaffected.");
                tracing::warn!("flow-over disabled for {disk}: {e}");
            }
        }
    }

    // Write down what the next server will need. See drive::handover: the
    // kernel remembers the device but not the volume behind it, and two
    // hand-written lists that must agree in order is a defect waiting for the
    // day a node gains a volume.
    //
    // **After the flow-over, not before.** The record names the slabs the
    // successor opens, and a disk that has just been laid out is one of them
    // — written first, it named only the appliance, and the engine that took
    // the devices over never learned there was a local disk at all. It was
    // written 380 milliseconds before the disk was laid.
    {
        let mut slabs = slab_paths.to_vec();
        if let Some(f) = &laid_flow_over {
            // The disk itself, not its partitions: `open_slabs_and_restore`
            // reads the GPT and finds both slabs in it, which is the same
            // thing `rd.stormblock.slab=/dev/sda` does on a composed disk.
            slabs.push(f.disk.clone());
        }
        // A flow-over cut short and resumed from a fresh clone (#171): the
        // successor opens the clone too, and moves what is left onto this
        // disk.
        if let Some(r) = &resumed {
            slabs.push(r.uri.clone());
            if laid_flow_over.is_none() {
                if let (Some(sys), Some(data)) = (r.system_slab, r.data_slab) {
                    laid_flow_over = Some(crate::drive::handover::FlowOver {
                        disk: slab_paths.first().cloned().unwrap_or_default(),
                        system_slab: sys.0.to_string(),
                        data_slab: data.0.to_string(),
                    });
                }
            }
        }
        // From here the appliance's system slabs are on their way out: a
        // write to an extent still on one lands on the local disk (#239).
        if let Some(f) = &laid_flow_over {
            quarantine_flow_sources(&mgr, f).await;
        }
        let record = crate::drive::handover::Record {
            slabs,
            meta: meta.map(|m| m.to_string()),
            devices: exports
                .iter()
                .map(|(dev_id, name, _)| crate::drive::handover::Device {
                    dev_id: *dev_id,
                    volume: name.clone(),
                })
                .collect(),
            flow_over: laid_flow_over.clone(),
            local_boot: local_boot_disk.clone(),
            // An install the appliance asked for is done when this disk is
            // (#148) — only when there is a flow-over to finish. With none,
            // the intent stays `install` and the next boot tries again.
            install: {
                let t = crate::drive::handover::InstallTicket::read(std::path::Path::new(
                    crate::drive::handover::INSTALL_TICKET_PATH,
                ));
                match (&t, &laid_flow_over) {
                    (Some(t), None) => {
                        println!(
                            "Install: {} asked for an install and no local disk was laid; \
                             its intent stays install",
                            t.boothost
                        );
                        None
                    }
                    _ => t,
                }
            },
        };
        let path = std::path::Path::new(crate::drive::handover::DEFAULT_PATH);
        match record.write(path) {
            Ok(()) => tracing::info!(
                "handover record written to {} ({} device(s), {} slab(s){})",
                path.display(),
                record.devices.len(),
                record.slabs.len(),
                if record.flow_over.is_some() { ", flow-over pending" } else { "" }
            ),
            // Not fatal: the successor can still be told explicitly. But it is
            // the difference between a handover that needs no arguments and
            // one that needs the right ones, so it is never silent.
            Err(e) => tracing::warn!(
                "could not write the handover record to {}: {e} — a successor will \
                 have to be given --slab and --volume explicitly",
                path.display()
            ),
        }
    }

    // 5. Export via ublk (Linux 6.0+ with ublk_drv).
    #[cfg(target_os = "linux")]
    {
        use crate::drive::ublk::UblkServer;

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
        let stuck = join_ublk_threads(ublk_threads, std::time::Duration::from_secs(10));
        if stuck > 0 {
            eprintln!("WARNING: {stuck} ublk export(s) did not finish their teardown");
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
    let iscsi = crate::drive::iscsi_dev::IscsiDevice::connect(source_portal, source_port, source_iqn)
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
    let local_dev = (
        open_storage(target_device).await?
    );

    // 3. Build registry + GEM from source slab
    let mut registry = crate::drive::slab_registry::SlabRegistry::new();
    let gem = crate::volume::gem::GlobalExtentMap::rebuild_from_slabs(
        std::iter::once((&source_slab_id, &source_slab))
    );
    registry.add(source_slab);

    println!("GEM rebuilt: {} extents across {} volumes",
        gem.total_extents(), gem.volume_count());

    // 4. Migrate via placement engine
    let engine = crate::placement::PlacementEngine::new();
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let mut gem = gem;
    let result = crate::migrate::migrate_to_slab(
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

#[cfg(test)]
mod tests {
    use super::note_no_intent;

    /// An appliance that states no intent (older than v20) leaves the marker
    /// the initramfs reads as "install without an intent" (#236); one that
    /// states any intent takes it away.
    #[test]
    fn no_intent_marker_follows_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/stormblock/no-intent");
        note_no_intent(&path, "http://forge:9090", true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "http://forge:9090\n");
        note_no_intent(&path, "http://forge:9090", false);
        assert!(!path.exists());
        // Removing what is not there is not an error.
        note_no_intent(&path, "http://forge:9090", false);
    }
}

/// A fresh install's flow-over, on files, end to end (#239).
#[cfg(all(test, target_os = "linux"))]
mod install_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use sha2::{Digest, Sha256};

    use crate::drive::BlockDevice;
    use crate::volume::VolumeManager;

    const MIB: u64 = 1024 * 1024;

    /// sha256 of every volume the manager holds, by name, read through the
    /// volume the way a consumer reads it.
    async fn digests(mgr: &VolumeManager) -> BTreeMap<String, (String, Vec<u64>)> {
        digests_where(mgr, false).await
    }

    async fn digests_where(mgr: &VolumeManager, sealed_only: bool) -> BTreeMap<String, (String, Vec<u64>)> {
        let mut out = BTreeMap::new();
        for (id, name, size, _) in mgr.list_volumes().await {
            if sealed_only && !mgr.is_sealed(&id) {
                continue;
            }
            let vol: Arc<dyn BlockDevice> = mgr.get_volume(&id).unwrap();
            let mut whole = Sha256::new();
            // Per MiB too, so a mismatch names where it is.
            let mut per: Vec<u64> = Vec::new();
            let mut buf = vec![0u8; MIB as usize];
            let mut off = 0;
            while off < size {
                let n = (size - off).min(MIB) as usize;
                vol.read(off, &mut buf[..n]).await.unwrap();
                whole.update(&buf[..n]);
                let h = Sha256::digest(&buf[..n]);
                per.push(u64::from_le_bytes(h[..8].try_into().unwrap()));
                off += n as u64;
            }
            out.insert(name, (format!("{:x}", whole.finalize()), per));
        }
        out
    }

    fn compare(
        when: &str,
        want: &BTreeMap<String, (String, Vec<u64>)>,
        got: &BTreeMap<String, (String, Vec<u64>)>,
    ) -> Vec<String> {
        let mut bad = Vec::new();
        for (name, (h, per)) in want {
            match got.get(name) {
                None => bad.push(format!("{when}: volume {name} is missing")),
                Some((g, gper)) if g != h => {
                    let first = per.iter().zip(gper).position(|(a, b)| a != b);
                    bad.push(format!(
                        "{when}: volume {name} differs, first at MiB {first:?} of {}",
                        per.len()
                    ));
                }
                Some(_) => {}
            }
        }
        bad
    }

    fn mkfs_ext4() -> Option<String> {
        for p in ["/usr/sbin/mkfs.ext4", "/sbin/mkfs.ext4", "/usr/bin/mkfs.ext4"] {
            if std::path::Path::new(p).exists() {
                return Some(p.to_string());
            }
        }
        None
    }

    /// An image whose data slab carries an e2fsprogs blank (the shape of
    /// stormcos's `cni-bin`) and a golden of noise, installed onto an empty
    /// disk: every volume must read the same after the seed, and again from a
    /// fresh open of the image and the disk (the engine that adopts the boot).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fresh_install_seeds_every_data_volume_byte_for_byte() {
        let Some(mkfs) = mkfs_ext4() else {
            eprintln!("SKIP: needs e2fsprogs mkfs.ext4 (the golden is made by it, #239)");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).display().to_string();

        // The blank, exactly as stormcos makes it.
        let blank = p("cni-bin.img");
        std::fs::File::create(&blank).unwrap().set_len(256 * MIB).unwrap();
        let st = std::process::Command::new(&mkfs)
            .args(["-q", "-F", "-b", "4096", &blank])
            .status()
            .unwrap();
        assert!(st.success(), "mkfs.ext4 failed");
        // Noise, so a slot that lands in the wrong place cannot pass.
        let mut seed = 0x239u64;
        let mut noise = |len: u64| {
            let mut v = vec![0u8; len as usize];
            for c in v.chunks_mut(8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                c.copy_from_slice(&seed.to_le_bytes()[..c.len()]);
            }
            v
        };
        let state = p("state.img");
        std::fs::write(&state, noise(37 * MIB + 4096)).unwrap();
        let root = p("root.img");
        std::fs::write(&root, noise(24 * MIB)).unwrap();

        let spec = format!(
            r#"
name = "install-239"
size = "1G"
[slab]
size = "rest"
[[slab.golden]]
name = "root"
file = "{root}"
[data_slab]
size = "512M"
[[data_slab.golden]]
name = "cni-bin"
file = "{blank}"
template = true
[[data_slab.golden]]
name = "state"
file = "{state}"
"#
        );
        let image = p("image.raw");
        crate::image::ImageBuilder::new(crate::image::ImageSpec::from_toml(&spec).unwrap())
            .build(std::path::Path::new(&image))
            .await
            .unwrap();

        // The install boot: the image's slabs, then the empty local disk.
        let (mut mgr, _) = super::open_slabs_resuming(&[image.clone()], None, true)
            .await
            .unwrap();
        let before = digests(&mgr).await;
        assert!(before.contains_key("cni-bin"), "volumes: {:?}", before.keys());
        let disk = p("disk.raw");
        std::fs::File::create(&disk).unwrap().set_len(80 * 1024 * MIB).unwrap();
        let flow = super::take_local_disk(&mut mgr, &disk, "hot", false)
            .await
            .unwrap()
            .expect("a fresh disk is laid");
        let mut bad = compare("after the seed", &before, &digests(&mgr).await);
        drop(mgr);

        // The engine that adopts the boot opens the image and the disk.
        let (succ, _) = super::open_slabs_resuming(&[image.clone(), flow.disk.clone()], None, true)
            .await
            .unwrap();
        bad.extend(compare("after a fresh open", &before, &digests(&succ).await));
        drop(succ);

        // And the disk alone: what the node boots from next time.
        let (local, _) = super::open_slabs_resuming(&[flow.disk.clone()], None, false)
            .await
            .unwrap();
        let alone = digests(&local).await;
        for name in ["cni-bin", "cni-bin.golden", "state", "state.golden"] {
            if let (Some(a), Some(b)) = (before.get(name), alone.get(name)) {
                if a.0 != b.0 {
                    bad.push(format!("the disk alone: volume {name} differs"));
                }
            } else {
                bad.push(format!("the disk alone: volume {name} is missing"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The appliance, as the node sees it: a device a network round trip
    /// away. Each I/O takes a millisecond or two, which is the window a move
    /// used to land in between an I/O finding its slot and using it.
    struct Remote(Arc<dyn BlockDevice>, Arc<Gate>);

    /// Stops the next read inside the device, after the I/O has found its
    /// slot and before it reads it, until the test lets it go.
    #[derive(Default)]
    struct Gate {
        armed: std::sync::atomic::AtomicBool,
        jitter: std::sync::atomic::AtomicBool,
        arrived: tokio::sync::Notify,
        go: tokio::sync::Notify,
    }

    /// 0–4 ms, different for every I/O: a round trip that is not always the
    /// same length, so an I/O that found its slot first can land last.
    async fn round_trip() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x239);
        let n = N.fetch_add(0x9E37_79B9_7F4A_7C15, std::sync::atomic::Ordering::Relaxed);
        let us = (n.wrapping_mul(0xBF58_476D_1CE4_E5B9) >> 40) % 4000;
        tokio::time::sleep(std::time::Duration::from_micros(us)).await;
    }

    #[async_trait::async_trait]
    impl BlockDevice for Remote {
        fn id(&self) -> &crate::drive::DeviceId {
            self.0.id()
        }
        fn capacity_bytes(&self) -> u64 {
            self.0.capacity_bytes()
        }
        fn block_size(&self) -> u32 {
            self.0.block_size()
        }
        fn optimal_io_size(&self) -> u32 {
            self.0.optimal_io_size()
        }
        fn device_type(&self) -> crate::drive::DriveType {
            self.0.device_type()
        }
        async fn read(&self, offset: u64, buf: &mut [u8]) -> crate::drive::DriveResult<usize> {
            if self.1.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.1.arrived.notify_one();
                self.1.go.notified().await;
            }
            if self.1.jitter.load(std::sync::atomic::Ordering::Relaxed) {
                round_trip().await;
            }
            self.0.read(offset, buf).await
        }
        async fn write(&self, offset: u64, buf: &[u8]) -> crate::drive::DriveResult<usize> {
            if self.1.jitter.load(std::sync::atomic::Ordering::Relaxed) {
                round_trip().await;
            }
            self.0.write(offset, buf).await
        }
        async fn flush(&self) -> crate::drive::DriveResult<()> {
            self.0.flush().await
        }
        async fn discard(&self, offset: u64, len: u64) -> crate::drive::DriveResult<()> {
            if self.1.jitter.load(std::sync::atomic::Ordering::Relaxed) {
                round_trip().await;
            }
            self.0.discard(offset, len).await
        }
        fn discard_granularity(&self) -> u32 {
            self.0.discard_granularity()
        }
        fn smart_status(&self) -> crate::drive::DriveResult<crate::drive::SmartData> {
            self.0.smart_status()
        }
    }

    /// A flow-over move stuck reading the appliance holds nothing the rest
    /// of the node needs (#269): the map and the registry are free, and a
    /// write and flush on another volume finish meanwhile. On the Dell (11.78)
    /// each copy held both write locks, and clones and volume listings timed
    /// out at 60 s for the whole install.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flow_over_copy_holds_no_lock_the_node_needs() {
        use crate::drive::filedev::FileDevice;
        use crate::drive::slab::{Slab, SlabFormat, SlabRole};
        use crate::placement::topology::StorageTier;
        use std::sync::atomic::Ordering;

        const SLOT: u64 = 64 * 1024;
        const EXTENTS: u64 = 16;
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Gate::default());
        let slab = |name: &str, gate: Option<Arc<Gate>>| {
            let path = dir.path().join(name).display().to_string();
            async move {
                let mut dev = Arc::new(FileDevice::open_with_capacity(&path, 4 * EXTENTS * SLOT).await.unwrap())
                    as Arc<dyn BlockDevice>;
                if let Some(g) = gate {
                    dev = Arc::new(Remote(dev, g));
                }
                Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System))
                    .await
                    .unwrap()
            }
        };
        let mut mgr = VolumeManager::new(SLOT);
        let source = slab("appliance.slab", Some(gate.clone())).await;
        let source_id = source.slab_id();
        mgr.add_slab(source).await;
        let golden = mgr.create_volume_any("golden", EXTENTS * SLOT).await.unwrap();
        let g = mgr.get_volume(&golden).unwrap();
        g.write(0, &vec![0x5A; (EXTENTS * SLOT) as usize]).await.unwrap();
        g.flush().await.unwrap();
        let local = slab("local.slab", None).await;
        let local_id = local.slab_id();
        mgr.add_slab(local).await;
        // A volume of the node's, written while the move is stuck.
        let app = mgr.create_volume_any("app", 4 * SLOT).await.unwrap();
        let app = mgr.get_volume(&app).unwrap();

        gate.armed.store(true, Ordering::SeqCst);
        let sources = [source_id];
        let flow = super::flow_system_half(mgr.gem(), mgr.registry(), &sources, local_id, || mgr.persist(), None);
        let check = async {
            gate.arrived.notified().await;
            // The move is inside its read of the appliance's slot now.
            assert!(mgr.gem().try_write().is_ok(), "the extent map is free during the copy");
            assert!(mgr.registry().try_write().is_ok(), "the registry is free during the copy");
            let io = async {
                app.write(0, &vec![0xA5; SLOT as usize]).await.unwrap();
                app.flush().await.unwrap();
            };
            let done = tokio::time::timeout(std::time::Duration::from_secs(2), io).await;
            gate.go.notify_one();
            assert!(done.is_ok(), "a write and flush on another volume finish while a move is stuck");
        };
        let (flowed, ()) = tokio::join!(flow, check);
        let (moved, failed) = flowed.expect("the flow-over finished");
        assert_eq!((moved, failed), (EXTENTS, 0));
        let mut back = vec![0u8; (EXTENTS * SLOT) as usize];
        g.read(0, &mut back).await.unwrap();
        assert!(back.iter().all(|&b| b == 0x5A), "the golden reads as written after the move");
        let mut back = vec![0u8; SLOT as usize];
        app.read(0, &mut back).await.unwrap();
        assert!(back.iter().all(|&b| b == 0xA5));
    }

    /// What `/api/v1/health` reports as `flow_over_remaining` (#260): the
    /// extents still on a source, from the first look to 0 when the move is
    /// done, never going up on the way.
    #[tokio::test]
    async fn the_flow_over_counts_down_what_is_left_on_the_appliance() {
        use crate::drive::filedev::FileDevice;
        use crate::drive::slab::{Slab, SlabFormat, SlabRole};
        use crate::placement::topology::StorageTier;
        use std::sync::atomic::{AtomicI64, Ordering};

        const SLOT: u64 = 64 * 1024;
        const EXTENTS: u64 = 24;
        let dir = tempfile::tempdir().unwrap();
        let slab = |name: &str| {
            let path = dir.path().join(name).display().to_string();
            async move {
                let dev = Arc::new(FileDevice::open_with_capacity(&path, 4 * EXTENTS * SLOT).await.unwrap())
                    as Arc<dyn BlockDevice>;
                Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System))
                    .await
                    .unwrap()
            }
        };
        let mut mgr = VolumeManager::new(SLOT);
        let source = slab("appliance.slab").await;
        let source_id = source.slab_id();
        mgr.add_slab(source).await;
        let golden = mgr.create_volume_any("golden", EXTENTS * SLOT).await.unwrap();
        let g = mgr.get_volume(&golden).unwrap();
        g.write(0, &vec![0x5A; (EXTENTS * SLOT) as usize]).await.unwrap();
        g.flush().await.unwrap();
        let local = slab("local.slab").await;
        let local_id = local.slab_id();
        mgr.add_slab(local).await;

        let remaining = AtomicI64::new(-1);
        let seen = std::sync::Mutex::new(Vec::new());
        let (moved, failed) = super::flow_system_half(
            mgr.gem(),
            mgr.registry(),
            &[source_id],
            local_id,
            || {
                seen.lock().unwrap().push(remaining.load(Ordering::Relaxed));
                mgr.persist()
            },
            Some(&remaining),
        )
        .await
        .expect("the flow-over finished");
        assert_eq!((moved, failed), (EXTENTS, 0));
        assert_eq!(remaining.load(Ordering::Relaxed), 0, "done is 0");
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&(EXTENTS as i64)), "the first extent moved with all of them left");
        assert!(seen.windows(2).all(|w| w[1] < w[0]), "one fewer per extent moved: {seen:?}");
    }

    /// The background flow-over moves a golden's slots while a clone of it
    /// is written and read (#239): what cni-bin went through on 11.56 while
    /// Cilium filled it. Every write must be there afterwards and the golden
    /// must read as it was, throughout.
    ///
    /// `FENCE_OFF_239=1` runs it without the slot fence, to see the race it
    /// closes (expected to fail then; not part of the routine check).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn the_flow_over_moves_a_live_clone_without_losing_a_byte() {
        use crate::drive::filedev::FileDevice;
        use crate::drive::slab::{Slab, SlabFormat, SlabRole};
        use crate::placement::topology::StorageTier;

        if std::env::var("FENCE_OFF_239").is_ok() {
            crate::volume::fence::OFF.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        const SLOT: u64 = 64 * 1024;
        const EXTENTS: u64 = 1024;
        const BLOCK: usize = 4096;
        let dir = tempfile::tempdir().unwrap();
        let slab = |name: &str, remote: bool| {
            let path = dir.path().join(name).display().to_string();
            async move {
                let mut dev = Arc::new(
                    FileDevice::open_with_capacity(&path, 3 * EXTENTS * SLOT).await.unwrap(),
                ) as Arc<dyn BlockDevice>;
                if remote {
                    let gate = Arc::new(Gate::default());
                    gate.jitter.store(true, std::sync::atomic::Ordering::Relaxed);
                    dev = Arc::new(Remote(dev, gate));
                }
                Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System))
                    .await
                    .unwrap()
            }
        };

        // The appliance's system slab, holding a golden and its clone.
        let mut mgr = VolumeManager::new(SLOT);
        let source = slab("appliance.slab", true).await;
        let source_id = source.slab_id();
        mgr.add_slab(source).await;
        let golden = mgr.create_volume_any("cni-bin.golden", EXTENTS * SLOT).await.unwrap();
        let mut seed = 0x239u64;
        let mut content = vec![0u8; (EXTENTS * SLOT) as usize];
        for c in content.chunks_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            c.copy_from_slice(&seed.to_le_bytes());
        }
        let g = mgr.get_volume(&golden).unwrap();
        g.write(0, &content).await.unwrap();
        g.flush().await.unwrap();
        mgr.seal_volume(golden, None).await.unwrap();
        let clone = mgr.create_snapshot(golden, "cni-bin").await.unwrap();
        // The local disk's system slab, laid by the install.
        let local = slab("local.slab", false).await;
        let local_id = local.slab_id();
        mgr.add_slab(local).await;

        let content = Arc::new(content);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bad = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        // Writers, each on its own blocks of the clone, keeping what they
        // wrote; readers of the golden, which must never change.
        let mut writers = Vec::new();
        for w in 0..8u64 {
            let vol = mgr.get_volume(&clone).unwrap();
            let stop = stop.clone();
            writers.push(tokio::spawn(async move {
                let mut mine: std::collections::HashMap<u64, u8> = Default::default();
                let blocks = EXTENTS * SLOT / BLOCK as u64;
                let mut x = 0x9E37_79B9u64 ^ w;
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) || n < 256 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    // Block b belongs to writer b % 8.
                    let b = ((x >> 33) % (blocks / 8)) * 8 + w;
                    let tag = (n % 250) as u8 + 1;
                    vol.write(b * BLOCK as u64, &vec![tag; BLOCK]).await.unwrap();
                    mine.insert(b, tag);
                    n += 1;
                }
                mine
            }));
        }
        let mut readers = Vec::new();
        for r in 0..4u64 {
            let vol = mgr.get_volume(&golden).unwrap();
            let (stop, content, bad) = (stop.clone(), content.clone(), bad.clone());
            readers.push(tokio::spawn(async move {
                let mut x = 0xC0FFEEu64 ^ r;
                let mut buf = vec![0u8; BLOCK];
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let off = ((x >> 33) % (EXTENTS * SLOT / BLOCK as u64)) * BLOCK as u64;
                    vol.read(off, &mut buf).await.unwrap();
                    if buf[..] != content[off as usize..off as usize + BLOCK] {
                        bad.lock().unwrap().push(format!("the golden read wrong at byte {off}"));
                    }
                }
            }));
        }

        let flowed = super::flow_system_half(
            mgr.gem(),
            mgr.registry(),
            &[source_id],
            local_id,
            || mgr.persist(),
            None,
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut wrote = std::collections::HashMap::new();
        for w in writers {
            wrote.extend(w.await.unwrap());
        }
        for r in readers {
            r.await.unwrap();
        }
        let (moved, failed) = flowed.expect("the flow-over finished");
        assert_eq!(failed, 0);
        assert!(moved >= EXTENTS, "moved {moved}");
        assert!(mgr.gem().read().await.slab_extents(source_id).is_empty(), "all moved");

        // The clone: the golden, with every write on top.
        let mut want = (*content).clone();
        for (b, tag) in &wrote {
            let at = *b as usize * BLOCK;
            want[at..at + BLOCK].fill(*tag);
        }
        let mut got = vec![0u8; want.len()];
        mgr.get_volume(&clone).unwrap().read(0, &mut got).await.unwrap();
        let mut bad = bad.lock().unwrap().clone();
        bad.truncate(8);
        for (i, (a, b)) in want.chunks(BLOCK).zip(got.chunks(BLOCK)).enumerate() {
            if a != b {
                bad.push(format!(
                    "the clone's block {i} (extent {}) reads {} where {} was expected",
                    i as u64 * BLOCK as u64 / SLOT,
                    if b.iter().all(|&v| v == 0) { "zeros".to_string() } else { format!("{:#04x}..", b[0]) },
                    if wrote.contains_key(&(i as u64)) { "a write" } else { "the golden" },
                ));
                if bad.len() > 24 {
                    break;
                }
            }
        }
        let mut gold = vec![0u8; content.len()];
        mgr.get_volume(&golden).unwrap().read(0, &mut gold).await.unwrap();
        if gold[..] != content[..] {
            bad.push("the golden changed".into());
        }
        assert!(bad.is_empty(), "{} writes; {}", wrote.len(), bad.join("\n"));
    }

    /// The interleaving itself, made to happen (#239): an I/O finds its slot
    /// in the map and is held inside the device read; the flow-over moves
    /// that slot and frees it — a discard, zeros — and then the I/O reads.
    /// A read of the golden and a copy-on-write of the clone, both.
    ///
    /// With the fence the move waits for the I/O, and both read the golden's
    /// bytes. `FENCE_OFF_239=1` shows what it did before: zeros.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_io_that_found_its_slot_before_a_move_reads_what_was_there() {
        use crate::drive::filedev::FileDevice;
        use crate::drive::slab::{Slab, SlabFormat, SlabRole};
        use crate::placement::topology::StorageTier;

        if std::env::var("FENCE_OFF_239").is_ok() {
            crate::volume::fence::OFF.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        const SLOT: u64 = 64 * 1024;
        const EXTENTS: u64 = 16;
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Gate::default());
        let open = |name: &str| {
            let path = dir.path().join(name).display().to_string();
            async move {
                Arc::new(FileDevice::open_with_capacity(&path, 4 * EXTENTS * SLOT).await.unwrap())
                    as Arc<dyn BlockDevice>
            }
        };
        let fmt = || SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System);
        let remote = Arc::new(Remote(open("appliance.slab").await, gate.clone())) as Arc<dyn BlockDevice>;
        let source = Slab::format_with(remote, fmt()).await.unwrap();
        let source_id = source.slab_id();
        let mut mgr = VolumeManager::new(SLOT);
        mgr.add_slab(source).await;
        let golden = mgr.create_volume_any("cni-bin.golden", EXTENTS * SLOT).await.unwrap();
        let content: Vec<u8> = (0..EXTENTS * SLOT).map(|i| (i / 4096 % 251) as u8 + 1).collect();
        let g = mgr.get_volume(&golden).unwrap();
        g.write(0, &content).await.unwrap();
        g.flush().await.unwrap();
        mgr.seal_volume(golden, None).await.unwrap();
        let clone = mgr.create_snapshot(golden, "cni-bin").await.unwrap();
        let local = Slab::format_with(open("local.slab").await, fmt()).await.unwrap();
        let local_id = local.slab_id();
        mgr.add_slab(local).await;
        let mgr = Arc::new(mgr);

        let mut bad = Vec::new();
        // 1. A read of the golden. 2. The clone's first write to a shared
        // extent: a copy-on-write, which reads the whole slot first.
        for (what, vext) in [("a read of the golden", 3u64), ("a copy-on-write of the clone", 5)] {
            gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
            let io = {
                let mgr = mgr.clone();
                tokio::spawn(async move {
                    if vext == 3 {
                        let mut buf = vec![0u8; SLOT as usize];
                        mgr.get_volume(&golden).unwrap().read(vext * SLOT, &mut buf).await.unwrap();
                        buf
                    } else {
                        let v = mgr.get_volume(&clone).unwrap();
                        v.write(vext * SLOT, &[0xEE; 4096]).await.unwrap();
                        let mut buf = vec![0u8; SLOT as usize];
                        v.read(vext * SLOT, &mut buf).await.unwrap();
                        buf
                    }
                })
            };
            gate.arrived.notified().await;
            // The move, while that I/O is inside the device.
            let flow = {
                let mgr = mgr.clone();
                tokio::spawn(async move {
                    super::flow_system_half(mgr.gem(), mgr.registry(), &[source_id], local_id, || {
                        mgr.persist()
                    }, None)
                    .await
                })
            };
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            gate.go.notify_one();
            let got = io.await.unwrap();
            flow.await.unwrap().expect("the flow-over finished");
            let mut want = content[(vext * SLOT) as usize..((vext + 1) * SLOT) as usize].to_vec();
            if vext == 5 {
                want[..4096].fill(0xEE);
            }
            if got != want {
                let zeros = got.chunks(4096).filter(|b| b.iter().all(|&v| v == 0)).count();
                bad.push(format!(
                    "{what}: extent {vext} read wrong ({zeros} of {} blocks zeros)",
                    SLOT / 4096
                ));
            }
            // Put everything back on the appliance for the next case.
            mgr.registry().write().await.set_quarantined(source_id, false);
            super::flow_system_half(mgr.gem(), mgr.registry(), &[local_id], source_id, || mgr.persist(), None)
                .await
                .unwrap();
            mgr.registry().write().await.set_quarantined(local_id, false);
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    async fn digests_where_unsealed(mgr: &VolumeManager) -> BTreeMap<String, (String, Vec<u64>)> {
        let all = digests(mgr).await;
        let mut out = BTreeMap::new();
        for (id, name, ..) in mgr.list_volumes().await {
            if !mgr.is_sealed(&id) {
                if let Some(d) = all.get(&name) {
                    out.insert(name, d.clone());
                }
            }
        }
        out
    }

    /// Every MiB but the first (which the live writers own) as it was.
    async fn compare_rest(
        when: &str,
        want: &BTreeMap<String, (String, Vec<u64>)>,
        mgr: &VolumeManager,
    ) -> Vec<String> {
        let got = digests(mgr).await;
        let mut bad = Vec::new();
        for (name, (_, per)) in want {
            match got.get(name) {
                None => bad.push(format!("{when}: {name} missing")),
                Some((_, g)) => {
                    if let Some(i) = (1..per.len()).find(|&i| per[i] != g[i]) {
                        bad.push(format!("{when}: {name} differs at MiB {i}"));
                    }
                }
            }
        }
        bad
    }

    /// A write the node made during its install boot must survive the boot
    /// being cut short (#239, reopened). Until the flow-over reaches it, a
    /// clone's private extent — the slot its UUID stamp copied, where an
    /// ext4's inode table is — is on the appliance's per-boot clone, while
    /// its copy-on-writes land on the local disk. The next boot claims a
    /// fresh, pristine clone of the release to finish the flow-over from. A
    /// write left in place on the old one is gone, and the volume reads a
    /// directory that names an inode its inode table never got: what
    /// cadvisor, stormlb, vmimages, stormvm and stormimds read on 11.57
    /// (#155–#164, the first free inodes), and hubble-relay on 11.50 (#1410).
    ///
    /// `RELOCATE_OFF_239=1` writes in place as before (expected to fail).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_during_the_install_boot_survives_a_flow_over_cut_short() {
        let Some(mkfs) = mkfs_ext4() else {
            eprintln!("SKIP: needs e2fsprogs mkfs.ext4");
            return;
        };
        if std::env::var("RELOCATE_OFF_239").is_ok() {
            crate::volume::fence::RELOCATE_OFF.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).display().to_string();
        // A service golden the way stormcentral builds one, so the image's
        // clone of it is stamped and owns its first extent.
        let blank = p("svc.img");
        std::fs::File::create(&blank).unwrap().set_len(32 * MIB).unwrap();
        let st = std::process::Command::new(&mkfs)
            .args(["-q", "-F", "-b", "4096", "-O", "^has_journal", "-m", "0", &blank])
            .status()
            .unwrap();
        assert!(st.success(), "mkfs.ext4 failed");
        let spec = format!(
            r#"
name = "resume-239"
size = "512M"
[slab]
size = "rest"
[[slab.golden]]
name = "svc"
file = "{blank}"
template = true
[data_slab]
size = "64M"
"#
        );
        let image = p("image.raw");
        crate::image::ImageBuilder::new(crate::image::ImageSpec::from_toml(&spec).unwrap())
            .build(std::path::Path::new(&image))
            .await
            .unwrap();
        // Each boot claims its own copy of the release.
        let claim = |n: &str| {
            let c = p(n);
            std::fs::copy(&image, &c).unwrap();
            c
        };

        // Boot 1: lay the disk, then the successor starts the flow-over
        // (its sources quarantined) and the node writes the clone: a block in
        // its first extent, which it owns, and one in an extent it shares.
        let disk = p("disk.raw");
        std::fs::File::create(&disk).unwrap().set_len(40 * 1024 * MIB).unwrap();
        let first = claim("claim1.raw");
        let (mut mgr, _) = super::open_slabs_resuming(&[first.clone()], None, true).await.unwrap();
        let flow = super::take_local_disk(&mut mgr, &disk, "hot", true).await.unwrap().expect("laid");
        drop(mgr);
        let mut mgr = super::open_slabs_and_restore(&[first.clone(), flow.disk.clone()], None).await.unwrap();
        let dest = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.system_slab).unwrap());
        let data = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.data_slab).unwrap());
        let local: Vec<_> = [data, dest].into_iter().filter(|id| mgr.is_metadata_slab(id)).collect();
        mgr.keep_metadata_in_first(&local);
        super::quarantine_flow_sources(&mgr, &flow).await;
        let svc = super::resolve_boot_volume(&mgr, "svc").await.unwrap();
        let v = mgr.get_volume(&svc).unwrap();
        // Block 16 is in the first MiB (owned), block 600 past it (shared).
        for b in [16u64, 600] {
            v.write(b * 4096, &[0xA5; 4096]).await.unwrap();
        }
        v.flush().await.unwrap();
        mgr.persist().await;
        // Power cut: the flow-over moved nothing yet.
        drop(v);
        drop(mgr);

        // Boot 2: the disk, and a fresh claim to finish the flow-over from.
        let second = claim("claim2.raw");
        std::env::set_var("STORMBLOCK_RESUME_SOURCE", &second);
        let (mgr, resumed) = super::open_slabs_resuming(&[flow.disk.clone()], None, true).await.unwrap();
        std::env::remove_var("STORMBLOCK_RESUME_SOURCE");
        let svc = super::resolve_boot_volume(&mgr, "svc").await.unwrap();
        let v = mgr.get_volume(&svc).unwrap();
        let mut bad = Vec::new();
        for b in [16u64, 600] {
            let mut got = vec![0u8; 4096];
            v.read(b * 4096, &mut got).await.unwrap();
            if got != [0xA5; 4096] {
                bad.push(format!(
                    "block {b} ({}) is not what boot 1 wrote",
                    if b < 256 { "the clone's own first extent" } else { "a shared extent" }
                ));
            }
        }
        assert!(bad.is_empty(), "resumed from {:?}: {}", resumed.map(|r| r.uri), bad.join("; "));
    }

    /// Every volume name the slabs on `disk` record, read the way the
    /// initramfs probe reads them (`slab volumes`).
    async fn names_on_disk(disk: &str) -> std::collections::BTreeSet<String> {
        let dev = super::open_storage(disk).await.unwrap();
        let mut names = std::collections::BTreeSet::new();
        for f in crate::drive::discover::slabs_in_partitions(&dev).await {
            if let Ok(Some(bytes)) = f.slab.read_metadata().await {
                for v in crate::volume::MetadataStore::decode(&bytes).unwrap().volumes {
                    names.insert(v.name);
                }
            }
        }
        names
    }

    /// A power cut during the install's flow-over, before it moved anything
    /// (#258): the disk alone must still name every volume the node boots, or
    /// the initramfs reads it as "missing N mounted volume(s)", asks the
    /// appliance, and — with no boot intent — installs over it, destroying
    /// the data half (11.63 on server3: 0 of 300 objects). And the next boot,
    /// from the disk, finishes the flow-over from a fresh clone (#171) with
    /// every volume as the image had it and the data half's writes kept.
    ///
    /// `RECORD_FLOW_OFF_258=1` records as before (expected to fail).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_power_cut_before_the_flow_over_moves_anything_keeps_the_disk_bootable() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).display().to_string();
        let mut seed = 0x258u64;
        let mut noise = |len: u64| {
            let mut v = vec![0u8; len as usize];
            for c in v.chunks_mut(8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                c.copy_from_slice(&seed.to_le_bytes()[..c.len()]);
            }
            v
        };
        let root = p("root.img");
        std::fs::write(&root, noise(6 * MIB)).unwrap();
        let pump = p("pump.img");
        std::fs::write(&pump, noise(5 * MIB + 4096)).unwrap();
        let state = p("state.img");
        std::fs::write(&state, noise(4 * MIB)).unwrap();
        let spec = format!(
            r#"
name = "cut-258"
size = "512M"
[slab]
size = "rest"
[[slab.golden]]
name = "stormpump"
file = "{root}"
[[slab.golden]]
name = "pump-svc"
file = "{pump}"
[data_slab]
size = "64M"
[[data_slab.golden]]
name = "state"
file = "{state}"
"#
        );
        let image = p("image.raw");
        crate::image::ImageBuilder::new(crate::image::ImageSpec::from_toml(&spec).unwrap())
            .build(std::path::Path::new(&image))
            .await
            .unwrap();
        let claim = |n: &str| {
            let c = p(n);
            std::fs::copy(&image, &c).unwrap();
            c
        };

        // Boot 1, the install: lay the disk (fresh, as #236 does), then the
        // engine that adopts it starts the flow-over and the node writes its
        // state.
        let disk = p("disk.raw");
        std::fs::File::create(&disk).unwrap().set_len(40 * 1024 * MIB).unwrap();
        let first = claim("claim1.raw");
        let (mut mgr, _) = super::open_slabs_resuming(&[first.clone()], None, true).await.unwrap();
        let want = digests(&mgr).await;
        let flow = super::take_local_disk(&mut mgr, &disk, "hot", true).await.unwrap().expect("laid");
        drop(mgr);
        let mut mgr = super::open_slabs_and_restore(&[first.clone(), flow.disk.clone()], None).await.unwrap();
        let dest = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.system_slab).unwrap());
        let data = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.data_slab).unwrap());
        let local: Vec<_> = [data, dest].into_iter().filter(|id| mgr.is_metadata_slab(id)).collect();
        mgr.keep_metadata_in_first(&local);
        super::quarantine_flow_sources(&mgr, &flow).await;
        let st = super::resolve_boot_volume(&mgr, "state").await.unwrap();
        let v = mgr.get_volume(&st).unwrap();
        v.write(MIB + 8192, &[0x58; 4096]).await.unwrap();
        v.flush().await.unwrap();
        mgr.persist().await;
        // The power goes: nothing has moved yet.
        drop(v);
        drop(mgr);

        // What the probe reads off the disk: every volume, not just the ones
        // the flow-over reached.
        let on_disk = names_on_disk(&flow.disk).await;
        let missing: Vec<_> = want.keys().filter(|n| !on_disk.contains(*n)).collect();
        assert!(missing.is_empty(), "the disk does not name {missing:?} (it names {on_disk:?})");
        // And the release question the probe asks next says "the same
        // release, cut short" (exit 3: boot it), not "not held" (install).
        let held = crate::image::local::release_held(
            &super::open_storage(&flow.disk).await.unwrap(),
            &super::open_storage(&image).await.unwrap(),
        )
        .await;
        assert!(
            matches!(held, crate::image::local::ReleaseHeld::Unfinished { .. }),
            "slab holds after the cut: {held:?}"
        );

        // Boot 2 claimed as the wrong machine (#259: the SMBIOS serial the
        // blades share, another machine's release): its clone does not carry
        // the slab the records need. The boot stops, naming it, rather than
        // dropping every mapping there and coming up on a root of holes.
        let other = p("other.raw");
        crate::image::ImageBuilder::new(crate::image::ImageSpec::from_toml(&spec).unwrap())
            .build(std::path::Path::new(&other))
            .await
            .unwrap();
        std::env::set_var("STORMBLOCK_RESUME_SOURCE", &other);
        let wrong = super::open_slabs_resuming(&[flow.disk.clone()], None, true).await;
        std::env::remove_var("STORMBLOCK_RESUME_SOURCE");
        match wrong {
            Ok(_) => panic!("the boot resumed from another machine's image and came up"),
            Err(e) => assert!(e.to_string().contains("refusing to boot"), "{e}"),
        }

        // Boot 2, from the disk, finishing the flow-over from a fresh claim.
        let second = claim("claim2.raw");
        std::env::set_var("STORMBLOCK_RESUME_SOURCE", &second);
        let (mgr, resumed) = super::open_slabs_resuming(&[flow.disk.clone()], None, true).await.unwrap();
        std::env::remove_var("STORMBLOCK_RESUME_SOURCE");
        assert!(resumed.is_some(), "the boot from the disk did not resume the flow-over");
        let got = digests(&mgr).await;
        let mut bad = compare("after the cut", &want.iter().filter(|(n, _)| *n != "state").map(|(k, v)| (k.clone(), v.clone())).collect(), &got);
        let st = super::resolve_boot_volume(&mgr, "state").await.unwrap();
        let mut back = vec![0u8; 4096];
        mgr.get_volume(&st).unwrap().read(MIB + 8192, &mut back).await.unwrap();
        if back != [0x58; 4096] {
            bad.push("the data half's write before the cut is gone".into());
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The resume claims as the machine the firmware claimed as (#259):
    /// `/init`'s export, then stormbootx's `StormBootTag`, and only then the
    /// SMBIOS guess.
    #[test]
    fn the_resume_names_the_machine_as_the_firmware_did() {
        let fw = |v: &str| {
            let mut b = vec![6u8, 0, 0, 0];
            b.extend_from_slice(v.as_bytes());
            Some(b)
        };
        let serial = Some("S11075924402016\n".to_string());
        let uuid = Some("4c4c4544-0000\n".to_string());
        let t = super::machine_tag_from;
        assert_eq!(t(Some("server3".into()), fw("other"), serial.clone(), uuid.clone()).as_deref(), Some("server3"));
        assert_eq!(t(None, fw("server3"), serial.clone(), uuid.clone()).as_deref(), Some("server3"));
        assert_eq!(t(Some("  ".into()), fw("server3"), serial.clone(), None).as_deref(), Some("server3"));
        // Not a name: ignored, as /init ignores it.
        assert_eq!(t(None, fw("a/b"), serial.clone(), None).as_deref(), Some("S11075924402016"));
        assert_eq!(t(None, Some(vec![6, 0, 0, 0]), serial.clone(), None).as_deref(), Some("S11075924402016"));
        assert_eq!(t(None, None, serial, uuid.clone()).as_deref(), Some("S11075924402016"));
        assert_eq!(t(None, None, Some("To Be Filled By O.E.M.".into()), uuid).as_deref(), Some("4c4c4544-0000"));
        assert_eq!(t(None, None, None, None), None);
    }

    /// Every unsealed volume overwritten, its first 64 MiB, with noise.
    async fn noise_over_clones(mgr: &VolumeManager) {
        let mut x = 0x239u64;
        for (id, _name, size, _) in mgr.list_volumes().await {
            if mgr.is_sealed(&id) {
                continue;
            }
            let v = mgr.get_volume(&id).unwrap();
            let mut buf = vec![0u8; MIB as usize];
            let mut off = 0;
            while off < size.min(64 * MIB) {
                for c in buf.chunks_mut(8) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    c.copy_from_slice(&x.to_le_bytes());
                }
                let n = (size - off).min(MIB) as usize;
                v.write(off, &buf[..n]).await.unwrap();
                off += n as u64;
            }
            v.flush().await.unwrap();
        }
    }

    /// `compare`, for the sealed volumes only.
    async fn compare_sealed(
        when: &str,
        want: &BTreeMap<String, (String, Vec<u64>)>,
        mgr: &VolumeManager,
    ) -> Vec<String> {
        let sealed: std::collections::HashSet<String> = mgr
            .list_volumes()
            .await
            .into_iter()
            .filter(|(id, ..)| mgr.is_sealed(id))
            .map(|(_, n, ..)| n)
            .collect();
        let want: BTreeMap<_, _> = want.iter().filter(|(n, _)| sealed.contains(*n)).map(|(k, v)| (k.clone(), v.clone())).collect();
        eprintln!("{when}: checking {} sealed volumes", want.len());
        compare(when, &want, &digests_where(mgr, true).await)
    }

    /// A real release (#239, reopened): `AUDIT_239_IMAGE` names a copy of a
    /// published image (`slab_audit fetch`). Installed fresh onto an empty
    /// disk with the system half flowed over, every volume must read as the
    /// image's; then every clone is overwritten with noise (a node that ran)
    /// and the same image installed fresh over that disk, which must leave
    /// no noise behind anywhere. Not routine: it needs the image.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn a_real_release_installs_byte_for_byte() {
        let Ok(image) = std::env::var("AUDIT_239_IMAGE") else {
            eprintln!("SKIP: set AUDIT_239_IMAGE to a release image");
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "tmp".into()));
        let disk = dir.join("audit-239-disk.raw").display().to_string();
        let _ = std::fs::remove_file(&disk);
        std::fs::File::create(&disk).unwrap().set_len(200 * 1024 * MIB).unwrap();

        let mut bad = Vec::new();
        let mut before = BTreeMap::new();
        // `AUDIT_239_ROUNDS=used` runs the second only (the disk is laid
        // fresh by it all the same, over an empty file).
        let only_used = std::env::var("AUDIT_239_ROUNDS").is_ok_and(|r| r == "used");
        for round in ["fresh disk", "over a used disk"] {
            if only_used && round == "fresh disk" {
                continue;
            }
            // Every boot claims a fresh clone of the release: a pristine copy.
            let claim = dir.join("audit-239-claim.raw").display().to_string();
            let st = std::process::Command::new("cp")
                .args(["--sparse=always", "--reflink=auto", &image, &claim])
                .status()
                .unwrap();
            assert!(st.success(), "copying the image");
            let (mut mgr, _) = super::open_slabs_resuming(&[claim.clone()], None, true).await.unwrap();
            if before.is_empty() {
                before = digests(&mgr).await;
                eprintln!("{} volumes in the image", before.len());
            }
            let flow = super::take_local_disk(&mut mgr, &disk, "hot", true)
                .await
                .unwrap()
                .expect("the disk is laid");
            bad.extend(compare(&format!("{round}: after the seed"), &before, &digests(&mgr).await));
            let dest = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.system_slab).unwrap());
            let data_dest = crate::drive::slab::SlabId(uuid::Uuid::parse_str(&flow.data_slab).unwrap());
            // The second time, as on a node: the initramfs engine's consumers
            // write the clones, the engine stops, and its successor opens the
            // claim and the disk from their records (`adopt-ublk`) and runs
            // the flow-over itself. It must see every write the first made.
            let used = round == "over a used disk";
            let mut after_noise = BTreeMap::new();
            if used {
                noise_over_clones(&mgr).await;
                bad.extend(compare_sealed(&format!("{round}: goldens after the incumbent's writes"), &before, &mgr).await);
                after_noise = digests_where_unsealed(&mgr).await;
                mgr.persist().await;
                drop(mgr);
                let mut succ = super::open_slabs_and_restore(&[claim.clone(), flow.disk.clone()], None).await.unwrap();
                let local: Vec<_> = [data_dest, dest].into_iter().filter(|id| succ.is_metadata_slab(id)).collect();
                if !local.is_empty() {
                    succ.keep_metadata_in_first(&local);
                }
                let got = digests(&succ).await;
                bad.extend(compare(&format!("{round}: the successor's clones"), &after_noise, &got));
                bad.extend(compare(&format!("{round}: the successor's goldens"), &before.iter().filter(|(n, _)| !after_noise.contains_key(*n)).map(|(k, v)| (k.clone(), v.clone())).collect(), &got));
                mgr = succ;
            }
            let sources: Vec<_> = {
                let reg = mgr.registry().read().await;
                reg.iter().filter(|(id, s)| !s.is_data() && **id != dest).map(|(id, _)| *id).collect()
            };
            // And while the move runs, one writer per clone keeps rewriting
            // the blocks of its first extent — where an ext4's inode table is,
            // the block the node lost — against a model of what it wrote.
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut writers = Vec::new();
            if used {
                for (id, name, _, _) in mgr.list_volumes().await {
                    if mgr.is_sealed(&id) {
                        continue;
                    }
                    let v = mgr.get_volume(&id).unwrap();
                    let stop = stop.clone();
                    writers.push(tokio::spawn(async move {
                        let mut model = vec![0u8; MIB as usize];
                        v.read(0, &mut model).await.unwrap();
                        let mut x = 0x5EEDu64 ^ name.len() as u64;
                        let mut n = 0u64;
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) || n < 64 {
                            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                            let b = ((x >> 33) % 256) as usize;
                            let blk = vec![(n % 251) as u8 + 1; 4096];
                            v.write(b as u64 * 4096, &blk).await.unwrap();
                            model[b * 4096..(b + 1) * 4096].copy_from_slice(&blk);
                            n += 1;
                        }
                        v.flush().await.unwrap();
                        (name, v, model, n)
                    }));
                }
            }
            let (moved, failed) =
                super::flow_system_half(mgr.gem(), mgr.registry(), &sources, dest, || mgr.persist(), None)
                    .await
                    .expect("the flow-over finished");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut models = BTreeMap::new();
            for w in writers {
                let (name, v, model, n) = w.await.unwrap();
                let mut got = vec![0u8; MIB as usize];
                v.read(0, &mut got).await.unwrap();
                let lost: Vec<usize> =
                    (0..256).filter(|&b| got[b * 4096..(b + 1) * 4096] != model[b * 4096..(b + 1) * 4096]).collect();
                if !lost.is_empty() {
                    bad.push(format!("{round}: clone {name}: {} of 256 blocks of extent 0 not as written ({n} writes), first {:?}", lost.len(), &lost[..lost.len().min(8)]));
                }
                models.insert(name, model);
            }
            eprintln!("{round}: moved {moved}, failed {failed}, {} live writers", models.len());
            if used {
                bad.extend(compare_rest(&format!("{round}: clones after the flow-over"), &after_noise, &mgr).await);
                bad.extend(compare_sealed(&format!("{round}: goldens after the flow-over"), &before, &mgr).await);
            } else {
                bad.extend(compare(&format!("{round}: after the flow-over"), &before, &digests(&mgr).await));
            }
            if used {
                bad.extend(compare_sealed(&format!("{round}: goldens after writes in the engine that moved them"), &before, &mgr).await);
                mgr.persist().await;
            }
            drop(mgr);

            let (local, _) = super::open_slabs_resuming(&[flow.disk.clone()], None, false).await.unwrap();
            if used {
                bad.extend(compare_sealed(&format!("{round}: goldens on the disk alone"), &before, &local).await);
                for (name, model) in &models {
                    let Ok(id) = super::resolve_boot_volume(&local, name).await else {
                        bad.push(format!("{round}: clone {name} missing on the disk alone"));
                        continue;
                    };
                    let mut got = vec![0u8; MIB as usize];
                    local.get_volume(&id).unwrap().read(0, &mut got).await.unwrap();
                    if &got != model {
                        bad.push(format!("{round}: clone {name}: extent 0 on the disk alone is not what was written"));
                    }
                }
            } else {
                bad.extend(compare(&format!("{round}: the disk alone"), &before, &digests(&local).await));
            }
            // A node that ran: every clone written over, and through the
            // live engine first, the map the move just rewrote. No golden
            // may change: a clone that writes in place into a slot it shares
            // is a golden that reads as the clone (#239).
            if round == "fresh disk" {
                drop(local);
                let (live, _) = super::open_slabs_resuming(&[flow.disk.clone()], None, false).await.unwrap();
                noise_over_clones(&live).await;
                bad.extend(compare_sealed(&format!("{round}: goldens after the clones were written"), &before, &live).await);
                live.persist().await;
                drop(live);
                let (local, _) = super::open_slabs_resuming(&[flow.disk.clone()], None, false).await.unwrap();
                bad.extend(compare_sealed(&format!("{round}: goldens after a reopen"), &before, &local).await);
                drop(local);
                continue;
            }
            drop(local);
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}

/// Forge mode on an adopting engine (#206): what a bastion's `adopt-ublk`
/// does with an `[nvmeof]` section, and what it does without one.
#[cfg(all(test, feature = "nvmeof"))]
mod forge_mode_tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
    use crate::mgmt::config::NvmeofExportConfig;

    const MIB: u64 = 1024 * 1024;
    const SHARED: &str = "nqn.2026-10.test:bastion";

    fn section(listen: &str) -> NvmeofExportConfig {
        NvmeofExportConfig {
            listen_addr: listen.into(),
            nqn: SHARED.into(),
            export_drives: true,
            allow_any_host: false,
            allowed_hosts: Vec::new(),
            require_dhchap: false,
            boothost_host_nqn: None,
        }
    }

    /// A stormcos node's own config has no `[nvmeof]`: no target, no port.
    #[test]
    fn no_section_no_target() {
        assert!(adopted_nvmeof_target(&StormBlockConfig::default()).unwrap().is_none());
        let mut c = StormBlockConfig::default();
        c.nvmeof = Some(section("not an address"));
        assert!(adopted_nvmeof_target(&c).is_err(), "a bad listen_addr is said, not guessed");
    }

    /// With `[nvmeof]`, a boothost claim answers with an NVMe/TCP attach a
    /// booting machine can connect to as its host NQN, and read the image.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_bastion_answers_a_boot_claim_with_something_to_attach() {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut config = StormBlockConfig::default();
        config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
        config.management.advertised_addr = Some("127.0.0.1".into());
        config.nvmeof = Some(section(&format!("127.0.0.1:{port}")));

        let mut vm = VolumeManager::new(MIB);
        let array = RaidArrayId(uuid::Uuid::new_v4());
        let dev = FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 128 * MIB)
            .await
            .unwrap();
        vm.add_backing_device(array, Arc::new(dev)).await;
        let golden = vm.create_volume("release-11.79", 8 * MIB, array).await.unwrap();
        let image: Vec<u8> = (0..MIB as usize).map(|i| (i % 253) as u8).collect();
        let g = vm.get_volume(&golden).unwrap();
        g.write(0, &image).await.unwrap();
        g.flush().await.unwrap();
        vm.seal_volume(golden, None).await.unwrap();
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        let state = Arc::new(AppState::new(config.clone(), vm, reg, gem));

        // What adopt-ublk does with the config.
        let target = adopted_nvmeof_target(&config).unwrap().expect("[nvmeof] starts a target");
        let reactor = Arc::new(ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false }));
        serve_shared_nvmeof(&state, &reactor, Arc::new(target)).await;
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = listener.local_addr().unwrap();
        let router = mgmt::api::router(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let c = reqwest::Client::new();
        let r = c
            .post(format!("http://{api}/api/v1/synonyms"))
            .json(&serde_json::json!({"namespace": "boothost", "name": "server8", "volume": golden.0.to_string()}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "boothost synonym: {}", r.status());
        let claim: serde_json::Value = c
            .post(format!("http://{api}/api/v1/synonyms/boothost/server8/claim"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let attach = &claim["attach"];
        assert_eq!(attach["protocol"], "nvme-tcp", "the claim names something to attach: {claim}");
        assert_eq!(attach["port"], port);
        let nqn = attach["nqn"].as_str().unwrap().to_string();
        let nsid = attach["nsid"].as_u64().unwrap() as u32;
        let host = attach["host_nqns"][0].as_str().unwrap().to_string();

        // The machine connects as itself and reads its image.
        let spec = NvmeTcpSpec { addr: addr.to_string(), nqn, nsid, host_nqn: Some(host), dhchap: None };
        let dev = NvmeofDevice::connect(&spec).await.expect("the booting machine connects");
        let mut back = vec![0u8; MIB as usize];
        dev.read(0, &mut back).await.unwrap();
        assert_eq!(back, image, "the clone reads as the release it was claimed from");
    }
}
