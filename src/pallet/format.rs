//! Pallet on-disk format v1 — writer and reader.
//!
//! Byte-compatible with `stormuefi-map`, which is the reference reader and the
//! one that runs in firmware. `docs/PALLET-SPEC.md` in `stormuefi` is the
//! specification; this module is deliberately a transcription of it rather
//! than an interpretation.
//!
//! ```text
//! +0            superblock (4096 B)
//! +4096         member table (member_count × 128 B)
//!               extent table (extent_count × 32 B)
//!               ... padding ...
//! +data         member content
//! ```
//!
//! **Every offset is relative to the partition start.** That is what makes a
//! pallet byte-for-byte copyable to another disk, image or ISO: assembling a
//! bootable image is concatenating pallets and writing a GPT, with nothing
//! inside any of them to rewrite.

use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{crc32, crc32_continue, MemberContent, PalletError, PartitionView, Result};

pub const MAGIC: [u8; 8] = *b"STORMPAL";
pub const VERSION: u32 = 1;
pub const SUPERBLOCK_LEN: usize = 4096;
pub const MEMBER_LEN: usize = 128;
pub const EXTENT_LEN: usize = 32;
pub const NAME_LEN: usize = 40;
pub const ROLE_LEN: usize = 16;

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

/// Chunk size for digesting and copying member content.
const IO_CHUNK: usize = 1024 * 1024;

/// Length of the human-readable version label in the superblock.
pub const VERSION_LABEL_LEN: usize = 32;

// The superblock's reserved area starts at 144 and the spec requires it to be
// zero, so every field placed here is defined such that **zero means
// "unspecified"** and an older reader that ignores it is still correct. See
// `docs/pallets.md`; raised upstream against `PALLET-SPEC.md` so v1.1 can
// bless the offsets rather than leave them de-facto.
const OFF_KIND: usize = 144;
const OFF_VERSION_LABEL: usize = 148;

// ---------------------------------------------------------- GPT attributes

/// Pallet state carried in GPT attribute bits 48–63, so boot selection is
/// readable before any filesystem exists — the approach ChromeOS uses, chosen
/// because it is proven and because it keeps boot state out of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// The superblock's `flags` field mirrors the sealed and read-only bits,
    /// for a consumer that reads the partition without ever seeing the GPT.
    pub fn to_superblock_flags(self) -> u64 {
        ((self.sealed as u64) << 57) | ((self.read_only as u64) << 58)
    }
}

// ---------------------------------------------------------------- pallet kind

/// What a pallet *is* — the discriminator a consumer selects on before it
/// looks at anything inside.
///
/// A node carries several pallets at once and they are not interchangeable:
/// stormuefi wants the boot pallet, stormpump wants the ones holding
/// application containers, and an operator asking "what kernel is this node
/// on" wants neither. Selection by name would work only as long as everyone
/// agreed on names, which is exactly the kind of agreement that rots.
///
/// `Unspecified` is zero so a pallet written before this field existed reads
/// back as "did not say" rather than as an accidental `Boot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PalletKind {
    Unspecified,
    /// Kernel + initramfs + cmdline: what firmware selects between.
    Boot,
    /// The platform itself — the root image and what the node needs to be a node.
    System,
    /// A kernel and its modules, versioned independently of the boot pallet
    /// that pairs it with an initramfs.
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

    pub fn parse(s: &str) -> PalletKind {
        match s.to_ascii_lowercase().as_str() {
            "" | "unspecified" => PalletKind::Unspecified,
            "boot" => PalletKind::Boot,
            "system" => PalletKind::System,
            "kernel" => PalletKind::Kernel,
            "kube" | "kubernetes" => PalletKind::Kube,
            "app" | "application" => PalletKind::App,
            "runtime" => PalletKind::Runtime,
            "data" => PalletKind::Data,
            other => match other.strip_prefix("other:").and_then(|v| v.parse().ok()) {
                Some(v) => PalletKind::Other(v),
                None => PalletKind::Unspecified,
            },
        }
    }
}

impl fmt::Display for PalletKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PalletKind::Unspecified => write!(f, "unspecified"),
            PalletKind::Boot => write!(f, "boot"),
            PalletKind::System => write!(f, "system"),
            PalletKind::Kernel => write!(f, "kernel"),
            PalletKind::Kube => write!(f, "kube"),
            PalletKind::App => write!(f, "app"),
            PalletKind::Runtime => write!(f, "runtime"),
            PalletKind::Data => write!(f, "data"),
            PalletKind::Other(v) => write!(f, "other:{v}"),
        }
    }
}

