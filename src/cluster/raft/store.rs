//! Raft log + state machine storage — file-backed, in-memory cache.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::PathBuf;
use std::fmt::Debug;

use serde::{Serialize, Deserialize};
use openraft::{
    Entry, LogId, LogState, Vote, StorageError, StoredMembership,
    SnapshotMeta, Snapshot, RaftLogReader, RaftSnapshotBuilder, RaftStorage,
    BasicNode, EntryPayload, StorageIOError, AnyError,
};

use super::StormTypeConfig;
use super::state::{ClusterResponse, ClusterState};

/// Helper to build a StorageError::IO from an std::io::Error.
fn io_err(e: std::io::Error) -> StorageError<u64> {
    StorageError::IO {
        source: StorageIOError::write(AnyError::new(&e)),
    }
}

/// Helper to build a StorageError::IO from a serde error.
fn serde_err(e: impl std::fmt::Display) -> StorageError<u64> {
    StorageError::IO {
        source: StorageIOError::write(AnyError::error(e)),
    }
}

/// Combined Raft log + state machine storage.
pub struct StormStore {
    /// Raft log entries, keyed by log index.
    log: BTreeMap<u64, Entry<StormTypeConfig>>,
    /// Last purged log id.
    last_purged_log_id: Option<LogId<u64>>,
    /// Persisted vote.
    vote: Option<Vote<u64>>,
    /// Last committed log id.
    committed: Option<LogId<u64>>,
    /// The cluster state machine.
    state: ClusterState,
    /// Last applied log id.
    last_applied: Option<LogId<u64>>,
    /// Last applied membership.
    last_membership: StoredMembership<u64, BasicNode>,
    /// Current snapshot data.
    snapshot: Option<StoredSnapshot>,
    /// Data directory for persistence.
    data_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct StoredSnapshot {
    meta: SnapshotMeta<u64, BasicNode>,
    data: Vec<u8>,
}

impl StormStore {
    /// Create or load a store from disk.
    pub async fn new(data_dir: &str) -> anyhow::Result<Self> {
        let data_dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&data_dir)?;

        let mut store = StormStore {
            log: BTreeMap::new(),
            last_purged_log_id: None,
            vote: None,
            committed: None,
            state: ClusterState::default(),
            last_applied: None,
            last_membership: StoredMembership::default(),
            snapshot: None,
            data_dir,
        };

        // Load persisted vote
        let vote_path = store.data_dir.join("raft-vote");
        if vote_path.exists() {
            let data = std::fs::read_to_string(&vote_path)?;
            store.vote = serde_json::from_str(&data).ok();
        }

        // Load persisted snapshot (restores state machine)
        let snap_path = store.data_dir.join("raft-snapshot");
        if snap_path.exists() {
            let data = std::fs::read(&snap_path)?;
            if let Ok(snap) = serde_json::from_slice::<PersistedSnapshot>(&data) {
                store.state = snap.state;
                store.last_applied = snap.meta.last_log_id;
                store.last_membership = snap.meta.last_membership.clone();
                store.last_purged_log_id = store.last_applied;
                store.snapshot = Some(StoredSnapshot {
                    meta: snap.meta,
                    data: serde_json::to_vec(&store.state)?,
                });
            }
        }

        // Load persisted log entries
        let log_path = store.data_dir.join("raft-log");
        if log_path.exists() {
            let data = std::fs::read_to_string(&log_path)?;
            for line in data.lines() {
                if line.is_empty() { continue; }
                if let Ok(entry) = serde_json::from_str::<Entry<StormTypeConfig>>(line) {
                    store.log.insert(entry.log_id.index, entry);
                }
            }
        }

        Ok(store)
    }

    /// Persist vote to disk.
    fn persist_vote(&self) -> Result<(), StorageError<u64>> {
        let path = self.data_dir.join("raft-vote");
        let data = serde_json::to_string(&self.vote).map_err(serde_err)?;
        std::fs::write(&path, data).map_err(io_err)?;
        Ok(())
    }

    /// Append log entries to the persistent log file.
    fn persist_log(&self) -> Result<(), StorageError<u64>> {
        let path = self.data_dir.join("raft-log");
        let mut content = String::new();
        for entry in self.log.values() {
            let line = serde_json::to_string(entry).map_err(serde_err)?;
            content.push_str(&line);
            content.push('\n');
        }
        std::fs::write(&path, content).map_err(io_err)?;
        Ok(())
    }

