//! Discovery — every pallet on every drive.
//!
//! Nothing here is configured. A node is handed its drives and the store finds
//! what is on them by reading each GPT and each pallet superblock, which is the
//! only arrangement that survives the cases that matter: a disk moved between
//! nodes, a pallet copied onto a spare, an image assembled elsewhere and
//! written whole. A configured list would have to be right about all of them.
//!
//! **Several pallets per drive, several drives per node** is the normal case,
//! not an edge one: an upgrade is a *new* partition beside the running one, so
//! a drive carrying a system has at least two the moment it has ever been
//! upgraded.

use std::sync::Arc;

use uuid::Uuid;

use crate::drive::BlockDevice;

use super::format::{Attributes, PalletKind};
use super::gpt::Gpt;
use super::{Pallet, PalletError, PartitionView, Result};

/// A drive the store may scan.
#[derive(Clone)]
pub struct DriveRef {
    pub path: String,
    pub device: Arc<dyn BlockDevice>,
}

/// Why a pallet partition could not be read as a pallet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PalletState {
    /// Superblock and both tables parse and check out. Content is not read
    /// here — that is [`super::PalletManager::verify`].
    Readable,
    /// The partition carries the pallet type GUID but does not hold a usable
    /// pallet. A publish interrupted before its last write looks exactly like
    /// this, which is the point of writing the superblock last.
    Unreadable { reason: String },
}

/// One pallet, and where it is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PalletLocation {
    /// The GPT `UniquePartitionGUID` — stable across byte-for-byte copies, so
    /// it survives a move to another drive.
    pub id: Uuid,
    pub drive: String,
    pub drive_index: usize,
    pub entry_index: usize,
    /// GPT `PartitionName`, which must match the superblock's name.
    pub partition_name: String,
    pub name: String,
    pub kind: PalletKind,
    pub version: u64,
    pub version_label: String,
    pub attributes: Attributes,
    pub start_bytes: u64,
    pub size_bytes: u64,
    /// Bytes actually occupied by the manifest and member content.
    pub used_bytes: u64,
    pub member_count: usize,
    pub state: PalletState,
}

impl PalletLocation {
    pub fn is_readable(&self) -> bool {
        matches!(self.state, PalletState::Readable)
    }

    /// Selection key, per the spec: priority first, then version.
    pub fn order_key(&self) -> (u8, u64) {
        (self.attributes.priority, self.version)
    }
}

/// Scans drives for pallets.
#[derive(Clone, Default)]
pub struct PalletStore {
    drives: Vec<DriveRef>,
}

impl PalletStore {
    pub fn new(drives: Vec<DriveRef>) -> Self {
        PalletStore { drives }
    }

    pub fn add_drive(&mut self, path: impl Into<String>, device: Arc<dyn BlockDevice>) {
        self.drives.push(DriveRef { path: path.into(), device });
    }

    pub fn drives(&self) -> &[DriveRef] {
        &self.drives
    }

    pub fn drive(&self, index: usize) -> Result<&DriveRef> {
        self.drives
            .get(index)
            .ok_or_else(|| PalletError::NotFound(format!("drive index {index}")))
    }

    /// Resolve a drive by path, or by index if the path parses as one.
    pub fn drive_index_of(&self, path: &str) -> Result<usize> {
        if let Some(i) = self.drives.iter().position(|d| d.path == path) {
            return Ok(i);
        }
        if let Ok(i) = path.parse::<usize>() {
            if i < self.drives.len() {
                return Ok(i);
            }
        }
        Err(PalletError::NotFound(format!("drive '{path}'")))
    }

    /// Read one drive's GPT.
    pub async fn gpt(&self, drive_index: usize) -> Result<Gpt> {
        Gpt::read(&self.drive(drive_index)?.device).await
    }

