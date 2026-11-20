//! A volume on a stormdist ISO, as a block device (#442).
//!
//! The release ISO carries each flavour's composed, tested disk as a member
//! (owner, 2026-11-20, option 1 on #442). An install with no forge boots the
//! ISO and reads that disk straight off it, through this device, exactly as
//! a forge install reads its claimed clone over NVMe/TCP: `boot-local` opens
//! its slabs, and the flow-over moves them onto the node's drives. The ISO
//! stays attached until the flow-over ends, because this device stays open
//! until then.
//!
//! `stormdist://<iso path>?flavour=<f>&key=<public key file>` (or `member=`
//! a golden's name or component in place of `flavour=`) is accepted wherever
//! a device path is. The ISO is opened with stormdist's own reader and the
//! release key: the catalogue's signature is checked before anything is
//! read, and every chunk is checked by its sha256 on every read (the medium
//! can change under us). Holes read as zeros.
//!
//! **Writes are kept in this process's memory** and never reach the ISO, as
//! `httpdev.rs` keeps them: the node runs from this disk while the flow-over
//! copies it, so a write must read back, and the flow-over copies what it
//! reads.
//!
//! The flavour's disk is the flavour's member named `stormcos-sno` (the
//! cilium flavour) or `stormcos-sno-<flavour>` (any other, e.g.
//! `stormcos-sno-flowsdn`), matched by its golden name or its component
//! (stormcentral#1556). A stormdist name has no dots, so a forge disk's
//! `stormcos-sno-12.78` goes on the ISO under its component's name.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use stormdist::format::Golden;
use stormdist::media::Media;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

/// The block size it reports: a GPT at 512 or 4096 is found (`Gpt::read`
/// probes both), and any slab's I/O is a multiple of it.
const BLOCK: u64 = 512;
/// Writes are kept in pages of this size.
const PAGE: u64 = 4096;
/// Verified chunks kept (stormdist's chunks are at most 4 MiB).
const CACHED_CHUNKS: usize = 16;

/// What a `stormdist://` path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoSpec {
    pub iso: String,
    /// The release's public key, a file of 64 hex digits.
    pub key: String,
    /// A golden's name or component; else the flavour's disk.
    pub member: Option<String>,
    pub flavour: Option<String>,
}

impl IsoSpec {
    /// `None` when `path` is not a `stormdist://` path; `Some(Err)` when it
    /// is and is malformed.
    pub fn parse(path: &str) -> Option<Result<IsoSpec, String>> {
        let rest = path.strip_prefix("stormdist://")?;
        let (iso, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut spec = IsoSpec { iso: iso.to_string(), key: String::new(), member: None, flavour: None };
        for kv in query.split('&').filter(|s| !s.is_empty()) {
            let Some((k, v)) = kv.split_once('=') else {
                return Some(Err(format!("{path}: '{kv}' is not key=value")));
            };
            match k {
                "key" => spec.key = v.to_string(),
                "member" => spec.member = Some(v.to_string()),
                "flavour" | "flavor" => spec.flavour = Some(v.to_string()),
                o => return Some(Err(format!("{path}: '{o}' is not known (iso path, then key=, flavour= or member=)"))),
            }
        }
        if spec.iso.is_empty() {
            return Some(Err(format!("{path}: no ISO path")));
        }
        if spec.key.is_empty() {
            return Some(Err(format!("{path}: no key= (the release's public key file): an ISO is never read unchecked")));
        }
        if spec.member.is_some() == spec.flavour.is_some() {
            return Some(Err(format!("{path}: name one of flavour= or member=")));
        }
        Some(Ok(spec))
    }
}

/// The name a flavour's composed disk has on the ISO (stormcentral#1556).
pub fn disk_name(flavour: &str) -> String {
    if flavour == "cilium" {
        "stormcos-sno".into()
    } else {
        format!("stormcos-sno-{flavour}")
    }
}

/// The flavour's disk among its members: exactly one, by name or component.
pub fn disk_member(media: &Media, flavour: &str) -> Result<Golden, String> {
    let goldens = media.flavour_goldens(flavour).map_err(|e| format!("flavour {flavour}: {e}"))?;
    let want = disk_name(flavour);
    let found: Vec<&Golden> = goldens.iter().filter(|g| g.name == want || g.component == want).collect();
    match found.as_slice() {
        [g] => Ok((*g).clone()),
        [] => Err(format!(
            "flavour {flavour} of release {} carries no disk named {want} (its members: {})",
            media.release.version,
            goldens.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
        _ => Err(format!("flavour {flavour} carries {} disks named {want}", found.len())),
    }
}

/// Read a public key file: 64 hex digits (whitespace around them allowed).
pub fn read_key(path: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("the release key {path}: {e}"))?;
    let bytes = hex::decode(text.trim()).map_err(|e| format!("the release key {path}: not hex ({e})"))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| format!("the release key {path}: not 32 bytes"))?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).map_err(|e| format!("the release key {path}: {e}"))
}

/// A golden on an ISO, read only, its writes kept in memory.
pub struct IsoDevice {
    id: DeviceId,
    media: Arc<Media>,
    golden: Golden,
    chunks: Mutex<(HashMap<String, Arc<Vec<u8>>>, VecDeque<String>)>,
    dirty: Mutex<BTreeMap<u64, Vec<u8>>>,
}

fn err(e: impl std::fmt::Display) -> DriveError {
    DriveError::Other(anyhow::anyhow!("{e}"))
}

impl IsoDevice {
    /// Open the ISO (its catalogue checked against the key) and the golden
    /// the spec names.
    pub async fn open(spec: &IsoSpec) -> DriveResult<IsoDevice> {
        let s = spec.clone();
        tokio::task::spawn_blocking(move || IsoDevice::open_blocking(&s)).await.map_err(err)?
    }

