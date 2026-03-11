//! RAID engine — software RAID 1/5/6/10 with SIMD parity.
//!
//! `RaidArray` implements `BlockDevice`, so the volume manager sees
//! a RAID array as just another block device. Member drives are also
//! `BlockDevice` instances (FileDevice, SasDevice, etc.).

pub mod parity;
pub mod rebuild;
pub mod journal;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};
use crate::raid::journal::WriteIntentJournal;
use crate::raid::parity::ParityEngine;
use crate::raid::rebuild::{RebuildConfig, RebuildProgress, ScrubConfig, ScrubProgress};

/// 1 MB data offset — room for superblock + future bitmap at start of each member.
pub const DATA_OFFSET: u64 = 1024 * 1024;

/// Default stripe size for RAID 5/6/10 (64 KB).
pub const DEFAULT_STRIPE_SIZE: u64 = 64 * 1024;

/// Superblock magic: "STRMBLK\0"
pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"STRMBLK\0";

/// Current superblock version.
pub const SUPERBLOCK_VERSION: u32 = 1;

// --- Core types ---

/// RAID level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaidLevel {
    /// Mirror: all members hold identical copies.
    Raid1,
    /// Distributed parity: N data + 1 rotating parity.
    Raid5,
    /// Dual parity: N data + 2 rotating parity (P + Q).
    Raid6,
    /// Striped mirrors: pairs of mirrors striped together.
    Raid10,
}

impl fmt::Display for RaidLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RaidLevel::Raid1 => write!(f, "RAID-1"),
            RaidLevel::Raid5 => write!(f, "RAID-5"),
            RaidLevel::Raid6 => write!(f, "RAID-6"),
            RaidLevel::Raid10 => write!(f, "RAID-10"),
        }
    }
}

/// Unique identifier for a RAID array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RaidArrayId(pub Uuid);

impl fmt::Display for RaidArrayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// State of an individual member drive within the array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaidMemberState {
    Active,
    Degraded,
    Spare,
    Failed,
    Rebuilding,
}

impl fmt::Display for RaidMemberState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RaidMemberState::Active => write!(f, "active"),
            RaidMemberState::Degraded => write!(f, "degraded"),
            RaidMemberState::Spare => write!(f, "spare"),
            RaidMemberState::Failed => write!(f, "failed"),
            RaidMemberState::Rebuilding => write!(f, "rebuilding"),
        }
    }
}

/// RAID errors.
#[derive(Debug)]
pub enum RaidError {
    /// Not enough members for this RAID level.
    InsufficientMembers { need: usize, have: usize },
    /// Array is degraded beyond the level's tolerance.
    TooManyFailures { failed: usize, max_tolerated: usize },
    /// Superblock mismatch (wrong array UUID, version, etc.).
    SuperblockMismatch(String),
    /// Checksum failure on superblock.
    ChecksumError,
    /// Underlying drive error.
    Drive(DriveError),
    /// I/O failed on a specific member.
    MemberIo { member_idx: usize, error: DriveError },
    /// Stripe geometry error.
    InvalidStripe(String),
}

impl fmt::Display for RaidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RaidError::InsufficientMembers { need, have } =>
                write!(f, "need {need} members, have {have}"),
            RaidError::TooManyFailures { failed, max_tolerated } =>
                write!(f, "{failed} members failed (max tolerated: {max_tolerated})"),
            RaidError::SuperblockMismatch(msg) =>
                write!(f, "superblock mismatch: {msg}"),
            RaidError::ChecksumError =>
                write!(f, "superblock checksum error"),
            RaidError::Drive(e) =>
                write!(f, "drive error: {e}"),
            RaidError::MemberIo { member_idx, error } =>
                write!(f, "member {member_idx} I/O error: {error}"),
            RaidError::InvalidStripe(msg) =>
                write!(f, "invalid stripe: {msg}"),
        }
    }
}

impl std::error::Error for RaidError {}

impl From<DriveError> for RaidError {
    fn from(e: DriveError) -> Self {
        RaidError::Drive(e)
    }
}

impl From<RaidError> for DriveError {
    fn from(e: RaidError) -> Self {
        DriveError::Other(anyhow::anyhow!("{e}"))
    }
}

// --- Superblock ---

/// On-disk superblock stored at byte 0 of each member drive.
/// Total serialized size fits within a single 4096-byte block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidSuperblock {
    /// Magic bytes: "STRMBLK\0"
    pub magic: [u8; 8],
    /// Superblock format version.
    pub version: u32,
    /// UUID of this RAID array.
    pub array_uuid: [u8; 16],
    /// Index of this member within the array (0-based).
    pub member_index: u32,
    /// UUID of this specific member.
    pub member_uuid: [u8; 16],
    /// RAID level.
    pub level: u8,
    /// Total number of members in the array.
    pub member_count: u32,
    /// Stripe size in bytes (for RAID 5/6/10).
    pub stripe_size: u64,
    /// Byte offset where data begins (after superblock + bitmap).
    pub data_offset: u64,
    /// Usable data size per member in bytes.
    pub data_size: u64,
    /// Array creation timestamp (Unix epoch seconds).
    pub create_time: u64,
    /// Last superblock update timestamp.
    pub update_time: u64,
    /// Array state (0=clean, 1=degraded, 2=rebuilding).
    pub state: u8,
    /// CRC32C of all preceding fields.
    pub checksum: u32,
}

impl RaidSuperblock {
    /// Create a new superblock for the given array parameters.
    pub fn new(
        array_uuid: Uuid,
        member_index: u32,
        member_uuid: Uuid,
        level: RaidLevel,
        member_count: u32,
        stripe_size: u64,
        data_size: u64,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let level_byte = match level {
            RaidLevel::Raid1 => 1,
            RaidLevel::Raid5 => 5,
            RaidLevel::Raid6 => 6,
            RaidLevel::Raid10 => 10,
        };

        let mut sb = RaidSuperblock {
            magic: SUPERBLOCK_MAGIC,
            version: SUPERBLOCK_VERSION,
            array_uuid: *array_uuid.as_bytes(),
            member_index,
            member_uuid: *member_uuid.as_bytes(),
            level: level_byte,
            member_count,
            stripe_size,
            data_offset: DATA_OFFSET,
            data_size,
            create_time: now,
            update_time: now,
            state: 0,
            checksum: 0,
        };
        sb.checksum = sb.compute_checksum();
        sb
    }

