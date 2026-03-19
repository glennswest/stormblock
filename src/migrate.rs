//! Live migration orchestrator — iSCSI → local disk via RAID 1 add/remove.
//!
//! Migrates a running root volume from a remote iSCSI device to a local disk
//! by leveraging RAID 1 add_member/remove_member. The system never notices
//! because all I/O flows through StormBlock's BlockDevice trait.

use std::sync::Arc;

use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::drive::pool::DiskPool;
use crate::raid::{RaidArray, RaidLevel};

/// Errors during migration.
#[derive(Debug)]
pub enum MigrateError {
    PoolFormat(String),
    VDriveCreate(String),
    RaidAdd(String),
    RaidRemove(String),
    NotRaid1,
    Other(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::PoolFormat(e) => write!(f, "pool format failed: {e}"),
            MigrateError::VDriveCreate(e) => write!(f, "VDrive creation failed: {e}"),
            MigrateError::RaidAdd(e) => write!(f, "RAID add member failed: {e}"),
            MigrateError::RaidRemove(e) => write!(f, "RAID remove member failed: {e}"),
            MigrateError::NotRaid1 => write!(f, "migration requires RAID 1 array"),
            MigrateError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// Result of a completed migration.
pub struct MigrateResult {
    pub pool_uuid: Uuid,
    pub vdrive_uuid: Uuid,
    pub removed_member: Uuid,
}

/// Migrate a volume's backing from a remote device to a local disk.
///
/// This is the full sanboot → local flow:
/// 1. Format local disk as DiskPool
/// 2. Create VDrive matching the array capacity
/// 3. Add VDrive as RAID 1 partner (triggers background rebuild)
/// 4. Wait for rebuild to complete
/// 5. Remove the old (remote) member
///
/// The array must be RAID 1. The system keeps running throughout —
/// RAID 1 transparently mirrors I/O to both legs during rebuild.
pub async fn migrate_to_local(
    array: Arc<RaidArray>,
    local_device: Arc<dyn BlockDevice>,
    local_device_path: &str,
    remote_member_uuid: Uuid,
    vdrive_label: &str,
) -> Result<MigrateResult, MigrateError> {
    // Verify RAID 1
    if array.level() != RaidLevel::Raid1 {
        return Err(MigrateError::NotRaid1);
    }

    tracing::info!("Migration: formatting local disk as DiskPool");
    let mut pool = DiskPool::format(local_device.clone(), local_device_path).await
        .map_err(|e| MigrateError::PoolFormat(e.to_string()))?;
    let pool_uuid = pool.pool_uuid();

    // Create VDrive sized to match the array's usable capacity
    let needed_size = array.capacity_bytes();
    tracing::info!("Migration: creating VDrive ({} bytes) on local pool", needed_size);
    let vdrive_entry = pool.create_vdrive(needed_size, vdrive_label).await
        .map_err(|e| MigrateError::VDriveCreate(e.to_string()))?;
    let vdrive_uuid = vdrive_entry.uuid;

    // Open the VDrive as a BlockDevice
    let vdrive = pool.open_vdrive(&vdrive_uuid)
        .map_err(|e| MigrateError::VDriveCreate(e.to_string()))?;
    let vdrive_device: Arc<dyn BlockDevice> = Arc::new(vdrive);

    // Add as RAID 1 partner — this starts background rebuild
    tracing::info!("Migration: adding local VDrive as RAID 1 partner");
    let member_uuid = array.add_member(vdrive_device).await
        .map_err(|e| MigrateError::RaidAdd(e.to_string()))?;
    let _ = member_uuid;

    // Wait for rebuild to complete by polling member states
    tracing::info!("Migration: waiting for RAID 1 rebuild to complete...");
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let states = array.member_states();
        let all_active = states.iter().all(|(_, s)| s.to_string() == "Active");
        if all_active {
            break;
        }
        let rebuilding: Vec<_> = states.iter()
            .filter(|(_, s)| s.to_string() == "Rebuilding")
            .collect();
        if rebuilding.is_empty() {
            break;
        }
        tracing::info!("Migration: {} member(s) still rebuilding", rebuilding.len());
    }
    tracing::info!("Migration: rebuild complete");

    // Remove the remote member
    tracing::info!("Migration: removing remote member {}", remote_member_uuid);
    array.remove_member(remote_member_uuid).await
        .map_err(|e| MigrateError::RaidRemove(e.to_string()))?;

    // Mark VDrive as in-array
    pool.set_vdrive_in_array(vdrive_uuid, Uuid::nil()).await
        .map_err(|e| MigrateError::Other(e.to_string()))?;

    tracing::info!("Migration complete: volume now running on local VDrive");

    Ok(MigrateResult {
        pool_uuid,
        vdrive_uuid,
        removed_member: remote_member_uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_error_display() {
        assert!(MigrateError::NotRaid1.to_string().contains("RAID 1"));
        assert!(MigrateError::PoolFormat("test".into()).to_string().contains("pool format"));
    }
}
