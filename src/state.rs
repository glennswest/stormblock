//! Where the engine keeps what it must remember.
//!
//! # Not on a filesystem it serves
//!
//! The engine used to write its own state — the export table, the LUN map, the
//! `/v1` fencing epochs, the filesystem templates — as files under a
//! `--data-dir`. On a node that is fine right up until the directory happens to
//! sit on a volume the engine itself exports, and then it is a cycle: every
//! volume create and delete ends in a metadata write, taken while the
//! volume-manager lock is held, and that write goes out through the VFS, into
//! ext4, down to a ublk device, and back into the same process that is holding
//! the lock.
//!
//! A stormcos node did exactly this and wedged four seconds into every boot.
//! One lock held forever, every container's disk I/O queued behind it, and the
//! registry, four supervisors, sshd and the console shell all accepting
//! connections they would never answer. The engine's own API kept replying in a
//! millisecond, which is what made it read as a network fault rather than a
//! storage one.
//!
//! # The slab is not a filesystem
//!
//! So state goes in the slab, which is not a filesystem and is not mounted: it
//! is the engine's own on-disk format in a raw partition — superblock, this
//! metadata region, a slot table, then the slots. Writing to it is
//! `device.write(offset, buf)` against a disk the engine already holds open.
//! Nothing in that path can come back around to the engine, because there is
//! no filesystem, no block device and no other process in it.
//!
//! `volumes.dat` has lived there since the slab became self-describing, with
//! two copies written alternately by generation and a CRC on each, so a torn
//! write costs the newer copy and never the record. This puts everything else
//! beside it, under keys, in the same two copies.
//!
//! # One writer
//!
//! The region has a single owner. `volumes.dat` is a key like any other rather
//! than a second writer racing for the same bytes, and the lock taken here is
//! this store's own — never the volume manager's, which is what made the
//! original cycle fatal instead of merely slow.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::drive::slab::Slab;

/// The keyed container written into a slab's metadata region.
///
/// Distinct from `STRMVOL\0`, which is what a bare `volumes.dat` payload starts
/// with — so a slab written before this reads back as a container holding that
/// one key, and no slab has to be reformatted.
const MAGIC: [u8; 8] = *b"STRMSTAT";
const VERSION: u32 = 1;

/// The volume metadata, which was the region's only occupant.
pub const VOLUMES: &str = "volumes.dat";

/// What the old, single-payload form starts with.
const VOLUMES_MAGIC: [u8; 8] = *b"STRMVOL\0";

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state store: {0}")]
    Io(#[from] std::io::Error),
    #[error("state store: {0}")]
    Device(#[from] crate::drive::DriveError),
    #[error("state store: malformed container: {0}")]
    Malformed(String),
}

/// Where the bytes actually go.
enum Backing {
    /// A slab's metadata region: a raw offset on a disk the engine holds open.
    Slab(Arc<Slab>),
    /// A directory, one file per key.
    ///
    /// What a CLI run, a test and a single-drive development node use. Kept
    /// because those have no slab to write into and nothing to deadlock
    /// against — the cycle only exists when the directory is on storage this
    /// engine is responsible for, which is a property of the deployment and
    /// not of the code.
    Dir(PathBuf),
}

struct Inner {
    blobs: BTreeMap<String, Vec<u8>>,
    backing: Backing,
}

pub struct StateStore {
    inner: Mutex<Inner>,
}

impl StateStore {
    /// Open the store held in a slab, reading what is already there.
    pub async fn open_slab(slab: Arc<Slab>) -> Result<StateStore, StateError> {
        let blobs = match slab.read_metadata().await? {
            Some(bytes) => decode(&bytes)?,
            None => BTreeMap::new(),
        };
        Ok(StateStore { inner: Mutex::new(Inner { blobs, backing: Backing::Slab(slab) }) })
    }

    /// Open the store held in a directory.
    pub fn open_dir(dir: impl Into<PathBuf>) -> StateStore {
        let dir = dir.into();
        let mut blobs = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
                if let Ok(bytes) = std::fs::read(e.path()) {
                    blobs.insert(name, bytes);
                }
            }
        }
        StateStore { inner: Mutex::new(Inner { blobs, backing: Backing::Dir(dir) }) }
    }

    /// What is stored under `key`, if anything.
    pub async fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().await.blobs.get(key).cloned()
    }

    /// Store `bytes` under `key`, durably.
    ///
    /// The whole container is rewritten for a slab, because that is what a
    /// two-copy generational region *is*: a copy is valid or it is not, and
    /// there is no partial update of one. For a directory only the one file is
    /// written, which is both cheaper and what was always there.
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), StateError> {
        let mut inner = self.inner.lock().await;
        inner.blobs.insert(key.to_owned(), bytes);
        inner.flush(Some(key)).await
    }

    /// Forget `key`.
    pub async fn remove(&self, key: &str) -> Result<(), StateError> {
        let mut inner = self.inner.lock().await;
        if inner.blobs.remove(key).is_none() {
            return Ok(());
        }
        match &inner.backing {
            Backing::Dir(dir) => {
                let _ = std::fs::remove_file(dir.join(key));
                Ok(())
            }
            Backing::Slab(_) => inner.flush(None).await,
        }
    }

    /// Every key held, in order.
    pub async fn keys(&self) -> Vec<String> {
        self.inner.lock().await.blobs.keys().cloned().collect()
    }

    /// Where this store writes, for a diagnostic.
    pub async fn describe(&self) -> String {
        match &self.inner.lock().await.backing {
            Backing::Slab(s) => format!("slab {}", s.id().0),
            Backing::Dir(d) => format!("directory {}", d.display()),
        }
    }
}