// ---------------------------------------------------------------- member kind

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

    pub fn parse(s: &str) -> MemberKind {
        match s {
            "raw" => MemberKind::Raw,
            "kernel" => MemberKind::Kernel,
            "initramfs" => MemberKind::Initramfs,
            "bootconfig" | "boot_config" | "cmdline" => MemberKind::BootConfig,
            "rootimage" | "root_image" | "root" => MemberKind::RootImage,
            "container" => MemberKind::Container,
            _ => MemberKind::Raw,
        }
    }
}

impl fmt::Display for MemberKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberKind::Raw => write!(f, "raw"),
            MemberKind::Kernel => write!(f, "kernel"),
            MemberKind::Initramfs => write!(f, "initramfs"),
            MemberKind::BootConfig => write!(f, "bootconfig"),
            MemberKind::RootImage => write!(f, "rootimage"),
            MemberKind::Container => write!(f, "container"),
            MemberKind::Unknown(v) => write!(f, "unknown({v})"),
        }
    }
}

// -------------------------------------------------------------- member spec

/// One member to publish: its identity, and where its bytes come from.
pub struct MemberSpec {
    pub name: String,
    pub role: String,
    pub kind: MemberKind,
    pub flags: u32,
    pub content: Arc<dyn MemberContent>,
}

impl MemberSpec {
    /// A sealed, read-only, digest-carrying member — what nearly every member
    /// is, because those three properties are what a pallet exists to provide.
    pub fn new(
        name: impl Into<String>,
        role: impl Into<String>,
        kind: MemberKind,
        content: Arc<dyn MemberContent>,
    ) -> Self {
        MemberSpec {
            name: name.into(),
            role: role.into(),
            kind,
            flags: FLAG_SEALED | FLAG_READ_ONLY | FLAG_DIGEST,
            content,
        }
    }

    pub fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }
}

// --------------------------------------------------------------- superblock

#[derive(Debug, Clone)]
pub struct Superblock {
    pub version: u32,
    pub block_size: u32,
    /// What this pallet is. Selection discriminator; see [`PalletKind`].
    pub kind: PalletKind,
    /// Human-readable version, e.g. `6.12.0-200.fc41` or `v9.3.0`. Advisory —
    /// ordering is always by `pallet_version`, which is monotonic and cannot be
    /// argued with the way a version string can.
    pub version_label: String,
    pub member_count: u32,
    pub extent_count: u32,
    pub pallet_version: u64,
    pub member_data_offset: u64,
    pub name: String,
    pub manifest_digest: [u8; 32],
    pub members_crc: u32,
    pub extents_crc: u32,
    pub flags: u64,
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn str_from(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn write_fixed(dst: &mut [u8], s: &str, field: &'static str) -> Result<()> {
    let b = s.as_bytes();
    if b.len() > dst.len() {
        return Err(PalletError::TooLong { field, max: dst.len(), got: b.len() });
    }
    dst[..b.len()].copy_from_slice(b);
    Ok(())
}

/// CRC over the superblock with its own CRC field read as zero.
fn superblock_crc(sb: &[u8]) -> u32 {
    let mut crc = crc32(&sb[..132]);
    crc = crc32_continue(crc, &[0, 0, 0, 0]);
    crc32_continue(crc, &sb[136..SUPERBLOCK_LEN])
}

impl Superblock {
    pub fn parse(b: &[u8]) -> Result<Superblock> {
        if b.len() < SUPERBLOCK_LEN {
            return Err(PalletError::Truncated { need: SUPERBLOCK_LEN, have: b.len() });
        }
        if b[0..8] != MAGIC {
            return Err(PalletError::BadMagic);
        }
        let version = rd_u32(b, 8);
        if version != VERSION {
            return Err(PalletError::UnsupportedVersion(version));
        }
        if rd_u32(b, 12) as usize != SUPERBLOCK_LEN {
            return Err(PalletError::BadGeometry("superblock_len".into()));
        }
        if rd_u32(b, 20) as usize != MEMBER_LEN {
            return Err(PalletError::BadGeometry("member_size".into()));
        }
        if rd_u32(b, 28) as usize != EXTENT_LEN {
            return Err(PalletError::BadGeometry("extent_size".into()));
        }
        let block_size = rd_u32(b, 16);
        if block_size < 512 || !block_size.is_power_of_two() {
            return Err(PalletError::BadGeometry(format!("block_size {block_size}")));
        }
        if superblock_crc(b) != rd_u32(b, 132) {
            return Err(PalletError::BadHeaderCrc);
        }
        let mut manifest_digest = [0u8; 32];
        manifest_digest.copy_from_slice(&b[92..124]);
        Ok(Superblock {
            version,
            block_size,
            kind: PalletKind::from_u32(rd_u32(b, OFF_KIND)),
            version_label: str_from(&b[OFF_VERSION_LABEL..OFF_VERSION_LABEL + VERSION_LABEL_LEN]),
            member_count: rd_u32(b, 24),
            extent_count: rd_u32(b, 32),
            pallet_version: rd_u64(b, 36),
            member_data_offset: rd_u64(b, 44),
            name: str_from(&b[52..52 + NAME_LEN]),
            manifest_digest,
            members_crc: rd_u32(b, 124),
            extents_crc: rd_u32(b, 128),
            flags: rd_u64(b, 136),
        })
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

// ------------------------------------------------------------------ member

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub kind: MemberKind,
    pub flags: u32,
    pub byte_len: u64,
    pub extent_first: u32,
    pub extent_count: u32,
    pub digest: [u8; 32],
}

impl Member {
    fn parse(b: &[u8]) -> Member {
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&b[80..112]);
        Member {
            name: str_from(&b[0..NAME_LEN]),
            role: str_from(&b[40..40 + ROLE_LEN]),
            kind: MemberKind::from_u32(rd_u32(b, 56)),
            flags: rd_u32(b, 60),
            byte_len: rd_u64(b, 64),
            extent_first: rd_u32(b, 72),
            extent_count: rd_u32(b, 76),
            digest,
        }
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

    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest)
    }
}

// ------------------------------------------------------------------ extent

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_block: u64,
    /// Relative to the partition start, not an absolute LBA.
    pub partition_block: u64,
    pub block_count: u64,
    pub flags: u32,
}

