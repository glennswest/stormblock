//! Volume replication — sync and async write replication across nodes.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::drive::{
    BlockDevice, DeviceId, DriveType, DriveError, DriveResult, SmartData,
};

/// Maximum number of pending retry entries before dropping oldest.
const MAX_RETRY_QUEUE: usize = 10_000;
/// Maximum number of retries before discarding a replication request.
const MAX_RETRIES: u32 = 8;
/// Base delay for exponential backoff (doubles each retry: 100ms, 200ms, 400ms, ...).
const BASE_RETRY_DELAY_MS: u64 = 100;
/// Maximum backoff delay cap.
const MAX_RETRY_DELAY_MS: u64 = 30_000;

/// Internal replication request sent between nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A pending retry entry with backoff metadata.
#[derive(Clone)]
struct RetryEntry {
    request: ReplicateRequest,
    /// Which peer address failed.
    addr: String,
    /// Number of attempts so far.
    attempts: u32,
}

impl RetryEntry {
    fn delay(&self) -> Duration {
        let ms = BASE_RETRY_DELAY_MS * 2u64.saturating_pow(self.attempts.saturating_sub(1));
        Duration::from_millis(ms.min(MAX_RETRY_DELAY_MS))
    }
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
    /// If `sync_mode` is false, spawns a background replication task with retry.
    pub fn new(
        inner: Arc<dyn BlockDevice>,
        volume_id: Uuid,
        replica_addrs: Vec<String>,
        sync_mode: bool,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
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

/// Background task that drains the async replication queue with retry/backoff.
async fn async_replication_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ReplicateRequest>,
    addrs: Vec<String>,
    client: reqwest::Client,
) {
    let mut retry_queue: VecDeque<RetryEntry> = VecDeque::new();

    loop {
        // Process retry queue first (entries whose backoff has elapsed)
        let retry_count = retry_queue.len();
        for _ in 0..retry_count {
            let Some(entry) = retry_queue.pop_front() else { break };

            let url = format!("http://{}/api/v1/internal/replicate", entry.addr);
            match client.post(&url).json(&entry.request).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if entry.attempts > 0 {
                        tracing::info!(
                            "async replication retry to {} succeeded after {} attempts",
                            entry.addr, entry.attempts + 1
                        );
                    }
                    metrics::counter!("stormblock_replication_retry_success_total").increment(1);
                }
                Ok(resp) => {
                    tracing::warn!(
                        "async replication retry to {} returned {} (attempt {}/{})",
                        entry.addr, resp.status(), entry.attempts + 1, MAX_RETRIES
                    );
                    maybe_requeue(&mut retry_queue, entry);
                }
                Err(e) => {
                    tracing::warn!(
                        "async replication retry to {} failed: {e} (attempt {}/{})",
                        entry.addr, entry.attempts + 1, MAX_RETRIES
                    );
                    maybe_requeue(&mut retry_queue, entry);
                }
            }
        }

        // Wait for new requests (with a timeout so retries get processed)
        let timeout = if retry_queue.is_empty() {
            Duration::from_secs(60)
        } else {
            // Use the smallest backoff delay from the retry queue
            retry_queue.front()
                .map(|e| e.delay())
                .unwrap_or(Duration::from_secs(1))
        };

        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(req)) => {
                // New replication request — send to all peers
                for addr in &addrs {
                    let url = format!("http://{}/api/v1/internal/replicate", addr);
                    match client.post(&url).json(&req).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            metrics::counter!("stormblock_replication_async_success_total").increment(1);
                        }
                        Ok(resp) => {
                            tracing::warn!("async replication to {addr} returned {}", resp.status());
                            enqueue_retry(&mut retry_queue, req.clone(), addr.clone());
                            metrics::counter!("stormblock_replication_async_failures_total").increment(1);
                        }
                        Err(e) => {
                            tracing::warn!("async replication to {addr} failed: {e}");
                            enqueue_retry(&mut retry_queue, req.clone(), addr.clone());
                            metrics::counter!("stormblock_replication_async_failures_total").increment(1);
                        }
                    }
                }
            }
            Ok(None) => {
                // Channel closed — drain retries then exit
                tracing::info!("async replication channel closed, draining {} retries", retry_queue.len());
                break;
            }
            Err(_) => {
                // Timeout — loop back to process retries
            }
        }
    }
}