impl Inner {
    async fn flush(&self, only: Option<&str>) -> Result<(), StateError> {
        match &self.backing {
            Backing::Dir(dir) => {
                std::fs::create_dir_all(dir)?;
                let write = |k: &String, v: &Vec<u8>| -> std::io::Result<()> {
                    // Through a temporary and a rename: a half-written file is
                    // indistinguishable from a truncated one, and this is the
                    // record that says what the node holds.
                    let tmp = dir.join(format!(".{k}.tmp"));
                    std::fs::write(&tmp, v)?;
                    std::fs::rename(&tmp, dir.join(k))
                };
                match only {
                    Some(k) => {
                        if let Some(v) = self.blobs.get(k) {
                            write(&k.to_string(), v)?;
                        }
                    }
                    None => {
                        for (k, v) in &self.blobs {
                            write(k, v)?;
                        }
                    }
                }
                Ok(())
            }
            Backing::Slab(slab) => {
                let bytes = encode(&self.blobs);
                slab.write_metadata(&bytes).await?;
                Ok(())
            }
        }
    }
}

/// `MAGIC | version | count | (klen, key, vlen, value)*`
///
/// Lengths ahead of their bytes so a reader never has to scan, and keys in
/// order so the same set of blobs always encodes to the same bytes — which
/// makes a write that changes nothing visibly a write that changes nothing.
fn encode(blobs: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + blobs.values().map(|v| v.len() + 16).sum::<usize>());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
    for (k, v) in blobs {
        out.extend_from_slice(&(k.len() as u32).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

fn decode(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, StateError> {
    // A slab written before this held one payload and no keys. It is not a
    // malformed container, it is the previous format, and it means exactly one
    // thing.
    if data.len() >= 8 && data[0..8] == VOLUMES_MAGIC {
        let mut m = BTreeMap::new();
        m.insert(VOLUMES.to_string(), data.to_vec());
        return Ok(m);
    }
    if data.len() < 16 || data[0..8] != MAGIC {
        return Err(StateError::Malformed("not a state container".into()));
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(StateError::Malformed(format!("version {version}")));
    }
    let count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;

    let mut blobs = BTreeMap::new();
    let mut at = 16usize;
    let need = |at: usize, n: usize, len: usize| -> Result<(), StateError> {
        if at + n > len {
            return Err(StateError::Malformed("truncated".into()));
        }
        Ok(())
    };
    for _ in 0..count {
        need(at, 4, data.len())?;
        let klen = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        need(at, klen, data.len())?;
        let key = String::from_utf8(data[at..at + klen].to_vec())
            .map_err(|_| StateError::Malformed("key is not utf-8".into()))?;
        at += klen;
        need(at, 8, data.len())?;
        let vlen = u64::from_le_bytes(data[at..at + 8].try_into().unwrap()) as usize;
        at += 8;
        need(at, vlen, data.len())?;
        blobs.insert(key, data[at..at + vlen].to_vec());
        at += vlen;
    }
    Ok(blobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_round_trips() {
        let mut m = BTreeMap::new();
        m.insert("volumes.dat".to_string(), vec![1, 2, 3]);
        m.insert("luns.json".to_string(), b"{\"a\":1}".to_vec());
        m.insert("empty".to_string(), Vec::new());
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn the_encoding_is_stable() {
        // Same blobs, same bytes — so a rewrite that changes nothing writes
        // the same thing, and a diff of two slabs means something.
        let mut a = BTreeMap::new();
        a.insert("b".to_string(), vec![2]);
        a.insert("a".to_string(), vec![1]);
        let mut b = BTreeMap::new();
        b.insert("a".to_string(), vec![1]);
        b.insert("b".to_string(), vec![2]);
        assert_eq!(encode(&a), encode(&b));
    }

    #[test]
    fn a_slab_from_before_this_reads_as_one_key() {
        // The previous format: a bare volumes.dat payload, no container. It is
        // not corrupt and must not be read as corrupt.
        let mut old = VOLUMES_MAGIC.to_vec();
        old.extend_from_slice(&[9u8; 40]);
        let back = decode(&old).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.get(VOLUMES).unwrap(), &old);
    }

    #[test]
    fn rubbish_is_refused_rather_than_guessed() {
        assert!(decode(b"").is_err());
        assert!(decode(b"NOTAMAGIC-and-then-some").is_err());
        // A truncated container: the count says two, the bytes hold one.
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), vec![1, 2, 3]);
        let mut bytes = encode(&m);
        bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    #[tokio::test]
    async fn a_directory_store_keeps_one_file_per_key() {
        let dir = std::env::temp_dir().join("stormblock-state-dir-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let s = StateStore::open_dir(&dir);
        s.put("luns.json", b"[]".to_vec()).await.unwrap();
        s.put(VOLUMES, vec![7, 7, 7]).await.unwrap();
        assert_eq!(std::fs::read(dir.join("luns.json")).unwrap(), b"[]");

        // And it is read back by a store opened fresh on the same directory,
        // which is what a restart is.
        let again = StateStore::open_dir(&dir);
        assert_eq!(again.get("luns.json").await.unwrap(), b"[]");
        assert_eq!(again.get(VOLUMES).await.unwrap(), vec![7, 7, 7]);

        again.remove("luns.json").await.unwrap();
        assert!(!dir.join("luns.json").exists());
        assert!(again.get("luns.json").await.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
