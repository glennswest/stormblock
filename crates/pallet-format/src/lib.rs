//! The pallet on-disk format v1 — **read only**.
//!
//! There is no write path in this crate and there must never be one. Firmware
//! links it: it runs before the kernel, before Secure Boot hands off, and
//! before anything can be debugged with a shell, so it has to be small enough
//! to read in one sitting. Parse-and-verify is; parse-verify-and-emit is not.
//!
//! # Why this crate exists
//!
//! One on-disk format with two consumers — the engine that writes pallets and
//! the firmware that boots them — is exactly the shape that produces two
//! hand-maintained readers which must stay bit-compatible forever, and whose
//! drift fails as *the node does not boot*. So the decode side lives here,
//! once, and both sides link it:
//!
//! - **stormblock** wraps it in the async I/O layer that publishes, verifies,
//!   activates and moves pallets. Its writer lays bytes down at the offsets
//!   [`layout`] defines, so emission has one implementation and the offsets
//!   have one definition.
//! - **stormuefi** compiles it for `x86_64-unknown-uefi` and reads pallets
//!   with no allocator, no runtime and no `async`.
//!
//! # Shape
//!
//! `no_std`, no allocation on the read path, no I/O of its own. A [`Pallet`]
//! borrows the bytes of a superblock and both tables; reading member *content*
//! goes through the caller's [`BlockReader`], because only the caller knows how
//! to reach the medium.
//!
//! ```text
//! +0            superblock (4096 B)
//! +4096         member table  (member_count × 128 B)
//!               extent table  (extent_count × 32 B)
//!               ... padding ...
//! +data         member content
//! ```
//!
//! **Every offset is relative to the partition start**, which is what makes a
//! pallet byte-for-byte copyable to another disk, image or ISO. The
//! specification is `docs/pallets.md` in the stormblock repository.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

pub const MAGIC: [u8; 8] = *b"STORMPAL";
pub const VERSION: u32 = 1;
pub const SUPERBLOCK_LEN: usize = 4096;
pub const MEMBER_LEN: usize = 128;
pub const EXTENT_LEN: usize = 32;
pub const NAME_LEN: usize = 40;
pub const ROLE_LEN: usize = 16;
pub const VERSION_LABEL_LEN: usize = 32;

/// GPT partition type GUID identifying a stormcos pallet:
/// `A324B90E-CED9-4019-B338-7A5B98E1B7D2`, in the mixed-endian form GPT
/// stores it on disk.
pub const PALLET_TYPE_GUID: [u8; 16] = [
    0x0E, 0xB9, 0x24, 0xA3, // Data1, little-endian
    0xD9, 0xCE, // Data2, little-endian
    0x19, 0x40, // Data3, little-endian
    0xB3, 0x38, // Data4, big-endian from here
    0x7A, 0x5B, 0x98, 0xE1, 0xB7, 0xD2,
];

/// Extents are immutable: never relocate, reuse or GC them while referenced.
pub const FLAG_SEALED: u32 = 1 << 0;
/// Never attach this member writably. Kernel and initramfs carry it.
pub const FLAG_READ_ONLY: u32 = 1 << 1;
/// `digest` is meaningful and content must be checked against it before use.
pub const FLAG_DIGEST: u32 = 1 << 2;

/// Where every field sits.
///
/// Defined once, here, and used by the reader below *and* by stormblock's
/// writer — which is the point of the split. A magic number that appears in
/// two places is a format that can drift; one that appears here cannot.
pub mod layout {
    /// Superblock field offsets.
    pub mod sb {
        pub const MAGIC: usize = 0;
        pub const VERSION: usize = 8;
        pub const SUPERBLOCK_LEN: usize = 12;
        pub const BLOCK_SIZE: usize = 16;
        pub const MEMBER_SIZE: usize = 20;
        pub const MEMBER_COUNT: usize = 24;
        pub const EXTENT_SIZE: usize = 28;
        pub const EXTENT_COUNT: usize = 32;
        pub const PALLET_VERSION: usize = 36;
        pub const MEMBER_DATA_OFFSET: usize = 44;
        pub const NAME: usize = 52;
        pub const MANIFEST_DIGEST: usize = 92;
        pub const MEMBERS_CRC: usize = 124;
        pub const EXTENTS_CRC: usize = 128;
        pub const SUPERBLOCK_CRC: usize = 132;
        pub const FLAGS: usize = 136;
        /// Extension field. Zero means "unspecified", so a pallet written
        /// before it existed reads back correctly and a reader that ignores it
        /// is still right.
        pub const KIND: usize = 144;
        /// Extension field, same rule.
        pub const VERSION_LABEL: usize = 148;
        /// First byte of the reserved area that must stay zero.
        pub const RESERVED: usize = 180;
    }

    /// Member table entry field offsets.
    pub mod member {
        pub const NAME: usize = 0;
        pub const ROLE: usize = 40;
        pub const KIND: usize = 56;
        pub const FLAGS: usize = 60;
        pub const BYTE_LEN: usize = 64;
        pub const EXTENT_FIRST: usize = 72;
        pub const EXTENT_COUNT: usize = 76;
        pub const DIGEST: usize = 80;
    }

    /// Extent table entry field offsets.
    pub mod extent {
        pub const LOGICAL_BLOCK: usize = 0;
        pub const PARTITION_BLOCK: usize = 8;
        pub const BLOCK_COUNT: usize = 16;
        pub const FLAGS: usize = 24;
    }
}