    /// Serialize to bytes (for writing to disk).
    pub fn to_bytes(&self) -> Vec<u8> {
        // Use a simple fixed-layout binary format for portability
        let mut buf = Vec::with_capacity(4096);
        buf.extend_from_slice(&self.magic);           // 0..8
        buf.extend_from_slice(&self.version.to_le_bytes());  // 8..12
        buf.extend_from_slice(&self.array_uuid);       // 12..28
        buf.extend_from_slice(&self.member_index.to_le_bytes()); // 28..32
        buf.extend_from_slice(&self.member_uuid);      // 32..48
        buf.push(self.level);                           // 48
        buf.extend_from_slice(&self.member_count.to_le_bytes()); // 49..53
        buf.extend_from_slice(&self.stripe_size.to_le_bytes());  // 53..61
        buf.extend_from_slice(&self.data_offset.to_le_bytes());  // 61..69
        buf.extend_from_slice(&self.data_size.to_le_bytes());    // 69..77
        buf.extend_from_slice(&self.create_time.to_le_bytes());  // 77..85
        buf.extend_from_slice(&self.update_time.to_le_bytes());  // 85..93
        buf.push(self.state);                           // 93
        buf.extend_from_slice(&self.checksum.to_le_bytes());     // 94..98
        // Pad to 4096 bytes
        buf.resize(4096, 0);
        buf
    }

    /// Deserialize from bytes (read from disk).
    pub fn from_bytes(data: &[u8]) -> Result<Self, RaidError> {
        if data.len() < 98 {
            return Err(RaidError::SuperblockMismatch("data too short".into()));
        }

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&data[0..8]);
        if magic != SUPERBLOCK_MAGIC {
            return Err(RaidError::SuperblockMismatch("bad magic".into()));
        }

        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != SUPERBLOCK_VERSION {
            return Err(RaidError::SuperblockMismatch(
                format!("version {version}, expected {SUPERBLOCK_VERSION}")
            ));
        }

        let mut array_uuid = [0u8; 16];
        array_uuid.copy_from_slice(&data[12..28]);

        let member_index = u32::from_le_bytes(data[28..32].try_into().unwrap());

        let mut member_uuid = [0u8; 16];
        member_uuid.copy_from_slice(&data[32..48]);

        let level = data[48];
        let member_count = u32::from_le_bytes(data[49..53].try_into().unwrap());
        let stripe_size = u64::from_le_bytes(data[53..61].try_into().unwrap());
        let data_offset = u64::from_le_bytes(data[61..69].try_into().unwrap());
        let data_size = u64::from_le_bytes(data[69..77].try_into().unwrap());
        let create_time = u64::from_le_bytes(data[77..85].try_into().unwrap());
        let update_time = u64::from_le_bytes(data[85..93].try_into().unwrap());
        let state = data[93];
        let checksum = u32::from_le_bytes(data[94..98].try_into().unwrap());

        let sb = RaidSuperblock {
            magic,
            version,
            array_uuid,
            member_index,
            member_uuid,
            level,
            member_count,
            stripe_size,
            data_offset,
            data_size,
            create_time,
            update_time,
            state,
            checksum,
        };

        let computed = sb.compute_checksum();
        if computed != checksum {
            return Err(RaidError::ChecksumError);
        }

        Ok(sb)
    }

    /// Compute CRC32C over all fields except the checksum itself.
    fn compute_checksum(&self) -> u32 {
        let bytes = self.to_bytes();
        // Checksum covers bytes 0..94 (everything before the checksum field)
        crc32c::crc32c(&bytes[..94])
    }

    /// Validate that this superblock is consistent.
    pub fn validate(&self) -> Result<(), RaidError> {
        if self.magic != SUPERBLOCK_MAGIC {
            return Err(RaidError::SuperblockMismatch("bad magic".into()));
        }
        if self.version != SUPERBLOCK_VERSION {
            return Err(RaidError::SuperblockMismatch(
                format!("version {}, expected {}", self.version, SUPERBLOCK_VERSION)
            ));
        }
        let computed = self.compute_checksum();
        if computed != self.checksum {
            return Err(RaidError::ChecksumError);
        }
        Ok(())
    }
}

// --- Geometry helpers ---

/// Compute which disk holds the parity for a given stripe in RAID 5.
/// Uses left-symmetric layout: parity on disk `(member_count - 1 - (stripe % member_count))`.
#[inline]
pub fn parity_disk_for_stripe(stripe: u64, member_count: u32) -> u32 {
    let n = member_count as u64;
    ((n - 1) - (stripe % n)) as u32
}

/// For a given data-disk logical index within a stripe, return the actual member disk.
/// Skips the parity disk for that stripe.
#[inline]
pub fn data_disk_index(data_idx: u32, stripe: u64, member_count: u32) -> u32 {
    let parity = parity_disk_for_stripe(stripe, member_count);
    if data_idx < parity {
        data_idx
    } else {
        data_idx + 1
    }
}

/// Convert a logical byte offset to (stripe_number, data_disk_offset_within_stripe).
#[inline]
pub fn offset_to_stripe(offset: u64, stripe_size: u64, data_disks: u32) -> (u64, u64) {
    let full_stripe_size = stripe_size * data_disks as u64;
    let stripe = offset / full_stripe_size;
    let offset_in_stripe = offset % full_stripe_size;
    (stripe, offset_in_stripe)
}

/// Convert stripe number + position to member disk physical offset.
#[inline]
pub fn stripe_to_disk_offset(stripe: u64, stripe_size: u64, data_offset: u64) -> u64 {
    data_offset + stripe * stripe_size
}

// --- Member info ---

struct MemberInfo {
    device: Arc<dyn BlockDevice>,
    state: RaidMemberState,
    _member_uuid: Uuid,
}

// --- RaidArray ---

/// A software RAID array that implements `BlockDevice`.
///
/// The volume manager and target protocols see this as just another block device.
pub struct RaidArray {
    id: RaidArrayId,
    device_id: DeviceId,
    level: RaidLevel,
    members: Vec<MemberInfo>,
    stripe_size: u64,
    /// Usable data capacity in bytes.
    capacity: u64,
    parity_engine: ParityEngine,
    journal: tokio::sync::Mutex<WriteIntentJournal>,
    _rebuild_config: RebuildConfig,
    /// Read counter for round-robin in RAID 1.
    read_counter: std::sync::atomic::AtomicU64,
}

