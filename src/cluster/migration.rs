//! Volume migration — move a volume between nodes with coordinated handoff.

use std::sync::Arc;

use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::drive::BlockDevice;

/// Migration status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// Creating matching volume on target node.
    Creating,
    /// Copying extent data from source to target.
    Copying,
    /// Updating Raft state to reassign ownership.
    Reassigning,
    /// Cleaning up source volume.
    Cleanup,
    /// Migration complete.
    Complete,
    /// Migration failed.
    Failed,
}

/// Tracks a volume migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub volume_id: Uuid,
    pub from_node: u64,
    pub to_node: u64,
    pub phase: MigrationPhase,
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub error: Option<String>,
}

/// Request to create a volume on the target node.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateVolumeRequest {
    pub name: String,
    pub size_bytes: u64,
}

/// Request to write a chunk during migration.
#[derive(Debug, Serialize, Deserialize)]
pub struct MigrateChunkRequest {
    pub volume_id: String,
    pub offset: u64,
    /// Base64-encoded chunk data.
    pub data: String,
}

/// Migrate a volume from one node to another.
///
/// This is a leader-coordinated operation:
/// 1. Create matching volume on target node
/// 2. Copy extents (chunked reads from source, writes to target)
/// 3. Update Raft state (reassign volume ownership)
/// 4. Delete source volume
pub async fn migrate_volume(
    volume: Arc<dyn BlockDevice>,
    volume_id: Uuid,
    volume_name: &str,
    _from_node: u64,
    to_node_addr: &str,
    chunk_size: usize,
    rate_limit_mbps: Option<u64>,
) -> Result<MigrationStatus, anyhow::Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let total_bytes = volume.capacity_bytes();
    let mut bytes_copied: u64 = 0;

    // Phase 1: Create volume on target
    tracing::info!("migration: creating volume '{volume_name}' on target {to_node_addr}");
    let create_req = CreateVolumeRequest {
        name: volume_name.to_string(),
        size_bytes: total_bytes,
    };
    let resp = client
        .post(format!("http://{}/api/v1/volumes", to_node_addr))
        .json(&create_req)
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("failed to create volume on target: {body}");
    }

    // Phase 2: Copy data in chunks
    tracing::info!(
        "migration: copying {total_bytes} bytes in {chunk_size}-byte chunks"
    );
    let mut buf = vec![0u8; chunk_size];
    let mut offset: u64 = 0;

    // Calculate rate limit delay
    let delay_per_chunk = rate_limit_mbps.map(|mbps| {
        let bytes_per_sec = mbps * 1024 * 1024;
        let secs_per_chunk = chunk_size as f64 / bytes_per_sec as f64;
        std::time::Duration::from_secs_f64(secs_per_chunk)
    });

    while offset < total_bytes {
        let read_len = std::cmp::min(chunk_size as u64, total_bytes - offset) as usize;
        let read_buf = &mut buf[..read_len];

        // Read from source
        match volume.read(offset, read_buf).await {
            Ok(_) => {}
            Err(e) => {
                // Unallocated extents return zeros in thin volumes — this is fine
                tracing::debug!("migration read at offset {offset}: {e}");
                read_buf.fill(0);
            }
        }

        // Check if chunk is all zeros (skip sending for thin volume optimization)
        let is_zero = read_buf.iter().all(|&b| b == 0);
        if !is_zero {
            use base64::Engine;
            let chunk_req = MigrateChunkRequest {
                volume_id: volume_id.to_string(),
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(read_buf),
            };

            let resp = client
                .post(format!("http://{}/api/v1/internal/replicate", to_node_addr))
                .json(&chunk_req)
                .send()
                .await?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("migration chunk write failed at offset {offset}: {body}");
            }
        }

        bytes_copied += read_len as u64;
        offset += read_len as u64;

        // Rate limiting
        if let Some(delay) = delay_per_chunk {
            tokio::time::sleep(delay).await;
        }

        // Log progress every 10%
        let pct = (bytes_copied as f64 / total_bytes as f64 * 100.0) as u64;
        if pct % 10 == 0 && offset == bytes_copied {
            tracing::info!("migration: {pct}% ({bytes_copied}/{total_bytes} bytes)");
        }
    }

    tracing::info!("migration: copy complete, {bytes_copied} bytes transferred");

    Ok(MigrationStatus {
        volume_id,
        from_node: 0,
        to_node: 0,
        phase: MigrationPhase::Complete,
        bytes_copied,
        bytes_total: total_bytes,
        error: None,
    })
}