    fn open_blocking(spec: &IsoSpec) -> DriveResult<IsoDevice> {
        let key = read_key(&spec.key).map_err(err)?;
        let media = Media::open_for_install(std::path::Path::new(&spec.iso), &key)
            .map_err(|e| err(format!("{}: not a release ISO this key signs: {e}", spec.iso)))?;
        let golden = match (&spec.member, &spec.flavour) {
            (Some(m), _) => media.select(m).map_err(|e| err(format!("{}: {e}", spec.iso)))?.clone(),
            (None, Some(f)) => disk_member(&media, f).map_err(|e| err(format!("{}: {e}", spec.iso)))?,
            (None, None) => return Err(err("name one of flavour= or member=")),
        };
        let path = format!(
            "stormdist://{}?{}",
            spec.iso,
            match &spec.member {
                Some(m) => format!("member={m}"),
                None => format!("flavour={}", spec.flavour.as_deref().unwrap_or_default()),
            }
        );
        let id = DeviceId {
            uuid: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, format!("{path}#{}", golden.device_sha256).as_bytes()),
            serial: golden.device_sha256.chars().take(16).collect(),
            model: format!("stormdist {}", golden.name),
            path,
            wwn: String::new(),
        };
        tracing::info!(
            "{}: release {} on the ISO, {} ({} bytes, {} extent(s)); its writes are kept in memory (#442)",
            spec.iso,
            media.release.version,
            golden.name,
            golden.size,
            golden.extents.len()
        );
        Ok(IsoDevice {
            id,
            media: Arc::new(media),
            golden,
            chunks: Default::default(),
            dirty: Default::default(),
        })
    }

    /// The golden this device reads.
    pub fn golden(&self) -> &Golden {
        &self.golden
    }

    async fn chunk(&self, hash: &str) -> DriveResult<Arc<Vec<u8>>> {
        if let Some(c) = self.chunks.lock().unwrap().0.get(hash) {
            return Ok(c.clone());
        }
        let (media, h) = (self.media.clone(), hash.to_string());
        let bytes = tokio::task::spawn_blocking(move || media.read_chunk(&h))
            .await
            .map_err(err)?
            .map_err(|e| err(format!("{}: chunk {hash}: {e}", self.id.path)))?;
        let bytes = Arc::new(bytes);
        let mut c = self.chunks.lock().unwrap();
        if c.0.insert(hash.to_string(), bytes.clone()).is_none() {
            c.1.push_back(hash.to_string());
        }
        while c.1.len() > CACHED_CHUNKS {
            if let Some(old) = c.1.pop_front() {
                c.0.remove(&old);
            }
        }
        Ok(bytes)
    }

    /// The ISO's bytes at `[offset, offset + buf.len())`, no writes applied.
    async fn read_base(&self, offset: u64, buf: &mut [u8]) -> DriveResult<()> {
        buf.fill(0);
        let end = offset + buf.len() as u64;
        let ex = &self.golden.extents;
        let first = ex.partition_point(|e| e.offset + e.length <= offset);
        for e in &ex[first..] {
            if e.offset >= end {
                break;
            }
            let (start, stop) = (offset.max(e.offset), end.min(e.offset + e.length));
            if start >= stop {
                continue;
            }
            let bytes = self.chunk(&e.sha256).await?;
            buf[(start - offset) as usize..(stop - offset) as usize]
                .copy_from_slice(&bytes[(start - e.offset) as usize..(stop - e.offset) as usize]);
        }
        Ok(())
    }

    fn check(&self, offset: u64, len: usize) -> DriveResult<()> {
        if offset % BLOCK != 0 || offset.checked_add(len as u64).is_none_or(|e| e > self.golden.size) {
            return Err(DriveError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{}: {len} bytes at {offset}: unaligned or past its {} bytes", self.id.path, self.golden.size),
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl BlockDevice for IsoDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }
    fn capacity_bytes(&self) -> u64 {
        self.golden.size
    }
    fn block_size(&self) -> u32 {
        BLOCK as u32
    }
    fn optimal_io_size(&self) -> u32 {
        1 << 20
    }
    fn device_type(&self) -> DriveType {
        DriveType::File
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        self.check(offset, buf.len())?;
        self.read_base(offset, buf).await?;
        let end = offset + buf.len() as u64;
        let dirty = self.dirty.lock().unwrap();
        for (page, bytes) in dirty.range(offset / PAGE * PAGE..end) {
            let (start, stop) = (offset.max(*page), end.min(page + PAGE));
            if start < stop {
                buf[(start - offset) as usize..(stop - offset) as usize]
                    .copy_from_slice(&bytes[(start - page) as usize..(stop - page) as usize]);
            }
        }
        Ok(buf.len())
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.check(offset, buf.len())?;
        let end = offset + buf.len() as u64;
        let mut page = offset / PAGE * PAGE;
        while page < end {
            let page_end = (page + PAGE).min(self.golden.size);
            let held = self.dirty.lock().unwrap().get(&page).cloned();
            let mut bytes = match held {
                Some(b) => b,
                None => {
                    let mut b = vec![0u8; (page_end - page) as usize];
                    if offset > page || end < page_end {
                        self.read_base(page, &mut b).await?;
                    }
                    b
                }
            };
            let (start, stop) = (offset.max(page), end.min(page_end));
            bytes[(start - page) as usize..(stop - page) as usize]
                .copy_from_slice(&buf[(start - offset) as usize..(stop - offset) as usize]);
            self.dirty.lock().unwrap().insert(page, bytes);
            page += PAGE;
        }
        Ok(buf.len())
    }
    async fn flush(&self) -> DriveResult<()> {
        Ok(())
    }
    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        Ok(())
    }
}