/// Where a logical read lands, relative to the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub partition_block: u64,
    pub offset_in_block: u64,
    pub run_len: u64,
}

// ------------------------------------------------------------------ builder

/// Where one member's content lands inside the partition.
#[derive(Debug, Clone)]
pub struct Placement {
    pub member: usize,
    pub offset: u64,
    pub len: u64,
    pub digest: [u8; 32],
}

/// A laid-out pallet: the header bytes, and where every member's content goes.
#[derive(Debug)]
pub struct BuiltPallet {
    /// Superblock + member table + extent table, padded to `member_data_offset`.
    pub header: Vec<u8>,
    pub placements: Vec<Placement>,
    pub member_data_offset: u64,
    pub block_size: u32,
    pub manifest_digest: [u8; 32],
    /// Bytes the partition must be able to hold.
    pub total_bytes: u64,
}

/// Builds a pallet image.
///
/// `build` sizes and digests every member without writing anything, so a
/// caller can find out how big a partition it needs before allocating one.
/// `publish` then writes content first and the superblock last.
pub struct PalletBuilder {
    name: String,
    kind: PalletKind,
    version_label: String,
    pallet_version: u64,
    block_size: u32,
    attributes: Attributes,
    members: Vec<MemberSpec>,
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

impl PalletBuilder {
    pub fn new(name: impl Into<String>, pallet_version: u64) -> Self {
        PalletBuilder {
            name: name.into(),
            kind: PalletKind::Unspecified,
            version_label: String::new(),
            pallet_version,
            block_size: 4096,
            attributes: Attributes::default(),
            members: Vec::new(),
        }
    }

    /// Block size the extent table is expressed in. It must be no smaller
    /// than the device's own block size, or an extent would name something the
    /// device cannot address.
    pub fn block_size(mut self, bs: u32) -> Self {
        self.block_size = bs;
        self
    }

    pub fn attributes(mut self, attrs: Attributes) -> Self {
        self.attributes = attrs;
        self
    }

    /// What this pallet is — `boot`, `system`, `kernel`, `kube`, …
    pub fn kind(mut self, kind: PalletKind) -> Self {
        self.kind = kind;
        self
    }

