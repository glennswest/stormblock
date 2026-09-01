//! Live migration orchestrator — RAID 1 add/remove and slab-based extent migration.
//!
//! Two migration paths:
//!
//! 1. **RAID-level** (`migrate_to_local`): Remote → local via RAID 1 add/rebuild/remove.
//!    The system never notices because all I/O flows through the BlockDevice trait.
//!
//! 2. **Slab-level** (`migrate_to_slab`): Format a new device as a slab, register it,
//!    then evacuate all extents from a source slab. Extent-granularity migration
//!    using the placement engine.

use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::drive::slab::{Slab, SlabId};
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::topology::StorageTier;
use crate::placement::PlacementEngine;
use crate::raid::{RaidArray, RaidLevel};
use crate::volume::gem::GlobalExtentMap;

/// How long a rebuild may take before the migration gives up.
///
/// Generous, because a rebuild is a whole-volume copy and a slow disk is not a
/// broken one. Finite, because the alternative — the previous behaviour — is a
/// drain that never ends and a node that never finishes leaving.
const REBUILD_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(6 * 60 * 60);

/// Errors during migration.
#[derive(Debug)]
pub enum MigrateError {
    RaidAdd(String),
    RaidRemove(String),
    /// The rebuild did not finish. The source is untouched — this error is the
    /// difference between a migration that stopped and one that destroyed the
    /// copy it was migrating.
    RebuildFailed(String),
    NotRaid1,
    SlabFormat(String),
    Evacuate(String),
    Other(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::RaidAdd(e) => write!(f, "RAID add member failed: {e}"),
            MigrateError::RebuildFailed(e) => write!(f, "rebuild did not complete: {e}"),
            MigrateError::RaidRemove(e) => write!(f, "RAID remove member failed: {e}"),
            MigrateError::NotRaid1 => write!(f, "migration requires RAID 1 array"),
            MigrateError::SlabFormat(e) => write!(f, "slab format failed: {e}"),
            MigrateError::Evacuate(e) => write!(f, "slab evacuation failed: {e}"),
            MigrateError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// Result of a completed slab-level migration.
pub struct SlabMigrateResult {
    pub source_slab: SlabId,
    pub dest_slab: SlabId,
    pub migrated: u64,
    pub failed: u64,
}

/// Migrate a volume's backing from a remote device to a local disk via RAID 1.
///
/// This is the full sanboot -> local flow:
/// 1. Add local device as RAID 1 partner (triggers background rebuild)
/// 2. Wait for rebuild to complete
/// 3. Remove the old (remote) member
///
/// The array must be RAID 1. The system keeps running throughout --
/// RAID 1 transparently mirrors I/O to both legs during rebuild.
pub async fn migrate_to_local(
    array: Arc<RaidArray>,
    local_device: Arc<dyn BlockDevice>,
    remote_member_uuid: uuid::Uuid,
) -> Result<(), MigrateError> {
    // Verify RAID 1
    if array.level() != RaidLevel::Raid1 {
        return Err(MigrateError::NotRaid1);
    }

    // Add as RAID 1 partner — this starts background rebuild
    tracing::info!("Migration: adding local device as RAID 1 partner");
    let member_uuid = array.add_member(local_device).await
        .map_err(|e| MigrateError::RaidAdd(e.to_string()))?;
    let _ = member_uuid;

    // Wait for the rebuild to finish, on an **explicit outcome**.
    //
    // This loop used to end when nothing was `Rebuilding`, and then remove the
    // remote member. "Nothing is rebuilding" is the absence of evidence, not
    // success: any state that was neither `Active` nor `Rebuilding` — `Failed`
    // included — left the loop, logged "rebuild complete", and deleted the
    // only copy that was not on the machine being drained. A failed rebuild
    // destroyed the data it was migrating (#69).
    //
    // So: synced, failed, or timed out, and nothing else. The old leg is
    // removed only on the first of those.
    tracing::info!("Migration: waiting for RAID 1 rebuild to complete...");
    let deadline = tokio::time::Instant::now() + REBUILD_TIMEOUT;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let states = array.member_states();

        // The new member is what is being waited on. Every member active is
        // the only positive statement that the copy is complete.
        if states.iter().all(|(_, s)| s.to_string() == "Active") {
            break;
        }

        // Any failure ends it, and the source is left alone. Removing the
        // remote member here is exactly the bug.
        if let Some((uuid, _)) =
            states.iter().find(|(_, s)| s.to_string() == "failed" || s.to_string() == "Failed")
        {
            return Err(MigrateError::RebuildFailed(format!(
                "member {uuid} failed during rebuild; the remote member is untouched \
                 and still holds the data"
            )));
        }

        let rebuilding = states.iter().filter(|(_, s)| s.to_string() == "Rebuilding").count();
        if rebuilding == 0 {
            // Neither finished nor failed nor rebuilding: unknown, and unknown
            // is not success. Previously this was the silent exit.
            return Err(MigrateError::RebuildFailed(format!(
                "no member is rebuilding and not all are active: {states:?} — refusing to \
                 remove the remote member on an unclear outcome"
            )));
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(MigrateError::RebuildFailed(format!(
                "rebuild did not finish within {REBUILD_TIMEOUT:?}; the remote member is \
                 untouched and still holds the data"
            )));
        }
        tracing::info!("Migration: {rebuilding} member(s) still rebuilding");
    }
    tracing::info!("Migration: rebuild complete — every member active");

    // Remove the remote member
    tracing::info!("Migration: removing remote member {}", remote_member_uuid);
    array.remove_member(remote_member_uuid).await
        .map_err(|e| MigrateError::RaidRemove(e.to_string()))?;

    tracing::info!("Migration complete: volume now running on local device");

    Ok(())
}

/// Migrate extents from one slab to a new device via the placement engine.
///
/// Flow:
/// 1. Format `dest_device` as a new Slab
/// 2. Register it in the `SlabRegistry`
/// 3. Call `engine.evacuate_slab()` to move all extents off the source slab
/// 4. Return migration result
///
/// The destination device can be on any tier. Extents from the source slab
/// will flow to the new slab (and any other available slabs if needed).
#[allow(clippy::too_many_arguments)]
pub async fn migrate_to_slab(
    gem: &mut GlobalExtentMap,
    registry: &mut SlabRegistry,
    engine: &PlacementEngine,
    source_slab: SlabId,
    dest_device: Arc<dyn BlockDevice>,
    dest_tier: StorageTier,
    slot_size: u64,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Result<SlabMigrateResult, MigrateError> {
    // Format destination device as a new slab
    tracing::info!(
        "slab migration: formatting destination device as {} slab",
        dest_tier
    );
    let slab = Slab::format(dest_device, slot_size, dest_tier).await
        .map_err(|e| MigrateError::SlabFormat(e.to_string()))?;
    let dest_slab_id = slab.slab_id();

    // Register in SlabRegistry
    registry.add(slab);
    tracing::info!(
        "slab migration: registered new slab {}, evacuating source {}",
        dest_slab_id, source_slab
    );

    // Evacuate source slab
    let result = engine.evacuate_slab(gem, registry, source_slab, shutdown).await
        .map_err(|e| MigrateError::Evacuate(e.to_string()))?;

    tracing::info!(
        "slab migration complete: {} migrated, {} failed",
        result.migrated, result.failed
    );
    // Name what was left behind. An evacuation runs against a slab that is
    // going away, so an extent that did not move is data about to be lost,
    // and a count does not say which (#67).
    for f in &result.failures {
        tracing::error!(
            "slab migration left {} {} of volume {} behind: {}",
            if f.parity { "stripe" } else { "extent" },
            f.vext_idx,
            f.volume_id,
            f.error
        );
    }

    Ok(SlabMigrateResult {
        source_slab,
        dest_slab: dest_slab_id,
        migrated: result.migrated,
        failed: result.failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
    use crate::volume::extent::VolumeId;
    use crate::volume::gem::ExtentLocation;

    #[test]
    fn migrate_error_display() {
        assert!(MigrateError::NotRaid1.to_string().contains("RAID 1"));
        assert!(MigrateError::SlabFormat("disk error".into()).to_string().contains("slab format"));
        assert!(MigrateError::Evacuate("no dest".into()).to_string().contains("evacuation"));
    }

    #[tokio::test]
    async fn test_migrate_to_slab() {
        let dir = std::env::temp_dir().join("stormblock-migrate-slab-test");
        std::fs::create_dir_all(&dir).unwrap();
        let test_id = uuid::Uuid::new_v4().simple().to_string();

        // Create source device + slab
        let src_path = dir.join(format!("{test_id}-src.bin"));
        let _ = std::fs::remove_file(&src_path);
        let src_dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(src_path.to_str().unwrap(), 10 * 1024 * 1024)
                .await.unwrap()
        );
        let mut src_slab = Slab::format(src_dev, DEFAULT_SLOT_SIZE, StorageTier::Hot)
            .await.unwrap();
        let src_slab_id = src_slab.slab_id();

        // Allocate and write data to 3 slots in source slab
        let vol = VolumeId::new();
        let slot0 = src_slab.allocate(vol, 0).await.unwrap();
        let slot1 = src_slab.allocate(vol, 1).await.unwrap();
        let slot2 = src_slab.allocate(vol, 2).await.unwrap();

        src_slab.write_slot(slot0, 0, &vec![0xAA; 4096]).await.unwrap();
        src_slab.write_slot(slot1, 0, &vec![0xBB; 4096]).await.unwrap();
        src_slab.write_slot(slot2, 0, &vec![0xCC; 4096]).await.unwrap();

        // Set up GEM with these extents
        let mut gem = GlobalExtentMap::new();
        gem.insert(vol, 0, ExtentLocation::new(src_slab_id, slot0));
        gem.insert(vol, 1, ExtentLocation::new(src_slab_id, slot1));
        gem.insert(vol, 2, ExtentLocation::new(src_slab_id, slot2));

        let mut registry = SlabRegistry::new();
        registry.add(src_slab);

        let engine = PlacementEngine::new();

        // Create destination device
        let dest_path = dir.join(format!("{test_id}-dst.bin"));
        let _ = std::fs::remove_file(&dest_path);
        let dest_dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(dest_path.to_str().unwrap(), 10 * 1024 * 1024)
                .await.unwrap()
        );

        let (_tx, rx) = tokio::sync::watch::channel(false);

        let result = migrate_to_slab(
            &mut gem, &mut registry, &engine,
            src_slab_id, dest_dev, StorageTier::Warm, DEFAULT_SLOT_SIZE,
            &rx,
        ).await.unwrap();

        assert_eq!(result.source_slab, src_slab_id);
        assert_eq!(result.migrated, 3);
        assert_eq!(result.failed, 0);

        // Verify GEM points to new slab
        let loc0 = gem.lookup(vol, 0).unwrap();
        assert_eq!(loc0.slab_id, result.dest_slab);
        let loc1 = gem.lookup(vol, 1).unwrap();
        assert_eq!(loc1.slab_id, result.dest_slab);
        let loc2 = gem.lookup(vol, 2).unwrap();
        assert_eq!(loc2.slab_id, result.dest_slab);

        // Verify data integrity via new slab
        let dest_slab = registry.get(&result.dest_slab).unwrap();
        let mut buf = vec![0u8; 4096];
        dest_slab.read_slot(loc0.slot_idx, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));
        dest_slab.read_slot(loc1.slot_idx, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));
        dest_slab.read_slot(loc2.slot_idx, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xCC));

        // Source slab should have no extents
        assert!(gem.slab_extents(src_slab_id).is_empty());

        // Cleanup
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&dest_path);
    }
}
