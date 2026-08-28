//! qcow2 reader — enough to lay a cloud image into a volume.
//!
//! Standalone images only: v2 and v3, zero clusters, zlib-compressed
//! clusters (raw deflate, as the spec says), no backing file, no external
//! data file, no extended L2, no encryption, no zstd. That covers what the
//! distributions publish; a chain or a zstd image is refused with a message
//! that names the feature, not decoded wrong.
//!
//! Read model: `read_at` answers any byte range, with zeros where the image
//! has none, so callers that probe (an ext4 superblock, a partition table)
//! do not care about clusters. `cluster_state` says which clusters carry
//! data, so the copy into a volume writes only those.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAGIC: &[u8; 4] = b"QFI\xfb";
const L1E_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const L2E_COMPRESSED: u64 = 1 << 62;
const L2E_ZERO: u64 = 1;

/// Why an image could not be opened as a qcow2.
#[derive(Debug)]
pub enum Qcow2Error {
    Io(std::io::Error),
    NotQcow2,
    Unsupported(String),
    Corrupt(String),
}

impl std::fmt::Display for Qcow2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Qcow2Error::Io(e) => write!(f, "io: {e}"),
            Qcow2Error::NotQcow2 => f.write_str("not a qcow2 image"),
            Qcow2Error::Unsupported(m) => write!(f, "unsupported qcow2 feature: {m}"),
            Qcow2Error::Corrupt(m) => write!(f, "corrupt qcow2: {m}"),
        }
    }
}

impl std::error::Error for Qcow2Error {}

impl From<std::io::Error> for Qcow2Error {
    fn from(e: std::io::Error) -> Self {
        Qcow2Error::Io(e)
    }
}

/// What a guest cluster holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterState {
    /// Never written: reads as zeros, not worth copying.
    Unallocated,
    /// Explicitly zero: same.
    Zero,
    /// Data in the file, possibly compressed.
    Data,
}

pub struct Qcow2 {
    file: tokio::fs::File,
    pub version: u32,
    pub cluster_bits: u32,
    pub cluster_size: u64,
    pub virtual_size: u64,
    l1: Vec<u64>,
    l2_entries: u64,
    l2_cache: HashMap<u64, Vec<u64>>,
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

impl Qcow2 {
    pub async fn open(path: &Path) -> Result<Self, Qcow2Error> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hdr = vec![0u8; 112];
        let n = file.read(&mut hdr).await?;
        if n < 72 || &hdr[..4] != MAGIC {
            return Err(Qcow2Error::NotQcow2);
        }
        let version = be32(&hdr, 4);
        if version != 2 && version != 3 {
            return Err(Qcow2Error::Unsupported(format!("version {version}")));
        }
        let backing_file_size = be32(&hdr, 16);
        if backing_file_size != 0 {
            return Err(Qcow2Error::Unsupported(
                "backing file (flatten it first: qemu-img convert -O qcow2)".into(),
            ));
        }
        let cluster_bits = be32(&hdr, 20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(Qcow2Error::Corrupt(format!("cluster_bits {cluster_bits}")));
        }
        let virtual_size = be64(&hdr, 24);
        if be32(&hdr, 32) != 0 {
            return Err(Qcow2Error::Unsupported("encryption".into()));
        }
        let l1_size = be32(&hdr, 36) as usize;
        let l1_offset = be64(&hdr, 40);
        if version == 3 {
            let incompatible = be64(&hdr, 72);
            if incompatible & (1 << 1) != 0 {
                return Err(Qcow2Error::Corrupt("image marked corrupt".into()));
            }
            if incompatible & (1 << 2) != 0 {
                return Err(Qcow2Error::Unsupported("external data file".into()));
            }
            if incompatible & (1 << 4) != 0 {
                return Err(Qcow2Error::Unsupported("extended L2 entries".into()));
            }
            let header_length = be32(&hdr, 100);
            if incompatible & (1 << 3) != 0 && header_length >= 105 && n >= 105 && hdr[104] != 0 {
                return Err(Qcow2Error::Unsupported("zstd compression (convert with qemu-img)".into()));
            }
            if incompatible & !((1 << 0) | (1 << 3)) != 0 {
                return Err(Qcow2Error::Unsupported(format!(
                    "incompatible feature bits {incompatible:#x}"
                )));
            }
        }
        let cluster_size = 1u64 << cluster_bits;
        let l2_entries = cluster_size / 8;
        let mut l1 = vec![0u8; l1_size * 8];
        if l1_size > 0 {
            file.seek(std::io::SeekFrom::Start(l1_offset)).await?;
            file.read_exact(&mut l1).await?;
        }
        let l1: Vec<u64> = l1.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
        Ok(Qcow2 { file, version, cluster_bits, cluster_size, virtual_size, l1, l2_entries, l2_cache: HashMap::new() })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn cluster_count(&self) -> u64 {
        self.virtual_size.div_ceil(self.cluster_size)
    }