// ------------------------------------------------------------------- errors

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    /// A newer on-disk version. Refused with a diagnostic, never guessed at.
    UnsupportedVersion(u32),
    /// Geometry a v1 reader cannot use: a bad block size, or a table stride
    /// that is not what this version fixes it at.
    BadGeometry,
    BadHeaderCrc,
    BadMemberCrc,
    BadExtentCrc,
    Truncated,
    NotFound,
    OutOfRange,
    /// The manifest digest does not cover the tables on disk.
    ManifestMismatch,
    /// Content does not hash to what the manifest demands.
    DigestMismatch,
    /// The member carries no digest, so nothing can be checked.
    NoDigest,
    /// The caller's `BlockReader` failed.
    ReadFailed,
    /// The scratch buffer is smaller than one block.
    ScratchTooSmall,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic => write!(f, "not a pallet: bad magic"),
            Error::UnsupportedVersion(v) => {
                write!(f, "pallet format version {v} is newer than v{VERSION}")
            }
            Error::BadGeometry => write!(f, "bad pallet geometry"),
            Error::BadHeaderCrc => write!(f, "pallet superblock CRC mismatch"),
            Error::BadMemberCrc => write!(f, "pallet member table CRC mismatch"),
            Error::BadExtentCrc => write!(f, "pallet extent table CRC mismatch"),
            Error::Truncated => write!(f, "pallet truncated"),
            Error::NotFound => write!(f, "not found"),
            Error::OutOfRange => write!(f, "out of range"),
            Error::ManifestMismatch => {
                write!(f, "manifest digest does not cover the tables on disk")
            }
            Error::DigestMismatch => write!(f, "content does not match its recorded digest"),
            Error::NoDigest => write!(f, "member carries no digest"),
            Error::ReadFailed => write!(f, "read failed"),
            Error::ScratchTooSmall => write!(f, "scratch buffer is smaller than one block"),
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

// ---------------------------------------------------------------------- crc

/// CRC-32/IEEE — the one GPT and the pallet superblock use.
///
/// Not `crc32c`: that is a different polynomial, and this is the one the
/// firmware reader checks.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_continue(0, data)
}

/// Continue a CRC over another run, so the superblock's CRC can skip its own
/// field without copying 4 KB — this runs on a small firmware stack.
pub fn crc32_continue(prev: u32, data: &[u8]) -> u32 {
    let mut crc = !prev;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The superblock's CRC, computed with its own field read as zero.
pub fn superblock_crc(sb: &[u8]) -> u32 {
    let mut crc = crc32(&sb[..layout::sb::SUPERBLOCK_CRC]);
    crc = crc32_continue(crc, &[0, 0, 0, 0]);
    crc32_continue(crc, &sb[layout::sb::FLAGS..SUPERBLOCK_LEN])
}

// ------------------------------------------------------------------ decoding

pub fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

/// A NUL-padded UTF-8 field as a string. Invalid UTF-8 reads as empty rather
/// than panicking: a hostile pallet must not be able to stop a boot by putting
/// a bad byte in a name.
pub fn str_field(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap_or("")
}

// ---------------------------------------------------------- GPT attributes

/// Pallet state carried in GPT attribute bits 48–63, so boot selection is
/// readable before any filesystem exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct Attributes {
    /// 0 = never boot; higher wins. Selection order.
    pub priority: u8,
    /// Decremented per boot attempt. At 0 with `successful` clear, skipped.
    pub tries_left: u8,
    /// Set once the pallet has booted and been confirmed good.
    pub successful: bool,
    pub sealed: bool,
    pub read_only: bool,
    /// UEFI bit 0 — the node must not have this partition removed.
    pub required: bool,
}

impl Default for Attributes {
    fn default() -> Self {
        Attributes {
            priority: 0,
            tries_left: 0,
            successful: false,
            sealed: true,
            read_only: true,
            required: true,
        }
    }
}

impl Attributes {
    pub fn from_u64(a: u64) -> Attributes {
        Attributes {
            priority: ((a >> 48) & 0xF) as u8,
            tries_left: ((a >> 52) & 0xF) as u8,
            successful: (a >> 56) & 1 != 0,
            sealed: (a >> 57) & 1 != 0,
            read_only: (a >> 58) & 1 != 0,
            required: a & 1 != 0,
        }
    }

    pub fn to_u64(self) -> u64 {
        ((self.priority as u64 & 0xF) << 48)
            | ((self.tries_left as u64 & 0xF) << 52)
            | ((self.successful as u64) << 56)
            | ((self.sealed as u64) << 57)
            | ((self.read_only as u64) << 58)
            | (self.required as u64)
    }

    /// A pallet is a boot candidate unless priority is zero, or it ran out of
    /// tries without ever being confirmed good.
    pub fn is_candidate(&self) -> bool {
        self.priority > 0 && (self.successful || self.tries_left > 0)
    }

    /// The superblock's `flags` mirrors sealed and read-only, for a consumer
    /// that reads the partition without ever seeing the GPT.
    pub fn to_superblock_flags(self) -> u64 {
        ((self.sealed as u64) << 57) | ((self.read_only as u64) << 58)
    }
}

// ----------------------------------------------------------------- the kinds

/// What a pallet *is* — the discriminator a consumer selects on before it
/// looks at anything inside.
///
/// `Unspecified` is zero so a pallet written before this field existed reads
/// back as "did not say" rather than as an accidental `Boot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PalletKind {
    Unspecified,
    /// Kernel + initramfs + cmdline: what firmware selects between.
    Boot,
    /// The platform itself — the root image and what a node needs to be a node.
    System,
    /// A kernel and its modules, versioned apart from the boot pallet that
    /// pairs it with an initramfs.
    Kernel,
    /// Kubernetes/mkube control-plane and node components.
    Kube,
    /// An application: the set of containers one workload needs.
    App,
    /// Runtime dependencies shared by applications.
    Runtime,
    /// Data or configuration shipped as a sealed set.
    Data,
    Other(u32),
}