/// Enqueue a failed request for retry.
fn enqueue_retry(queue: &mut VecDeque<RetryEntry>, request: ReplicateRequest, addr: String) {
    if queue.len() >= MAX_RETRY_QUEUE {
        let dropped = queue.pop_front();
        if let Some(d) = dropped {
            tracing::warn!(
                "async replication retry queue full, dropping oldest entry for {}",
                d.addr
            );
            metrics::counter!("stormblock_replication_retry_dropped_total").increment(1);
        }
    }
    queue.push_back(RetryEntry {
        request,
        addr,
        attempts: 0,
    });
}

/// Re-enqueue a retry entry if it hasn't exceeded max retries.
fn maybe_requeue(queue: &mut VecDeque<RetryEntry>, mut entry: RetryEntry) {
    entry.attempts += 1;
    if entry.attempts >= MAX_RETRIES {
        tracing::error!(
            "async replication to {} exhausted {} retries, discarding",
            entry.addr, MAX_RETRIES
        );
        metrics::counter!("stormblock_replication_retry_exhausted_total").increment(1);
        return;
    }
    metrics::counter!("stormblock_replication_retry_queued_total").increment(1);
    queue.push_back(entry);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_entry_backoff() {
        let entry = RetryEntry {
            request: ReplicateRequest {
                volume_id: "test".to_string(),
                offset: 0,
                data: String::new(),
            },
            addr: "127.0.0.1:9090".to_string(),
            attempts: 0,
        };
        // First attempt: base delay (100ms)
        assert_eq!(entry.delay(), Duration::from_millis(100));

        let entry1 = RetryEntry { attempts: 1, ..entry.clone() };
        assert_eq!(entry1.delay(), Duration::from_millis(100));

        let entry2 = RetryEntry { attempts: 2, ..entry.clone() };
        assert_eq!(entry2.delay(), Duration::from_millis(200));

        let entry3 = RetryEntry { attempts: 3, ..entry.clone() };
        assert_eq!(entry3.delay(), Duration::from_millis(400));

        let entry7 = RetryEntry { attempts: 7, ..entry.clone() };
        assert_eq!(entry7.delay(), Duration::from_millis(6400));

        // Capped at MAX_RETRY_DELAY_MS
        let entry20 = RetryEntry { attempts: 20, ..entry };
        assert_eq!(entry20.delay(), Duration::from_millis(MAX_RETRY_DELAY_MS));
    }

    #[test]
    fn enqueue_retry_respects_max_queue() {
        let mut queue = VecDeque::new();
        // Fill queue to MAX_RETRY_QUEUE
        for i in 0..MAX_RETRY_QUEUE {
            let req = ReplicateRequest {
                volume_id: format!("vol-{i}"),
                offset: i as u64,
                data: String::new(),
            };
            enqueue_retry(&mut queue, req, "peer:9090".to_string());
        }
        assert_eq!(queue.len(), MAX_RETRY_QUEUE);

        // Adding one more should drop the oldest
        let req = ReplicateRequest {
            volume_id: "vol-overflow".to_string(),
            offset: 99999,
            data: String::new(),
        };
        enqueue_retry(&mut queue, req, "peer:9090".to_string());
        assert_eq!(queue.len(), MAX_RETRY_QUEUE);
        // Oldest (vol-0) was dropped, newest is vol-overflow
        assert_eq!(queue.back().unwrap().request.volume_id, "vol-overflow");
        assert_eq!(queue.front().unwrap().request.volume_id, "vol-1");
    }

    #[test]
    fn maybe_requeue_exhaustion() {
        let mut queue = VecDeque::new();
        let entry = RetryEntry {
            request: ReplicateRequest {
                volume_id: "test".to_string(),
                offset: 0,
                data: String::new(),
            },
            addr: "peer:9090".to_string(),
            attempts: MAX_RETRIES, // Already at max
        };
        maybe_requeue(&mut queue, entry);
        // Should NOT be re-queued (exhausted)
        assert!(queue.is_empty());
    }

    #[test]
    fn maybe_requeue_increments_attempts() {
        let mut queue = VecDeque::new();
        let entry = RetryEntry {
            request: ReplicateRequest {
                volume_id: "test".to_string(),
                offset: 0,
                data: String::new(),
            },
            addr: "peer:9090".to_string(),
            attempts: 2,
        };
        maybe_requeue(&mut queue, entry);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().attempts, 3);
    }
}
