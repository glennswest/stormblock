//! Volume replication — sync and async write replication across nodes.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::drive::{
    BlockDevice, DeviceId, DriveType, DriveError, DriveResult, SmartData,
};

/// Internal replication request sent between nodes.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplicateRequest {
    pub volume_id: String,
    pub offset: u64,
    /// Base64-encoded write data.
    pub data: String,
}

/// Internal replication response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplicateResponse {
    pub ok: bool,
    pub error: Option<String>,
}

/// A volume wrapper that replicates writes to peer nodes.
///
/// Implements `BlockDevice` — target protocols (iSCSI, NVMe-oF) see it as a normal device.
/// Reads are always served locally. Writes are sent to replicas based on replication mode.
pub struct ReplicatedVolume {
    /// The local volume.
    inner: Arc<dyn BlockDevice>,
    /// Volume ID for replication protocol.
    volume_id: Uuid,
    /// Peer node addresses that hold replicas.
    replica_addrs: Vec<String>,
    /// True = sync (wait for all replicas), false = async (fire and forget).
    sync_mode: bool,
    /// HTTP client for replica RPCs.
    client: reqwest::Client,
    /// Channel for async replication queue (used only in async mode).
    async_tx: Option<tokio::sync::mpsc::UnboundedSender<ReplicateRequest>>,
}

impl ReplicatedVolume {
    /// Create a new replicated volume.
    /// If `sync_mode` is false, spawns a background replication task.
    pub fn new(
        inner: Arc<dyn BlockDevice>,
        volume_id: Uuid,
        replica_addrs: Vec<String>,
        sync_mode: bool,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        let async_tx = if !sync_mode && !replica_addrs.is_empty() {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ReplicateRequest>();
            let addrs = replica_addrs.clone();
            let bg_client = client.clone();
            tokio::spawn(async_replication_task(rx, addrs, bg_client));
            Some(tx)
        } else {
            None
        };

        ReplicatedVolume {
            inner,
            volume_id,
            replica_addrs,
            sync_mode,
            client,
            async_tx,
        }
    }

    /// Send a write to all replicas synchronously (wait for all acks).
    async fn replicate_sync(&self, offset: u64, buf: &[u8]) -> DriveResult<()> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(buf);
        let req = ReplicateRequest {
            volume_id: self.volume_id.to_string(),
            offset,
            data,
        };

        let mut futs = Vec::with_capacity(self.replica_addrs.len());
        for addr in &self.replica_addrs {
            let url = format!("http://{}/api/v1/internal/replicate", addr);
            futs.push(self.client.post(url).json(&req).send());
        }

        let results = futures::future::join_all(futs).await;
        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    tracing::warn!(
                        "sync replication to {} returned status {}",
                        self.replica_addrs[i], resp.status()
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "sync replication to {} failed: {e}",
                        self.replica_addrs[i]
                    );
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "replication to {} failed: {e}", self.replica_addrs[i]
                    )));
                }
            }
        }
        Ok(())
    }

    /// Queue a write for async replication.
    fn replicate_async(&self, offset: u64, buf: &[u8]) {
        use base64::Engine;
        if let Some(ref tx) = self.async_tx {
            let data = base64::engine::general_purpose::STANDARD.encode(buf);
            let req = ReplicateRequest {
                volume_id: self.volume_id.to_string(),
                offset,
                data,
            };
            let _ = tx.send(req);
        }
    }
}

/// Background task that drains the async replication queue.
async fn async_replication_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ReplicateRequest>,
    addrs: Vec<String>,
    client: reqwest::Client,
) {
    while let Some(req) = rx.recv().await {
        for addr in &addrs {
            let url = format!("http://{}/api/v1/internal/replicate", addr);
            match client.post(&url).json(&req).send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    tracing::warn!("async replication to {addr} returned {}", resp.status());
                }
                Err(e) => {
                    tracing::warn!("async replication to {addr} failed: {e}");
                    // TODO: retry queue / exponential backoff
                }
            }
        }
    }
}

#[async_trait]
impl BlockDevice for ReplicatedVolume {
    fn id(&self) -> &DeviceId {
        self.inner.id()
    }

    fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }

    fn device_type(&self) -> DriveType {
        self.inner.device_type()
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        // Reads are always local
        self.inner.read(offset, buf).await
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        // Write locally first
        let result = self.inner.write(offset, buf).await?;

        // Then replicate
        if !self.replica_addrs.is_empty() {
            if self.sync_mode {
                self.replicate_sync(offset, buf).await?;
            } else {
                self.replicate_async(offset, buf);
            }
        }

        Ok(result)
    }

    async fn flush(&self) -> DriveResult<()> {
        self.inner.flush().await
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.discard(offset, len).await
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        self.inner.smart_status()
    }

    fn media_errors(&self) -> u64 {
        self.inner.media_errors()
    }
}
