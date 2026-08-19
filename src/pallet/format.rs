//! Writing pallets, and reading them over async I/O.
//!
//! The **decode** side of the format is not here. It lives in
//! [`stormblock_pallet_format`] — `no_std`, no allocation, no I/O, no write
//! path — because firmware reads the same format this writes, and two
//! hand-maintained readers of one on-disk layout drift into a node that does
//! not boot. This module is the half that cannot go in a firmware crate: the
//! async I/O, the content abstraction, and the layout arithmetic that emits
//! bytes.
//!
//! Emission has exactly one implementation — this one — and the offsets it
//! writes at come from [`stormblock_pallet_format::layout`], so there is one
//! definition of where every field sits and nothing to keep in sync.
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
//! inside any of them to rewrite. The specification is `docs/pallets.md`.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use stormblock_pallet_format as fmt1;
use stormblock_pallet_format::layout;

use super::{MemberContent, PalletError, PartitionView, Result};

// The format's vocabulary is the shared crate's. Re-exported so the rest of the
// engine keeps one import path, and so nothing here can define a second
// version of any of it.
pub use fmt1::{
    crc32, crc32_continue, superblock_crc, Attributes, Extent, Mapping, Member, MemberKind,
    PalletKind, Superblock, EXTENT_LEN, FLAG_DIGEST, FLAG_READ_ONLY, FLAG_SEALED, MAGIC,
    MEMBER_LEN, NAME_LEN, PALLET_TYPE_GUID, ROLE_LEN, SUPERBLOCK_LEN, VERSION,
    VERSION_LABEL_LEN,
};

/// Chunk size for digesting and copying member content.
const IO_CHUNK: usize = 1024 * 1024;

/// A member's digest as hex — for an API response or a CLI line, neither of
/// which a `no_std` crate should know about.
pub trait MemberExt {
    fn digest_hex(&self) -> String;
}

