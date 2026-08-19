//! Target-driven space reclamation (issue #3).
//!
//! Measured live: thin allocation went 72 → 116 MB across formats,
//! extractions and whole-tree deletes and never came back down. The engine's
//! UNMAP path is complete — `handle_unmap` → `BlockDevice::discard` → GEM
//! `remove` + slab `dec_ref` — but the initiator never sends one, because
//! Linux's `sd` only enables discard when the target advertises VPD page B2h
//! and the engine advertises LBPME without it. That is an engine gap (filed
//! upstream; mk never patches the engine), so mk reclaims from its own end:
//! read the filesystem's free-block bitmaps and discard the whole slots they
//! cover.
//!
//! Safety rules, in order of importance:
//!
//! 1. Never trim a volume with a live session. The bitmap would be a moving
//!    target and a stale "free" bit is a discarded live block.
//! 2. Never trim a filesystem that is not cleanly unmounted (journal replay
//!    pending means the bitmaps on disk are not authoritative).
//! 3. Never trim the first slot, which carries the superblock and the group
//!    descriptor table.
//! 4. Report first, act only on an explicit `apply`.

use std::sync::Arc;

use crate::drive::BlockDevice;

use crate::fs::ext4_free as ext4;

#[derive(Debug, Clone, Default)]
pub struct TrimReport {
    pub scanned: bool,
    pub applied: bool,
    pub fs_free_bytes: u64,
    pub reclaimable_bytes: u64,
    pub discarded_bytes: u64,
    pub allocated_before: u64,
    pub allocated_after: u64,
    pub slots: u64,
    pub groups_scanned: u64,
    pub groups_skipped: u64,
    pub clean: bool,
}

impl TrimReport {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "scanned": self.scanned,
            "applied": self.applied,
            "clean": self.clean,
            "fs_free_bytes": self.fs_free_bytes,
            "reclaimable_bytes": self.reclaimable_bytes,
            "discarded_bytes": self.discarded_bytes,
            "allocated_before": self.allocated_before,
            "allocated_after": self.allocated_after,
            "freed_bytes": self.allocated_before.saturating_sub(self.allocated_after),
            "slots": self.slots,
            "groups_scanned": self.groups_scanned,
            "groups_skipped": self.groups_skipped,
        })
    }
}

/// Scan a volume and, when `apply` is set, discard every whole free slot.
///
/// `slot_size` must be the volume manager's allocation granularity: discards
/// smaller than a slot are dropped by the engine, so anything else would
/// report reclaim that never happens.
pub async fn trim(
    dev: &Arc<dyn BlockDevice>,
    slot_size: u64,
    allocated_before: u64,
    apply: bool,
    require_clean: bool,
) -> anyhow::Result<TrimReport> {
    let map = ext4::scan(dev, slot_size).await?;
    let clean = map.layout.as_ref().map(|l| l.clean).unwrap_or(false);

    let mut report = TrimReport {
        scanned: true,
        clean,
        fs_free_bytes: map.free_bytes,
        reclaimable_bytes: map.reclaimable_bytes,
        allocated_before,
        allocated_after: allocated_before,
        groups_scanned: map.groups_scanned,
        groups_skipped: map.groups_skipped,
        ..Default::default()
    };

    if !apply {
        return Ok(report);
    }
    if require_clean && !clean {
        anyhow::bail!(
            "filesystem is not cleanly unmounted — refusing to trim (pass force=true only if you are certain nothing is mounted)"
        );
    }

    for (start, len) in &map.runs {
        let Some((off, span)) = ext4::aligned_span(*start, *len, slot_size) else { continue };
        dev.discard(off, span)
            .await
            .map_err(|e| anyhow::anyhow!("discard {off}+{span}: {e}"))?;
        report.discarded_bytes += span;
        report.slots += span / slot_size;
    }
    dev.flush().await.map_err(|e| anyhow::anyhow!("flush after trim: {e}"))?;
    report.applied = true;
    Ok(report)
}