impl PalletKind {
    pub fn to_u32(self) -> u32 {
        match self {
            PalletKind::Unspecified => 0,
            PalletKind::Boot => 1,
            PalletKind::System => 2,
            PalletKind::Kernel => 3,
            PalletKind::Kube => 4,
            PalletKind::App => 5,
            PalletKind::Runtime => 6,
            PalletKind::Data => 7,
            PalletKind::Other(v) => v,
        }
    }

    pub fn from_u32(v: u32) -> PalletKind {
        match v {
            0 => PalletKind::Unspecified,
            1 => PalletKind::Boot,
            2 => PalletKind::System,
            3 => PalletKind::Kernel,
            4 => PalletKind::Kube,
            5 => PalletKind::App,
            6 => PalletKind::Runtime,
            7 => PalletKind::Data,
            other => PalletKind::Other(other),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PalletKind::Unspecified => "unspecified",
            PalletKind::Boot => "boot",
            PalletKind::System => "system",
            PalletKind::Kernel => "kernel",
            PalletKind::Kube => "kube",
            PalletKind::App => "app",
            PalletKind::Runtime => "runtime",
            PalletKind::Data => "data",
            PalletKind::Other(_) => "other",
        }
    }
}

impl fmt::Display for PalletKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PalletKind::Other(v) => write!(f, "other:{v}"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// What one member is.
///
/// Note that 5 is `Container` here. An earlier interim descriptor format in
/// `stormuefi-map` used 5 for `Pallet`, which is a different enumeration
/// entirely — conflating them is how a container member comes to render as a
/// pallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MemberKind {
    Raw,
    Kernel,
    Initramfs,
    BootConfig,
    RootImage,
    Container,
    Unknown(u32),
}

impl MemberKind {
    pub fn to_u32(self) -> u32 {
        match self {
            MemberKind::Raw => 0,
            MemberKind::Kernel => 1,
            MemberKind::Initramfs => 2,
            MemberKind::BootConfig => 3,
            MemberKind::RootImage => 4,
            MemberKind::Container => 5,
            MemberKind::Unknown(v) => v,
        }
    }

    pub fn from_u32(v: u32) -> MemberKind {
        match v {
            0 => MemberKind::Raw,
            1 => MemberKind::Kernel,
            2 => MemberKind::Initramfs,
            3 => MemberKind::BootConfig,
            4 => MemberKind::RootImage,
            5 => MemberKind::Container,
            other => MemberKind::Unknown(other),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MemberKind::Raw => "raw",
            MemberKind::Kernel => "kernel",
            MemberKind::Initramfs => "initramfs",
            MemberKind::BootConfig => "bootconfig",
            MemberKind::RootImage => "rootimage",
            MemberKind::Container => "container",
            MemberKind::Unknown(_) => "unknown",
        }
    }
}

impl fmt::Display for MemberKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberKind::Unknown(v) => write!(f, "unknown({v})"),
            other => f.write_str(other.as_str()),
        }
    }
}

// ------------------------------------------------------------- superblock

#[derive(Debug, Clone, Copy)]
pub struct Superblock {
    pub version: u32,
    pub block_size: u32,
    /// What this pallet is. Selection discriminator; see [`PalletKind`].
    pub kind: PalletKind,
    pub member_count: u32,
    pub extent_count: u32,
    /// Monotonic. Ordering is always by this, never by the version label,
    /// which cannot be argued with the way a version string can.
    pub pallet_version: u64,
    pub member_data_offset: u64,
    pub manifest_digest: [u8; 32],
    pub members_crc: u32,
    pub extents_crc: u32,
    pub flags: u64,
    name: [u8; NAME_LEN],
    version_label: [u8; VERSION_LABEL_LEN],
}

impl Superblock {
    pub fn parse(b: &[u8]) -> Result<Superblock> {
        use layout::sb as o;
        if b.len() < SUPERBLOCK_LEN {
            return Err(Error::Truncated);
        }
        if b[o::MAGIC..o::MAGIC + 8] != MAGIC {
            return Err(Error::BadMagic);
        }
        let version = rd_u32(b, o::VERSION);
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        if rd_u32(b, o::SUPERBLOCK_LEN) as usize != SUPERBLOCK_LEN
            || rd_u32(b, o::MEMBER_SIZE) as usize != MEMBER_LEN
            || rd_u32(b, o::EXTENT_SIZE) as usize != EXTENT_LEN
        {
            return Err(Error::BadGeometry);
        }
        let block_size = rd_u32(b, o::BLOCK_SIZE);
        if block_size < 512 || !block_size.is_power_of_two() {
            return Err(Error::BadGeometry);
        }
        if superblock_crc(b) != rd_u32(b, o::SUPERBLOCK_CRC) {
            return Err(Error::BadHeaderCrc);
        }

        let mut manifest_digest = [0u8; 32];
        manifest_digest.copy_from_slice(&b[o::MANIFEST_DIGEST..o::MANIFEST_DIGEST + 32]);
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&b[o::NAME..o::NAME + NAME_LEN]);
        let mut version_label = [0u8; VERSION_LABEL_LEN];
        version_label
            .copy_from_slice(&b[o::VERSION_LABEL..o::VERSION_LABEL + VERSION_LABEL_LEN]);

