//! StormBlock — Pure Rust Enterprise Block Storage Engine
//!
//! Single binary serving NVMe-oF/TCP and iSCSI targets from
//! NVMe SSDs (VFIO userspace) and SAS drives (io_uring).

mod drive;
mod raid;
mod volume;
mod target;
mod mgmt;

use clap::Parser;

#[derive(Parser)]
#[command(name = "stormblock", version, about = "Pure Rust block storage engine")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/stormblock/stormblock.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    tracing::info!("StormBlock starting, config: {}", cli.config);

    // TODO: Phase 1 implementation
    // 1. Parse config
    // 2. Initialize NVMe userspace driver (VFIO)
    // 3. Enumerate SAS drives (io_uring)
    // 4. Load/create RAID arrays
    // 5. Load/create volume manager metadata
    // 6. Start NVMe-oF/TCP target (:4420)
    // 7. Start iSCSI target (:3260)
    // 8. Start management API (:8443)

    Ok(())
}