    /// A human-readable version to sit beside the monotonic `pallet_version`.
    pub fn version_label(mut self, label: impl Into<String>) -> Self {
        self.version_label = label.into();
        self
    }

    pub fn member(mut self, m: MemberSpec) -> Self {
        self.members.push(m);
        self
    }

    pub fn members(&self) -> &[MemberSpec] {
        &self.members
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pallet_version(&self) -> u64 {
        self.pallet_version
    }

    pub fn pallet_kind(&self) -> PalletKind {
        self.kind
    }

    /// Size, digest and lay out every member, and produce the header bytes.
    pub async fn build(&self) -> Result<BuiltPallet> {
        if self.block_size < 512 || !self.block_size.is_power_of_two() {
            return Err(PalletError::BadGeometry(format!("block_size {}", self.block_size)));
        }
        if self.name.len() > NAME_LEN {
            return Err(PalletError::TooLong {
                field: "pallet name",
                max: NAME_LEN,
                got: self.name.len(),
            });
        }
        if self.version_label.len() > VERSION_LABEL_LEN {
            return Err(PalletError::TooLong {
                field: "version label",
                max: VERSION_LABEL_LEN,
                got: self.version_label.len(),
            });
        }
        let bs = self.block_size as u64;
        let n = self.members.len();
        // One extent per member today. The reader handles many, so a sparse
        // layout is a change here and nowhere else.
        let extent_count = self.members.iter().filter(|m| m.content.byte_len() > 0).count();

        let tables_end = SUPERBLOCK_LEN + n * MEMBER_LEN + extent_count * EXTENT_LEN;
        let member_data_offset = align_up(tables_end as u64, bs.max(SUPERBLOCK_LEN as u64));

        let mut members_tbl = vec![0u8; n * MEMBER_LEN];
        let mut extents_tbl = vec![0u8; extent_count * EXTENT_LEN];
        let mut placements = Vec::with_capacity(n);

        let mut cursor = member_data_offset;
        let mut extent_idx = 0usize;
        for (i, m) in self.members.iter().enumerate() {
            let len = m.content.byte_len();
            let digest = digest_content(m.content.as_ref()).await?;

            let (first, count) = if len > 0 {
                let blocks = len.div_ceil(bs);
                let e = &mut extents_tbl[extent_idx * EXTENT_LEN..(extent_idx + 1) * EXTENT_LEN];
                wr_u64(e, 0, 0);
                wr_u64(e, 8, cursor / bs);
                wr_u64(e, 16, blocks);
                wr_u32(e, 24, 0);
                let first = extent_idx as u32;
                extent_idx += 1;
                placements.push(Placement { member: i, offset: cursor, len, digest });
                cursor += blocks * bs;
                (first, 1u32)
            } else {
                placements.push(Placement { member: i, offset: cursor, len: 0, digest });
                (0u32, 0u32)
            };

            let e = &mut members_tbl[i * MEMBER_LEN..(i + 1) * MEMBER_LEN];
            write_fixed(&mut e[0..NAME_LEN], &m.name, "member name")?;
            write_fixed(&mut e[40..40 + ROLE_LEN], &m.role, "member role")?;
            wr_u32(e, 56, m.kind.to_u32());
            wr_u32(e, 60, m.flags);
            wr_u64(e, 64, len);
            wr_u32(e, 72, first);
            wr_u32(e, 76, count);
            e[80..112].copy_from_slice(&digest);
        }

        // The manifest digest covers the member and extent tables together —
        // which is what makes one signature cover the *combination*. A signed
        // kernel paired with a different signed initramfs is a different
        // manifest and will not verify.
        let mut h = Sha256::new();
        h.update(&members_tbl);
        h.update(&extents_tbl);
        let manifest_digest: [u8; 32] = h.finalize().into();

        let mut header = vec![0u8; member_data_offset as usize];
        header[0..8].copy_from_slice(&MAGIC);
        wr_u32(&mut header, 8, VERSION);
        wr_u32(&mut header, 12, SUPERBLOCK_LEN as u32);
        wr_u32(&mut header, 16, self.block_size);
        wr_u32(&mut header, 20, MEMBER_LEN as u32);
        wr_u32(&mut header, 24, n as u32);
        wr_u32(&mut header, 28, EXTENT_LEN as u32);
        wr_u32(&mut header, 32, extent_count as u32);
        wr_u64(&mut header, 36, self.pallet_version);
        wr_u64(&mut header, 44, member_data_offset);
        write_fixed(&mut header[52..52 + NAME_LEN], &self.name, "pallet name")?;
        header[92..124].copy_from_slice(&manifest_digest);
        wr_u32(&mut header, 124, crc32(&members_tbl));
        wr_u32(&mut header, 128, crc32(&extents_tbl));
        wr_u64(&mut header, 136, self.attributes.to_superblock_flags());
        wr_u32(&mut header, OFF_KIND, self.kind.to_u32());
        write_fixed(
            &mut header[OFF_VERSION_LABEL..OFF_VERSION_LABEL + VERSION_LABEL_LEN],
            &self.version_label,
            "version label",
        )?;
        header[SUPERBLOCK_LEN..SUPERBLOCK_LEN + members_tbl.len()].copy_from_slice(&members_tbl);
        let xo = SUPERBLOCK_LEN + members_tbl.len();
        header[xo..xo + extents_tbl.len()].copy_from_slice(&extents_tbl);
        let crc = superblock_crc(&header);
        wr_u32(&mut header, 132, crc);

        Ok(BuiltPallet {
            header,
            placements,
            member_data_offset,
            block_size: self.block_size,
            manifest_digest,
            total_bytes: align_up(cursor, bs),
        })
    }

    /// Build, then write into `view`.
    pub async fn publish(&self, view: &PartitionView) -> Result<BuiltPallet> {
        let built = self.build().await?;
        self.write(&built, view).await?;
        Ok(built)
    }

    /// Write an already-built pallet into `view`.
    ///
    /// Content goes down first and the superblock last, so an interrupted
    /// publish leaves a partition that fails its own CRC — refused by any
    /// consumer — rather than a manifest describing content that is not there.
    pub async fn write(&self, built: &BuiltPallet, view: &PartitionView) -> Result<()> {
        if built.total_bytes > view.len() {
            return Err(PalletError::NoSpace {
                need: built.total_bytes,
                largest_free: view.len(),
            });
        }
        if self.block_size < view.block_size() {
            return Err(PalletError::BadGeometry(format!(
                "pallet block size {} is smaller than the device's {}",
                self.block_size,
                view.block_size()
            )));
        }

        for p in &built.placements {
            if p.len == 0 {
                continue;
            }
            let content = self.members[p.member].content.as_ref();
            let mut off = 0u64;
            let mut buf = vec![0u8; IO_CHUNK];
            while off < p.len {
                let take = ((p.len - off) as usize).min(IO_CHUNK);
                let chunk = &mut buf[..take];
                content.read_at(off, chunk).await?;
                view.write_at(p.offset + off, chunk).await?;
                off += take as u64;
            }
        }
        view.flush().await?;

        view.write_at(0, &built.header).await?;
        view.flush().await?;
        Ok(())
    }
}

async fn digest_content(c: &dyn MemberContent) -> Result<[u8; 32]> {
    let len = c.byte_len();
    let mut h = Sha256::new();
    let mut off = 0u64;
    let mut buf = vec![0u8; IO_CHUNK];
    while off < len {
        let take = ((len - off) as usize).min(IO_CHUNK);
        let chunk = &mut buf[..take];
        c.read_at(off, chunk).await?;
        h.update(&*chunk);
        off += take as u64;
    }
    Ok(h.finalize().into())
}

// ------------------------------------------------------------------- reader

/// A pallet read back from a partition: superblock plus both tables.
///
/// Parsing checks magic, version, geometry and all three CRCs, in that order.
/// Content is not touched until [`Pallet::verify_member`] asks for it.
pub struct Pallet {
    pub sb: Superblock,
    buf: Vec<u8>,
}

impl Pallet {
    /// `buf` must hold superblock + member table + extent table, read from the
    /// partition start.
    pub fn parse(buf: Vec<u8>) -> Result<Pallet> {
        let sb = Superblock::parse(&buf)?;
        if buf.len() < sb.tables_end() {
            return Err(PalletError::Truncated { need: sb.tables_end(), have: buf.len() });
        }
        let m = &buf[SUPERBLOCK_LEN..SUPERBLOCK_LEN + sb.members_len()];
        if crc32(m) != sb.members_crc {
            return Err(PalletError::BadMemberCrc);
        }
        let x = &buf[SUPERBLOCK_LEN + sb.members_len()..sb.tables_end()];
        if crc32(x) != sb.extents_crc {
            return Err(PalletError::BadExtentCrc);
        }
        Ok(Pallet { sb, buf })
    }