impl RaidArray {
    /// Create a new RAID array from member devices.
    ///
    /// Writes superblocks to all members. Members should be raw/unformatted.
    pub async fn create(
        level: RaidLevel,
        members: Vec<Arc<dyn BlockDevice>>,
        stripe_size: Option<u64>,
    ) -> Result<Self, RaidError> {
        let member_count = members.len();

        // Validate member count for level
        match level {
            RaidLevel::Raid1 => {
                if member_count < 2 {
                    return Err(RaidError::InsufficientMembers { need: 2, have: member_count });
                }
            }
            RaidLevel::Raid5 => {
                if member_count < 3 {
                    return Err(RaidError::InsufficientMembers { need: 3, have: member_count });
                }
            }
            RaidLevel::Raid6 => {
                if member_count < 4 {
                    return Err(RaidError::InsufficientMembers { need: 4, have: member_count });
                }
            }
            RaidLevel::Raid10 => {
                if member_count < 4 || member_count % 2 != 0 {
                    return Err(RaidError::InsufficientMembers { need: 4, have: member_count });
                }
            }
        }

        let stripe_size = stripe_size.unwrap_or(DEFAULT_STRIPE_SIZE);
        let array_uuid = Uuid::new_v4();
        let array_id = RaidArrayId(array_uuid);

        // Compute usable data size: min capacity across members minus data offset
        let min_capacity = members.iter()
            .map(|m| m.capacity_bytes())
            .min()
            .unwrap_or(0);

        if min_capacity <= DATA_OFFSET {
            return Err(RaidError::Drive(DriveError::OutOfRange {
                offset: DATA_OFFSET,
                len: 0,
                capacity: min_capacity,
            }));
        }

        let per_member_data = min_capacity - DATA_OFFSET;

        let capacity = match level {
            RaidLevel::Raid1 => per_member_data,
            RaidLevel::Raid5 => {
                let data_disks = (member_count - 1) as u64;
                // Align to full stripes
                let stripes_per_member = per_member_data / stripe_size;
                stripes_per_member * stripe_size * data_disks
            }
            RaidLevel::Raid6 => {
                let data_disks = (member_count - 2) as u64;
                let stripes_per_member = per_member_data / stripe_size;
                stripes_per_member * stripe_size * data_disks
            }
            RaidLevel::Raid10 => {
                let mirror_pairs = (member_count / 2) as u64;
                per_member_data * mirror_pairs
            }
        };

        // Write superblocks to all members
        let mut member_infos = Vec::with_capacity(member_count);
        for (i, dev) in members.iter().enumerate() {
            let member_uuid = Uuid::new_v4();
            let sb = RaidSuperblock::new(
                array_uuid,
                i as u32,
                member_uuid,
                level,
                member_count as u32,
                stripe_size,
                per_member_data,
            );
            let sb_bytes = sb.to_bytes();
            dev.write(0, &sb_bytes).await.map_err(|e| {
                RaidError::MemberIo { member_idx: i, error: e }
            })?;
            dev.flush().await.map_err(|e| {
                RaidError::MemberIo { member_idx: i, error: e }
            })?;

            member_infos.push(MemberInfo {
                device: Arc::clone(dev),
                state: RaidMemberState::Active,
                _member_uuid: member_uuid,
            });
        }

        // Compute stripe count for journal
        let stripe_count = match level {
            RaidLevel::Raid1 => per_member_data / stripe_size.max(4096),
            RaidLevel::Raid5 | RaidLevel::Raid6 => per_member_data / stripe_size,
            RaidLevel::Raid10 => per_member_data / stripe_size,
        };

        let journal = WriteIntentJournal::in_memory(stripe_count);

        let device_id = DeviceId {
            uuid: array_uuid,
            serial: format!("{level}"),
            model: "RaidArray".to_string(),
            path: format!("raid:{array_id}"),
        };

        Ok(RaidArray {
            id: array_id,
            device_id,
            level,
            members: member_infos,
            stripe_size,
            capacity,
            parity_engine: ParityEngine::detect(),
            journal: tokio::sync::Mutex::new(journal),
            _rebuild_config: RebuildConfig::default(),
            read_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Array UUID.
    pub fn array_id(&self) -> RaidArrayId {
        self.id
    }

    /// RAID level.
    pub fn level(&self) -> RaidLevel {
        self.level
    }

    /// Number of member drives.
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Stripe size in bytes.
    pub fn stripe_size(&self) -> u64 {
        self.stripe_size
    }

    /// State of each member.
    pub fn member_states(&self) -> Vec<(usize, RaidMemberState)> {
        self.members.iter().enumerate()
            .map(|(i, m)| (i, m.state))
            .collect()
    }

    /// Access the journal (for testing/recovery).
    pub async fn journal_mut(&self) -> tokio::sync::MutexGuard<'_, WriteIntentJournal> {
        self.journal.lock().await
    }

    /// Set the state of a member drive (for testing degraded mode).
    pub fn set_member_state(&mut self, idx: usize, state: RaidMemberState) {
        if idx < self.members.len() {
            self.members[idx].state = state;
        }
    }

    /// Number of failed members.
    fn failed_count(&self) -> usize {
        self.members.iter()
            .filter(|m| m.state == RaidMemberState::Failed)
            .count()
    }

    /// Number of active members.
    fn active_members(&self) -> Vec<(usize, &MemberInfo)> {
        self.members.iter().enumerate()
            .filter(|(_, m)| m.state == RaidMemberState::Active || m.state == RaidMemberState::Rebuilding)
            .collect()
    }

    /// Start a rebuild on a replacement drive.
    pub async fn start_rebuild(&self, _target_member: usize) -> Result<Arc<RebuildProgress>, RaidError> {
        let stripe_count = self.capacity / self.stripe_size.max(1);
        let progress = RebuildProgress::new(stripe_count);
        // Full rebuild implementation would iterate stripes in a spawned task.
        // For now, return the progress tracker.
        Ok(progress)
    }

    /// Repair parity for a single stripe by reading data strips and rewriting parity.
    ///
    /// Used by both journal recovery and scrub. Only applicable to RAID 5/6.
    /// Returns true if parity was actually rewritten (was different from expected).
    async fn repair_stripe_parity(&self, stripe: u64) -> Result<bool, RaidError> {
        let member_count = self.members.len() as u32;
        let data_disks = self.data_disks();
        let strip_size = self.stripe_size as usize;
        let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);
        let parity_disk = parity_disk_for_stripe(stripe, member_count);

        // Read all data strips
        let mut data_strips: Vec<Vec<u8>> = Vec::with_capacity(data_disks as usize);
        for d in 0..data_disks {
            let disk = data_disk_index(d, stripe, member_count);
            let mut buf = vec![0u8; strip_size];
            self.members[disk as usize].device.read(phys_offset, &mut buf).await
                .map_err(|e| RaidError::MemberIo { member_idx: disk as usize, error: e })?;
            data_strips.push(buf);
        }

        // Read current parity
        let mut stored_parity = vec![0u8; strip_size];
        self.members[parity_disk as usize].device.read(phys_offset, &mut stored_parity).await
            .map_err(|e| RaidError::MemberIo { member_idx: parity_disk as usize, error: e })?;

        // Compute expected parity
        let strip_refs: Vec<&[u8]> = data_strips.iter().map(|s| s.as_slice()).collect();
        let mut expected_parity = vec![0u8; strip_size];
        self.parity_engine.compute_xor_parity(&strip_refs, &mut expected_parity);

        if stored_parity != expected_parity {
            // Rewrite correct parity
            self.members[parity_disk as usize].device.write(phys_offset, &expected_parity).await
                .map_err(|e| RaidError::MemberIo { member_idx: parity_disk as usize, error: e })?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Verify parity consistency for a single stripe. Returns true if consistent.
    async fn verify_stripe(&self, stripe: u64) -> Result<bool, RaidError> {
        match self.level {
            RaidLevel::Raid5 => {
                let member_count = self.members.len() as u32;
                let strip_size = self.stripe_size as usize;
                let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);

                // Read ALL strips (data + parity) and XOR — result should be all zeros
                let mut strips: Vec<Vec<u8>> = Vec::with_capacity(member_count as usize);
                for i in 0..member_count {
                    let mut buf = vec![0u8; strip_size];
                    self.members[i as usize].device.read(phys_offset, &mut buf).await
                        .map_err(|e| RaidError::MemberIo { member_idx: i as usize, error: e })?;
                    strips.push(buf);
                }

                let strip_refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
                let mut check = vec![0u8; strip_size];
                self.parity_engine.compute_xor_parity(&strip_refs, &mut check);
                Ok(check.iter().all(|&x| x == 0))
            }
            RaidLevel::Raid6 => {
                let member_count = self.members.len() as u32;
                let data_disks = self.data_disks();
                let strip_size = self.stripe_size as usize;
                let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);

                // Read data strips
                let mut data_strips: Vec<Vec<u8>> = Vec::with_capacity(data_disks as usize);
                for d in 0..data_disks {
                    let disk = data_disk_index(d, stripe, member_count);
                    let mut buf = vec![0u8; strip_size];
                    self.members[disk as usize].device.read(phys_offset, &mut buf).await
                        .map_err(|e| RaidError::MemberIo { member_idx: disk as usize, error: e })?;
                    data_strips.push(buf);
                }

                // Read stored P and Q (last two disks in rotation, but for simplicity
                // we use the same parity_disk_for_stripe for P; Q is on the next one)
                let p_disk = parity_disk_for_stripe(stripe, member_count);
                let mut stored_p = vec![0u8; strip_size];
                self.members[p_disk as usize].device.read(phys_offset, &mut stored_p).await
                    .map_err(|e| RaidError::MemberIo { member_idx: p_disk as usize, error: e })?;

                // Compute expected P and Q
                let strip_refs: Vec<&[u8]> = data_strips.iter().map(|s| s.as_slice()).collect();
                let mut expected_p = vec![0u8; strip_size];
                let mut _expected_q = vec![0u8; strip_size];
                self.parity_engine.compute_raid6_parity(&strip_refs, &mut expected_p, &mut _expected_q);

                Ok(stored_p == expected_p)
            }
            RaidLevel::Raid1 => {
                // Compare all active mirrors byte-for-byte
                let active = self.active_members();
                if active.len() < 2 {
                    return Ok(true);
                }
                let strip_size = self.stripe_size.max(4096) as usize;
                let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size.max(4096), DATA_OFFSET);

                let mut first = vec![0u8; strip_size];
                active[0].1.device.read(phys_offset, &mut first).await
                    .map_err(|e| RaidError::MemberIo { member_idx: active[0].0, error: e })?;

                for (idx, member) in &active[1..] {
                    let mut buf = vec![0u8; strip_size];
                    member.device.read(phys_offset, &mut buf).await
                        .map_err(|e| RaidError::MemberIo { member_idx: *idx, error: e })?;
                    if buf != first {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            RaidLevel::Raid10 => {
                // RAID 10 = striped mirrors; mirror consistency check
                Ok(true)
            }
        }
    }

    /// Recover from an unclean shutdown by repairing parity on dirty stripes.
    ///
    /// For RAID 5/6: reads data strips from each dirty stripe, recomputes parity,
    /// and rewrites it if it doesn't match. Data strips are treated as authoritative
    /// (writes are data-then-parity ordered).
    ///
    /// For RAID 1/10: mirrors are self-consistent; just clears the journal.
    ///
    /// Returns the number of stripes that had parity repaired.
    pub async fn recover_journal(&self) -> Result<u64, RaidError> {
        let dirty_stripes = {
            let j = self.journal.lock().await;
            j.dirty_stripes()
        };

        match self.level {
            RaidLevel::Raid1 | RaidLevel::Raid10 => {
                // Mirrors don't need parity repair — just clear
                let mut j = self.journal.lock().await;
                j.clear_all().map_err(|e| RaidError::Drive(DriveError::Other(e.into())))?;
                Ok(0)
            }
            RaidLevel::Raid5 | RaidLevel::Raid6 => {
                let mut repaired = 0u64;
                for stripe in &dirty_stripes {
                    if self.repair_stripe_parity(*stripe).await? {
                        repaired += 1;
                    }
                }
                let mut j = self.journal.lock().await;
                j.clear_all().map_err(|e| RaidError::Drive(DriveError::Other(e.into())))?;
                Ok(repaired)
            }
        }
    }

    /// Start a background scrub that verifies (and optionally repairs) parity
    /// across all stripes. Returns a progress handle immediately.
    pub fn start_scrub(self: &Arc<Self>, config: ScrubConfig) -> Arc<ScrubProgress> {
        let stripe_count = match self.level {
            RaidLevel::Raid1 | RaidLevel::Raid10 => {
                let unit = self.stripe_size.max(4096);
                let per_member_data = self.members.first()
                    .map(|m| m.device.capacity_bytes().saturating_sub(DATA_OFFSET))
                    .unwrap_or(0);
                per_member_data / unit
            }
            RaidLevel::Raid5 | RaidLevel::Raid6 => {
                let per_member_data = self.members.first()
                    .map(|m| m.device.capacity_bytes().saturating_sub(DATA_OFFSET))
                    .unwrap_or(0);
                per_member_data / self.stripe_size
            }
        };

        let progress = ScrubProgress::new(stripe_count);
        let progress_clone = Arc::clone(&progress);
        let array = Arc::clone(self);
        let delay = config.inter_stripe_delay();

        tokio::spawn(async move {
            for stripe in 0..stripe_count {
                if progress_clone.is_cancelled() {
                    break;
                }

                match array.verify_stripe(stripe).await {
                    Ok(true) => {}
                    Ok(false) => {
                        progress_clone.errors_found.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if config.repair {
                            match array.repair_stripe_parity(stripe).await {
                                Ok(_) => {
                                    progress_clone.errors_repaired.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                Err(e) => {
                                    tracing::error!("scrub repair failed on stripe {stripe}: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("scrub verify failed on stripe {stripe}: {e}");
                    }
                }

                progress_clone.advance();

                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        });

        progress
    }

    // --- RAID 1 I/O ---

    async fn raid1_read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let active = self.active_members();
        if active.is_empty() {
            return Err(RaidError::TooManyFailures {
                failed: self.failed_count(),
                max_tolerated: self.members.len() - 1,
            }.into());
        }

        // Round-robin across active members
        let idx = self.read_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (_, member) = active[(idx as usize) % active.len()];

        let phys_offset = DATA_OFFSET + offset;
        member.device.read(phys_offset, buf).await
    }

    async fn raid1_write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let active = self.active_members();
        if active.is_empty() {
            return Err(RaidError::TooManyFailures {
                failed: self.failed_count(),
                max_tolerated: self.members.len() - 1,
            }.into());
        }

        let phys_offset = DATA_OFFSET + offset;

        // Write to all active members in parallel
        let mut handles = Vec::with_capacity(active.len());
        for (idx, member) in &active {
            let dev = Arc::clone(&member.device);
            let data = buf.to_vec();
            let member_idx = *idx;
            handles.push(tokio::spawn(async move {
                (member_idx, dev.write(phys_offset, &data).await)
            }));
        }

        let mut last_result = Ok(0);
        for handle in handles {
            match handle.await {
                Ok((_, Ok(n))) => last_result = Ok(n),
                Ok((idx, Err(e))) => {
                    tracing::error!("RAID-1 write failed on member {idx}: {e}");
                    // In production, would mark member as failed
                    last_result = Err(e);
                }
                Err(e) => {
                    tracing::error!("RAID-1 write task panicked: {e}");
                    return Err(DriveError::Other(e.into()));
                }
            }
        }
        last_result
    }

    async fn raid1_flush(&self) -> DriveResult<()> {
        let active = self.active_members();
        let mut handles = Vec::with_capacity(active.len());
        for (idx, member) in &active {
            let dev = Arc::clone(&member.device);
            let member_idx = *idx;
            handles.push(tokio::spawn(async move {
                (member_idx, dev.flush().await)
            }));
        }
        for handle in handles {
            match handle.await {
                Ok((_, Ok(()))) => {}
                Ok((idx, Err(e))) => {
                    tracing::error!("RAID-1 flush failed on member {idx}: {e}");
                    return Err(e);
                }
                Err(e) => return Err(DriveError::Other(e.into())),
            }
        }
        Ok(())
    }

    // --- RAID 5 I/O ---

    fn data_disks(&self) -> u32 {
        match self.level {
            RaidLevel::Raid5 => self.members.len() as u32 - 1,
            RaidLevel::Raid6 => self.members.len() as u32 - 2,
            _ => self.members.len() as u32,
        }
    }

    async fn raid5_read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let data_disks = self.data_disks();
        let member_count = self.members.len() as u32;
        let buf_len = buf.len() as u64;

        // Process one stripe-unit at a time
        let mut bytes_read = 0u64;
        let mut pos = offset;

        while bytes_read < buf_len {
            let (stripe, offset_in_stripe) = offset_to_stripe(pos, self.stripe_size, data_disks);
            let data_idx = (offset_in_stripe / self.stripe_size) as u32;
            let offset_in_unit = offset_in_stripe % self.stripe_size;

            let disk = data_disk_index(data_idx, stripe, member_count);
            let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET) + offset_in_unit;

            let remaining_in_unit = self.stripe_size - offset_in_unit;
            let remaining_in_buf = buf_len - bytes_read;
            let to_read = remaining_in_unit.min(remaining_in_buf) as usize;

            let buf_start = bytes_read as usize;
            let buf_end = buf_start + to_read;

            let member = &self.members[disk as usize];
            if member.state == RaidMemberState::Failed {
                // Degraded read: reconstruct from surviving members
                self.raid5_degraded_read(stripe, data_idx, &mut buf[buf_start..buf_end]).await?;
            } else {
                member.device.read(phys_offset, &mut buf[buf_start..buf_end]).await?;
            }

            bytes_read += to_read as u64;
            pos += to_read as u64;
        }

        Ok(bytes_read as usize)
    }

    async fn raid5_degraded_read(&self, stripe: u64, missing_data_idx: u32, output: &mut [u8]) -> DriveResult<()> {
        let member_count = self.members.len() as u32;
        let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);
        let missing_disk = data_disk_index(missing_data_idx, stripe, member_count);
        let read_len = output.len();

        // Read all surviving strips (including parity)
        let mut strips: Vec<Vec<u8>> = Vec::new();
        for i in 0..member_count {
            if i == missing_disk {
                continue;
            }
            let member = &self.members[i as usize];
            if member.state == RaidMemberState::Failed {
                return Err(RaidError::TooManyFailures {
                    failed: self.failed_count(),
                    max_tolerated: 1,
                }.into());
            }
            let mut strip = vec![0u8; read_len];
            member.device.read(phys_offset, &mut strip).await?;
            strips.push(strip);
        }

        // XOR all surviving strips to reconstruct the missing one
        let strip_refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
        self.parity_engine.reconstruct_xor(&strip_refs, output);
        Ok(())
    }

    async fn raid5_write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let data_disks = self.data_disks();
        let buf_len = buf.len() as u64;

        let mut bytes_written = 0u64;
        let mut pos = offset;

        while bytes_written < buf_len {
            let (stripe, offset_in_stripe) = offset_to_stripe(pos, self.stripe_size, data_disks);
            let offset_in_unit = offset_in_stripe % self.stripe_size;

            // Check if this is a full-stripe write
            let remaining = buf_len - bytes_written;
            let full_stripe_bytes = self.stripe_size * data_disks as u64;
            let start_of_stripe = offset_in_stripe == 0;

            if start_of_stripe && remaining >= full_stripe_bytes {
                // Full-stripe write: compute parity from all data strips, write everything
                self.raid5_full_stripe_write(stripe, &buf[bytes_written as usize..(bytes_written + full_stripe_bytes) as usize]).await?;
                bytes_written += full_stripe_bytes;
                pos += full_stripe_bytes;
            } else {
                // Partial-stripe write: read-modify-write
                let data_idx = (offset_in_stripe / self.stripe_size) as u32;
                let remaining_in_unit = self.stripe_size - offset_in_unit;
                let to_write = remaining_in_unit.min(remaining) as usize;

                let buf_start = bytes_written as usize;
                let buf_end = buf_start + to_write;

                // Mark stripe dirty in journal
                {
                    let mut j = self.journal.lock().await;
                    j.mark_dirty(stripe);
                }

                self.raid5_partial_write(stripe, data_idx, offset_in_unit, &buf[buf_start..buf_end]).await?;

                {
                    let mut j = self.journal.lock().await;
                    j.mark_clean(stripe);
                }

                bytes_written += to_write as u64;
                pos += to_write as u64;
            }
        }

        Ok(bytes_written as usize)
    }

    async fn raid5_full_stripe_write(&self, stripe: u64, data: &[u8]) -> DriveResult<()> {
        let data_disks = self.data_disks();
        let member_count = self.members.len() as u32;
        let strip_size = self.stripe_size as usize;

        // Mark dirty
        {
            let mut j = self.journal.lock().await;
            j.mark_dirty(stripe);
        }

        // Split data into per-disk strips
        let mut data_strips: Vec<&[u8]> = Vec::with_capacity(data_disks as usize);
        for i in 0..data_disks {
            let start = i as usize * strip_size;
            let end = start + strip_size;
            data_strips.push(&data[start..end]);
        }

        // Compute parity
        let mut parity = vec![0u8; strip_size];
        self.parity_engine.compute_xor_parity(&data_strips, &mut parity);

        let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);
        let parity_disk = parity_disk_for_stripe(stripe, member_count);

        // Write all strips + parity in parallel
        let mut handles = Vec::with_capacity(member_count as usize);

        for data_i in 0..data_disks {
            let disk = data_disk_index(data_i, stripe, member_count);
            let dev = Arc::clone(&self.members[disk as usize].device);
            let strip_data = data_strips[data_i as usize].to_vec();
            handles.push(tokio::spawn(async move {
                dev.write(phys_offset, &strip_data).await
            }));
        }

        // Write parity
        let parity_dev = Arc::clone(&self.members[parity_disk as usize].device);
        let parity_data = parity;
        handles.push(tokio::spawn(async move {
            parity_dev.write(phys_offset, &parity_data).await
        }));

        for handle in handles {
            handle.await.map_err(|e| DriveError::Other(e.into()))??;
        }

        // Mark clean
        {
            let mut j = self.journal.lock().await;
            j.mark_clean(stripe);
        }

        Ok(())
    }

    async fn raid5_partial_write(
        &self,
        stripe: u64,
        data_idx: u32,
        _offset_in_unit: u64,
        new_data: &[u8],
    ) -> DriveResult<()> {
        let member_count = self.members.len() as u32;
        let disk = data_disk_index(data_idx, stripe, member_count);
        let parity_disk = parity_disk_for_stripe(stripe, member_count);
        let phys_offset = stripe_to_disk_offset(stripe, self.stripe_size, DATA_OFFSET);

        // Read old data and old parity
        let mut old_data = vec![0u8; new_data.len()];
        let mut old_parity = vec![0u8; new_data.len()];

        self.members[disk as usize].device.read(phys_offset, &mut old_data).await?;
        self.members[parity_disk as usize].device.read(phys_offset, &mut old_parity).await?;

        // new_parity = old_parity ^ old_data ^ new_data
        let mut new_parity = old_parity;
        self.parity_engine.xor_in_place(&mut new_parity, &old_data);
        self.parity_engine.xor_in_place(&mut new_parity, new_data);

        // Write new data and new parity in parallel
        let data_dev = Arc::clone(&self.members[disk as usize].device);
        let parity_dev = Arc::clone(&self.members[parity_disk as usize].device);
        let data_buf = new_data.to_vec();
        let parity_buf = new_parity;

        let h1 = tokio::spawn(async move { data_dev.write(phys_offset, &data_buf).await });
        let h2 = tokio::spawn(async move { parity_dev.write(phys_offset, &parity_buf).await });

        h1.await.map_err(|e| DriveError::Other(e.into()))??;
        h2.await.map_err(|e| DriveError::Other(e.into()))??;

        Ok(())
    }

    async fn raid5_flush(&self) -> DriveResult<()> {
        let mut handles = Vec::with_capacity(self.members.len());
        for (i, member) in self.members.iter().enumerate() {
            if member.state != RaidMemberState::Failed {
                let dev = Arc::clone(&member.device);
                handles.push(tokio::spawn(async move {
                    (i, dev.flush().await)
                }));
            }
        }
        for handle in handles {
            match handle.await {
                Ok((_, Ok(()))) => {}
                Ok((idx, Err(e))) => {
                    tracing::error!("RAID-5 flush failed on member {idx}: {e}");
                    return Err(e);
                }
                Err(e) => return Err(DriveError::Other(e.into())),
            }
        }
        Ok(())
    }
}

// --- BlockDevice implementation for RaidArray ---

#[async_trait]
impl BlockDevice for RaidArray {
    fn id(&self) -> &DeviceId {
        &self.device_id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        // Use the block size of the first member
        self.members.first()
            .map(|m| m.device.block_size())
            .unwrap_or(4096)
    }

    fn optimal_io_size(&self) -> u32 {
        match self.level {
            RaidLevel::Raid1 => self.members.first()
                .map(|m| m.device.optimal_io_size())
                .unwrap_or(4096),
            RaidLevel::Raid5 | RaidLevel::Raid6 => {
                // Full stripe is optimal
                (self.stripe_size * self.data_disks() as u64) as u32
            }
            RaidLevel::Raid10 => self.stripe_size as u32,
        }
    }

    fn device_type(&self) -> DriveType {
        // Report as the type of the first member
        self.members.first()
            .map(|m| m.device.device_type())
            .unwrap_or(DriveType::File)
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        match self.level {
            RaidLevel::Raid1 => self.raid1_read(offset, buf).await,
            RaidLevel::Raid5 => self.raid5_read(offset, buf).await,
            RaidLevel::Raid6 => {
                // RAID 6 reads are identical to RAID 5 (just more parity disks)
                self.raid5_read(offset, buf).await
            }
            RaidLevel::Raid10 => {
                // RAID 10: for now, delegate to RAID 1 (simplified)
                self.raid1_read(offset, buf).await
            }
        }
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        match self.level {
            RaidLevel::Raid1 => self.raid1_write(offset, buf).await,
            RaidLevel::Raid5 => self.raid5_write(offset, buf).await,
            RaidLevel::Raid6 => self.raid5_write(offset, buf).await,
            RaidLevel::Raid10 => self.raid1_write(offset, buf).await,
        }
    }

    async fn flush(&self) -> DriveResult<()> {
        match self.level {
            RaidLevel::Raid1 | RaidLevel::Raid10 => self.raid1_flush().await,
            RaidLevel::Raid5 | RaidLevel::Raid6 => self.raid5_flush().await,
        }
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        match self.level {
            RaidLevel::Raid1 => {
                // Discard on all active members
                for member in &self.members {
                    if member.state == RaidMemberState::Active {
                        member.device.discard(DATA_OFFSET + offset, len).await?;
                    }
                }
                Ok(())
            }
            _ => Ok(()), // Discard for striped levels is complex — skip for now
        }
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        // Aggregate: healthy if all active members are healthy
        let healthy = self.members.iter()
            .filter(|m| m.state == RaidMemberState::Active)
            .all(|m| m.device.smart_status().map(|s| s.healthy).unwrap_or(false));
        Ok(SmartData { healthy, ..Default::default() })
    }

    fn media_errors(&self) -> u64 {
        self.members.iter().map(|m| m.device.media_errors()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn create_test_devices(count: usize, size: u64) -> (Vec<Arc<dyn BlockDevice>>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-raid-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
        let mut paths = Vec::new();
        for i in 0..count {
            let path = dir.join(format!("{test_id}-member-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, size).await.unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }
        (devices, paths)
    }

    fn cleanup_test_files(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    // --- Superblock tests ---

    #[test]
    fn superblock_roundtrip() {
        let uuid = Uuid::new_v4();
        let member_uuid = Uuid::new_v4();
        let sb = RaidSuperblock::new(uuid, 0, member_uuid, RaidLevel::Raid5, 4, 65536, 1024 * 1024);
        let bytes = sb.to_bytes();
        assert_eq!(bytes.len(), 4096);

        let sb2 = RaidSuperblock::from_bytes(&bytes).unwrap();
        assert_eq!(sb2.array_uuid, *uuid.as_bytes());
        assert_eq!(sb2.member_index, 0);
        assert_eq!(sb2.level, 5);
        assert_eq!(sb2.member_count, 4);
        assert_eq!(sb2.stripe_size, 65536);
        assert_eq!(sb2.data_offset, DATA_OFFSET);
    }

    #[test]
    fn superblock_bad_magic() {
        let mut bytes = vec![0u8; 4096];
        bytes[0..8].copy_from_slice(b"BADMAGIC");
        assert!(RaidSuperblock::from_bytes(&bytes).is_err());
    }

    #[test]
    fn superblock_checksum_corruption() {
        let uuid = Uuid::new_v4();
        let sb = RaidSuperblock::new(uuid, 0, Uuid::new_v4(), RaidLevel::Raid1, 2, 65536, 1024 * 1024);
        let mut bytes = sb.to_bytes();
        // Corrupt a data byte
        bytes[50] ^= 0xFF;
        assert!(matches!(
            RaidSuperblock::from_bytes(&bytes),
            Err(RaidError::ChecksumError)
        ));
    }

    // --- Geometry tests ---

    #[test]
    fn parity_rotation() {
        // 4 disks: parity rotates 3, 2, 1, 0, 3, 2, ...
        assert_eq!(parity_disk_for_stripe(0, 4), 3);
        assert_eq!(parity_disk_for_stripe(1, 4), 2);
        assert_eq!(parity_disk_for_stripe(2, 4), 1);
        assert_eq!(parity_disk_for_stripe(3, 4), 0);
        assert_eq!(parity_disk_for_stripe(4, 4), 3);
    }

    #[test]
    fn data_disk_skips_parity() {
        // Stripe 0 with 4 disks: parity on disk 3
        // data_idx 0 -> disk 0, data_idx 1 -> disk 1, data_idx 2 -> disk 2
        assert_eq!(data_disk_index(0, 0, 4), 0);
        assert_eq!(data_disk_index(1, 0, 4), 1);
        assert_eq!(data_disk_index(2, 0, 4), 2);

        // Stripe 3 with 4 disks: parity on disk 0
        // data_idx 0 -> disk 1, data_idx 1 -> disk 2, data_idx 2 -> disk 3
        assert_eq!(data_disk_index(0, 3, 4), 1);
        assert_eq!(data_disk_index(1, 3, 4), 2);
        assert_eq!(data_disk_index(2, 3, 4), 3);
    }

    // --- RAID 1 integration tests ---

    #[tokio::test]
    async fn raid1_create_and_roundtrip() {
        let (devices, paths) = create_test_devices(3, 2 * 1024 * 1024).await;
        let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();

        assert_eq!(array.level(), RaidLevel::Raid1);
        assert_eq!(array.member_count(), 3);
        assert!(array.capacity_bytes() > 0);

        // Write data
        let write_data = vec![0xAB_u8; 4096];
        let written = array.write(0, &write_data).await.unwrap();
        assert_eq!(written, 4096);

        // Read back
        let mut read_data = vec![0u8; 4096];
        let read = array.read(0, &mut read_data).await.unwrap();
        assert_eq!(read, 4096);
        assert_eq!(read_data, write_data);

        // Flush
        array.flush().await.unwrap();

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn raid1_write_at_offset() {
        let (devices, paths) = create_test_devices(2, 2 * 1024 * 1024).await;
        let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();

        // Write at various offsets
        let data_a = vec![0xAA_u8; 4096];
        let data_b = vec![0xBB_u8; 4096];
        array.write(0, &data_a).await.unwrap();
        array.write(4096, &data_b).await.unwrap();

        let mut buf = vec![0u8; 4096];
        array.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));

        array.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn raid1_insufficient_members() {
        let (devices, paths) = create_test_devices(1, 2 * 1024 * 1024).await;
        let result = RaidArray::create(RaidLevel::Raid1, devices, None).await;
        assert!(result.is_err());
        cleanup_test_files(&paths);
    }

    // --- RAID 5 integration tests ---

    #[tokio::test]
    async fn raid5_create_and_roundtrip() {
        let (devices, paths) = create_test_devices(4, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64; // Small stripe for easier testing
        let array = RaidArray::create(RaidLevel::Raid5, devices, Some(stripe_size)).await.unwrap();

        assert_eq!(array.level(), RaidLevel::Raid5);
        assert_eq!(array.member_count(), 4);
        assert!(array.capacity_bytes() > 0);

        // Full-stripe write: 3 data disks * 4096 = 12288 bytes
        let full_stripe: Vec<u8> = (0..12288u32).map(|i| (i % 256) as u8).collect();
        let written = array.write(0, &full_stripe).await.unwrap();
        assert_eq!(written, 12288);

        // Read back
        let mut read_buf = vec![0u8; 12288];
        let read = array.read(0, &mut read_buf).await.unwrap();
        assert_eq!(read, 12288);
        assert_eq!(read_buf, full_stripe);

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn raid5_partial_stripe_write() {
        let (devices, paths) = create_test_devices(4, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64;
        let array = RaidArray::create(RaidLevel::Raid5, devices, Some(stripe_size)).await.unwrap();

        // Write less than a full stripe
        let data = vec![0xCD_u8; 4096];
        let written = array.write(0, &data).await.unwrap();
        assert_eq!(written, 4096);

        let mut read_buf = vec![0u8; 4096];
        array.read(0, &mut read_buf).await.unwrap();
        assert_eq!(read_buf, data);

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn raid5_insufficient_members() {
        let (devices, paths) = create_test_devices(2, 2 * 1024 * 1024).await;
        let result = RaidArray::create(RaidLevel::Raid5, devices, None).await;
        assert!(result.is_err());
        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn raid5_parity_verify() {
        // Create RAID 5 array, write known data, then verify parity is correct
        let (devices, paths) = create_test_devices(3, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64;
        let array = RaidArray::create(RaidLevel::Raid5, devices.clone(), Some(stripe_size)).await.unwrap();

        // Write a full stripe: 2 data disks * 4096 = 8192
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 256) as u8).collect();
        array.write(0, &data).await.unwrap();
        array.flush().await.unwrap();

        // Read raw strips from each member to verify parity
        let phys_offset = DATA_OFFSET;
        let mut strip0 = vec![0u8; 4096];
        let mut strip1 = vec![0u8; 4096];
        let mut strip2 = vec![0u8; 4096];

        devices[0].read(phys_offset, &mut strip0).await.unwrap();
        devices[1].read(phys_offset, &mut strip1).await.unwrap();
        devices[2].read(phys_offset, &mut strip2).await.unwrap();

        // XOR of all three should be zero (data0 ^ data1 ^ parity = 0)
        let engine = ParityEngine::detect();
        let mut check = vec![0u8; 4096];
        engine.compute_xor_parity(&[&strip0, &strip1, &strip2], &mut check);
        assert!(check.iter().all(|&x| x == 0), "parity check failed");

        cleanup_test_files(&paths);
    }

    // --- Scrub tests ---

    #[tokio::test]
    async fn scrub_detects_and_repairs_bad_parity() {
        use crate::raid::rebuild::ScrubConfig;

        let (devices, paths) = create_test_devices(4, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64;
        let raw_devices: Vec<Arc<dyn BlockDevice>> = devices.iter().map(Arc::clone).collect();

        let array = RaidArray::create(RaidLevel::Raid5, devices, Some(stripe_size)).await.unwrap();

        // Write a full stripe
        let full_stripe: Vec<u8> = (0..12288u32).map(|i| (i % 256) as u8).collect();
        array.write(0, &full_stripe).await.unwrap();
        array.flush().await.unwrap();

        // Corrupt parity on stripe 0 (parity disk = 3)
        let corrupt = vec![0xFF_u8; 4096];
        raw_devices[3].write(DATA_OFFSET, &corrupt).await.unwrap();
        raw_devices[3].flush().await.unwrap();

        // Run scrub with repair
        let array = Arc::new(array);
        let progress = array.start_scrub(ScrubConfig {
            max_stripes_per_sec: 0,
            repair: true,
        });

        // Wait for completion
        for _ in 0..1000 {
            if progress.percent() >= 100.0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(progress.percent() >= 100.0, "scrub did not complete");
        assert_eq!(progress.found(), 1, "expected 1 error found");
        assert_eq!(progress.repaired(), 1, "expected 1 error repaired");

        // Verify data is still intact
        let mut readback = vec![0u8; 12288];
        array.read(0, &mut readback).await.unwrap();
        assert_eq!(readback, full_stripe);

        // Run scrub again — should find no errors
        let progress2 = array.start_scrub(ScrubConfig {
            max_stripes_per_sec: 0,
            repair: true,
        });
        for _ in 0..1000 {
            if progress2.percent() >= 100.0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(progress2.found(), 0, "second scrub should find no errors");

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn scrub_report_only() {
        use crate::raid::rebuild::ScrubConfig;

        let (devices, paths) = create_test_devices(4, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64;
        let raw_devices: Vec<Arc<dyn BlockDevice>> = devices.iter().map(Arc::clone).collect();

        let array = RaidArray::create(RaidLevel::Raid5, devices, Some(stripe_size)).await.unwrap();

        // Write data then corrupt parity
        let data: Vec<u8> = (0..12288u32).map(|i| (i % 256) as u8).collect();
        array.write(0, &data).await.unwrap();
        array.flush().await.unwrap();

        let corrupt = vec![0xDE_u8; 4096];
        raw_devices[3].write(DATA_OFFSET, &corrupt).await.unwrap();
        raw_devices[3].flush().await.unwrap();

        // Report-only scrub
        let array = Arc::new(array);
        let progress = array.start_scrub(ScrubConfig {
            max_stripes_per_sec: 0,
            repair: false,
        });

        for _ in 0..1000 {
            if progress.percent() >= 100.0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(progress.found(), 1, "expected 1 error found");
        assert_eq!(progress.repaired(), 0, "report-only should not repair");

        cleanup_test_files(&paths);
    }

    #[tokio::test]
    async fn scrub_cancel() {
        use crate::raid::rebuild::ScrubConfig;

        // Use larger devices so there are many stripes to scrub
        let (devices, paths) = create_test_devices(4, 2 * 1024 * 1024).await;
        let stripe_size = 4096u64;

        let array = RaidArray::create(RaidLevel::Raid5, devices, Some(stripe_size)).await.unwrap();
        let array = Arc::new(array);

        let progress = array.start_scrub(ScrubConfig {
            max_stripes_per_sec: 100, // slow rate so we can cancel
            repair: false,
        });

        // Let it start, then cancel
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        progress.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(progress.completed() < progress.total_stripes,
            "scrub should have been cancelled before completing (completed {}, total {})",
            progress.completed(), progress.total_stripes);

        cleanup_test_files(&paths);
    }
}
