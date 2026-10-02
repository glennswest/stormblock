//! RAID engine — drive-level RAID 1/5/6/10 sets (#252, #168).
//!
//! `RaidArray` implements `BlockDevice`, so the volume manager sees an array
//! as one more device and formats a slab on it. A shelf is laid out as several
//! arrays ("sets"), each its own failure domain, with hot spares beside them.
//!
//! What an array keeps on every member (the first `DATA_OFFSET` bytes):
//! - the **superblock** (`superblock.rs`): the array's description, the slot
//!   table and an `events` counter, so any member says what the array is and
//!   which copy of that description is newest — what `assemble` reads;
//! - the **write-intent bitmap** (`bitmap.rs`): the chunks with writes that
//!   may be half done, set before a write and cleared lazily, so a crash
//!   costs a resync of those chunks only.
//!
//! The rules the I/O paths keep:
//! - **One stripe at a time is changed under its lock** (`StripeLocks`), so
//!   two writers never read-modify-write the same parity, and a rebuild or a
//!   resync never sees a stripe half written.
//! - **A member is read only where it holds the data**: an `Active` member
//!   anywhere, a `Rebuilding` one only below how far its rebuild has got
//!   (#175). Above that it is treated as missing.
//! - **A member whose I/O fails is failed** and the I/O continues degraded,
//!   unless failing it would lose data (then the I/O errors and the member
//!   stays: the array keeps serving what it still can). The superblocks say
//!   so before the I/O is acknowledged — otherwise a restart would trust the
//!   stale member again.
//! - **A failed slot takes a hot spare** from the array's pool, then the
//!   global one, and rebuilds onto it in the background (`start`).

pub mod bitmap;
pub mod layout;
pub mod parity;
pub mod rebuild;
pub mod spares;
pub mod superblock;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};
use crate::raid::bitmap::{IntentBitmap, BITMAP_OFFSET};
use crate::raid::layout::{Geometry, StripRole};
use crate::raid::parity::{ParityEngine, StripeStrips};
use crate::raid::rebuild::{RebuildConfig, RebuildProgress, ScrubConfig, ScrubProgress};
use crate::raid::spares::SparePool;
pub use crate::raid::superblock::{SlotRecord, Superblock, MAX_SLOTS};

/// Room at the start of each member for the superblock and the bitmap.
pub const DATA_OFFSET: u64 = 1024 * 1024;

/// Default stripe unit for RAID 5/6/10 (64 KB).
pub const DEFAULT_STRIPE_SIZE: u64 = 64 * 1024;

/// RAID-1's lock and rebuild unit: a mirror has no stripes, so its ranges are
/// locked a MiB at a time.
pub const MIRROR_UNIT: u64 = 1024 * 1024;

const LOCK_SHARDS: usize = 1024;
/// A rebuild or resync locks this much member data at once.
const REBUILD_BATCH: u64 = 4 * 1024 * 1024;
/// How long a chunk must have been idle before a flush clears its bit.
const CLEAR_DELAY: Duration = Duration::from_secs(5);
/// How often a running rebuild records how far it has got.
const CHECKPOINT_EVERY: Duration = Duration::from_secs(30);

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// --- Core types ---

/// RAID level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaidLevel {
    /// Mirror: all members hold identical copies.
    #[serde(alias = "raid1", alias = "RAID1", alias = "mirror")]
    Raid1,
    /// Distributed parity: N data + 1 rotating parity.
    #[serde(alias = "raid5", alias = "RAID5")]
    Raid5,
    /// Dual parity: N data + 2 rotating parity (P + Q).
    #[serde(alias = "raid6", alias = "RAID6")]
    Raid6,
    /// Striped mirrors: pairs of mirrors striped together.
    #[serde(alias = "raid10", alias = "RAID10")]
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

impl RaidLevel {
    /// The fewest members the level is built from.
    pub fn min_members(&self) -> usize {
        match self {
            RaidLevel::Raid1 => 2,
            RaidLevel::Raid5 => 3,
            RaidLevel::Raid6 => 4,
            RaidLevel::Raid10 => 4,
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

/// State of an individual member slot within the array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaidMemberState {
    Active,
    /// Not used by this engine; read from nothing it writes. Kept so a value
    /// that was once serialised still parses.
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
    /// Stripe geometry error, or a request that does not fit the array.
    InvalidStripe(String),
    /// Operation not supported for this RAID level.
    NotSupported(String),
    /// Cannot remove member (would leave array with insufficient members).
    CannotRemoveMember(String),
}

impl fmt::Display for RaidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RaidError::InsufficientMembers { need, have } => write!(f, "need {need} members, have {have}"),
            RaidError::TooManyFailures { failed, max_tolerated } => {
                write!(f, "{failed} members failed (max tolerated: {max_tolerated})")
            }
            RaidError::SuperblockMismatch(msg) => write!(f, "superblock mismatch: {msg}"),
            RaidError::ChecksumError => write!(f, "superblock checksum error"),
            RaidError::Drive(e) => write!(f, "drive error: {e}"),
            RaidError::MemberIo { member_idx, error } => write!(f, "member {member_idx} I/O error: {error}"),
            RaidError::InvalidStripe(msg) => write!(f, "{msg}"),
            RaidError::NotSupported(msg) => write!(f, "not supported: {msg}"),
            RaidError::CannotRemoveMember(msg) => write!(f, "cannot remove member: {msg}"),
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

// --- Stripe locks ---

/// A fixed table of locks, a stripe (or a mirror unit, or a RAID-10 row)
/// hashed onto one. Whoever takes several takes them in shard order, so two
/// lockers never wait on each other in a circle.
struct StripeLocks {
    shards: Vec<tokio::sync::Mutex<()>>,
}

impl StripeLocks {
    fn new() -> Self {
        StripeLocks { shards: (0..LOCK_SHARDS).map(|_| tokio::sync::Mutex::new(())).collect() }
    }

    async fn lock(&self, key: u64) -> tokio::sync::MutexGuard<'_, ()> {
        self.shards[(key % LOCK_SHARDS as u64) as usize].lock().await
    }

    async fn lock_many(&self, keys: std::ops::Range<u64>) -> Vec<tokio::sync::MutexGuard<'_, ()>> {
        let mut idx: Vec<usize> = if keys.end - keys.start >= LOCK_SHARDS as u64 {
            (0..LOCK_SHARDS).collect()
        } else {
            keys.map(|k| (k % LOCK_SHARDS as u64) as usize).collect()
        };
        idx.sort_unstable();
        idx.dedup();
        let mut out = Vec::with_capacity(idx.len());
        for i in idx {
            out.push(self.shards[i].lock().await);
        }
        out
    }
}

// --- Members ---

pub(crate) struct Member {
    /// `None` when the array was assembled without this member's drive.
    pub(crate) device: Option<Arc<dyn BlockDevice>>,
    pub(crate) state: RaidMemberState,
    pub(crate) uuid: Uuid,
    /// For a `Rebuilding` member: member data bytes that hold the data.
    pub(crate) rebuilt_to: u64,
}

impl Member {
    /// Whether this member holds the data of a member range ending at `end`.
    fn holds(&self, end: u64) -> bool {
        self.device.is_some()
            && match self.state {
                RaidMemberState::Active => true,
                RaidMemberState::Rebuilding => end <= self.rebuilt_to,
                _ => false,
            }
    }

    /// Whether it takes superblocks, bitmap pages and flushes.
    fn attached(&self) -> bool {
        self.device.is_some() && matches!(self.state, RaidMemberState::Active | RaidMemberState::Rebuilding)
    }
}

/// How an array is doing, for the API and health.
#[derive(Debug, Clone, Serialize)]
pub struct ArrayStatus {
    /// `clean`, `degraded` (lost redundancy, nothing rebuilding),
    /// `rebuilding`, or `failed` (lost data).
    pub state: &'static str,
    pub failed: usize,
    pub rebuilding: usize,
    pub tolerated: usize,
    /// Bitmap chunks with writes that may be half done.
    pub dirty_chunks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<rebuild::RebuildStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrub: Option<rebuild::ScrubStatus>,
}

/// One member as the API shows it.
#[derive(Debug, Clone)]
pub struct MemberView {
    pub slot: usize,
    pub uuid: Uuid,
    pub state: RaidMemberState,
    /// The drive's identity, when the drive is here.
    pub drive: Option<DeviceId>,
    /// For a rebuilding member: bytes of its data rebuilt.
    pub rebuilt_to: u64,
}

/// How to make an array.
pub struct CreateOptions {
    pub level: RaidLevel,
    pub members: Vec<Arc<dyn BlockDevice>>,
    pub stripe_size: Option<u64>,
    /// The set's name (≤ 64 bytes).
    pub name: String,
    /// The spare pool it takes spares from; "" = only the global one.
    pub pool: String,
}

// --- RaidArray ---