    /// Read a pallet from the start of a partition: one read for the
    /// superblock, a second sized by what it says the tables are.
    pub async fn read(view: &PartitionView) -> Result<Pallet> {
        let mut head = vec![0u8; SUPERBLOCK_LEN];
        view.read_at(0, &mut head).await?;
        let sb = Superblock::parse(&head)?;
        let mut buf = vec![0u8; sb.tables_end()];
        view.read_at(0, &mut buf).await?;
        Pallet::parse(buf)
    }

    pub fn name(&self) -> &str {
        &self.sb.name
    }

    pub fn version(&self) -> u64 {
        self.sb.pallet_version
    }

    pub fn kind(&self) -> PalletKind {
        self.sb.kind
    }

    pub fn version_label(&self) -> &str {
        &self.sb.version_label
    }

    pub fn member_count(&self) -> usize {
        self.sb.member_count as usize
    }

    pub fn member(&self, i: usize) -> Result<Member> {
        if i >= self.member_count() {
            return Err(PalletError::NotFound(format!("member index {i}")));
        }
        let o = SUPERBLOCK_LEN + i * MEMBER_LEN;
        Ok(Member::parse(&self.buf[o..o + MEMBER_LEN]))
    }

    pub fn members(&self) -> Vec<Member> {
        (0..self.member_count()).filter_map(|i| self.member(i).ok()).collect()
    }