        Ok(Superblock {
            version,
            block_size,
            kind: PalletKind::from_u32(rd_u32(b, o::KIND)),
            member_count: rd_u32(b, o::MEMBER_COUNT),
            extent_count: rd_u32(b, o::EXTENT_COUNT),
            pallet_version: rd_u64(b, o::PALLET_VERSION),
            member_data_offset: rd_u64(b, o::MEMBER_DATA_OFFSET),
            manifest_digest,
            members_crc: rd_u32(b, o::MEMBERS_CRC),
            extents_crc: rd_u32(b, o::EXTENTS_CRC),
            flags: rd_u64(b, o::FLAGS),
            name,
            version_label,
        })
    }

    pub fn name(&self) -> &str {
        str_field(&self.name)
    }

    /// Human-readable version, e.g. `6.12.0-200.fc41`. Advisory only.
    pub fn version_label(&self) -> &str {
        str_field(&self.version_label)
    }

    pub fn members_len(&self) -> usize {
        self.member_count as usize * MEMBER_LEN
    }

    pub fn extents_len(&self) -> usize {
        self.extent_count as usize * EXTENT_LEN
    }

    /// Bytes to read from the partition start to hold superblock plus tables.
    pub fn tables_end(&self) -> usize {
        SUPERBLOCK_LEN + self.members_len() + self.extents_len()
    }

    pub fn sealed(&self) -> bool {
        self.flags >> 57 & 1 != 0
    }

    pub fn read_only(&self) -> bool {
        self.flags >> 58 & 1 != 0
    }
}

// ----------------------------------------------------------------- member

#[derive(Debug, Clone, Copy)]
pub struct Member {
    pub kind: MemberKind,
    pub flags: u32,
    pub byte_len: u64,
    pub extent_first: u32,
    pub extent_count: u32,
    pub digest: [u8; 32],
    name: [u8; NAME_LEN],
    role: [u8; ROLE_LEN],
}

impl Member {
    pub fn parse(b: &[u8]) -> Result<Member> {
        use layout::member as o;
        if b.len() < MEMBER_LEN {
            return Err(Error::Truncated);
        }
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&b[o::NAME..o::NAME + NAME_LEN]);
        let mut role = [0u8; ROLE_LEN];
        role.copy_from_slice(&b[o::ROLE..o::ROLE + ROLE_LEN]);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&b[o::DIGEST..o::DIGEST + 32]);
        Ok(Member {
            kind: MemberKind::from_u32(rd_u32(b, o::KIND)),
            flags: rd_u32(b, o::FLAGS),
            byte_len: rd_u64(b, o::BYTE_LEN),
            extent_first: rd_u32(b, o::EXTENT_FIRST),
            extent_count: rd_u32(b, o::EXTENT_COUNT),
            digest,
            name,
            role,
        })
    }

    pub fn name(&self) -> &str {
        str_field(&self.name)
    }

    pub fn role(&self) -> &str {
        str_field(&self.role)
    }

    pub fn is_sealed(&self) -> bool {
        self.flags & FLAG_SEALED != 0
    }

    pub fn is_read_only(&self) -> bool {
        self.flags & FLAG_READ_ONLY != 0
    }

    pub fn has_digest(&self) -> bool {
        self.flags & FLAG_DIGEST != 0
    }

    /// Blocks this member occupies, for a block device published over it.
    pub fn block_count(&self, block_size: u64) -> u64 {
        self.byte_len.div_ceil(block_size)
    }
}

// ----------------------------------------------------------------- extent

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_block: u64,
    /// **Relative to the partition start**, not an absolute LBA.
    pub partition_block: u64,
    pub block_count: u64,
    pub flags: u32,
}

impl Extent {
    pub fn parse(b: &[u8]) -> Result<Extent> {
        use layout::extent as o;
        if b.len() < EXTENT_LEN {
            return Err(Error::Truncated);
        }
        Ok(Extent {
            logical_block: rd_u64(b, o::LOGICAL_BLOCK),
            partition_block: rd_u64(b, o::PARTITION_BLOCK),
            block_count: rd_u64(b, o::BLOCK_COUNT),
            flags: rd_u32(b, o::FLAGS),
        })
    }
}

/// Where a logical read lands, relative to the partition. The caller adds the
/// partition's start; this crate never sees an absolute address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub partition_block: u64,
    pub offset_in_block: u64,
    pub run_len: u64,
}

// ------------------------------------------------------------------ reader

/// Reads blocks **relative to the partition start**, in the pallet's own
/// `block_size` — which stormblock writes equal to the media's sector size, so
/// on a real device the two coincide and there is no unit to convert.
pub trait BlockReader {
    fn read_blocks(&self, block: u64, buf: &mut [u8]) -> core::result::Result<(), ()>;
}

/// A pallet: a superblock and both tables, borrowed.
#[derive(Debug)]
pub struct Pallet<'a> {
    pub sb: Superblock,
    buf: &'a [u8],
}