    /// Persist a snapshot to disk.
    fn persist_snapshot(&self, meta: &SnapshotMeta<u64, BasicNode>) -> Result<(), StorageError<u64>> {
        let path = self.data_dir.join("raft-snapshot");
        let snap = PersistedSnapshot {
            meta: meta.clone(),
            state: self.state.clone(),
        };
        let data = serde_json::to_vec(&snap).map_err(serde_err)?;
        std::fs::write(&path, data).map_err(io_err)?;
        Ok(())
    }

    /// Get a reference to the cluster state.
    pub fn cluster_state(&self) -> &ClusterState {
        &self.state
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedSnapshot {
    meta: SnapshotMeta<u64, BasicNode>,
    state: ClusterState,
}

// --- RaftLogReader ---

impl RaftLogReader<StormTypeConfig> for StormStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<StormTypeConfig>>, StorageError<u64>> {
        let entries: Vec<_> = self.log.range(range)
            .map(|(_, e)| e.clone())
            .collect();
        Ok(entries)
    }
}

// --- RaftSnapshotBuilder ---

impl RaftSnapshotBuilder<StormTypeConfig> for StormStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot<StormTypeConfig>, StorageError<u64>> {
        let data = serde_json::to_vec(&self.state).map_err(serde_err)?;

        let snapshot_id = format!("snap-{}", uuid::Uuid::new_v4().simple());
        let meta = SnapshotMeta {
            last_log_id: self.last_applied,
            last_membership: self.last_membership.clone(),
            snapshot_id,
        };

        self.persist_snapshot(&meta)?;

        let snapshot_data = data.clone();
        self.snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(snapshot_data)),
        })
    }
}

// --- RaftStorage (combined trait) ---

impl RaftStorage<StormTypeConfig> for StormStore {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        self.vote = Some(*vote);
        self.persist_vote()
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Ok(self.vote)
    }

    async fn save_committed(&mut self, committed: Option<LogId<u64>>) -> Result<(), StorageError<u64>> {
        self.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        Ok(self.committed)
    }

    async fn get_log_state(&mut self) -> Result<LogState<StormTypeConfig>, StorageError<u64>> {
        let last_log_id = self.log.values().last()
            .map(|e| e.log_id)
            .or(self.last_purged_log_id);
        Ok(LogState {
            last_purged_log_id: self.last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        unreachable!("log reader is managed by Adaptor")
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<StormTypeConfig>> + Send,
    {
        for entry in entries {
            self.log.insert(entry.log_id.index, entry);
        }
        self.persist_log()
    }

    async fn delete_conflict_logs_since(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let keys: Vec<u64> = self.log.range(log_id.index..)
            .map(|(&k, _)| k)
            .collect();
        for key in keys {
            self.log.remove(&key);
        }
        self.persist_log()
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        self.last_purged_log_id = Some(log_id);
        let keys: Vec<u64> = self.log.range(..=log_id.index)
            .map(|(&k, _)| k)
            .collect();
        for key in keys {
            self.log.remove(&key);
        }
        self.persist_log()
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        Ok((self.last_applied, self.last_membership.clone()))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<StormTypeConfig>],
    ) -> Result<Vec<ClusterResponse>, StorageError<u64>> {
        let mut responses = Vec::with_capacity(entries.len());
        for entry in entries {
            self.last_applied = Some(entry.log_id);

            match &entry.payload {
                EntryPayload::Blank => {
                    responses.push(ClusterResponse::Ok);
                }
                EntryPayload::Normal(cmd) => {
                    let resp = self.state.apply(cmd);
                    responses.push(resp);
                }
                EntryPayload::Membership(mem) => {
                    self.last_membership = StoredMembership::new(
                        Some(entry.log_id),
                        mem.clone(),
                    );
                    responses.push(ClusterResponse::Ok);
                }
            }
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        unreachable!("snapshot builder is managed by Adaptor")
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let data = snapshot.into_inner();
        let state: ClusterState = serde_json::from_slice(&data)
            .map_err(serde_err)?;

        self.state = state;
        self.last_applied = meta.last_log_id;
        self.last_membership = meta.last_membership.clone();

        self.snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        });

        self.persist_snapshot(meta)?;

        // Purge logs up to snapshot
        if let Some(log_id) = meta.last_log_id {
            let keys: Vec<u64> = self.log.range(..=log_id.index)
                .map(|(&k, _)| k)
                .collect();
            for key in keys {
                self.log.remove(&key);
            }
            self.last_purged_log_id = Some(log_id);
            self.persist_log()?;
        }

        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<StormTypeConfig>>, StorageError<u64>> {
        match &self.snapshot {
            Some(stored) => {
                Ok(Some(Snapshot {
                    meta: stored.meta.clone(),
                    snapshot: Box::new(Cursor::new(stored.data.clone())),
                }))
            }
            None => Ok(None),
        }
    }
}
