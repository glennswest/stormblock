//! VMDK reader — VMware's sparse extents, and the descriptor that names them.
//!
//! Three shapes matter:
//! - **streamOptimized**: what an OVA/OVF export carries. One file, grains
//!   deflate-compressed behind markers, grain directory at the *end* (the
//!   header says `gdOffset = -1` and a footer near EOF has the real one).
//! - **monolithicSparse**: Workstation/Fusion's default. Same grain tables,
//!   uncompressed, directory at the front.
//! - **monolithicFlat / twoGbMaxExtentFlat**: a text descriptor listing
//!   `RW <sectors> FLAT "x-flat.vmdk" 0` extents that are raw files beside
//!   it (or `SPARSE "x-s001.vmdk"` pieces).
//!
//! `read_at` answers any guest byte range with zeros where the extent has
//! none; `grain_state` says what is worth copying. Grains are 64 KiB by
//! default (`grainSize` sectors), which is also the copy unit.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAGIC: &[u8; 4] = b"KDMV";
const SECTOR: u64 = 512;
const FLAG_COMPRESSED: u32 = 1 << 16;
const FLAG_MARKERS: u32 = 1 << 17;
const GTE_ZERO: u32 = 1;

#[derive(Debug)]
pub enum VmdkError {
    Io(std::io::Error),
    NotVmdk,
    Unsupported(String),
    Corrupt(String),
}

impl std::fmt::Display for VmdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmdkError::Io(e) => write!(f, "io: {e}"),
            VmdkError::NotVmdk => f.write_str("not a vmdk"),
            VmdkError::Unsupported(m) => write!(f, "unsupported vmdk: {m}"),
            VmdkError::Corrupt(m) => write!(f, "corrupt vmdk: {m}"),
        }
    }
}

impl std::error::Error for VmdkError {}