    /// Every pallet on one drive, in GPT entry order.
    pub async fn scan_drive(&self, drive_index: usize) -> Result<Vec<PalletLocation>> {
        let d = self.drive(drive_index)?;
        let gpt = Gpt::read(&d.device).await?;
        let bs = gpt.block_size;
        let mut out = Vec::new();
        for (entry_index, e) in gpt.pallets() {
            let view = PartitionView::new(
                d.device.clone(),
                e.start_bytes(bs),
                e.size_bytes(bs),
            );
            let attrs = Attributes::from_u64(e.attributes);
            let loc = match Pallet::read(&view).await {
                Ok(p) => PalletLocation {
                    id: e.uuid(),
                    drive: d.path.clone(),
                    drive_index,
                    entry_index,
                    partition_name: e.name.clone(),
                    name: p.name().to_string(),
                    kind: p.kind(),
                    version: p.version(),
                    version_label: p.version_label().to_string(),
                    attributes: attrs,
                    start_bytes: e.start_bytes(bs),
                    size_bytes: e.size_bytes(bs),
                    used_bytes: used_bytes(&p),
                    member_count: p.member_count(),
                    state: PalletState::Readable,
                },
                Err(err) => PalletLocation {
                    id: e.uuid(),
                    drive: d.path.clone(),
                    drive_index,
                    entry_index,
                    partition_name: e.name.clone(),
                    name: e.name.clone(),
                    kind: PalletKind::Unspecified,
                    version: 0,
                    version_label: String::new(),
                    attributes: attrs,
                    start_bytes: e.start_bytes(bs),
                    size_bytes: e.size_bytes(bs),
                    used_bytes: 0,
                    member_count: 0,
                    state: PalletState::Unreadable { reason: err.to_string() },
                },
            };
            out.push(loc);
        }
        Ok(out)
    }

    /// Every pallet on every drive. A drive without a GPT is skipped rather
    /// than fatal — a node's other drives are not its business.
    pub async fn scan(&self) -> Vec<PalletLocation> {
        let mut out = Vec::new();
        for i in 0..self.drives.len() {
            match self.scan_drive(i).await {
                Ok(mut v) => out.append(&mut v),
                Err(e) => {
                    tracing::debug!("pallet scan skipped drive {}: {e}", self.drives[i].path);
                }
            }
        }
        out
    }

    /// Boot candidates in selection order: priority descending, then version
    /// descending, skipping anything that is not a candidate.
    ///
    /// `kind` filters the ladder. Priority only orders pallets that compete
    /// with each other — a kube pallet does not outrank a boot pallet by
    /// carrying a bigger number, it is simply not in the same race.
    pub async fn candidates(&self, kind: Option<PalletKind>) -> Vec<PalletLocation> {
        let mut v: Vec<PalletLocation> = self
            .scan()
            .await
            .into_iter()
            .filter(|p| p.attributes.is_candidate())
            .filter(|p| kind.map_or(true, |k| p.kind == k))
            .collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.order_key()));
        v
    }

    /// Find a pallet by its partition GUID, across every drive.
    pub async fn find(&self, id: Uuid) -> Result<PalletLocation> {
        self.scan()
            .await
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| PalletError::NotFound(format!("pallet {id}")))
    }

    /// Find by name, newest version first.
    pub async fn find_by_name(&self, name: &str) -> Vec<PalletLocation> {
        let mut v: Vec<PalletLocation> =
            self.scan().await.into_iter().filter(|p| p.name == name).collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.version));
        v
    }

    /// A byte window onto the pallet's partition.
    pub fn view(&self, loc: &PalletLocation) -> Result<PartitionView> {
        let d = self.drive(loc.drive_index)?;
        Ok(PartitionView::new(d.device.clone(), loc.start_bytes, loc.size_bytes))
    }

    /// Parse the pallet at a location.
    pub async fn open(&self, loc: &PalletLocation) -> Result<Pallet> {
        Pallet::read(&self.view(loc)?).await
    }
}

/// How much of a partition a pallet actually occupies — the end of its
/// furthest extent. A pallet is usually laid into a partition with headroom,
/// and moving one should copy the pallet, not the headroom.
pub fn used_bytes(p: &Pallet) -> u64 {
    let bs = p.sb.block_size as u64;
    let mut end = p.sb.member_data_offset;
    for i in 0..p.sb.extent_count as usize {
        if let Ok(x) = p.extent(i) {
            end = end.max((x.partition_block + x.block_count) * bs);
        }
    }
    end
}