    async fn l2_entry(&mut self, guest_cluster: u64) -> Result<u64, Qcow2Error> {
        let l1_idx = (guest_cluster / self.l2_entries) as usize;
        let l2_idx = (guest_cluster % self.l2_entries) as usize;
        let Some(&l1e) = self.l1.get(l1_idx) else { return Ok(0) };
        let l2_off = l1e & L1E_OFFSET_MASK;
        if l2_off == 0 {
            return Ok(0);
        }
        if !self.l2_cache.contains_key(&l2_off) {
            let mut raw = vec![0u8; self.cluster_size as usize];
            self.file.seek(std::io::SeekFrom::Start(l2_off)).await?;
            self.file.read_exact(&mut raw).await?;
            let table: Vec<u64> = raw.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
            if self.l2_cache.len() > 64 {
                self.l2_cache.clear();
            }
            self.l2_cache.insert(l2_off, table);
        }
        Ok(self.l2_cache[&l2_off][l2_idx])
    }

    /// What a guest cluster holds.
    pub async fn cluster_state(&mut self, guest_cluster: u64) -> Result<ClusterState, Qcow2Error> {
        let e = self.l2_entry(guest_cluster).await?;
        if e & L2E_COMPRESSED != 0 {
            return Ok(ClusterState::Data);
        }
        if e & L2E_ZERO != 0 {
            return Ok(ClusterState::Zero);
        }
        if e & L1E_OFFSET_MASK == 0 {
            return Ok(ClusterState::Unallocated);
        }
        Ok(ClusterState::Data)
    }