impl<'a> Pallet<'a> {
    /// `buf` must hold superblock + member table + extent table, read from the
    /// partition start. Checks magic, version, geometry and all three CRCs, in
    /// that order.
    pub fn parse(buf: &'a [u8]) -> Result<Pallet<'a>> {
        let sb = Superblock::parse(buf)?;
        if buf.len() < sb.tables_end() {
            return Err(Error::Truncated);
        }
        let m = &buf[SUPERBLOCK_LEN..SUPERBLOCK_LEN + sb.members_len()];
        if crc32(m) != sb.members_crc {
            return Err(Error::BadMemberCrc);
        }
        let x = &buf[SUPERBLOCK_LEN + sb.members_len()..sb.tables_end()];
        if crc32(x) != sb.extents_crc {
            return Err(Error::BadExtentCrc);
        }
        Ok(Pallet { sb, buf })
    }

    /// Re-attach to bytes whose tables were already checked by [`Pallet::parse`].
    ///
    /// For a caller that owns the buffer and needs a view of it repeatedly —
    /// re-validating three CRCs per lookup would make reading a member
    /// quadratic in the size of its own tables.
    pub fn attach(sb: Superblock, buf: &'a [u8]) -> Pallet<'a> {
        Pallet { sb, buf }
    }

    pub fn name(&self) -> &str {
        self.sb.name()
    }

    pub fn version(&self) -> u64 {
        self.sb.pallet_version
    }

    pub fn kind(&self) -> PalletKind {
        self.sb.kind
    }

    pub fn version_label(&self) -> &str {
        self.sb.version_label()
    }

    pub fn member_count(&self) -> usize {
        self.sb.member_count as usize
    }

    pub fn member(&self, i: usize) -> Result<Member> {
        if i >= self.member_count() {
            return Err(Error::NotFound);
        }
        let o = SUPERBLOCK_LEN + i * MEMBER_LEN;
        Member::parse(&self.buf[o..o + MEMBER_LEN])
    }

    pub fn extent(&self, i: usize) -> Result<Extent> {
        if i >= self.sb.extent_count as usize {
            return Err(Error::NotFound);
        }
        let o = SUPERBLOCK_LEN + self.sb.members_len() + i * EXTENT_LEN;
        Extent::parse(&self.buf[o..o + EXTENT_LEN])
    }

    pub fn find(&self, name: &str) -> Result<Member> {
        for i in 0..self.member_count() {
            let m = self.member(i)?;
            if m.name() == name {
                return Ok(m);
            }
        }
        Err(Error::NotFound)
    }

    pub fn find_role(&self, role: &str) -> Result<Member> {
        for i in 0..self.member_count() {
            let m = self.member(i)?;
            if m.role() == role {
                return Ok(m);
            }
        }
        Err(Error::NotFound)
    }

    /// The remap: the longest contiguous run from `offset`, so reading a whole
    /// member costs one lookup per extent rather than one per block.
    pub fn map(&self, m: &Member, offset: u64) -> Result<Mapping> {
        if offset >= m.byte_len {
            return Err(Error::OutOfRange);
        }
        let bs = self.sb.block_size as u64;
        let lb = offset / bs;
        let off_in = offset % bs;
        for i in 0..m.extent_count as usize {
            let x = self.extent(m.extent_first as usize + i)?;
            if lb < x.logical_block || lb >= x.logical_block + x.block_count {
                continue;
            }
            let into = lb - x.logical_block;
            let run_in_extent = (x.block_count - into) * bs - off_in;
            let run_to_eof = m.byte_len - offset;
            return Ok(Mapping {
                partition_block: x.partition_block + into,
                offset_in_block: off_in,
                run_len: run_in_extent.min(run_to_eof),
            });
        }
        Err(Error::OutOfRange)
    }

    /// The member and extent tables, as the manifest digest covers them.
    pub fn tables(&self) -> &[u8] {
        &self.buf[SUPERBLOCK_LEN..self.sb.tables_end()]
    }
}

#[cfg(feature = "verify")]
mod verify {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Constant-time-ish comparison: no early exit on the first differing byte.
    fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    impl Pallet<'_> {
        /// Recompute the manifest digest over the member + extent tables.
        ///
        /// This is the quantity a signature covers, and it is why one signature
        /// covers the **combination**: the tables name every member by digest,
        /// so a different pairing is a different manifest.
        pub fn manifest_digest(&self) -> [u8; 32] {
            let mut h = Sha256::new();
            h.update(self.tables());
            h.finalize().into()
        }

        pub fn verify_manifest(&self) -> Result<()> {
            if !same(&self.manifest_digest(), &self.sb.manifest_digest) {
                return Err(Error::ManifestMismatch);
            }
            Ok(())
        }

        /// Hash a member's content through the extent map, using `scratch` for
        /// I/O — no allocation, and the caller decides how much memory to lend.
        pub fn digest_of(
            &self,
            m: &Member,
            reader: &impl BlockReader,
            scratch: &mut [u8],
        ) -> Result<[u8; 32]> {
            let bs = self.sb.block_size as usize;
            if scratch.len() < bs {
                return Err(Error::ScratchTooSmall);
            }
            let cap = scratch.len() / bs;
            let mut h = Sha256::new();
            let mut off = 0u64;
            while off < m.byte_len {
                let map = self.map(m, off)?;
                let mut remaining = map.run_len;
                let mut blk = map.partition_block;
                let mut in_block = map.offset_in_block as usize;
                while remaining > 0 {
                    let need = ((in_block as u64 + remaining).div_ceil(bs as u64))
                        .min(cap as u64) as usize;
                    let buf = &mut scratch[..need * bs];
                    reader.read_blocks(blk, buf).map_err(|_| Error::ReadFailed)?;
                    let avail = (need * bs - in_block) as u64;
                    let take = avail.min(remaining) as usize;
                    h.update(&buf[in_block..in_block + take]);
                    remaining -= take as u64;
                    off += take as u64;
                    blk += need as u64;
                    in_block = 0;
                }
            }
            Ok(h.finalize().into())
        }

        /// Verify a member against the digest recorded in **this manifest** —
        /// never against any other index. A descriptor is an unsigned map of
        /// where bytes live; the manifest is signed policy.
        pub fn verify_member(
            &self,
            m: &Member,
            reader: &impl BlockReader,
            scratch: &mut [u8],
        ) -> Result<()> {
            if !m.has_digest() {
                return Err(Error::NoDigest);
            }
            if !same(&self.digest_of(m, reader, scratch)?, &m.digest) {
                return Err(Error::DigestMismatch);
            }
            Ok(())
        }

        /// Full check in spec order: tables (done at parse), manifest digest,
        /// then every member's content. A member failing fails the whole
        /// pallet — partial acceptance would defeat combination signing.
        pub fn verify_all(&self, reader: &impl BlockReader, scratch: &mut [u8]) -> Result<()> {
            self.verify_manifest()?;
            for i in 0..self.member_count() {
                let m = self.member(i)?;
                if m.has_digest() {
                    self.verify_member(&m, reader, scratch)?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a superblock by hand, at the offsets the spec fixes. Nothing here
    /// calls the writer — that lives in stormblock, and a decoder tested only
    /// against its own encoder proves nothing about either.
    fn superblock(members: &[u8], extents: &[u8], kind: PalletKind, label: &str) -> [u8; SUPERBLOCK_LEN] {
        use layout::sb as o;
        let mut b = [0u8; SUPERBLOCK_LEN];
        b[o::MAGIC..o::MAGIC + 8].copy_from_slice(&MAGIC);
        b[o::VERSION..o::VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        b[o::SUPERBLOCK_LEN..o::SUPERBLOCK_LEN + 4]
            .copy_from_slice(&(SUPERBLOCK_LEN as u32).to_le_bytes());
        b[o::BLOCK_SIZE..o::BLOCK_SIZE + 4].copy_from_slice(&512u32.to_le_bytes());
        b[o::MEMBER_SIZE..o::MEMBER_SIZE + 4].copy_from_slice(&(MEMBER_LEN as u32).to_le_bytes());
        b[o::MEMBER_COUNT..o::MEMBER_COUNT + 4]
            .copy_from_slice(&((members.len() / MEMBER_LEN) as u32).to_le_bytes());
        b[o::EXTENT_SIZE..o::EXTENT_SIZE + 4].copy_from_slice(&(EXTENT_LEN as u32).to_le_bytes());
        b[o::EXTENT_COUNT..o::EXTENT_COUNT + 4]
            .copy_from_slice(&((extents.len() / EXTENT_LEN) as u32).to_le_bytes());
        b[o::PALLET_VERSION..o::PALLET_VERSION + 8].copy_from_slice(&7u64.to_le_bytes());
        b[o::MEMBER_DATA_OFFSET..o::MEMBER_DATA_OFFSET + 8]
            .copy_from_slice(&8192u64.to_le_bytes());
        b[o::NAME..o::NAME + 13].copy_from_slice(b"stormcos-boot");
        b[o::MEMBERS_CRC..o::MEMBERS_CRC + 4].copy_from_slice(&crc32(members).to_le_bytes());
        b[o::EXTENTS_CRC..o::EXTENTS_CRC + 4].copy_from_slice(&crc32(extents).to_le_bytes());
        b[o::KIND..o::KIND + 4].copy_from_slice(&kind.to_u32().to_le_bytes());
        b[o::VERSION_LABEL..o::VERSION_LABEL + label.len()].copy_from_slice(label.as_bytes());
        let crc = superblock_crc(&b);
        b[o::SUPERBLOCK_CRC..o::SUPERBLOCK_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn member(name: &str, role: &str, kind: MemberKind, len: u64, first: u32, count: u32) -> [u8; MEMBER_LEN] {
        use layout::member as o;
        let mut m = [0u8; MEMBER_LEN];
        m[o::NAME..o::NAME + name.len()].copy_from_slice(name.as_bytes());
        m[o::ROLE..o::ROLE + role.len()].copy_from_slice(role.as_bytes());
        m[o::KIND..o::KIND + 4].copy_from_slice(&kind.to_u32().to_le_bytes());
        m[o::FLAGS..o::FLAGS + 4]
            .copy_from_slice(&(FLAG_SEALED | FLAG_READ_ONLY | FLAG_DIGEST).to_le_bytes());
        m[o::BYTE_LEN..o::BYTE_LEN + 8].copy_from_slice(&len.to_le_bytes());
        m[o::EXTENT_FIRST..o::EXTENT_FIRST + 4].copy_from_slice(&first.to_le_bytes());
        m[o::EXTENT_COUNT..o::EXTENT_COUNT + 4].copy_from_slice(&count.to_le_bytes());
        m
    }

    fn extent(logical: u64, partition: u64, blocks: u64) -> [u8; EXTENT_LEN] {
        use layout::extent as o;
        let mut e = [0u8; EXTENT_LEN];
        e[o::LOGICAL_BLOCK..o::LOGICAL_BLOCK + 8].copy_from_slice(&logical.to_le_bytes());
        e[o::PARTITION_BLOCK..o::PARTITION_BLOCK + 8].copy_from_slice(&partition.to_le_bytes());
        e[o::BLOCK_COUNT..o::BLOCK_COUNT + 8].copy_from_slice(&blocks.to_le_bytes());
        e
    }

    fn image(members: &[u8], extents: &[u8]) -> std::vec::Vec<u8> {
        let sb = superblock(members, extents, PalletKind::Boot, "6.12.0");
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&sb);
        v.extend_from_slice(members);
        v.extend_from_slice(extents);
        v
    }

    #[test]
    fn a_hand_built_pallet_decodes_field_for_field() {
        let m = member("kernel", "kernel", MemberKind::Kernel, 1000, 0, 1);
        let x = extent(0, 16, 2);
        let buf = image(&m, &x);
        let p = Pallet::parse(&buf).expect("parse");

        assert_eq!(p.name(), "stormcos-boot");
        assert_eq!(p.version(), 7);
        assert_eq!(p.kind(), PalletKind::Boot);
        assert_eq!(p.version_label(), "6.12.0");
        assert_eq!(p.sb.block_size, 512);
        assert_eq!(p.member_count(), 1);

        let m = p.member(0).unwrap();
        assert_eq!(m.name(), "kernel");
        assert_eq!(m.role(), "kernel");
        assert_eq!(m.kind, MemberKind::Kernel);
        assert_eq!(m.byte_len, 1000);
        assert!(m.is_sealed() && m.is_read_only() && m.has_digest());
        assert_eq!(m.block_count(512), 2);
        assert_eq!(p.find("kernel").unwrap().byte_len, 1000);
        assert_eq!(p.find_role("kernel").unwrap().byte_len, 1000);
        assert!(p.find("nope").is_err());
    }

    /// 5 is `Container` in a member table. An earlier interim descriptor format
    /// used 5 for `Pallet`, and conflating the two is how a container member
    /// comes to render as a pallet.
    #[test]
    fn member_kind_five_is_a_container() {
        assert_eq!(MemberKind::from_u32(5), MemberKind::Container);
        assert_eq!(MemberKind::Container.to_u32(), 5);
        assert_eq!(MemberKind::from_u32(99), MemberKind::Unknown(99));
    }

    #[test]
    fn the_extension_fields_read_as_unspecified_when_they_are_zero() {
        let m = member("m", "r", MemberKind::Raw, 1, 0, 1);
        let x = extent(0, 16, 1);
        let mut buf = image(&m, &x);
        // Zero them and re-stamp the CRC, which is what a pallet written before
        // these fields existed looks like.
        use layout::sb as o;
        buf[o::KIND..o::KIND + 4].fill(0);
        buf[o::VERSION_LABEL..o::VERSION_LABEL + VERSION_LABEL_LEN].fill(0);
        let crc = superblock_crc(&buf[..SUPERBLOCK_LEN]);
        buf[o::SUPERBLOCK_CRC..o::SUPERBLOCK_CRC + 4].copy_from_slice(&crc.to_le_bytes());

        let p = Pallet::parse(&buf).unwrap();
        assert_eq!(p.kind(), PalletKind::Unspecified);
        assert_eq!(p.version_label(), "");
    }

    #[test]
    fn the_remap_walks_several_extents() {
        // One member of 3 blocks, laid out in two discontiguous runs.
        let m = member("split", "raw", MemberKind::Raw, 1536, 0, 2);
        let mut x = std::vec::Vec::new();
        x.extend_from_slice(&extent(0, 100, 1));
        x.extend_from_slice(&extent(1, 200, 2));
        let buf = image(&m, &x);
        let p = Pallet::parse(&buf).unwrap();
        let m = p.member(0).unwrap();

        let a = p.map(&m, 0).unwrap();
        assert_eq!(a.partition_block, 100);
        assert_eq!(a.offset_in_block, 0);
        assert_eq!(a.run_len, 512, "the first run ends where its extent does");

        let b = p.map(&m, 512).unwrap();
        assert_eq!(b.partition_block, 200);
        assert_eq!(b.run_len, 1024);

        let c = p.map(&m, 700).unwrap();
        assert_eq!(c.partition_block, 200);
        assert_eq!(c.offset_in_block, 188);
        assert_eq!(c.run_len, 1536 - 700, "a run never passes the end of the member");

        assert!(p.map(&m, 1536).is_err());
    }

    #[test]
    fn a_flipped_bit_is_caught_by_whichever_crc_covers_it() {
        let m = member("kernel", "kernel", MemberKind::Kernel, 1000, 0, 1);
        let x = extent(0, 16, 2);
        let good = image(&m, &x);
        assert!(Pallet::parse(&good).is_ok());

        let mut bad = good.clone();
        bad[SUPERBLOCK_LEN] ^= 1;
        assert_eq!(Pallet::parse(&bad).unwrap_err(), Error::BadMemberCrc);

        let mut bad = good.clone();
        bad[SUPERBLOCK_LEN + MEMBER_LEN] ^= 1;
        assert_eq!(Pallet::parse(&bad).unwrap_err(), Error::BadExtentCrc);

        let mut bad = good.clone();
        bad[layout::sb::PALLET_VERSION] ^= 1;
        assert_eq!(Pallet::parse(&bad).unwrap_err(), Error::BadHeaderCrc);
    }

    #[test]
    fn a_newer_version_and_a_wrong_stride_are_refused_not_guessed_at() {
        let m = member("m", "r", MemberKind::Raw, 1, 0, 1);
        let x = extent(0, 16, 1);
        let good = image(&m, &x);

        let mut newer = good.clone();
        newer[layout::sb::VERSION..layout::sb::VERSION + 4].copy_from_slice(&2u32.to_le_bytes());
        let crc = superblock_crc(&newer[..SUPERBLOCK_LEN]);
        newer[layout::sb::SUPERBLOCK_CRC..layout::sb::SUPERBLOCK_CRC + 4]
            .copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Pallet::parse(&newer).unwrap_err(), Error::UnsupportedVersion(2));

        let mut stride = good.clone();
        stride[layout::sb::MEMBER_SIZE..layout::sb::MEMBER_SIZE + 4]
            .copy_from_slice(&64u32.to_le_bytes());
        let crc = superblock_crc(&stride[..SUPERBLOCK_LEN]);
        stride[layout::sb::SUPERBLOCK_CRC..layout::sb::SUPERBLOCK_CRC + 4]
            .copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Pallet::parse(&stride).unwrap_err(), Error::BadGeometry);

        let short = &good[..SUPERBLOCK_LEN - 1];
        assert_eq!(Pallet::parse(short).unwrap_err(), Error::Truncated);
    }

    #[test]
    fn a_bad_name_cannot_stop_a_boot() {
        let mut m = member("kernel", "kernel", MemberKind::Kernel, 1, 0, 1);
        m[layout::member::NAME] = 0xFF; // not UTF-8
        let x = extent(0, 16, 1);
        let buf = image(&m, &x);
        let p = Pallet::parse(&buf).unwrap();
        assert_eq!(p.member(0).unwrap().name(), "", "invalid UTF-8 reads as empty");
    }

    #[test]
    fn attributes_round_trip_through_the_gpt_bits() {
        let a = Attributes {
            priority: 15,
            tries_left: 3,
            successful: true,
            sealed: true,
            read_only: false,
            required: true,
        };
        let raw = a.to_u64();
        assert_eq!(raw >> 48 & 0xF, 15);
        assert_eq!(raw >> 52 & 0xF, 3);
        assert_eq!(raw >> 56 & 1, 1);
        assert_eq!(raw >> 57 & 1, 1);
        assert_eq!(raw >> 58 & 1, 0);
        assert_eq!(raw & 1, 1);
        assert_eq!(Attributes::from_u64(raw), a);
        assert!(a.is_candidate());
        assert!(!Attributes { priority: 0, ..a }.is_candidate());
        assert!(!Attributes { priority: 5, tries_left: 0, successful: false, ..a }.is_candidate());
    }

    /// CRC-32/IEEE, against the value everyone else's implementation produces.
    #[test]
    fn the_crc_is_the_one_gpt_uses() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_continue(crc32(b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[cfg(feature = "verify")]
    mod verify {
        use super::*;

        struct Ram {
            bytes: std::vec::Vec<u8>,
            block: usize,
        }

        impl BlockReader for Ram {
            fn read_blocks(&self, block: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
                let at = block as usize * self.block;
                if at + buf.len() > self.bytes.len() {
                    return Err(());
                }
                buf.copy_from_slice(&self.bytes[at..at + buf.len()]);
                Ok(())
            }
        }

        fn with_digest(m: &mut [u8; MEMBER_LEN], content: &[u8]) {
            use sha2::{Digest, Sha256};
            let d: [u8; 32] = Sha256::digest(content).into();
            m[layout::member::DIGEST..layout::member::DIGEST + 32].copy_from_slice(&d);
        }

        #[test]
        fn a_member_verifies_against_the_manifests_digest_and_nothing_else() {
            let content = b"vmlinuz-payload";
            let mut m = member("kernel", "kernel", MemberKind::Kernel, content.len() as u64, 0, 1);
            with_digest(&mut m, content);
            let x = extent(0, 16, 1);

            // Stamp the manifest digest over the two tables, as a writer does.
            let mut tables = std::vec::Vec::new();
            tables.extend_from_slice(&m);
            tables.extend_from_slice(&x);
            use sha2::{Digest, Sha256};
            let manifest: [u8; 32] = Sha256::digest(&tables).into();

            let mut buf = image(&m, &x);
            buf[layout::sb::MANIFEST_DIGEST..layout::sb::MANIFEST_DIGEST + 32]
                .copy_from_slice(&manifest);
            let crc = superblock_crc(&buf[..SUPERBLOCK_LEN]);
            buf[layout::sb::SUPERBLOCK_CRC..layout::sb::SUPERBLOCK_CRC + 4]
                .copy_from_slice(&crc.to_le_bytes());

            // The medium: the pallet, then the content at partition block 16.
            let mut media = buf.clone();
            media.resize(16 * 512, 0);
            media.extend_from_slice(content);
            media.resize(17 * 512, 0);
            let ram = Ram { bytes: media.clone(), block: 512 };

            let p = Pallet::parse(&buf).unwrap();
            let mut scratch = [0u8; 4096];
            p.verify_manifest().expect("manifest");
            let m0 = p.member(0).unwrap();
            p.verify_member(&m0, &ram, &mut scratch).expect("member");
            p.verify_all(&ram, &mut scratch).expect("all");

            // Change one byte of content and it fails — nothing else changed.
            let mut tampered = media.clone();
            tampered[16 * 512] ^= 0xFF;
            let ram = Ram { bytes: tampered, block: 512 };
            assert_eq!(
                p.verify_member(&m0, &ram, &mut scratch),
                Err(Error::DigestMismatch)
            );
        }

        #[test]
        fn a_manifest_that_does_not_cover_the_tables_fails_before_any_member() {
            let m = member("kernel", "kernel", MemberKind::Kernel, 4, 0, 1);
            let x = extent(0, 16, 1);
            let buf = image(&m, &x); // manifest digest left zero
            let p = Pallet::parse(&buf).unwrap();
            assert_eq!(p.verify_manifest(), Err(Error::ManifestMismatch));
        }

        #[test]
        fn a_scratch_smaller_than_a_block_is_an_error_not_a_short_read() {
            let m = member("kernel", "kernel", MemberKind::Kernel, 4, 0, 1);
            let x = extent(0, 16, 1);
            let buf = image(&m, &x);
            let p = Pallet::parse(&buf).unwrap();
            let ram = Ram { bytes: std::vec![0u8; 32 * 512], block: 512 };
            let mut tiny = [0u8; 16];
            assert_eq!(
                p.digest_of(&p.member(0).unwrap(), &ram, &mut tiny),
                Err(Error::ScratchTooSmall)
            );
        }
    }
}