/// A software RAID array that implements `BlockDevice`.
pub struct RaidArray {
    id: RaidArrayId,
    device_id: DeviceId,
    level: RaidLevel,
    stripe_size: u64,
    /// Member data bytes each member carries.
    data_size: u64,
    capacity: AtomicU64,
    create_time: u64,
    names: std::sync::Mutex<(String, String)>,
    members: std::sync::RwLock<Vec<Member>>,
    events: AtomicU64,
    parity_engine: ParityEngine,
    locks: StripeLocks,
    bitmap: IntentBitmap,
    sb_write: tokio::sync::Mutex<()>,
    sb_dirty: AtomicBool,
    read_counter: AtomicU64,
    /// Signalled when a member fails, for the supervisor.
    failures: Arc<tokio::sync::Notify>,
    spares: std::sync::RwLock<Option<Arc<SparePool>>>,
    rebuild_running: AtomicBool,
    rebuild_progress: std::sync::Mutex<Option<Arc<RebuildProgress>>>,
    scrub_progress: std::sync::Mutex<Option<Arc<ScrubProgress>>>,
    rebuild_config: std::sync::Mutex<RebuildConfig>,
    stopped: AtomicBool,
}

fn device_id_for(id: RaidArrayId, level: RaidLevel) -> DeviceId {
    DeviceId {
        wwn: String::new(),
        uuid: id.0,
        // Unique per array: a failure domain keys on the serial, and two sets
        // of one level must never be one domain.
        serial: format!("raid-{}", id.0),
        model: format!("RaidArray {level}"),
        path: format!("raid:{id}"),
    }
}

impl RaidArray {
    /// Create a new array from member devices, writing a fresh superblock and
    /// a clear bitmap to every member. Whatever was on them is gone.
    pub async fn create(
        level: RaidLevel,
        members: Vec<Arc<dyn BlockDevice>>,
        stripe_size: Option<u64>,
    ) -> Result<Self, RaidError> {
        Self::create_with(CreateOptions { level, members, stripe_size, name: String::new(), pool: String::new() })
            .await
    }