    /// One whole guest cluster, decompressed.
    pub async fn read_cluster(&mut self, guest_cluster: u64) -> Result<Vec<u8>, Qcow2Error> {
        let e = self.l2_entry(guest_cluster).await?;
        let mut out = vec![0u8; self.cluster_size as usize];
        if e & L2E_COMPRESSED != 0 {
            // Compressed descriptor: x = 62 - (cluster_bits - 8); low x bits
            // are the host offset, the next (cluster_bits - 8) bits are the
            // number of *additional* 512-byte sectors after the one holding
            // the offset. Raw deflate, per the spec.
            let x = 62 - (self.cluster_bits - 8);
            let host_off = e & ((1u64 << x) - 1);
            let extra = (e >> x) & ((1u64 << (self.cluster_bits - 8)) - 1);
            let start_sector = host_off & !511;
            let len = ((extra + 1) * 512) as usize + (host_off - start_sector) as usize;
            let mut comp = vec![0u8; len];
            self.file.seek(std::io::SeekFrom::Start(host_off)).await?;
            // Read what is there; the last sector may run past EOF.
            let mut got = 0usize;
            while got < len {
                let n = self.file.read(&mut comp[got..]).await?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            let mut dec = flate2::read::DeflateDecoder::new(&comp[..got]);
            let mut filled = 0usize;
            loop {
                match dec.read(&mut out[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => return Err(Qcow2Error::Corrupt(format!("cluster {guest_cluster}: {e}"))),
                }
                if filled == out.len() {
                    break;
                }
            }
            return Ok(out);
        }
        if e & L2E_ZERO != 0 {
            return Ok(out);
        }
        let host = e & L1E_OFFSET_MASK;
        if host == 0 {
            return Ok(out);
        }
        self.file.seek(std::io::SeekFrom::Start(host)).await?;
        let mut got = 0usize;
        while got < out.len() {
            let n = self.file.read(&mut out[got..]).await?;
            if n == 0 {
                break; // a short final cluster reads as zeros past EOF
            }
            got += n;
        }
        Ok(out)
    }

    /// Any byte range of the guest disk, zeros where nothing was written.
    pub async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Qcow2Error> {
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let cluster = pos / self.cluster_size;
            let within = (pos % self.cluster_size) as usize;
            let take = ((self.cluster_size as usize) - within).min(buf.len() - done);
            if pos >= self.virtual_size {
                buf[done..done + take].fill(0);
            } else {
                match self.cluster_state(cluster).await? {
                    ClusterState::Data => {
                        let data = self.read_cluster(cluster).await?;
                        buf[done..done + take].copy_from_slice(&data[within..within + take]);
                    }
                    _ => buf[done..done + take].fill(0),
                }
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testimg {
    //! A tiny qcow2 writer, for tests only: one L1 entry, one L2 table,
    //! clusters given as data / zero / compressed. Enough to prove the
    //! reader against bytes that did not come from the reader.
    use std::io::Write as _;

    pub enum C {
        Data(Vec<u8>),
        Zero,
        Compressed(Vec<u8>),
        Hole,
    }

    pub fn build(cluster_bits: u32, clusters: &[C]) -> Vec<u8> {
        let cs = 1usize << cluster_bits;
        let virtual_size = (clusters.len() * cs) as u64;
        // Layout: header cluster 0, L1 cluster 1, L2 cluster 2, refcount
        // table cluster 3 (unused by the reader), data from cluster 4.
        let l1_off = cs as u64;
        let l2_off = 2 * cs as u64;
        let mut out = vec![0u8; 4 * cs];
        out[..4].copy_from_slice(b"QFI\xfb");
        out[4..8].copy_from_slice(&3u32.to_be_bytes());
        out[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        out[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        out[36..40].copy_from_slice(&1u32.to_be_bytes());
        out[40..48].copy_from_slice(&l1_off.to_be_bytes());
        out[48..56].copy_from_slice(&(3 * cs as u64).to_be_bytes());
        out[56..60].copy_from_slice(&1u32.to_be_bytes());
        out[96..100].copy_from_slice(&4u32.to_be_bytes()); // refcount_order
        out[100..104].copy_from_slice(&104u32.to_be_bytes()); // header_length
        // L1[0] -> L2 (refcount-one flag set, as qemu writes it)
        out[l1_off as usize..l1_off as usize + 8].copy_from_slice(&((1u64 << 63) | l2_off).to_be_bytes());
        for (i, c) in clusters.iter().enumerate() {
            let entry: u64 = match c {
                C::Hole => 0,
                C::Zero => 1,
                C::Data(d) => {
                    let host = out.len() as u64;
                    let mut padded = d.clone();
                    padded.resize(cs, 0);
                    out.extend_from_slice(&padded);
                    (1u64 << 63) | host
                }
                C::Compressed(d) => {
                    let mut padded = d.clone();
                    padded.resize(cs, 0);
                    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                    enc.write_all(&padded).unwrap();
                    let comp = enc.finish().unwrap();
                    let host = out.len() as u64; // sector aligned: out is cluster-sized so far
                    let x = 62 - (cluster_bits - 8);
                    let sectors = comp.len().div_ceil(512) as u64;
                    let extra = sectors - 1;
                    out.extend_from_slice(&comp);
                    let pad = (512 - comp.len() % 512) % 512;
                    out.extend(std::iter::repeat_n(0u8, pad));
                    (1u64 << 62) | (extra << x) | host
                }
            };
            let at = l2_off as usize + i * 8;
            out[at..at + 8].copy_from_slice(&entry.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testimg::C;

    async fn write_tmp(bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("stormblock-qcow2-{}.img", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&p, bytes).await.unwrap();
        p
    }

    #[tokio::test]
    async fn reads_data_zero_hole_and_compressed_clusters() {
        let cb = 12; // 4 KiB clusters
        let a: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let c: Vec<u8> = (0..4096).map(|i| ((i * 7) % 253) as u8).collect();
        let img = testimg::build(cb, &[C::Data(a.clone()), C::Zero, C::Hole, C::Compressed(c.clone())]);
        let p = write_tmp(&img).await;
        let mut q = Qcow2::open(&p).await.unwrap();
        assert_eq!(q.version, 3);
        assert_eq!(q.cluster_size, 4096);
        assert_eq!(q.virtual_size(), 4 * 4096);
        assert_eq!(q.cluster_state(0).await.unwrap(), ClusterState::Data);
        assert_eq!(q.cluster_state(1).await.unwrap(), ClusterState::Zero);
        assert_eq!(q.cluster_state(2).await.unwrap(), ClusterState::Unallocated);
        assert_eq!(q.cluster_state(3).await.unwrap(), ClusterState::Data);
        assert_eq!(q.read_cluster(0).await.unwrap(), a);
        assert_eq!(q.read_cluster(3).await.unwrap(), c, "compressed cluster inflates");
        assert!(q.read_cluster(1).await.unwrap().iter().all(|&b| b == 0));
        // A range across clusters, and past the end.
        let mut buf = vec![0xAAu8; 6000];
        q.read_at(4096 - 100, &mut buf).await.unwrap();
        assert_eq!(&buf[..100], &a[4096 - 100..]);
        assert!(buf[100..].iter().all(|&b| b == 0));
        let mut tail = vec![0xAAu8; 100];
        q.read_at(4 * 4096 - 50, &mut tail).await.unwrap();
        assert_eq!(&tail[..50], &c[4096 - 50..]);
        assert!(tail[50..].iter().all(|&b| b == 0), "past the end is zeros");
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn refuses_what_it_cannot_decode_honestly() {
        let mut img = testimg::build(12, &[C::Zero]);
        img[16..20].copy_from_slice(&5u32.to_be_bytes()); // backing file name length
        let p = write_tmp(&img).await;
        let err = match Qcow2::open(&p).await {
            Err(e) => e,
            Ok(_) => panic!("a backing file must be refused"),
        };
        assert!(matches!(err, Qcow2Error::Unsupported(ref m) if m.contains("backing")), "{err}");
        let _ = std::fs::remove_file(&p);
        let p = write_tmp(b"not a qcow2 at all, just bytes").await;
        assert!(matches!(Qcow2::open(&p).await.err(), Some(Qcow2Error::NotQcow2)));
        let _ = std::fs::remove_file(p);
    }
}