    pub fn extent(&self, i: usize) -> Result<Extent> {
        if i >= self.sb.extent_count as usize {
            return Err(PalletError::NotFound(format!("extent index {i}")));
        }
        let o = SUPERBLOCK_LEN + self.sb.members_len() + i * EXTENT_LEN;
        let b = &self.buf[o..o + EXTENT_LEN];
        Ok(Extent {
            logical_block: rd_u64(b, 0),
            partition_block: rd_u64(b, 8),
            block_count: rd_u64(b, 16),
            flags: rd_u32(b, 24),
        })
    }

    pub fn find(&self, name: &str) -> Result<Member> {
        (0..self.member_count())
            .filter_map(|i| self.member(i).ok())
            .find(|m| m.name == name)
            .ok_or_else(|| PalletError::NotFound(format!("member '{name}'")))
    }

    pub fn find_role(&self, role: &str) -> Result<Member> {
        (0..self.member_count())
            .filter_map(|i| self.member(i).ok())
            .find(|m| m.role == role)
            .ok_or_else(|| PalletError::NotFound(format!("member with role '{role}'")))
    }

    /// The remap: the longest contiguous run from `offset`, so reading a whole
    /// member costs one lookup per extent rather than one per block.
    pub fn map(&self, m: &Member, offset: u64) -> Result<Mapping> {
        if offset >= m.byte_len {
            return Err(PalletError::OutOfRange { offset, len: 0 });
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
        Err(PalletError::OutOfRange { offset, len: 0 })
    }

    /// Read a member's content through the extent map.
    pub async fn read_member(
        &self,
        m: &Member,
        view: &PartitionView,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if offset + buf.len() as u64 > m.byte_len {
            return Err(PalletError::OutOfRange { offset, len: buf.len() as u64 });
        }
        let bs = self.sb.block_size as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let map = self.map(m, offset + done as u64)?;
            let take = (map.run_len as usize).min(buf.len() - done);
            let at = map.partition_block * bs + map.offset_in_block;
            view.read_at(at, &mut buf[done..done + take]).await?;
            done += take;
        }
        Ok(())
    }

    /// Recompute the manifest digest over the member + extent tables.
    pub fn manifest_digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.buf[SUPERBLOCK_LEN..self.sb.tables_end()]);
        h.finalize().into()
    }

    pub fn verify_manifest(&self) -> Result<()> {
        if self.manifest_digest() != self.sb.manifest_digest {
            return Err(PalletError::ManifestMismatch);
        }
        Ok(())
    }