impl MemberExt for Member {
    fn digest_hex(&self) -> String {
        hex::encode(self.digest)
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

/// Parse a kind from a spec, a query string or a CLI flag.
pub fn parse_pallet_kind(s: &str) -> PalletKind {
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

/// Parse a member kind the same way.
pub fn parse_member_kind(s: &str) -> MemberKind {
    match s.to_ascii_lowercase().as_str() {
        "raw" => MemberKind::Raw,
        "kernel" => MemberKind::Kernel,
        "initramfs" => MemberKind::Initramfs,
        "bootconfig" | "boot_config" | "cmdline" => MemberKind::BootConfig,
        "rootimage" | "root_image" | "root" => MemberKind::RootImage,
        "container" => MemberKind::Container,
        _ => MemberKind::Raw,
    }
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

fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn write_fixed(dst: &mut [u8], s: &str, field: &'static str) -> Result<()> {
    let b = s.as_bytes();
    if b.len() > dst.len() {
        return Err(PalletError::TooLong { field, max: dst.len(), got: b.len() });
    }
    dst[..b.len()].copy_from_slice(b);
    Ok(())
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

    /// Block size the extent table is expressed in. It must be no smaller than
    /// the sector size the device addresses, or an extent would name something
    /// the device cannot reach.
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

        // Extents are counted in the pallet's own block size — which is the
        // media's, so a pre-kernel reader working in sectors needs no scaling.
        // Content is nonetheless *placed* on 4 KiB boundaries, because a 512
        // byte block size should not cost every member its alignment.
        let content_align = bs.max(SUPERBLOCK_LEN as u64);
        let tables_end = SUPERBLOCK_LEN + n * MEMBER_LEN + extent_count * EXTENT_LEN;
        let member_data_offset = align_up(tables_end as u64, content_align);

        let mut members_tbl = vec![0u8; n * MEMBER_LEN];
        let mut extents_tbl = vec![0u8; extent_count * EXTENT_LEN];
        let mut placements = Vec::with_capacity(n);

        let mut cursor = member_data_offset;
        let mut extent_idx = 0usize;
        for (i, m) in self.members.iter().enumerate() {
            let len = m.content.byte_len();
            let digest = digest_content(m.content.as_ref()).await?;

            let (first, count) = if len > 0 {
                cursor = align_up(cursor, content_align);
                let blocks = len.div_ceil(bs);
                let e = &mut extents_tbl[extent_idx * EXTENT_LEN..(extent_idx + 1) * EXTENT_LEN];
                wr_u64(e, layout::extent::LOGICAL_BLOCK, 0);
                wr_u64(e, layout::extent::PARTITION_BLOCK, cursor / bs);
                wr_u64(e, layout::extent::BLOCK_COUNT, blocks);
                wr_u32(e, layout::extent::FLAGS, 0);
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
            write_fixed(
                &mut e[layout::member::NAME..layout::member::NAME + NAME_LEN],
                &m.name,
                "member name",
            )?;
            write_fixed(
                &mut e[layout::member::ROLE..layout::member::ROLE + ROLE_LEN],
                &m.role,
                "member role",
            )?;
            wr_u32(e, layout::member::KIND, m.kind.to_u32());
            wr_u32(e, layout::member::FLAGS, m.flags);
            wr_u64(e, layout::member::BYTE_LEN, len);
            wr_u32(e, layout::member::EXTENT_FIRST, first);
            wr_u32(e, layout::member::EXTENT_COUNT, count);
            e[layout::member::DIGEST..layout::member::DIGEST + 32].copy_from_slice(&digest);
        }

        // The manifest digest covers the member and extent tables together —
        // which is what makes one signature cover the *combination*. A signed
        // kernel paired with a different signed initramfs is a different
        // manifest and will not verify.
        let mut h = Sha256::new();
        h.update(&members_tbl);
        h.update(&extents_tbl);
        let manifest_digest: [u8; 32] = h.finalize().into();

        use layout::sb as o;
        let mut header = vec![0u8; member_data_offset as usize];
        header[o::MAGIC..o::MAGIC + 8].copy_from_slice(&MAGIC);
        wr_u32(&mut header, o::VERSION, VERSION);
        wr_u32(&mut header, o::SUPERBLOCK_LEN, SUPERBLOCK_LEN as u32);
        wr_u32(&mut header, o::BLOCK_SIZE, self.block_size);
        wr_u32(&mut header, o::MEMBER_SIZE, MEMBER_LEN as u32);
        wr_u32(&mut header, o::MEMBER_COUNT, n as u32);
        wr_u32(&mut header, o::EXTENT_SIZE, EXTENT_LEN as u32);
        wr_u32(&mut header, o::EXTENT_COUNT, extent_count as u32);
        wr_u64(&mut header, o::PALLET_VERSION, self.pallet_version);
        wr_u64(&mut header, o::MEMBER_DATA_OFFSET, member_data_offset);
        write_fixed(&mut header[o::NAME..o::NAME + NAME_LEN], &self.name, "pallet name")?;
        header[o::MANIFEST_DIGEST..o::MANIFEST_DIGEST + 32].copy_from_slice(&manifest_digest);
        wr_u32(&mut header, o::MEMBERS_CRC, crc32(&members_tbl));
        wr_u32(&mut header, o::EXTENTS_CRC, crc32(&extents_tbl));
        wr_u64(&mut header, o::FLAGS, self.attributes.to_superblock_flags());
        wr_u32(&mut header, o::KIND, self.kind.to_u32());
        write_fixed(
            &mut header[o::VERSION_LABEL..o::VERSION_LABEL + VERSION_LABEL_LEN],
            &self.version_label,
            "version label",
        )?;
        header[SUPERBLOCK_LEN..SUPERBLOCK_LEN + members_tbl.len()].copy_from_slice(&members_tbl);
        let xo = SUPERBLOCK_LEN + members_tbl.len();
        header[xo..xo + extents_tbl.len()].copy_from_slice(&extents_tbl);
        let crc = superblock_crc(&header);
        wr_u32(&mut header, o::SUPERBLOCK_CRC, crc);

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
        // The floor is the smallest unit the media can actually address, not
        // the largest it would prefer: a file device asks for 4 KiB I/O but
        // addresses 512-byte sectors, and an image has to be readable by
        // firmware that assumes exactly that.
        let min_bs = super::gpt::default_lba_size(view.device());
        if self.block_size < min_bs {
            return Err(PalletError::BadGeometry(format!(
                "pallet block size {} is smaller than the {min_bs}-byte sectors this device \
                 addresses",
                self.block_size,
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

/// A pallet read back from a partition, over async I/O.
///
/// Owns the superblock and both tables; every decode below is the shared
/// `no_std` reader working on those bytes, so what the engine sees and what
/// firmware sees cannot disagree.
pub struct Pallet {
    pub sb: Superblock,
    buf: Vec<u8>,
}

impl Pallet {
    /// `buf` must hold superblock + member table + extent table, read from the
    /// partition start. Checks magic, version, geometry and all three CRCs.
    pub fn parse(buf: Vec<u8>) -> Result<Pallet> {
        let sb = fmt1::Pallet::parse(&buf)?.sb;
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

    /// The shared reader over the bytes this owns. The tables were checked in
    /// `parse`, so re-validating three CRCs on every lookup would make reading
    /// a member quadratic in the size of its own tables.
    fn view(&self) -> fmt1::Pallet<'_> {
        fmt1::Pallet::attach(self.sb, &self.buf)
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
        Ok(self.view().member(i)?)
    }

    pub fn members(&self) -> Vec<Member> {
        let v = self.view();
        (0..self.member_count()).filter_map(|i| v.member(i).ok()).collect()
    }

    pub fn extent(&self, i: usize) -> Result<Extent> {
        Ok(self.view().extent(i)?)
    }

    pub fn find(&self, name: &str) -> Result<Member> {
        self.view()
            .find(name)
            .map_err(|_| PalletError::NotFound(format!("member '{name}'")))
    }

    pub fn find_role(&self, role: &str) -> Result<Member> {
        self.view()
            .find_role(role)
            .map_err(|_| PalletError::NotFound(format!("member with role '{role}'")))
    }

    /// The remap: the longest contiguous run from `offset`.
    pub fn map(&self, m: &Member, offset: u64) -> Result<Mapping> {
        Ok(self.view().map(m, offset)?)
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
        let pallet = self.view();
        let mut done = 0usize;
        while done < buf.len() {
            let map = pallet.map(m, offset + done as u64)?;
            let take = (map.run_len as usize).min(buf.len() - done);
            let at = map.partition_block * bs + map.offset_in_block;
            view.read_at(at, &mut buf[done..done + take]).await?;
            done += take;
        }
        Ok(())
    }

    /// Recompute the manifest digest over the member + extent tables.
    pub fn manifest_digest(&self) -> [u8; 32] {
        self.view().manifest_digest()
    }

    pub fn verify_manifest(&self) -> Result<()> {
        Ok(self.view().verify_manifest()?)
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
            return Err(PalletError::NoDigest { member: m.name().to_string() });
        }
        if self.digest_of(m, view).await? != m.digest {
            return Err(PalletError::DigestMismatch { member: m.name().to_string() });
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
    use stormblock_pallet_format::{rd_u32, rd_u64, str_field};

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
        assert_eq!(str_field(&h[52..52 + NAME_LEN]), "stormcos-boot");
        assert_eq!(&h[92..124], &built.manifest_digest);
        assert_eq!(rd_u32(h, 124), crc32(&h[SUPERBLOCK_LEN..SUPERBLOCK_LEN + MEMBER_LEN]));
        assert_eq!(rd_u32(h, 132), superblock_crc(h), "superblock_crc skips itself");
        // Sealed and read-only are mirrored for a consumer that never sees the GPT.
        assert_eq!(rd_u64(h, 136), (1u64 << 57) | (1u64 << 58));
        // Extension fields, defined so that zero reads back as "unspecified".
        assert_eq!(rd_u32(h, layout::sb::KIND), PalletKind::Boot.to_u32());
        assert_eq!(
            str_field(&h[layout::sb::VERSION_LABEL..layout::sb::VERSION_LABEL + VERSION_LABEL_LEN]),
            "6.12.0"
        );
        assert!(h[180..SUPERBLOCK_LEN].iter().all(|&b| b == 0), "reserved stays zero");

        // Member table at 4096, fields where the spec puts them.
        let m = &h[SUPERBLOCK_LEN..SUPERBLOCK_LEN + MEMBER_LEN];
        assert_eq!(str_field(&m[0..NAME_LEN]), "kernel");
        assert_eq!(str_field(&m[40..40 + ROLE_LEN]), "container");
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
            Err(stormblock_pallet_format::Error::UnsupportedVersion(2)) => {}
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