    pub async fn create_with(opts: CreateOptions) -> Result<Self, RaidError> {
        let CreateOptions { level, members, stripe_size, name, pool } = opts;
        let count = members.len();
        if count < level.min_members() {
            return Err(RaidError::InsufficientMembers { need: level.min_members(), have: count });
        }
        if level == RaidLevel::Raid10 && count % 2 != 0 {
            return Err(RaidError::InvalidStripe(format!("RAID-10 needs an even number of members, not {count}")));
        }
        if count > MAX_SLOTS {
            return Err(RaidError::InvalidStripe(format!("at most {MAX_SLOTS} members, not {count}")));
        }
        superblock::check_name("name", &name)?;
        superblock::check_name("pool", &pool)?;
        let stripe_size = stripe_size.unwrap_or(DEFAULT_STRIPE_SIZE);
        if stripe_size < 4096 || stripe_size % 4096 != 0 {
            return Err(RaidError::InvalidStripe(format!("stripe unit {stripe_size} is not a multiple of 4096")));
        }
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                if a.id().uuid == b.id().uuid && a.id().path == b.id().path {
                    return Err(RaidError::InvalidStripe(format!("drive {} is named twice", a.id().path)));
                }
            }
        }

        let min_capacity = members.iter().map(|m| m.capacity_bytes()).min().unwrap_or(0);
        if min_capacity <= DATA_OFFSET + stripe_size {
            return Err(RaidError::Drive(DriveError::OutOfRange { offset: DATA_OFFSET, len: 0, capacity: min_capacity }));
        }
        let raw = min_capacity - DATA_OFFSET;
        let data_size = match level {
            RaidLevel::Raid1 => raw / 4096 * 4096,
            _ => raw / stripe_size * stripe_size,
        };

        let id = RaidArrayId(Uuid::new_v4());
        let members: Vec<Member> = members
            .into_iter()
            .map(|d| Member { device: Some(d), state: RaidMemberState::Active, uuid: Uuid::new_v4(), rebuilt_to: 0 })
            .collect();
        let array = Self::build(id, level, stripe_size, data_size, now_secs(), name, pool, members, 1);
        // A clear bitmap first, then the superblocks that point at it.
        array.zero_bitmaps(None).await?;
        array.write_superblocks().await?;
        if array.failed_count() > 0 {
            return Err(RaidError::TooManyFailures { failed: array.failed_count(), max_tolerated: 0 });
        }
        Ok(array)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: RaidArrayId,
        level: RaidLevel,
        stripe_size: u64,
        data_size: u64,
        create_time: u64,
        name: String,
        pool: String,
        members: Vec<Member>,
        events: u64,
    ) -> Self {
        let geo = Geometry { level, n: members.len(), unit: stripe_size };
        let align = if level == RaidLevel::Raid1 { MIRROR_UNIT } else { stripe_size };
        let (chunk, bits, bytes) = bitmap::geometry(data_size, align);
        RaidArray {
            id,
            device_id: device_id_for(id, level),
            level,
            stripe_size,
            data_size,
            capacity: AtomicU64::new(geo.capacity(data_size)),
            create_time,
            names: std::sync::Mutex::new((name, pool)),
            members: std::sync::RwLock::new(members),
            events: AtomicU64::new(events),
            parity_engine: ParityEngine::detect(),
            locks: StripeLocks::new(),
            bitmap: IntentBitmap::new(chunk, bits, bytes),
            sb_write: tokio::sync::Mutex::new(()),
            sb_dirty: AtomicBool::new(false),
            read_counter: AtomicU64::new(0),
            failures: Arc::new(tokio::sync::Notify::new()),
            spares: std::sync::RwLock::new(None),
            rebuild_running: AtomicBool::new(false),
            rebuild_progress: std::sync::Mutex::new(None),
            scrub_progress: std::sync::Mutex::new(None),
            rebuild_config: std::sync::Mutex::new(RebuildConfig::default()),
            stopped: AtomicBool::new(false),
        }
    }

    /// Put an array back together from its members' superblocks.
    ///
    /// The superblock with the most `events` is the description; each of its
    /// slots is matched to the drive whose superblock carries that member
    /// uuid. A slot whose drive is absent is failed. Refused when more are
    /// missing than the level tolerates — the data would not be whole, and
    /// assembling it anyway would hand out holes as data.
    ///
    /// Chunks the bitmap marks are resynced before this returns. A slot that
    /// was rebuilding stays rebuilding, from where it had got; `start` resumes
    /// it.
    pub async fn assemble(found: Vec<(Arc<dyn BlockDevice>, Superblock)>) -> Result<Self, RaidError> {
        let newest = found
            .iter()
            .map(|(_, sb)| sb)
            .filter(|sb| !sb.is_spare())
            .max_by_key(|sb| sb.events)
            .cloned()
            .ok_or_else(|| RaidError::SuperblockMismatch("no member superblock to assemble from".into()))?;
        let level = newest.level.unwrap();
        for (dev, sb) in &found {
            if sb.array_uuid != newest.array_uuid {
                return Err(RaidError::SuperblockMismatch(format!(
                    "{} belongs to array {}, not {}",
                    dev.id().path, sb.array_uuid, newest.array_uuid
                )));
            }
        }
        let mut changed = false;
        let mut members = Vec::with_capacity(newest.slots.len());
        for (slot, rec) in newest.slots.iter().enumerate() {
            let dev = found.iter().find(|(_, sb)| sb.member_uuid == rec.member_uuid).map(|(d, _)| d.clone());
            let mut state = rec.state;
            if dev.is_none() && state != RaidMemberState::Failed {
                tracing::warn!(
                    "array {} ({}): slot {slot}'s drive (member {}) is not here — failed",
                    newest.array_uuid, newest.name, rec.member_uuid
                );
                state = RaidMemberState::Failed;
                changed = true;
            }
            if let Some(d) = &dev {
                if d.capacity_bytes() < DATA_OFFSET + newest.data_size {
                    tracing::warn!("array {}: slot {slot}'s drive {} is too small — failed", newest.array_uuid, d.id().path);
                    state = RaidMemberState::Failed;
                    changed = true;
                }
            }
            members.push(Member { device: dev, state, uuid: rec.member_uuid, rebuilt_to: rec.rebuilt_to });
        }
        let events = newest.events + changed as u64;
        let array = Self::build(
            RaidArrayId(newest.array_uuid),
            level,
            newest.stripe_size,
            newest.data_size,
            newest.create_time,
            newest.name.clone(),
            newest.pool.clone(),
            members,
            events,
        );
        if !array.data_intact() {
            return Err(RaidError::TooManyFailures {
                failed: array.failed_count() + array.rebuilding_count(),
                max_tolerated: array.geometry().tolerated(),
            });
        }

        // What may be half written: the union of every attached member's map.
        let mut images = Vec::new();
        for d in array.attached_devices() {
            let mut img = vec![0u8; array.bitmap.bytes as usize];
            if d.1.read(BITMAP_OFFSET, &mut img).await.is_ok() {
                images.push(img);
            }
        }
        let dirty = bitmap::dirty_chunks(&images, array.bitmap.bits);
        if !dirty.is_empty() {
            tracing::warn!(
                "array {} ({}): {} chunk(s) were being written when it stopped — resyncing them",
                array.id, newest.name, dirty.len()
            );
            for c in &dirty {
                let start = c * array.bitmap.chunk;
                let end = (start + array.bitmap.chunk).min(array.data_size);
                array.resync_range(start, end, true, None).await?;
            }
            array.zero_bitmaps(None).await?;
        }
        array.sb_dirty.store(true, Ordering::SeqCst);
        array.persist_if_dirty().await;
        Ok(array)
    }

    // --- identity and views ---

    pub fn array_id(&self) -> RaidArrayId {
        self.id
    }

    pub fn level(&self) -> RaidLevel {
        self.level
    }

    pub fn member_count(&self) -> usize {
        self.members.read().unwrap().len()
    }

    pub fn stripe_size(&self) -> u64 {
        self.stripe_size
    }

    /// Member data bytes per member.
    pub fn data_size(&self) -> u64 {
        self.data_size
    }

    /// The size a drive must be to take a slot.
    pub fn member_bytes_needed(&self) -> u64 {
        DATA_OFFSET + self.data_size
    }

    pub fn name(&self) -> String {
        self.names.lock().unwrap().0.clone()
    }

    pub fn pool(&self) -> String {
        self.names.lock().unwrap().1.clone()
    }

    pub fn events(&self) -> u64 {
        self.events.load(Ordering::SeqCst)
    }

    pub fn create_time(&self) -> u64 {
        self.create_time
    }

    fn geometry(&self) -> Geometry {
        Geometry { level: self.level, n: self.member_count(), unit: self.stripe_size }
    }

    /// The lock (and rebuild) unit in member data bytes.
    fn lock_unit(&self) -> u64 {
        if self.level == RaidLevel::Raid1 { MIRROR_UNIT } else { self.stripe_size }
    }

    pub fn member_states(&self) -> Vec<(usize, RaidMemberState)> {
        self.members.read().unwrap().iter().enumerate().map(|(i, m)| (i, m.state)).collect()
    }

    pub fn member_uuids(&self) -> Vec<(Uuid, RaidMemberState)> {
        self.members.read().unwrap().iter().map(|m| (m.uuid, m.state)).collect()
    }

    /// Index, member uuid, state, and the drive's path ("" when absent).
    pub fn member_details(&self) -> Vec<(usize, Uuid, RaidMemberState, String)> {
        self.members
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, m)| (i, m.uuid, m.state, m.device.as_ref().map(|d| d.id().path.clone()).unwrap_or_default()))
            .collect()
    }

    /// Each member's index, state and the identity of the drive it is on —
    /// a partner named the way stormdrive names drives (#136).
    pub fn member_drives(&self) -> Vec<(usize, RaidMemberState, DeviceId)> {
        self.members
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let id = m.device.as_ref().map(|d| d.drive_id()).unwrap_or_else(|| DeviceId {
                    wwn: String::new(),
                    uuid: m.uuid,
                    serial: String::new(),
                    model: String::new(),
                    path: String::new(),
                });
                (i, m.state, id)
            })
            .collect()
    }

    pub fn members_view(&self) -> Vec<MemberView> {
        self.members
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(slot, m)| MemberView {
                slot,
                uuid: m.uuid,
                state: m.state,
                drive: m.device.as_ref().map(|d| d.drive_id()),
                rebuilt_to: m.rebuilt_to,
            })
            .collect()
    }

    /// The slot a device is in.
    pub fn slot_of(&self, id: &DeviceId) -> Option<usize> {
        self.members.read().unwrap().iter().position(|m| {
            m.device.as_ref().is_some_and(|d| d.id().uuid == id.uuid && d.id().path == id.path)
        })
    }

    /// The device ids (uuid, path) of every drive this array holds.
    pub fn drive_ids(&self) -> Vec<DeviceId> {
        self.members.read().unwrap().iter().filter_map(|m| m.device.as_ref().map(|d| d.id().clone())).collect()
    }

    fn attached_devices(&self) -> Vec<(usize, Arc<dyn BlockDevice>)> {
        self.members
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, m)| m.attached())
            .map(|(i, m)| (i, m.device.clone().unwrap()))
            .collect()
    }

    /// Per member, the device if it holds the data of a range ending at `end`.
    fn holders(&self, end: u64) -> Vec<Option<Arc<dyn BlockDevice>>> {
        self.members.read().unwrap().iter().map(|m| if m.holds(end) { m.device.clone() } else { None }).collect()
    }

    pub fn failed_count(&self) -> usize {
        self.members.read().unwrap().iter().filter(|m| m.state == RaidMemberState::Failed).count()
    }

    fn rebuilding_count(&self) -> usize {
        self.members.read().unwrap().iter().filter(|m| m.state == RaidMemberState::Rebuilding).count()
    }

    /// Whether every byte is still held by enough members to be read.
    fn data_intact(&self) -> bool {
        let members = self.members.read().unwrap();
        let geo = Geometry { level: self.level, n: members.len(), unit: self.stripe_size };
        // Counted at the worst point: a rebuilding member is missing above
        // its watermark.
        let lost = |m: &Member| !(m.device.is_some() && m.state == RaidMemberState::Active);
        match self.level {
            RaidLevel::Raid1 => members.iter().any(|m| !lost(m)),
            RaidLevel::Raid10 => (0..members.len() / 2).all(|p| !lost(&members[2 * p]) || !lost(&members[2 * p + 1])),
            _ => members.iter().filter(|m| lost(m)).count() <= geo.tolerated(),
        }
    }

    pub fn status(&self) -> ArrayStatus {
        let failed = self.failed_count();
        let rebuilding = self.rebuilding_count();
        let tolerated = self.geometry().tolerated();
        let state = if !self.data_intact() {
            "failed"
        } else if rebuilding > 0 {
            "rebuilding"
        } else if failed > 0 {
            "degraded"
        } else {
            "clean"
        };
        ArrayStatus {
            state,
            failed,
            rebuilding,
            tolerated,
            dirty_chunks: self.bitmap.dirty_count(),
            rebuild: self.rebuild_progress.lock().unwrap().as_ref().map(|p| p.status()),
            scrub: self.scrub_progress.lock().unwrap().as_ref().map(|p| p.status()),
        }
    }

    pub fn set_rebuild_config(&self, c: RebuildConfig) {
        *self.rebuild_config.lock().unwrap() = c;
    }

    // --- member state ---

    /// Set a member's state directly. A test hook: it records nothing on
    /// disk and starts nothing.
    pub fn set_member_state(&self, idx: usize, state: RaidMemberState) {
        let mut members = self.members.write().unwrap();
        if idx < members.len() {
            members[idx].state = state;
        }
    }

    /// Mark a member failed, and say why.
    ///
    /// Refused (returns `false`, nothing changes) when the member is already
    /// failed, or when failing it would lose data — the last member of a
    /// mirror, a RAID-10 member whose partner is gone, a parity member beyond
    /// the level's tolerance. Then the I/O that hit the error returns it, and
    /// the array keeps serving everything else.
    ///
    /// Otherwise the member is failed, `events` moves, the superblocks are
    /// marked to be rewritten (`persist_if_dirty`, which every I/O path calls
    /// before it answers), and the supervisor is woken to take a spare.
    pub fn fail_member(&self, idx: usize, why: &str) -> bool {
        let (uuid, path) = {
            let mut members = self.members.write().unwrap();
            if idx >= members.len() || members[idx].state == RaidMemberState::Failed {
                return false;
            }
            let was_rebuilding = members[idx].state == RaidMemberState::Rebuilding;
            if !was_rebuilding && !self.can_lose(&members, idx) {
                tracing::error!(
                    "{} {} member {idx} ({}): {why} — NOT failed: the array would lose data; \
                     the I/O returns the error instead",
                    self.level, self.id, members[idx].uuid
                );
                return false;
            }
            members[idx].state = RaidMemberState::Failed;
            let path = members[idx].device.as_ref().map(|d| d.drive_id().path).unwrap_or_default();
            (members[idx].uuid, path)
        };
        self.events.fetch_add(1, Ordering::SeqCst);
        self.sb_dirty.store(true, Ordering::SeqCst);
        tracing::warn!(
            "{} {} ({}) member {idx} ({uuid}, {path}) failed: {why}",
            self.level, self.id, self.name()
        );
        self.failures.notify_one();
        true
    }

    fn can_lose(&self, members: &[Member], idx: usize) -> bool {
        let ok = |m: &Member| m.device.is_some() && m.state == RaidMemberState::Active;
        match self.level {
            RaidLevel::Raid1 => members.iter().enumerate().any(|(i, m)| i != idx && ok(m)),
            RaidLevel::Raid10 => ok(&members[idx ^ 1]),
            _ => {
                let lost = members.iter().enumerate().filter(|(i, m)| *i != idx && !ok(m)).count();
                lost < Geometry { level: self.level, n: members.len(), unit: self.stripe_size }.tolerated()
            }
        }
    }

    // --- superblocks and the bitmap on disk ---

    fn superblock_for(&self, members: &[Member], slot: usize, events: u64) -> Superblock {
        let (name, pool) = self.names.lock().unwrap().clone();
        Superblock {
            array_uuid: self.id.0,
            member_uuid: members[slot].uuid,
            slot: Some(slot as u32),
            level: Some(self.level),
            stripe_size: self.stripe_size,
            data_offset: DATA_OFFSET,
            data_size: self.data_size,
            create_time: self.create_time,
            update_time: now_secs(),
            events,
            bitmap_offset: BITMAP_OFFSET,
            bitmap_bytes: self.bitmap.bytes,
            bitmap_chunk: self.bitmap.chunk,
            name,
            pool,
            slots: members
                .iter()
                .map(|m| SlotRecord { member_uuid: m.uuid, state: m.state, rebuilt_to: m.rebuilt_to })
                .collect(),
        }
    }

    /// Write the current description to every attached member. A member
    /// that cannot take it is failed and the round is repeated, so the
    /// members that remain all say the same thing.
    pub async fn write_superblocks(&self) -> Result<(), RaidError> {
        let _g = self.sb_write.lock().await;
        for _ in 0..=MAX_SLOTS {
            self.sb_dirty.store(false, Ordering::SeqCst);
            let events = self.events.load(Ordering::SeqCst);
            let writes: Vec<(usize, Arc<dyn BlockDevice>, Vec<u8>)> = {
                let members = self.members.read().unwrap();
                (0..members.len())
                    .filter(|i| members[*i].attached())
                    .map(|i| (i, members[i].device.clone().unwrap(), self.superblock_for(&members, i, events).to_bytes()))
                    .collect()
            };
            let results = join_all(writes.iter().map(|(i, d, b)| async move {
                let r = match d.write(0, b).await {
                    Ok(_) => d.flush().await,
                    Err(e) => Err(e),
                };
                (*i, r)
            }))
            .await;
            let mut again = false;
            for (i, r) in results {
                if let Err(e) = r {
                    if self.fail_member(i, &format!("superblock write: {e}")) {
                        again = true;
                    } else {
                        return Err(RaidError::MemberIo { member_idx: i, error: e });
                    }
                }
            }
            if !again {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Rewrite the superblocks if anything changed since they were written.
    /// Errors are logged: the members that could not take them are failed.
    pub async fn persist_if_dirty(&self) {
        if self.sb_dirty.load(Ordering::SeqCst) {
            if let Err(e) = self.write_superblocks().await {
                tracing::error!("{} {}: writing superblocks: {e}", self.level, self.id);
            }
        }
    }

    /// Zero the bitmap area on every attached member (or one).
    async fn zero_bitmaps(&self, only: Option<usize>) -> Result<(), RaidError> {
        let zeros = vec![0u8; self.bitmap.bytes as usize];
        for (i, d) in self.attached_devices() {
            if only.is_some_and(|o| o != i) {
                continue;
            }
            if let Err(e) = d.write(BITMAP_OFFSET, &zeros).await {
                if !self.fail_member(i, &format!("bitmap write: {e}")) {
                    return Err(RaidError::MemberIo { member_idx: i, error: e });
                }
            }
        }
        Ok(())
    }

    /// Write the bitmap pages that differ from the disk to every attached
    /// member, and flush them.
    async fn write_bitmap_pages(&self) -> DriveResult<()> {
        let _io = self.bitmap.io.lock().await;
        let w = self.bitmap.pending();
        if w.pages.is_empty() {
            return Ok(());
        }
        let devs = self.attached_devices();
        let results = join_all(devs.iter().map(|(i, d)| {
            let w = &w;
            async move {
                let r: DriveResult<()> = async {
                    for (p, bytes) in &w.pages {
                        d.write(BITMAP_OFFSET + p * bitmap::PAGE as u64, bytes).await?;
                    }
                    d.flush().await
                }
                .await;
                (*i, r)
            }
        }))
        .await;
        let mut any = false;
        for (i, r) in results {
            match r {
                Ok(_) => any = true,
                Err(e) => {
                    if !self.fail_member(i, &format!("bitmap write: {e}")) {
                        return Err(e);
                    }
                }
            }
        }
        if !any {
            return Err(RaidError::TooManyFailures { failed: self.failed_count(), max_tolerated: self.geometry().tolerated() }.into());
        }
        self.bitmap.commit(&w);
        Ok(())
    }

    /// Before a write: its bits on disk.
    async fn intent_begin(&self, r: std::ops::RangeInclusive<u64>) -> DriveResult<()> {
        if self.bitmap.begin(r.clone()) {
            while !self.bitmap.is_durable(r.clone()) {
                if let Err(e) = self.write_bitmap_pages().await {
                    self.bitmap.end(r);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    // --- RAID-1 / RAID-10 ---

    /// Write to every member of `set` that should hold `[moff, moff+len)`.
    /// Called with the range locked.
    async fn mirror_write_at(&self, set: &[usize], moff: u64, data: &[u8]) -> DriveResult<()> {
        let end = moff + data.len() as u64;
        // Each member gets what it holds: all of it, or — a member rebuilding
        // through this range — the part below its watermark. The rest is
        // locked by this write and the rebuild copies it after.
        let targets: Vec<(usize, Arc<dyn BlockDevice>, usize)> = {
            let members = self.members.read().unwrap();
            set.iter()
                .filter_map(|&i| {
                    let m = &members[i];
                    let d = m.device.clone()?;
                    if m.holds(end) {
                        Some((i, d, data.len()))
                    } else if m.state == RaidMemberState::Rebuilding && m.rebuilt_to > moff {
                        Some((i, d, (m.rebuilt_to - moff) as usize))
                    } else {
                        None
                    }
                })
                .collect()
        };
        if !targets.iter().any(|(_, _, n)| *n == data.len()) {
            return Err(self.too_many());
        }
        let results = join_all(targets.iter().map(|(i, d, n)| async move {
            (*i, *n == data.len(), d.write(DATA_OFFSET + moff, &data[..*n]).await)
        }))
        .await;
        let mut wrote = false;
        let mut last_err = None;
        for (i, whole, r) in results {
            match r {
                Ok(_) => wrote |= whole,
                Err(e) => {
                    // Not failing it would leave a leg that never got the data
                    // and still says it holds it.
                    if !self.fail_member(i, &format!("write error: {e}")) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        match (wrote, last_err) {
            (true, _) => Ok(()),
            (false, Some(e)) => Err(e),
            (false, None) => Err(self.too_many()),
        }
    }

    /// Read `[moff, moff+len)` from a member of `set` that holds it, failing
    /// and moving past any that cannot be read.
    async fn mirror_read_at(&self, set: &[usize], moff: u64, buf: &mut [u8]) -> DriveResult<()> {
        let mut tried = Vec::new();
        loop {
            let holders = self.holders(moff + buf.len() as u64);
            let cands: Vec<usize> = set.iter().copied().filter(|i| holders[*i].is_some() && !tried.contains(i)).collect();
            if cands.is_empty() {
                return Err(RaidError::TooManyFailures { failed: self.failed_count(), max_tolerated: self.geometry().tolerated() }.into());
            }
            let pick = cands[(self.read_counter.fetch_add(1, Ordering::Relaxed) as usize) % cands.len()];
            match holders[pick].as_ref().unwrap().read(DATA_OFFSET + moff, buf).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    tried.push(pick);
                    if !self.fail_member(pick, &format!("read error at {moff}: {e}")) && cands.len() == 1 {
                        return Err(e);
                    }
                }
            }
        }
    }

    fn all_slots(&self) -> Vec<usize> {
        (0..self.member_count()).collect()
    }

    async fn raid1_write(&self, offset: u64, buf: &[u8]) -> DriveResult<()> {
        let len = buf.len() as u64;
        let r = self.bitmap.chunks(offset, len);
        self.intent_begin(r.clone()).await?;
        let res = {
            let _g = self.locks.lock_many(offset / MIRROR_UNIT..(offset + len).div_ceil(MIRROR_UNIT)).await;
            self.mirror_write_at(&self.all_slots(), offset, buf).await
        };
        self.bitmap.end(r);
        self.persist_if_dirty().await;
        res
    }

    async fn raid1_read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<()> {
        let res = self.mirror_read_at(&self.all_slots(), offset, buf).await;
        self.persist_if_dirty().await;
        res
    }

    async fn raid10_write(&self, offset: u64, buf: &[u8]) -> DriveResult<()> {
        let geo = self.geometry();
        let segs = geo.segments(offset, buf.len() as u64);
        let first = segs.first().map(|s| s.stripe).unwrap_or(0);
        let last = segs.last().map(|s| s.stripe).unwrap_or(0);
        let r = self.bitmap.chunks(first * self.stripe_size, (last + 1 - first) * self.stripe_size);
        self.intent_begin(r.clone()).await?;
        let mut res = Ok(());
        for s in &segs {
            let _g = self.locks.lock(s.stripe).await;
            let moff = s.stripe * self.stripe_size + s.offset_in_unit;
            let data = &buf[s.buf_offset..s.buf_offset + s.len as usize];
            if let Err(e) = self.mirror_write_at(&geo.pair_members(s.index), moff, data).await {
                res = Err(e);
                break;
            }
        }
        self.bitmap.end(r);
        self.persist_if_dirty().await;
        res
    }

    async fn raid10_read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<()> {
        let geo = self.geometry();
        let mut res = Ok(());
        for s in geo.segments(offset, buf.len() as u64) {
            let moff = s.stripe * self.stripe_size + s.offset_in_unit;
            let out = &mut buf[s.buf_offset..s.buf_offset + s.len as usize];
            if let Err(e) = self.mirror_read_at(&geo.pair_members(s.index), moff, out).await {
                res = Err(e);
                break;
            }
        }
        self.persist_if_dirty().await;
        res
    }

    // --- RAID-5 / RAID-6 ---

    /// Read bytes `[lo, lo+len)` of every strip of a stripe from the members
    /// that hold it; a member whose read fails is failed and left out.
    async fn read_column(&self, geo: &Geometry, stripe: u64, lo: u64, len: usize) -> StripeStrips {
        let end = (stripe + 1) * self.stripe_size;
        let holders = self.holders(end);
        let at = DATA_OFFSET + stripe * self.stripe_size + lo;
        let reads = join_all(holders.iter().enumerate().map(|(i, d)| async move {
            match d {
                Some(d) => {
                    let mut b = vec![0u8; len];
                    match d.read(at, &mut b).await {
                        Ok(_) => (i, Ok(Some(b))),
                        Err(e) => (i, Err(e)),
                    }
                }
                None => (i, Ok(None)),
            }
        }))
        .await;
        let mut strips = StripeStrips {
            data: vec![None; geo.data_count()],
            p: None,
            q: if self.level == RaidLevel::Raid6 { Some(None) } else { None },
        };
        for (i, r) in reads {
            let b = match r {
                Ok(b) => b,
                Err(e) => {
                    self.fail_member(i, &format!("read error in stripe {stripe}: {e}"));
                    None
                }
            };
            match geo.role_of(stripe, i) {
                StripRole::Data(d) => strips.data[d] = b,
                StripRole::P => strips.p = b,
                StripRole::Q => strips.q = Some(b),
            }
        }
        strips
    }

    fn too_many(&self) -> DriveError {
        RaidError::TooManyFailures { failed: self.failed_count(), max_tolerated: self.geometry().tolerated() }.into()
    }

    async fn parity_read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<()> {
        let geo = self.geometry();
        let segs = geo.segments(offset, buf.len() as u64);
        // Every piece from its own member, at once.
        let mut pieces: Vec<&mut [u8]> = Vec::with_capacity(segs.len());
        let mut rest: &mut [u8] = buf;
        for s in &segs {
            let (a, b) = std::mem::take(&mut rest).split_at_mut(s.len as usize);
            pieces.push(a);
            rest = b;
        }
        let snapshot: Vec<Vec<Option<Arc<dyn BlockDevice>>>> = segs.iter().map(|s| self.holders((s.stripe + 1) * self.stripe_size)).collect();
        let results = join_all(segs.iter().zip(pieces.iter_mut()).zip(snapshot.iter()).map(|((s, out), holders)| {
            let m = geo.data_member(s.stripe, s.index);
            let at = DATA_OFFSET + s.stripe * self.stripe_size + s.offset_in_unit;
            async move {
                match &holders[m] {
                    Some(d) => match d.read(at, out).await {
                        Ok(_) => None,
                        Err(e) => Some((m, Some(e))),
                    },
                    None => Some((m, None)),
                }
            }
        }))
        .await;
        let mut res = Ok(());
        for ((s, out), r) in segs.iter().zip(pieces.into_iter()).zip(results) {
            let Some((m, err)) = r else { continue };
            if let Some(e) = err {
                self.fail_member(m, &format!("read error in stripe {}: {e}", s.stripe));
            }
            // Degraded: rebuild the piece from the rest of its stripe.
            let _g = self.locks.lock(s.stripe).await;
            let mut strips = self.read_column(&geo, s.stripe, s.offset_in_unit, s.len as usize).await;
            match strips.recover(s.len as usize) {
                Ok(()) => out.copy_from_slice(strips.data[s.index].as_ref().unwrap()),
                Err(_) => {
                    res = Err(self.too_many());
                    break;
                }
            }
        }
        self.persist_if_dirty().await;
        res
    }

    async fn parity_write(&self, offset: u64, buf: &[u8]) -> DriveResult<()> {
        let geo = self.geometry();
        let segs = geo.segments(offset, buf.len() as u64);
        if segs.is_empty() {
            return Ok(());
        }
        let first = segs[0].stripe;
        let last = segs[segs.len() - 1].stripe;
        let r = self.bitmap.chunks(first * self.stripe_size, (last + 1 - first) * self.stripe_size);
        self.intent_begin(r.clone()).await?;
        let mut res = Ok(());
        let mut i = 0;
        while i < segs.len() {
            let mut j = i;
            while j < segs.len() && segs[j].stripe == segs[i].stripe {
                j += 1;
            }
            if let Err(e) = self.parity_write_stripe(&geo, &segs[i..j], buf).await {
                res = Err(e);
                break;
            }
            i = j;
        }
        self.bitmap.end(r);
        self.persist_if_dirty().await;
        res
    }

    /// Write the pieces of one stripe, with its parity, under its lock.
    async fn parity_write_stripe(&self, geo: &Geometry, segs: &[layout::Segment], buf: &[u8]) -> DriveResult<()> {
        let stripe = segs[0].stripe;
        let _g = self.locks.lock(stripe).await;
        let unit = self.stripe_size;
        let base = DATA_OFFSET + stripe * unit;
        let dd = geo.data_count();
        let raid6 = self.level == RaidLevel::Raid6;
        let p_m = geo.p_member(stripe);
        let q_m = geo.q_member(stripe);

        for _attempt in 0..3 {
            let holders = self.holders((stripe + 1) * unit);
            let parity_held = holders[p_m].is_some() && (!raid6 || holders[q_m].is_some());
            let touched_held = segs.iter().all(|s| holders[geo.data_member(stripe, s.index)].is_some());
            let full = segs.len() == dd && segs.iter().all(|s| s.offset_in_unit == 0 && s.len == unit);

            // (member, offset on the member, bytes)
            let mut writes: Vec<(usize, u64, Vec<u8>)> = Vec::new();
            if full {
                let data: Vec<&[u8]> = segs.iter().map(|s| &buf[s.buf_offset..s.buf_offset + unit as usize]).collect();
                let mut p = vec![0u8; unit as usize];
                let mut q = vec![0u8; unit as usize];
                if raid6 {
                    self.parity_engine.compute_raid6_parity(&data, &mut p, &mut q);
                } else {
                    self.parity_engine.compute_xor_parity(&data, &mut p);
                }
                for s in segs {
                    writes.push((geo.data_member(stripe, s.index), base, data[s.index].to_vec()));
                }
                writes.push((p_m, base, p));
                if raid6 {
                    writes.push((q_m, base, q));
                }
            } else if parity_held && touched_held && segs.len() * 2 <= dd {
                // Read-modify-write: old data, old P (and Q), and the change.
                let mut ok = true;
                let mut pending: Vec<(usize, u64, Vec<u8>)> = Vec::new();
                for s in segs {
                    let m = geo.data_member(stripe, s.index);
                    let at = base + s.offset_in_unit;
                    let len = s.len as usize;
                    let mut old = vec![0u8; len];
                    let mut p = vec![0u8; len];
                    let mut q = vec![0u8; len];
                    let reads = join_all([
                        holders[m].as_ref().unwrap().read(at, &mut old),
                        holders[p_m].as_ref().unwrap().read(at, &mut p),
                    ])
                    .await;
                    let mut failed = None;
                    for (k, r) in reads.into_iter().enumerate() {
                        if let Err(e) = r {
                            failed = Some(([m, p_m][k], e));
                        }
                    }
                    if failed.is_none() && raid6 {
                        if let Err(e) = holders[q_m].as_ref().unwrap().read(at, &mut q).await {
                            failed = Some((q_m, e));
                        }
                    }
                    if let Some((who, e)) = failed {
                        if !self.fail_member(who, &format!("read error in stripe {stripe}: {e}")) {
                            return Err(e);
                        }
                        ok = false;
                        break;
                    }
                    let new = &buf[s.buf_offset..s.buf_offset + len];
                    let mut delta = old;
                    self.parity_engine.xor_in_place(&mut delta, new);
                    self.parity_engine.xor_in_place(&mut p, &delta);
                    if raid6 {
                        self.parity_engine.q_update(&mut q, &delta, s.index);
                    }
                    pending.push((m, at, new.to_vec()));
                    pending.push((p_m, at, p));
                    if raid6 {
                        pending.push((q_m, at, q));
                    }
                    // Later pieces of this stripe read the parity this one
                    // wrote, so write it now.
                    let results = self.issue_writes(&holders, std::mem::take(&mut pending)).await;
                    if let Err(e) = results {
                        return Err(e);
                    }
                }
                if ok {
                    return self.check_after_write();
                }
                continue;
            } else {
                // Reconstruct-write: the whole column the pieces span, every
                // missing strip recovered first, the new data laid over it,
                // parity recomputed from the result.
                let lo = segs.iter().map(|s| s.offset_in_unit).min().unwrap();
                let hi = segs.iter().map(|s| s.offset_in_unit + s.len).max().unwrap();
                let len = (hi - lo) as usize;
                let mut strips = self.read_column(geo, stripe, lo, len).await;
                if strips.recover(len).is_err() {
                    return Err(self.too_many());
                }
                for s in segs {
                    let d = strips.data[s.index].as_mut().unwrap();
                    let from = (s.offset_in_unit - lo) as usize;
                    d[from..from + s.len as usize].copy_from_slice(&buf[s.buf_offset..s.buf_offset + s.len as usize]);
                }
                let data: Vec<&[u8]> = strips.data.iter().map(|d| d.as_deref().unwrap()).collect();
                let mut p = vec![0u8; len];
                let mut q = vec![0u8; len];
                if raid6 {
                    self.parity_engine.compute_raid6_parity(&data, &mut p, &mut q);
                } else {
                    self.parity_engine.compute_xor_parity(&data, &mut p);
                }
                let at = base + lo;
                for s in segs {
                    writes.push((geo.data_member(stripe, s.index), at, strips.data[s.index].clone().unwrap()));
                }
                writes.push((p_m, at, p));
                if raid6 {
                    writes.push((q_m, at, q));
                }
            }
            self.issue_writes(&holders, writes).await?;
            return self.check_after_write();
        }
        Err(self.too_many())
    }

    /// Write to the members that hold their range (the rest are skipped:
    /// missing, or rebuilding and not this far yet). A member that fails is
    /// failed; refused failures are returned.
    async fn issue_writes(&self, holders: &[Option<Arc<dyn BlockDevice>>], writes: Vec<(usize, u64, Vec<u8>)>) -> DriveResult<()> {
        let results = join_all(writes.iter().filter_map(|(m, at, bytes)| {
            holders[*m].as_ref().map(|d| async move { (*m, d.write(*at, bytes).await) })
        }))
        .await;
        for (m, r) in results {
            if let Err(e) = r {
                if !self.fail_member(m, &format!("write error: {e}")) {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn check_after_write(&self) -> DriveResult<()> {
        if self.data_intact() { Ok(()) } else { Err(self.too_many()) }
    }

    // --- resync, scrub, rebuild ---

    /// Make the members agree over member data `[start, end)`: parity
    /// recomputed from the data, mirrors copied from the first member that
    /// holds the range. With `repair` false, only count. Returns the units
    /// that disagreed. Takes the locks itself.
    async fn resync_range(&self, start: u64, end: u64, repair: bool, progress: Option<&ScrubProgress>) -> Result<u64, RaidError> {
        let lu = self.lock_unit();
        let mut found = 0u64;
        let mut key = start / lu;
        let last = end.div_ceil(lu);
        while key < last {
            if progress.is_some_and(|p| p.is_cancelled()) || self.stopped.load(Ordering::SeqCst) {
                break;
            }
            let _g = self.locks.lock(key).await;
            let moff = key * lu;
            let len = lu.min(self.data_size - moff) as usize;
            if self.resync_unit(key, moff, len, repair).await? {
                found += 1;
                if let Some(p) = progress {
                    p.errors_found.fetch_add(1, Ordering::Relaxed);
                    if repair {
                        p.errors_repaired.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if let Some(p) = progress {
                p.advance_bytes(len as u64);
            }
            key += 1;
        }
        self.persist_if_dirty().await;
        Ok(found)
    }

    /// One lock unit; `true` when the members disagreed.
    async fn resync_unit(&self, key: u64, moff: u64, len: usize, repair: bool) -> Result<bool, RaidError> {
        let geo = self.geometry();
        match self.level {
            RaidLevel::Raid5 | RaidLevel::Raid6 => {
                let stripe = key;
                let strips = self.read_column(&geo, stripe, 0, len).await;
                if strips.data.iter().any(|d| d.is_none()) {
                    // A data strip is missing: what parity should say cannot
                    // be told from what it does say.
                    return Ok(false);
                }
                let data: Vec<&[u8]> = strips.data.iter().map(|d| d.as_deref().unwrap()).collect();
                let mut p = vec![0u8; len];
                let mut q = vec![0u8; len];
                if self.level == RaidLevel::Raid6 {
                    self.parity_engine.compute_raid6_parity(&data, &mut p, &mut q);
                } else {
                    self.parity_engine.compute_xor_parity(&data, &mut p);
                }
                let at = DATA_OFFSET + moff;
                let mut writes = Vec::new();
                if strips.p.as_ref().is_some_and(|sp| *sp != p) {
                    writes.push((geo.p_member(stripe), at, p));
                }
                if let Some(Some(sq)) = &strips.q {
                    if *sq != q {
                        writes.push((geo.q_member(stripe), at, q));
                    }
                }
                let differ = !writes.is_empty();
                if differ && repair {
                    let holders = self.holders(moff + len as u64);
                    self.issue_writes(&holders, writes).await?;
                }
                Ok(differ)
            }
            RaidLevel::Raid1 | RaidLevel::Raid10 => {
                let sets: Vec<Vec<usize>> = if self.level == RaidLevel::Raid1 {
                    vec![self.all_slots()]
                } else {
                    (0..geo.data_count()).map(|p| geo.pair_members(p).to_vec()).collect()
                };
                let holders = self.holders(moff + len as u64);
                let mut differ = false;
                for set in sets {
                    let have: Vec<(usize, Arc<dyn BlockDevice>)> =
                        set.iter().filter_map(|&i| holders[i].clone().map(|d| (i, d))).collect();
                    if have.len() < 2 {
                        continue;
                    }
                    let mut first = vec![0u8; len];
                    if let Err(e) = have[0].1.read(DATA_OFFSET + moff, &mut first).await {
                        self.fail_member(have[0].0, &format!("read error: {e}"));
                        continue;
                    }
                    for (i, d) in &have[1..] {
                        let mut b = vec![0u8; len];
                        if let Err(e) = d.read(DATA_OFFSET + moff, &mut b).await {
                            self.fail_member(*i, &format!("read error: {e}"));
                            continue;
                        }
                        if b != first {
                            differ = true;
                            if repair {
                                if let Err(e) = d.write(DATA_OFFSET + moff, &first).await {
                                    self.fail_member(*i, &format!("write error: {e}"));
                                }
                            }
                        }
                    }
                }
                Ok(differ)
            }
        }
    }

    /// Verify (and with `repair`, fix) every stripe or mirror unit, in the
    /// background. Returns the running scrub's progress if one is running.
    pub fn start_scrub(self: &Arc<Self>, config: ScrubConfig) -> Arc<ScrubProgress> {
        let mut slot = self.scrub_progress.lock().unwrap();
        if let Some(p) = slot.as_ref() {
            if !p.is_finished() {
                return p.clone();
            }
        }
        let progress = ScrubProgress::new(self.data_size, config.repair);
        *slot = Some(progress.clone());
        drop(slot);
        let array = Arc::clone(self);
        let p = progress.clone();
        tokio::spawn(async move {
            let lu = array.lock_unit();
            let batch = (REBUILD_BATCH / lu).max(1) * lu;
            let mut pos = 0;
            let mut err = None;
            let started = Instant::now();
            while pos < array.data_size {
                if p.is_cancelled() || array.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let end = (pos + batch).min(array.data_size);
                if let Err(e) = array.resync_range(pos, end, config.repair, Some(&p)).await {
                    err = Some(e.to_string());
                    break;
                }
                pos = end;
                throttle(started, pos, config.max_bytes_per_sec).await;
            }
            p.finish(err);
            tracing::info!(
                "{} {} scrub finished: {} mismatch(es), {} repaired",
                array.level, array.id, p.found(), p.repaired()
            );
        });
        progress
    }

    pub fn scrub_progress(&self) -> Option<Arc<ScrubProgress>> {
        self.scrub_progress.lock().unwrap().clone()
    }

    pub fn rebuild_progress(&self) -> Option<Arc<RebuildProgress>> {
        self.rebuild_progress.lock().unwrap().clone()
    }

    /// Put a drive into a failed slot and rebuild onto it in the background.
    pub async fn replace(self: &Arc<Self>, slot: usize, device: Arc<dyn BlockDevice>) -> Result<Uuid, RaidError> {
        if device.capacity_bytes() < self.member_bytes_needed() {
            return Err(RaidError::InvalidStripe(format!(
                "{} holds {} bytes; a slot of this array needs {}",
                device.id().path,
                device.capacity_bytes(),
                self.member_bytes_needed()
            )));
        }
        let uuid = Uuid::new_v4();
        {
            let mut members = self.members.write().unwrap();
            let Some(m) = members.get_mut(slot) else {
                return Err(RaidError::InvalidStripe(format!("no slot {slot}")));
            };
            if m.state != RaidMemberState::Failed {
                return Err(RaidError::InvalidStripe(format!("slot {slot} is {}, not failed", m.state)));
            }
            *m = Member { device: Some(device), state: RaidMemberState::Rebuilding, uuid, rebuilt_to: 0 };
        }
        self.events.fetch_add(1, Ordering::SeqCst);
        self.zero_bitmaps(Some(slot)).await?;
        self.write_superblocks().await?;
        tracing::info!("{} {} ({}): slot {slot} rebuilding onto a new drive (member {uuid})", self.level, self.id, self.name());
        self.spawn_rebuild();
        Ok(uuid)
    }

    /// Add a member to a RAID-1 (it rebuilds in the background). Returns
    /// its member uuid.
    pub async fn add_member(self: &Arc<Self>, device: Arc<dyn BlockDevice>) -> Result<Uuid, RaidError> {
        if self.level != RaidLevel::Raid1 {
            return Err(RaidError::NotSupported(
                "add_member only for RAID 1 — a parity set's width is fixed; replace a failed slot instead".into(),
            ));
        }
        if device.capacity_bytes() < self.member_bytes_needed() {
            return Err(RaidError::Drive(DriveError::OutOfRange {
                offset: 0,
                len: self.member_bytes_needed(),
                capacity: device.capacity_bytes(),
            }));
        }
        let uuid = Uuid::new_v4();
        let slot = {
            let mut members = self.members.write().unwrap();
            if members.len() >= MAX_SLOTS {
                return Err(RaidError::InvalidStripe(format!("at most {MAX_SLOTS} members")));
            }
            members.push(Member { device: Some(device), state: RaidMemberState::Rebuilding, uuid, rebuilt_to: 0 });
            members.len() - 1
        };
        self.events.fetch_add(1, Ordering::SeqCst);
        self.zero_bitmaps(Some(slot)).await?;
        self.write_superblocks().await?;
        self.spawn_rebuild();
        Ok(uuid)
    }

    /// Remove a RAID-1 member by its uuid; refused for the last active one.
    pub async fn remove_member(&self, member_uuid: Uuid) -> Result<(), RaidError> {
        if self.level != RaidLevel::Raid1 {
            return Err(RaidError::NotSupported("remove_member only supported for RAID 1".into()));
        }
        let removed = {
            let mut members = self.members.write().unwrap();
            let idx = members
                .iter()
                .position(|m| m.uuid == member_uuid)
                .ok_or_else(|| RaidError::CannotRemoveMember(format!("member {member_uuid} not found")))?;
            let active_remaining = members
                .iter()
                .enumerate()
                .filter(|(i, m)| *i != idx && m.state == RaidMemberState::Active && m.device.is_some())
                .count();
            if active_remaining == 0 {
                return Err(RaidError::CannotRemoveMember("cannot remove last active member".into()));
            }
            members.remove(idx)
        };
        self.events.fetch_add(1, Ordering::SeqCst);
        self.write_superblocks().await?;
        // Its superblock would otherwise still claim a slot.
        if let Some(d) = removed.device {
            let _ = d.write(0, &vec![0u8; superblock::SUPERBLOCK_BYTES]).await;
            let _ = d.flush().await;
        }
        Ok(())
    }

    /// Start the rebuild task if a slot is rebuilding and none is running.
    pub fn spawn_rebuild(self: &Arc<Self>) {
        if self.rebuilding_count() == 0 || self.rebuild_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let array = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = array.run_rebuild().await {
                tracing::error!("{} {} rebuild stopped: {e}", array.level, array.id);
            }
            array.rebuild_running.store(false, Ordering::SeqCst);
            // A slot replaced while this ran.
            if array.rebuilding_count() > 0 && !array.stopped.load(Ordering::SeqCst) && array.data_intact() {
                array.spawn_rebuild();
            }
        });
    }

    /// Rebuild every rebuilding slot, from the lowest watermark up.
    async fn run_rebuild(self: &Arc<Self>) -> Result<(), RaidError> {
        let targets: Vec<usize> = self
            .members
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, m)| m.state == RaidMemberState::Rebuilding && m.device.is_some())
            .map(|(i, _)| i)
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let lu = self.lock_unit();
        let start = {
            let m = self.members.read().unwrap();
            targets.iter().map(|t| m[*t].rebuilt_to).min().unwrap() / lu * lu
        };
        let progress = RebuildProgress::new(targets.clone(), self.data_size, start);
        *self.rebuild_progress.lock().unwrap() = Some(progress.clone());
        tracing::info!(
            "{} {} ({}): rebuilding slot(s) {targets:?} from {start} of {} bytes",
            self.level, self.id, self.name(), self.data_size
        );
        let batch_bytes = match self.rebuild_config.lock().unwrap().batch_bytes {
            0 => REBUILD_BATCH,
            b => b,
        };
        let batch = (batch_bytes / lu).max(1);
        let mut key = start / lu;
        let last = self.data_size.div_ceil(lu);
        let started = Instant::now();
        let mut checkpoint = Instant::now();
        while key < last {
            if self.stopped.load(Ordering::SeqCst) || progress.is_cancelled() {
                progress.finish(Some("cancelled".into()));
                self.persist_rebuild_position().await;
                return Ok(());
            }
            let end_key = (key + batch).min(last);
            {
                let _g = self.locks.lock_many(key..end_key).await;
                for k in key..end_key {
                    if let Err(e) = self.rebuild_unit(k, &targets).await {
                        progress.finish(Some(e.to_string()));
                        self.persist_if_dirty().await;
                        return Err(e);
                    }
                }
                let end = (end_key * lu).min(self.data_size);
                let mut members = self.members.write().unwrap();
                for t in &targets {
                    let m = &mut members[*t];
                    if m.state == RaidMemberState::Rebuilding && m.rebuilt_to < end {
                        m.rebuilt_to = end;
                    }
                }
            }
            let done = (end_key * lu).min(self.data_size);
            progress.set_done(done);
            self.publish_metrics();
            self.persist_if_dirty().await;
            if checkpoint.elapsed() >= CHECKPOINT_EVERY {
                self.persist_rebuild_position().await;
                checkpoint = Instant::now();
            }
            let rate = self.rebuild_config.lock().unwrap().max_bytes_per_sec;
            throttle(started, done - start, rate).await;
            key = end_key;
        }
        // Flushed before it is called whole.
        for t in &targets {
            let d = self.members.read().unwrap()[*t].device.clone();
            if let Some(d) = d {
                if let Err(e) = d.flush().await {
                    self.fail_member(*t, &format!("flush after rebuild: {e}"));
                }
            }
        }
        let mut finished = Vec::new();
        {
            let mut members = self.members.write().unwrap();
            for t in &targets {
                if members[*t].state == RaidMemberState::Rebuilding {
                    members[*t].state = RaidMemberState::Active;
                    members[*t].rebuilt_to = 0;
                    finished.push(*t);
                }
            }
        }
        self.events.fetch_add(1, Ordering::SeqCst);
        self.write_superblocks().await?;
        progress.finish(None);
        self.publish_metrics();
        tracing::info!(
            "{} {} ({}): rebuild of slot(s) {finished:?} complete in {:.0?}",
            self.level, self.id, self.name(), started.elapsed()
        );
        Ok(())
    }

    async fn persist_rebuild_position(&self) {
        self.sb_dirty.store(true, Ordering::SeqCst);
        self.persist_if_dirty().await;
    }

    /// Rebuild one lock unit of each target slot. Called with it locked.
    async fn rebuild_unit(&self, key: u64, targets: &[usize]) -> Result<(), RaidError> {
        let lu = self.lock_unit();
        let moff = key * lu;
        let len = lu.min(self.data_size - moff) as usize;
        let live_targets: Vec<(usize, Arc<dyn BlockDevice>)> = {
            let m = self.members.read().unwrap();
            targets
                .iter()
                .filter(|t| m[**t].state == RaidMemberState::Rebuilding && m[**t].rebuilt_to <= moff)
                .filter_map(|t| m[*t].device.clone().map(|d| (*t, d)))
                .collect()
        };
        if live_targets.is_empty() {
            return Ok(());
        }
        let geo = self.geometry();
        let mut writes: Vec<(usize, Arc<dyn BlockDevice>, Vec<u8>)> = Vec::new();
        match self.level {
            RaidLevel::Raid5 | RaidLevel::Raid6 => {
                let mut strips = self.read_column(&geo, key, 0, len).await;
                strips.recover(len).map_err(|_| RaidError::TooManyFailures {
                    failed: self.failed_count(),
                    max_tolerated: geo.tolerated(),
                })?;
                for (t, d) in live_targets {
                    let bytes = match geo.role_of(key, t) {
                        StripRole::Data(i) => strips.data[i].clone().unwrap(),
                        StripRole::P => strips.p.clone().unwrap(),
                        StripRole::Q => strips.q.clone().unwrap().unwrap(),
                    };
                    writes.push((t, d, bytes));
                }
            }
            RaidLevel::Raid1 => {
                let mut b = vec![0u8; len];
                let sources: Vec<usize> = self.all_slots().into_iter().filter(|i| !targets.contains(i)).collect();
                self.mirror_read_at(&sources, moff, &mut b).await.map_err(RaidError::Drive)?;
                for (t, d) in live_targets {
                    writes.push((t, d, b.clone()));
                }
            }
            RaidLevel::Raid10 => {
                for (t, d) in live_targets {
                    let mut b = vec![0u8; len];
                    self.mirror_read_at(&[t ^ 1], moff, &mut b).await.map_err(RaidError::Drive)?;
                    writes.push((t, d, b));
                }
                let _ = geo;
            }
        }
        for (t, d, bytes) in writes {
            if let Err(e) = d.write(DATA_OFFSET + moff, &bytes).await {
                self.fail_member(t, &format!("rebuild write at {moff}: {e}"));
            }
        }
        Ok(())
    }

    // --- spares and the supervisor ---

    /// Give the array a spare pool and start its supervisor: it resumes a
    /// rebuild that was under way, and puts a spare into every failed slot
    /// (the array's own pool first, then the global one), now and whenever a
    /// member fails or a spare arrives. Runs until `stop`.
    pub fn start(self: &Arc<Self>, spares: Option<Arc<SparePool>>) {
        *self.spares.write().unwrap() = spares.clone();
        let weak = Arc::downgrade(self);
        let failures = self.failures.clone();
        let arrived = spares.as_ref().map(|s| s.arrived.clone());
        tokio::spawn(async move {
            loop {
                {
                    let Some(a) = weak.upgrade() else { break };
                    if a.stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    a.persist_if_dirty().await;
                    a.take_spares().await;
                    a.spawn_rebuild();
                    a.publish_metrics();
                }
                let wait = async {
                    match &arrived {
                        Some(n) => tokio::select! {
                            _ = failures.notified() => {}
                            _ = n.notified() => {}
                        },
                        None => failures.notified().await,
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(30), wait).await;
            }
        });
    }

    /// The set's state as gauges: `stormblock_raid_state` (0 clean,
    /// 1 rebuilding, 2 degraded, 3 failed), failed members, rebuild percent.
    pub fn publish_metrics(&self) {
        let st = self.status();
        let code = match st.state {
            "clean" => 0.0,
            "rebuilding" => 1.0,
            "degraded" => 2.0,
            _ => 3.0,
        };
        let (id, name) = (self.id.to_string(), self.name());
        metrics::gauge!("stormblock_raid_state", "array" => id.clone(), "name" => name.clone()).set(code);
        metrics::gauge!("stormblock_raid_failed_members", "array" => id.clone(), "name" => name.clone())
            .set(st.failed as f64);
        let pct = st.rebuild.filter(|r| r.running).map(|r| r.percent).unwrap_or(0.0);
        metrics::gauge!("stormblock_raid_rebuild_percent", "array" => id, "name" => name).set(pct);
    }

    /// Put spares into failed slots, as many as there are.
    pub async fn take_spares(self: &Arc<Self>) {
        let Some(pool) = self.spares.read().unwrap().clone() else { return };
        if !self.data_intact() {
            return;
        }
        loop {
            let slot = self
                .members
                .read()
                .unwrap()
                .iter()
                .position(|m| m.state == RaidMemberState::Failed);
            let Some(slot) = slot else { return };
            let Some(spare) = pool.take(&self.pool(), self.member_bytes_needed()) else {
                tracing::warn!(
                    "{} {} ({}): slot {slot} failed and no spare fits (pool '{}' or global, ≥ {} bytes)",
                    self.level, self.id, self.name(), self.pool(), self.member_bytes_needed()
                );
                return;
            };
            let path = spare.device.id().path.clone();
            match self.replace(slot, spare.device.clone()).await {
                Ok(_) => tracing::warn!(
                    "{} {} ({}): slot {slot} replaced by spare {} ({path}) from pool '{}'",
                    self.level, self.id, self.name(), spare.uuid, spare.pool
                ),
                Err(e) => {
                    tracing::error!("{} {}: spare {path} could not take slot {slot}: {e}", self.level, self.id);
                    return;
                }
            }
        }
    }

    /// Stop the supervisor, any rebuild and any scrub.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.failures.notify_one();
    }

    /// Clear every idle bit and rewrite the superblocks — a clean stop.
    pub async fn close(&self) -> DriveResult<()> {
        self.flush_members().await?;
        self.bitmap.clear_idle(Instant::now() + Duration::from_secs(1));
        self.write_bitmap_pages().await?;
        self.sb_dirty.store(true, Ordering::SeqCst);
        self.persist_if_dirty().await;
        Ok(())
    }

    /// Wipe the superblocks of every member, so its drives can be used again.
    pub async fn wipe(&self) {
        self.stop();
        let zeros = vec![0u8; superblock::SUPERBLOCK_BYTES];
        let devs: Vec<Arc<dyn BlockDevice>> =
            self.members.read().unwrap().iter().filter_map(|m| m.device.clone()).collect();
        for d in devs {
            let _ = d.write(0, &zeros).await;
            let _ = d.flush().await;
        }
    }

    async fn flush_members(&self) -> DriveResult<()> {
        let devs = self.attached_devices();
        let results = join_all(devs.iter().map(|(i, d)| async move { (*i, d.flush().await) })).await;
        for (i, r) in results {
            if let Err(e) = r {
                if !self.fail_member(i, &format!("flush error: {e}")) {
                    return Err(e);
                }
            }
        }
        if !self.data_intact() {
            return Err(self.too_many());
        }
        Ok(())
    }

    /// Chunks marked in the bitmap right now (for tests and status).
    pub fn dirty_chunks(&self) -> usize {
        self.bitmap.dirty_count()
    }
}

/// What a set of drives carries: arrays (each a group of members' superblocks),
/// spares, and drives whose superblock is damaged.
#[derive(Default)]
pub struct Scan {
    pub arrays: Vec<Vec<(Arc<dyn BlockDevice>, Superblock)>>,
    pub spares: Vec<(Arc<dyn BlockDevice>, Superblock)>,
    /// (drive path, what is wrong)
    pub damaged: Vec<(String, String)>,
}

impl Scan {
    /// Whether a drive is a member of one of the arrays found or a spare.
    pub fn claims(&self, dev: &Arc<dyn BlockDevice>) -> bool {
        let same = |d: &Arc<dyn BlockDevice>| d.id().uuid == dev.id().uuid && d.id().path == dev.id().path;
        self.arrays.iter().flatten().any(|(d, _)| same(d)) || self.spares.iter().any(|(d, _)| same(d))
    }
}

/// The superblock on a drive, if it carries one of this format.
pub async fn read_superblock(dev: &Arc<dyn BlockDevice>) -> Result<Option<Superblock>, RaidError> {
    if dev.capacity_bytes() < DATA_OFFSET {
        return Ok(None);
    }
    let mut b = vec![0u8; superblock::SUPERBLOCK_BYTES];
    dev.read(0, &mut b).await?;
    Superblock::from_bytes(&b)
}

/// Read every drive's superblock and group what they say.
pub async fn scan(devs: &[Arc<dyn BlockDevice>]) -> Scan {
    let mut out = Scan::default();
    let mut by_array: Vec<(Uuid, Vec<(Arc<dyn BlockDevice>, Superblock)>)> = Vec::new();
    for d in devs {
        match read_superblock(d).await {
            Ok(None) => {}
            Ok(Some(sb)) if sb.is_spare() => out.spares.push((d.clone(), sb)),
            Ok(Some(sb)) => match by_array.iter_mut().find(|(u, _)| *u == sb.array_uuid) {
                Some((_, v)) => v.push((d.clone(), sb)),
                None => by_array.push((sb.array_uuid, vec![(d.clone(), sb)])),
            },
            Err(e) => out.damaged.push((d.id().path.clone(), e.to_string())),
        }
    }
    out.arrays = by_array.into_iter().map(|(_, v)| v).collect();
    out
}

/// Sleep enough to keep `done` bytes since `started` under `rate` a second.
async fn throttle(started: Instant, done: u64, rate: u64) {
    if rate == 0 {
        return;
    }
    let want = Duration::from_secs_f64(done as f64 / rate as f64);
    let el = started.elapsed();
    if want > el {
        tokio::time::sleep(want - el).await;
    }
}

// --- BlockDevice implementation for RaidArray ---

#[async_trait]
impl BlockDevice for RaidArray {
    fn id(&self) -> &DeviceId {
        &self.device_id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    fn block_size(&self) -> u32 {
        // The largest of the members': a 4Kn drive among 512 ones makes the
        // whole array 4Kn.
        self.members
            .read()
            .unwrap()
            .iter()
            .filter_map(|m| m.device.as_ref().map(|d| d.block_size()))
            .max()
            .unwrap_or(4096)
    }

    fn optimal_io_size(&self) -> u32 {
        match self.level {
            RaidLevel::Raid1 => self
                .members
                .read()
                .unwrap()
                .iter()
                .find_map(|m| m.device.as_ref().map(|d| d.optimal_io_size()))
                .unwrap_or(4096),
            _ => self.geometry().stripe_bytes().min(u32::MAX as u64) as u32,
        }
    }

    fn device_type(&self) -> DriveType {
        self.members
            .read()
            .unwrap()
            .iter()
            .find_map(|m| m.device.as_ref().map(|d| d.device_type()))
            .unwrap_or(DriveType::File)
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let len = buf.len() as u64;
        let cap = self.capacity_bytes();
        if offset + len > cap {
            return Err(DriveError::OutOfRange { offset, len, capacity: cap });
        }
        match self.level {
            RaidLevel::Raid1 => self.raid1_read(offset, buf).await?,
            RaidLevel::Raid10 => self.raid10_read(offset, buf).await?,
            RaidLevel::Raid5 | RaidLevel::Raid6 => self.parity_read(offset, buf).await?,
        }
        Ok(buf.len())
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let len = buf.len() as u64;
        let cap = self.capacity_bytes();
        if offset + len > cap {
            return Err(DriveError::OutOfRange { offset, len, capacity: cap });
        }
        match self.level {
            RaidLevel::Raid1 => self.raid1_write(offset, buf).await?,
            RaidLevel::Raid10 => self.raid10_write(offset, buf).await?,
            RaidLevel::Raid5 | RaidLevel::Raid6 => self.parity_write(offset, buf).await?,
        }
        Ok(buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        let started = Instant::now();
        let res = self.flush_members().await;
        // What was idle before this flush began is durable now: its bits can go.
        if let (Ok(()), Some(before)) = (&res, started.checked_sub(CLEAR_DELAY)) {
            if self.bitmap.clear_idle(before) > 0 {
                let _ = self.write_bitmap_pages().await;
            }
        }
        self.persist_if_dirty().await;
        res
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        // Only a mirror passes a discard down: on a parity set a discarded
        // strip would no longer agree with its parity. Advisory either way —
        // nothing relies on a discard reading back as zeros.
        if self.level != RaidLevel::Raid1 || self.rebuilding_count() > 0 {
            return Ok(());
        }
        let _g = self.locks.lock_many(offset / MIRROR_UNIT..(offset + len).div_ceil(MIRROR_UNIT)).await;
        for (_, d) in self.attached_devices() {
            let _ = d.discard(DATA_OFFSET + offset, len).await;
        }
        Ok(())
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        let members = self.members.read().unwrap();
        let healthy = members
            .iter()
            .filter(|m| m.state == RaidMemberState::Active)
            .all(|m| m.device.as_ref().map(|d| d.smart_status().map(|s| s.healthy).unwrap_or(false)).unwrap_or(false));
        Ok(SmartData { healthy, ..Default::default() })
    }

    fn media_errors(&self) -> u64 {
        self.members.read().unwrap().iter().filter_map(|m| m.device.as_ref().map(|d| d.media_errors())).sum()
    }
}