    pub async fn digest_of(&self, m: &Member, view: &PartitionView) -> Result<[u8; 32]> {
        let mut h = Sha256::new();
        let mut off = 0u64;
        let mut buf = vec![0u8; IO_CHUNK];
        while off < m.byte_len {
            let take = ((m.byte_len - off) as usize).min(IO_CHUNK);
            let chunk = &mut buf[..take];
            self.read_member(m, view, off, chunk).await?;
            h.update(&*chunk);
            off += take as u64;
        }
        Ok(h.finalize().into())
    }

    /// Verify a member against the digest recorded in **this manifest** —
    /// never against any other index. The descriptor is an unsigned map of
    /// where bytes live; the manifest is signed policy.
    pub async fn verify_member(&self, m: &Member, view: &PartitionView) -> Result<()> {
        if !m.has_digest() {
            return Err(PalletError::NoDigest { member: m.name.clone() });
        }
        if self.digest_of(m, view).await? != m.digest {
            return Err(PalletError::DigestMismatch { member: m.name.clone() });
        }
        Ok(())
    }

    /// Full check in spec order: tables (done at parse), manifest digest, then
    /// every member's content. A member failing fails the whole pallet —
    /// partial acceptance would defeat combination signing.
    pub async fn verify_all(&self, view: &PartitionView) -> Result<()> {
        self.verify_manifest()?;
        for m in self.members() {
            if m.has_digest() {
                self.verify_member(&m, view).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pallet::BytesContent;

    fn spec(name: &str, bytes: &[u8]) -> MemberSpec {
        MemberSpec::new(
            name,
            "container",
            MemberKind::Container,
            Arc::new(BytesContent(bytes.to_vec())),
        )
    }

    /// The layout is a contract with a reader that runs in firmware and cannot
    /// be updated in lockstep with this. Round-tripping through our own reader
    /// would prove nothing about it, so the offsets are asserted directly.
    #[tokio::test]
    async fn the_superblock_lands_exactly_where_the_spec_says() {
        let built = PalletBuilder::new("stormcos-boot", 7)
            .block_size(4096)
            .kind(PalletKind::Boot)
            .version_label("6.12.0")
            .member(spec("kernel", b"payload"))
            .build()
            .await
            .unwrap();
        let h = &built.header;

        assert_eq!(&h[0..8], b"STORMPAL");
        assert_eq!(rd_u32(h, 8), 1, "format version");
        assert_eq!(rd_u32(h, 12), 4096, "superblock_len");
        assert_eq!(rd_u32(h, 16), 4096, "block_size");
        assert_eq!(rd_u32(h, 20), 128, "member_size");
        assert_eq!(rd_u32(h, 24), 1, "member_count");
        assert_eq!(rd_u32(h, 28), 32, "extent_size");
        assert_eq!(rd_u32(h, 32), 1, "extent_count");
        assert_eq!(rd_u64(h, 36), 7, "pallet_version");
        assert_eq!(rd_u64(h, 44), 8192, "member_data_offset");
        assert_eq!(str_from(&h[52..52 + NAME_LEN]), "stormcos-boot");
        assert_eq!(&h[92..124], &built.manifest_digest);
        assert_eq!(rd_u32(h, 124), crc32(&h[SUPERBLOCK_LEN..SUPERBLOCK_LEN + MEMBER_LEN]));
        assert_eq!(rd_u32(h, 132), superblock_crc(h), "superblock_crc skips itself");
        // Sealed and read-only are mirrored for a consumer that never sees the GPT.
        assert_eq!(rd_u64(h, 136), (1u64 << 57) | (1u64 << 58));
        // Extension fields, defined so that zero reads back as "unspecified".
        assert_eq!(rd_u32(h, OFF_KIND), PalletKind::Boot.to_u32());
        assert_eq!(
            str_from(&h[OFF_VERSION_LABEL..OFF_VERSION_LABEL + VERSION_LABEL_LEN]),
            "6.12.0"
        );
        assert!(h[180..SUPERBLOCK_LEN].iter().all(|&b| b == 0), "reserved stays zero");

        // Member table at 4096, fields where the spec puts them.
        let m = &h[SUPERBLOCK_LEN..SUPERBLOCK_LEN + MEMBER_LEN];
        assert_eq!(str_from(&m[0..NAME_LEN]), "kernel");
        assert_eq!(str_from(&m[40..40 + ROLE_LEN]), "container");
        assert_eq!(rd_u32(m, 56), MemberKind::Container.to_u32());
        assert_eq!(rd_u32(m, 60), FLAG_SEALED | FLAG_READ_ONLY | FLAG_DIGEST);
        assert_eq!(rd_u64(m, 64), 7, "byte_len");
        assert_eq!(rd_u32(m, 72), 0, "extent_first");
        assert_eq!(rd_u32(m, 76), 1, "extent_count");

        // Extent table follows it; partition_block is partition-relative.
        let x = &h[SUPERBLOCK_LEN + MEMBER_LEN..SUPERBLOCK_LEN + MEMBER_LEN + EXTENT_LEN];
        assert_eq!(rd_u64(x, 0), 0, "logical_block");
        assert_eq!(rd_u64(x, 8), 2, "partition_block = 8192/4096");
        assert_eq!(rd_u64(x, 16), 1, "block_count");
    }

    #[tokio::test]
    async fn the_manifest_digest_covers_the_combination() {
        let a = PalletBuilder::new("p", 1)
            .member(spec("kernel", b"k"))
            .member(spec("initramfs", b"i"))
            .build()
            .await
            .unwrap();
        // Same members, one of them different content: a different manifest,
        // which is the property signing each image separately cannot give.
        let b = PalletBuilder::new("p", 1)
            .member(spec("kernel", b"k"))
            .member(spec("initramfs", b"i-other"))
            .build()
            .await
            .unwrap();
        assert_ne!(a.manifest_digest, b.manifest_digest);

        // And the same members in the other order is also a different manifest.
        let c = PalletBuilder::new("p", 1)
            .member(spec("initramfs", b"i"))
            .member(spec("kernel", b"k"))
            .build()
            .await
            .unwrap();
        assert_ne!(a.manifest_digest, c.manifest_digest);
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
        assert_eq!(raw & 1, 1, "UEFI required-partition bit");
        assert_eq!(Attributes::from_u64(raw), a);
    }

    #[test]
    fn a_pallet_out_of_tries_is_not_a_candidate_unless_it_was_confirmed_good() {
        let mut a = Attributes { priority: 5, tries_left: 0, successful: false, ..Default::default() };
        assert!(!a.is_candidate());
        a.successful = true;
        assert!(a.is_candidate());
        a.priority = 0;
        assert!(!a.is_candidate(), "priority 0 means never boot");
    }

    #[tokio::test]
    async fn a_newer_format_version_is_refused_rather_than_guessed_at() {
        let mut built = PalletBuilder::new("p", 1)
            .member(spec("m", b"x"))
            .build()
            .await
            .unwrap();
        wr_u32(&mut built.header, 8, 2);
        let crc = superblock_crc(&built.header);
        wr_u32(&mut built.header, 132, crc);
        match Superblock::parse(&built.header) {
            Err(PalletError::UnsupportedVersion(2)) => {}
            other => panic!("expected UnsupportedVersion(2), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_flipped_bit_in_the_tables_fails_the_crc() {
        let built = PalletBuilder::new("p", 1)
            .member(spec("m", b"x"))
            .build()
            .await
            .unwrap();
        let mut buf = built.header.clone();
        buf.truncate(SUPERBLOCK_LEN + MEMBER_LEN + EXTENT_LEN);
        assert!(Pallet::parse(buf.clone()).is_ok());
        buf[SUPERBLOCK_LEN] ^= 0x01;
        assert!(matches!(Pallet::parse(buf), Err(PalletError::BadMemberCrc)));
    }

    #[tokio::test]
    async fn a_name_too_long_for_the_field_is_an_error_not_a_truncation() {
        let long = "x".repeat(NAME_LEN + 1);
        let err = PalletBuilder::new(long, 1).build().await.unwrap_err();
        assert!(matches!(err, PalletError::TooLong { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_empty_member_carries_no_extent() {
        let built = PalletBuilder::new("p", 1)
            .member(spec("empty", b""))
            .member(spec("full", b"data"))
            .build()
            .await
            .unwrap();
        let mut buf = built.header.clone();
        buf.truncate(SUPERBLOCK_LEN + 2 * MEMBER_LEN + EXTENT_LEN);
        let p = Pallet::parse(buf).unwrap();
        assert_eq!(p.sb.extent_count, 1);
        assert_eq!(p.find("empty").unwrap().extent_count, 0);
        assert_eq!(p.find("full").unwrap().extent_count, 1);
    }
}
