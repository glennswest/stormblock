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
//!
//! A file-backed device is a drive like any other here — same GPT, same
//! partitions, same everything. That matters for the migration this module has
//! to survive: the earlier arrangement put **one pallet on a whole device**,
//! superblock at byte zero and no partition table at all. Such a device is
//! still discovered ([`PalletLocation::is_whole_drive`]) rather than quietly
//! disappearing, because subdividing it is a copy — the GPT wants the very
//! bytes its superblock sits in, so the pallet has to move before the table
//! can exist. [`super::PalletManager::adopt_whole_drive`] is that move.

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
    /// Where the drive is, when registered with labels (#70). Empty otherwise.
    pub labels: crate::placement::domain::FailureDomain,
}

impl DriveRef {
    /// What fails with this drive: its identity under its labels.
    pub fn domain(&self) -> crate::placement::domain::FailureDomain {
        crate::placement::domain::FailureDomain::from_device(self.device.id()).merged_under(&self.labels)
    }
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

/// Entry index for a pallet that occupies a whole device with no GPT — the
/// arrangement that predates partitioned drives.
pub const WHOLE_DRIVE: usize = usize::MAX;

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
    /// True when this pallet owns the entire device and there is no GPT entry
    /// behind it — so it has no attribute bits, and cannot take part in the
    /// priority ladder until it is adopted into a partition.
    pub fn is_whole_drive(&self) -> bool {
        self.entry_index == WHOLE_DRIVE
    }

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
        self.add_drive_labelled(path, device, Default::default());
    }

    pub fn add_drive_labelled(
        &mut self,
        path: impl Into<String>,
        device: Arc<dyn BlockDevice>,
        labels: crate::placement::domain::FailureDomain,
    ) {
        self.drives.push(DriveRef { path: path.into(), device, labels });
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
    ///
    /// A device with no usable GPT is not necessarily empty: it may be a
    /// whole-drive pallet from before drives were subdivided. That is checked
    /// before giving up, because a pallet nobody can find is the same as a
    /// pallet that is gone.
    pub async fn scan_drive(&self, drive_index: usize) -> Result<Vec<PalletLocation>> {
        let d = self.drive(drive_index)?;
        let gpt = match Gpt::read(&d.device).await {
            Ok(g) => g,
            Err(e) => {
                return match self.whole_drive_pallet(drive_index).await {
                    Some(loc) => Ok(vec![loc]),
                    None => Err(e),
                }
            }
        };
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

    /// A pallet occupying the whole device, superblock at byte zero, with no
    /// partition table — the pre-subdivision layout.
    ///
    /// Its attributes cannot come from a GPT entry it does not have, so the
    /// sealed and read-only bits are read from the superblock mirror and it is
    /// given one try at the bottom of the ladder. It can be selected and read;
    /// it cannot be promoted, because there is nowhere to record that.
    pub async fn whole_drive_pallet(&self, drive_index: usize) -> Option<PalletLocation> {
        let d = self.drive(drive_index).ok()?;
        let view = PartitionView::whole(d.device.clone());
        let p = Pallet::read(&view).await.ok()?;
        Some(PalletLocation {
            // No GPT means no UniquePartitionGUID, and a pallet still needs a
            // handle callers can name. Derived from the device path so it is
            // at least stable for as long as the arrangement lasts.
            id: derived_id(&d.path),
            drive: d.path.clone(),
            drive_index,
            entry_index: WHOLE_DRIVE,
            partition_name: String::new(),
            name: p.name().to_string(),
            kind: p.kind(),
            version: p.version(),
            version_label: p.version_label().to_string(),
            attributes: Attributes {
                priority: 1,
                tries_left: 1,
                successful: false,
                sealed: p.sb.sealed(),
                read_only: p.sb.read_only(),
                required: true,
            },
            start_bytes: 0,
            size_bytes: d.device.capacity_bytes(),
            used_bytes: used_bytes(&p),
            member_count: p.member_count(),
            state: PalletState::Readable,
        })
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

/// A stable stand-in identity for a pallet that has no GPT entry to carry one.
fn derived_id(path: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(b)
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