impl From<std::io::Error> for VmdkError {
    fn from(e: std::io::Error) -> Self {
        VmdkError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrainState {
    Unallocated,
    Zero,
    Data,
}

/// A byte window over a file: the whole file, or a member inside a tar.
pub struct Window {
    file: tokio::fs::File,
    base: u64,
    len: u64,
}

impl Window {
    pub async fn whole(path: &Path) -> std::io::Result<Window> {
        let file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();
        Ok(Window { file, base: 0, len })
    }

    pub fn inside(file: tokio::fs::File, base: u64, len: u64) -> Window {
        Window { file, base, len }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read at `off` within the window; short at the window's end.
    pub async fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if off >= self.len {
            return Ok(0);
        }
        let take = ((self.len - off) as usize).min(buf.len());
        self.file.seek(std::io::SeekFrom::Start(self.base + off)).await?;
        let mut got = 0usize;
        while got < take {
            let n = self.file.read(&mut buf[got..take]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }

    async fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), VmdkError> {
        let n = self.read_at(off, buf).await?;
        if n != buf.len() {
            return Err(VmdkError::Corrupt(format!("short read at {off}: {n} of {}", buf.len())));
        }
        Ok(())
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// One sparse extent (a `KDMV` file).
pub struct SparseExtent {
    win: Window,
    pub capacity_sectors: u64,
    pub grain_sectors: u64,
    compressed: bool,
    gtes_per_gt: u64,
    gd: Vec<u32>,
    gt_cache: HashMap<u32, Vec<u32>>,
}

impl SparseExtent {
    pub async fn open(mut win: Window) -> Result<Self, VmdkError> {
        let mut hdr = vec![0u8; 512];
        let n = win.read_at(0, &mut hdr).await?;
        if n < 512 || &hdr[..4] != MAGIC {
            return Err(VmdkError::NotVmdk);
        }
        let mut header = hdr.clone();
        let flags = le32(&header, 8);
        let mut gd_offset = le64(&header, 56);
        if gd_offset == u64::MAX {
            // streamOptimized: the footer, a copy of the header with the real
            // directory offset, sits two sectors before the end.
            let len = win.len();
            if len < 3 * SECTOR {
                return Err(VmdkError::Corrupt("no room for a footer".into()));
            }
            let mut footer = vec![0u8; 512];
            win.read_exact_at(len - 2 * SECTOR, &mut footer).await?;
            if &footer[..4] != MAGIC {
                return Err(VmdkError::Corrupt("footer missing".into()));
            }
            header = footer;
            gd_offset = le64(&header, 56);
        }
        let version = le32(&header, 4);
        if !(1..=3).contains(&version) {
            return Err(VmdkError::Unsupported(format!("version {version}")));
        }
        let capacity_sectors = le64(&header, 12);
        let grain_sectors = le64(&header, 20);
        let gtes_per_gt = le32(&header, 44) as u64;
        let compressed = flags & FLAG_COMPRESSED != 0;
        let algo = u16::from_le_bytes([header[77], header[78]]);
        if compressed && algo != 1 {
            return Err(VmdkError::Unsupported(format!("compression algorithm {algo}")));
        }
        if grain_sectors == 0 || gtes_per_gt == 0 {
            return Err(VmdkError::Corrupt("zero grain or table size".into()));
        }
        let grains = capacity_sectors.div_ceil(grain_sectors);
        let gts = grains.div_ceil(gtes_per_gt);
        let mut gd = vec![0u8; (gts * 4) as usize];
        win.read_exact_at(gd_offset * SECTOR, &mut gd).await?;
        let gd: Vec<u32> = gd.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let _ = flags & FLAG_MARKERS;
        Ok(SparseExtent { win, capacity_sectors, grain_sectors, compressed, gtes_per_gt, gd, gt_cache: HashMap::new() })
    }

    pub fn grain_size(&self) -> u64 {
        self.grain_sectors * SECTOR
    }

    pub fn virtual_size(&self) -> u64 {
        self.capacity_sectors * SECTOR
    }

    pub fn grain_count(&self) -> u64 {
        self.capacity_sectors.div_ceil(self.grain_sectors)
    }

    async fn gte(&mut self, grain: u64) -> Result<u32, VmdkError> {
        let gd_idx = (grain / self.gtes_per_gt) as usize;
        let gt_idx = (grain % self.gtes_per_gt) as usize;
        let Some(&gt_off) = self.gd.get(gd_idx) else { return Ok(0) };
        if gt_off == 0 {
            return Ok(0);
        }
        if !self.gt_cache.contains_key(&gt_off) {
            let mut raw = vec![0u8; (self.gtes_per_gt * 4) as usize];
            self.win.read_exact_at(gt_off as u64 * SECTOR, &mut raw).await?;
            let t: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
            if self.gt_cache.len() > 64 {
                self.gt_cache.clear();
            }
            self.gt_cache.insert(gt_off, t);
        }
        Ok(self.gt_cache[&gt_off][gt_idx])
    }

    pub async fn grain_state(&mut self, grain: u64) -> Result<GrainState, VmdkError> {
        Ok(match self.gte(grain).await? {
            0 => GrainState::Unallocated,
            GTE_ZERO => GrainState::Zero,
            _ => GrainState::Data,
        })
    }

    pub async fn read_grain(&mut self, grain: u64) -> Result<Vec<u8>, VmdkError> {
        let size = self.grain_size() as usize;
        let mut out = vec![0u8; size];
        let gte = self.gte(grain).await?;
        if gte == 0 || gte == GTE_ZERO {
            return Ok(out);
        }
        let off = gte as u64 * SECTOR;
        if !self.compressed {
            let n = self.win.read_at(off, &mut out).await?;
            let _ = n; // short at EOF reads as zeros
            return Ok(out);
        }
        // Compressed grain: marker {lba: u64, size: u32} then a zlib stream.
        let mut marker = [0u8; 12];
        self.win.read_exact_at(off, &mut marker).await?;
        let csize = le32(&marker, 8) as usize;
        if csize == 0 || csize > 64 * 1024 * 1024 {
            return Err(VmdkError::Corrupt(format!("grain {grain}: compressed size {csize}")));
        }
        let mut comp = vec![0u8; csize];
        self.win.read_exact_at(off + 12, &mut comp).await?;
        let mut dec = flate2::read::ZlibDecoder::new(&comp[..]);
        let mut filled = 0usize;
        loop {
            match dec.read(&mut out[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(VmdkError::Corrupt(format!("grain {grain}: {e}"))),
            }
            if filled == size {
                break;
            }
        }
        Ok(out)
    }
}

/// A whole VMDK: one sparse extent, or a descriptor's list of extents.
pub struct Vmdk {
    extents: Vec<Extent>,
    virtual_size: u64,
    /// Copy unit: the largest grain among the extents, or 64 KiB for flat.
    pub chunk: u64,
}

enum Extent {
    Sparse { start: u64, len: u64, ext: SparseExtent },
    Flat { start: u64, len: u64, win: Window, file_off: u64 },
}

impl Vmdk {
    /// Open a `.vmdk` (sparse or descriptor) at `path`.
    pub async fn open(path: &Path) -> Result<Self, VmdkError> {
        let mut win = Window::whole(path).await?;
        let mut head = vec![0u8; 512];
        let n = win.read_at(0, &mut head).await?;
        if n >= 4 && &head[..4] == MAGIC {
            let ext = SparseExtent::open(win).await?;
            let len = ext.virtual_size();
            let chunk = ext.grain_size();
            // A sparse extent may embed its own descriptor; it is not needed.
            return Ok(Vmdk { virtual_size: len, chunk, extents: vec![Extent::Sparse { start: 0, len, ext }] });
        }
        if !head.starts_with(b"# Disk DescriptorFile") {
            return Err(VmdkError::NotVmdk);
        }
        let text = tokio::fs::read_to_string(path).await?;
        Self::from_descriptor(&text, path.parent().unwrap_or(Path::new("."))).await
    }

    /// Open a sparse extent from a window (a member of an OVA).
    pub async fn from_window(win: Window) -> Result<Self, VmdkError> {
        let ext = SparseExtent::open(win).await?;
        let len = ext.virtual_size();
        let chunk = ext.grain_size();
        Ok(Vmdk { virtual_size: len, chunk, extents: vec![Extent::Sparse { start: 0, len, ext }] })
    }

    async fn from_descriptor(text: &str, dir: &Path) -> Result<Self, VmdkError> {
        let mut extents = Vec::new();
        let mut start = 0u64;
        let mut chunk = 64 * 1024u64;
        for line in text.lines() {
            let line = line.trim();
            if !(line.starts_with("RW ") || line.starts_with("RDONLY ") || line.starts_with("NOACCESS ")) {
                continue;
            }
            // RW <sectors> <TYPE> "<file>" [offset]
            let mut parts = line.splitn(4, ' ');
            let _access = parts.next();
            let sectors: u64 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| VmdkError::Corrupt(format!("extent line: {line}")))?;
            let kind = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("");
            let file = rest.split('"').nth(1).ok_or_else(|| VmdkError::Corrupt(format!("extent line: {line}")))?;
            let file_off: u64 = rest
                .split('"')
                .nth(2)
                .and_then(|s| s.trim().split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let len = sectors * SECTOR;
            let p: PathBuf = if Path::new(file).is_absolute() { PathBuf::from(file) } else { dir.join(file) };
            match kind {
                "FLAT" | "VMFS" => {
                    let win = Window::whole(&p).await.map_err(|e| {
                        VmdkError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))
                    })?;
                    extents.push(Extent::Flat { start, len, win, file_off: file_off * SECTOR });
                }
                "SPARSE" | "VMFSSPARSE" => {
                    let win = Window::whole(&p).await.map_err(|e| {
                        VmdkError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))
                    })?;
                    let ext = SparseExtent::open(win).await?;
                    chunk = chunk.max(ext.grain_size());
                    extents.push(Extent::Sparse { start, len, ext });
                }
                "ZERO" => {}
                other => return Err(VmdkError::Unsupported(format!("extent type {other}"))),
            }
            start += len;
        }
        if extents.is_empty() {
            return Err(VmdkError::Corrupt("descriptor names no extents".into()));
        }
        Ok(Vmdk { extents, virtual_size: start, chunk })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    /// Whether the `chunk`-sized piece at `offset` carries data worth
    /// copying (a flat extent always does; the copy still skips zeros).
    pub async fn chunk_state(&mut self, offset: u64) -> Result<GrainState, VmdkError> {
        for e in self.extents.iter_mut() {
            match e {
                Extent::Sparse { start, len, ext } if offset >= *start && offset < *start + *len => {
                    let g = (offset - *start) / ext.grain_size();
                    return ext.grain_state(g).await;
                }
                Extent::Flat { start, len, .. } if offset >= *start && offset < *start + *len => {
                    return Ok(GrainState::Data);
                }
                _ => {}
            }
        }
        Ok(GrainState::Unallocated)
    }

    /// Any guest byte range, zeros where nothing was written.
    pub async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), VmdkError> {
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let mut served = false;
            for e in self.extents.iter_mut() {
                match e {
                    Extent::Sparse { start, len, ext } if pos >= *start && pos < *start + *len => {
                        let rel = pos - *start;
                        let g = rel / ext.grain_size();
                        let within = (rel % ext.grain_size()) as usize;
                        let take = ((ext.grain_size() as usize) - within).min(buf.len() - done).min((*start + *len - pos) as usize);
                        match ext.grain_state(g).await? {
                            GrainState::Data => {
                                let data = ext.read_grain(g).await?;
                                buf[done..done + take].copy_from_slice(&data[within..within + take]);
                            }
                            _ => buf[done..done + take].fill(0),
                        }
                        pos += take as u64;
                        done += take;
                        served = true;
                        break;
                    }
                    Extent::Flat { start, len, win, file_off } if pos >= *start && pos < *start + *len => {
                        let rel = pos - *start;
                        let take = (buf.len() - done).min((*len - rel) as usize);
                        let n = win.read_at(*file_off + rel, &mut buf[done..done + take]).await?;
                        buf[done + n..done + take].fill(0);
                        pos += take as u64;
                        done += take;
                        served = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !served {
                buf[done..].fill(0);
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testimg {
    //! A tiny sparse-extent writer (monolithicSparse and streamOptimized
    //! shapes), for tests only.
    use super::*;
    use std::io::Write as _;

    pub enum G {
        Data(Vec<u8>),
        Zero,
        Hole,
    }

    /// `grain_sectors` sectors per grain; one grain table covers everything.
    pub fn build(grain_sectors: u64, grains: &[G], stream_optimized: bool) -> Vec<u8> {
        let gs = (grain_sectors * SECTOR) as usize;
        let capacity = grains.len() as u64 * grain_sectors;
        let gtes = 512u64;
        // Layout in sectors: header 0, descriptor 1..20, GD at 21, GT at 22..
        // (512 entries * 4 B = 4 sectors), data from sector 26 (grain aligned
        // is not required by the reader).
        let mut out = vec![0u8; 26 * 512];
        let mut gt = vec![0u32; gtes as usize];
        for (i, g) in grains.iter().enumerate() {
            match g {
                G::Hole => {}
                G::Zero => gt[i] = GTE_ZERO,
                G::Data(d) => {
                    let mut padded = d.clone();
                    padded.resize(gs, 0);
                    let sector = (out.len() / 512) as u32;
                    gt[i] = sector;
                    if stream_optimized {
                        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                        enc.write_all(&padded).unwrap();
                        let comp = enc.finish().unwrap();
                        out.extend_from_slice(&((i as u64) * grain_sectors).to_le_bytes());
                        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
                        out.extend_from_slice(&comp);
                        let pad = (512 - out.len() % 512) % 512;
                        out.extend(std::iter::repeat_n(0u8, pad));
                    } else {
                        out.extend_from_slice(&padded);
                    }
                }
            }
        }
        let gd_sector: u64 = if stream_optimized {
            // Directory at the end, footer after it, EOS marker last.
            let s = (out.len() / 512) as u64;
            out.extend_from_slice(&(22u32).to_le_bytes());
            out.resize(out.len() + 508, 0);
            s
        } else {
            21
        };
        let mut hdr = vec![0u8; 512];
        hdr[..4].copy_from_slice(MAGIC);
        hdr[4..8].copy_from_slice(&1u32.to_le_bytes());
        let flags: u32 = if stream_optimized { FLAG_COMPRESSED | FLAG_MARKERS | 3 } else { 3 };
        hdr[8..12].copy_from_slice(&flags.to_le_bytes());
        hdr[12..20].copy_from_slice(&capacity.to_le_bytes());
        hdr[20..28].copy_from_slice(&grain_sectors.to_le_bytes());
        hdr[28..36].copy_from_slice(&1u64.to_le_bytes()); // descriptorOffset
        hdr[36..44].copy_from_slice(&20u64.to_le_bytes()); // descriptorSize
        hdr[44..48].copy_from_slice(&(gtes as u32).to_le_bytes());
        hdr[48..56].copy_from_slice(&0u64.to_le_bytes()); // rgdOffset
        hdr[56..64].copy_from_slice(&gd_sector.to_le_bytes());
        hdr[64..72].copy_from_slice(&26u64.to_le_bytes()); // overHead
        hdr[77] = 1; // deflate
        // GT at sector 22
        for (i, e) in gt.iter().enumerate() {
            out[22 * 512 + i * 4..22 * 512 + i * 4 + 4].copy_from_slice(&e.to_le_bytes());
        }
        if !stream_optimized {
            out[21 * 512..21 * 512 + 4].copy_from_slice(&22u32.to_le_bytes());
            out[..512].copy_from_slice(&hdr);
        } else {
            let mut front = hdr.clone();
            front[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
            out[..512].copy_from_slice(&front);
            // footer marker (1 sector), footer, EOS marker
            out.resize(out.len() + 512, 0);
            out.extend_from_slice(&hdr);
            out.resize(out.len() + 512, 0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testimg::G;

    async fn tmp(bytes: &[u8], ext: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("stormblock-vmdk-{}.{ext}", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&p, bytes).await.unwrap();
        p
    }

    fn pat(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i as u32 * 31 + seed as u32) % 251) as u8).collect()
    }

    #[tokio::test]
    async fn monolithic_sparse_and_stream_optimized_read_the_same() {
        let gs = 8 * 512;
        let (a, b) = (pat(1, gs), pat(9, gs));
        for stream in [false, true] {
            let img = testimg::build(8, &[G::Data(a.clone()), G::Zero, G::Hole, G::Data(b.clone())], stream);
            let p = tmp(&img, "vmdk").await;
            let mut v = Vmdk::open(&p).await.unwrap();
            assert_eq!(v.virtual_size(), 4 * gs as u64);
            assert_eq!(v.chunk, gs as u64);
            assert_eq!(v.chunk_state(0).await.unwrap(), GrainState::Data);
            assert_eq!(v.chunk_state(gs as u64).await.unwrap(), GrainState::Zero);
            assert_eq!(v.chunk_state(2 * gs as u64).await.unwrap(), GrainState::Unallocated);
            let mut buf = vec![0xAAu8; gs];
            v.read_at(0, &mut buf).await.unwrap();
            assert_eq!(buf, a, "stream={stream}");
            v.read_at(3 * gs as u64, &mut buf).await.unwrap();
            assert_eq!(buf, b, "stream={stream}");
            let mut span = vec![0xAAu8; 2 * gs];
            v.read_at(gs as u64 - 16, &mut span).await.unwrap();
            assert_eq!(&span[..16], &a[gs - 16..]);
            assert!(span[16..].iter().all(|&x| x == 0));
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn a_descriptor_with_a_flat_extent_is_the_raw_file() {
        let flat = pat(5, 4 * 512);
        let dir = std::env::temp_dir().join(format!("stormblock-vmdk-flat-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disk-flat.vmdk"), &flat).unwrap();
        let desc = "# Disk DescriptorFile\nversion=1\ncreateType=\"monolithicFlat\"\nRW 4 FLAT \"disk-flat.vmdk\" 0\n";
        std::fs::write(dir.join("disk.vmdk"), desc).unwrap();
        let mut v = Vmdk::open(&dir.join("disk.vmdk")).await.unwrap();
        assert_eq!(v.virtual_size(), 4 * 512);
        let mut buf = vec![0u8; 4 * 512];
        v.read_at(0, &mut buf).await.unwrap();
        assert_eq!(buf, flat);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
