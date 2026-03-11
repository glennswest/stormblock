//! On-disk metadata persistence for volume state.
//!
//! Binary envelope: magic + version + payload length + timestamp + bincode payload + CRC32C.
//! Atomic writes via temp-file + fsync + rename. Keeps `.bak` of previous state.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, Deserialize};

use crate::raid::RaidArrayId;
use crate::volume::extent::VolumeId;
use crate::volume::thin::PhysicalExtent;

/// Magic bytes: "STRMVOL\0"
const MAGIC: [u8; 8] = *b"STRMVOL\0";

/// Current metadata format version.
const VERSION: u32 = 1;

/// Metadata filename.
const METADATA_FILE: &str = "volumes.dat";
const METADATA_TMP: &str = "volumes.dat.tmp";
const METADATA_BAK: &str = "volumes.dat.bak";

/// Serializable volume metadata payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeMetadata {
    pub extent_size: u64,
    pub arrays: Vec<ArrayRecord>,
    pub volumes: Vec<VolumeRecord>,
}

/// Persisted array info — just enough to verify arrays exist on recovery.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArrayRecord {
    pub array_id: RaidArrayId,
    pub total_capacity: u64,
}

/// Persisted volume state.
#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeRecord {
    pub id: VolumeId,
    pub name: String,
    pub virtual_size: u64,
    pub array_id: RaidArrayId,
    pub extent_map: BTreeMap<u64, PhysicalExtent>,
}

/// Handles reading/writing volume metadata to disk.
pub struct MetadataStore {
    data_dir: PathBuf,
}

impl MetadataStore {
    pub fn new(data_dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        Ok(MetadataStore { data_dir })
    }

    /// Serialize metadata into the binary envelope format.
    fn encode(metadata: &VolumeMetadata) -> io::Result<Vec<u8>> {
        let payload = bincode::serde::encode_to_vec(metadata, bincode::config::standard())
            .map_err(|e| io::Error::other(format!("bincode encode: {e}")))?;

        let payload_len = payload.len() as u64;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Header: magic(8) + version(4) + payload_len(8) + timestamp(8) = 28 bytes
        let total = 28 + payload.len() + 4; // +4 for CRC32C
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&timestamp.to_le_bytes());
        buf.extend_from_slice(&payload);

        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        Ok(buf)
    }

    /// Decode the binary envelope, verify magic + CRC, return payload.
    fn decode(data: &[u8]) -> io::Result<VolumeMetadata> {
        if data.len() < 32 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "metadata too short"));
        }

        // Check magic
        if data[0..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }

        // Check version
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported metadata version {version}"),
            ));
        }

        let payload_len = u64::from_le_bytes(data[12..20].try_into().unwrap()) as usize;
        let _timestamp = u64::from_le_bytes(data[20..28].try_into().unwrap());

        let expected_total = 28 + payload_len + 4;
        if data.len() < expected_total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("truncated metadata: expected {expected_total} bytes, got {}", data.len()),
            ));
        }

        // Verify CRC32C
        let crc_offset = 28 + payload_len;
        let stored_crc = u32::from_le_bytes(data[crc_offset..crc_offset + 4].try_into().unwrap());
        let computed_crc = crc32c::crc32c(&data[..crc_offset]);
        if stored_crc != computed_crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CRC32C mismatch: stored={stored_crc:#x}, computed={computed_crc:#x}"),
            ));
        }

        let payload = &data[28..28 + payload_len];
        let (metadata, _): (VolumeMetadata, _) =
            bincode::serde::decode_from_slice(payload, bincode::config::standard())
                .map_err(|e| io::Error::other(format!("bincode decode: {e}")))?;

        Ok(metadata)
    }

    /// Persist volume metadata to disk atomically.
    pub fn save(&self, metadata: &VolumeMetadata) -> io::Result<()> {
        let dat_path = self.data_dir.join(METADATA_FILE);
        let tmp_path = self.data_dir.join(METADATA_TMP);
        let bak_path = self.data_dir.join(METADATA_BAK);

        let buf = Self::encode(metadata)?;

        // Backup current .dat → .bak
        if dat_path.exists() {
            let _ = std::fs::rename(&dat_path, &bak_path);
        }

        // Write to .tmp
        std::fs::write(&tmp_path, &buf)?;

        // fsync the file
        let file = std::fs::File::open(&tmp_path)?;
        file.sync_all()?;
        drop(file);

        // Rename .tmp → .dat
        std::fs::rename(&tmp_path, &dat_path)?;

        // fsync the directory
        if let Ok(dir) = std::fs::File::open(&self.data_dir) {
            let _ = dir.sync_all();
        }

        Ok(())
    }

    /// Load volume metadata from disk. Tries `.dat` first, falls back to `.bak`.
    pub fn load(&self) -> io::Result<VolumeMetadata> {
        let dat_path = self.data_dir.join(METADATA_FILE);
        let bak_path = self.data_dir.join(METADATA_BAK);

        // Try primary
        if dat_path.exists() {
            match Self::try_load(&dat_path) {
                Ok(m) => return Ok(m),
                Err(e) => {
                    tracing::warn!("Primary metadata corrupt: {e}, trying backup");
                }
            }
        }

        // Try backup
        if bak_path.exists() {
            match Self::try_load(&bak_path) {
                Ok(m) => {
                    tracing::info!("Restored metadata from backup");
                    return Ok(m);
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("both primary and backup metadata corrupt: {e}"),
                    ));
                }
            }
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "no metadata file found"))
    }

    fn try_load(path: &Path) -> io::Result<VolumeMetadata> {
        let data = std::fs::read(path)?;
        Self::decode(&data)
    }

    /// Check if any metadata file exists.
    pub fn exists(&self) -> bool {
        self.data_dir.join(METADATA_FILE).exists() || self.data_dir.join(METADATA_BAK).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_metadata() -> VolumeMetadata {
        let array_id = RaidArrayId(Uuid::new_v4());
        let vol_id = VolumeId(Uuid::new_v4());
        let mut extent_map = BTreeMap::new();
        extent_map.insert(0, PhysicalExtent {
            array_id,
            offset: 0,
            length: 4 * 1024 * 1024,
            ref_count: 1,
        });
        extent_map.insert(1, PhysicalExtent {
            array_id,
            offset: 4 * 1024 * 1024,
            length: 4 * 1024 * 1024,
            ref_count: 1,
        });

        VolumeMetadata {
            extent_size: 4 * 1024 * 1024,
            arrays: vec![ArrayRecord {
                array_id,
                total_capacity: 64 * 1024 * 1024,
            }],
            volumes: vec![VolumeRecord {
                id: vol_id,
                name: "test-vol".to_string(),
                virtual_size: 100 * 1024 * 1024,
                array_id,
                extent_map,
            }],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let meta = test_metadata();
        let encoded = MetadataStore::encode(&meta).unwrap();

        // Verify header
        assert_eq!(&encoded[0..8], b"STRMVOL\0");
        let version = u32::from_le_bytes(encoded[8..12].try_into().unwrap());
        assert_eq!(version, 1);

        let decoded = MetadataStore::decode(&encoded).unwrap();
        assert_eq!(decoded.extent_size, meta.extent_size);
        assert_eq!(decoded.volumes.len(), 1);
        assert_eq!(decoded.volumes[0].name, "test-vol");
        assert_eq!(decoded.volumes[0].extent_map.len(), 2);
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("stormblock-meta-test-{}", Uuid::new_v4()));
        let store = MetadataStore::new(dir.clone()).unwrap();
        let meta = test_metadata();

        store.save(&meta).unwrap();
        assert!(store.exists());

        let loaded = store.load().unwrap();
        assert_eq!(loaded.extent_size, meta.extent_size);
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.volumes[0].name, "test-vol");
        assert_eq!(loaded.volumes[0].extent_map.len(), 2);
        assert_eq!(loaded.arrays.len(), 1);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let dir = std::env::temp_dir().join(format!("stormblock-meta-backup-{}", Uuid::new_v4()));
        let store = MetadataStore::new(dir.clone()).unwrap();
        let meta = test_metadata();

        // Save good data (creates .dat)
        store.save(&meta).unwrap();

        // Save again (moves previous .dat → .bak, writes new .dat)
        store.save(&meta).unwrap();

        // Corrupt the primary .dat
        let dat_path = dir.join("volumes.dat");
        std::fs::write(&dat_path, b"CORRUPTED DATA").unwrap();

        // Load should fall back to .bak
        let loaded = store.load().unwrap();
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.volumes[0].name, "test-vol");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_bad_magic() {
        let mut data = MetadataStore::encode(&test_metadata()).unwrap();
        data[0..8].copy_from_slice(b"BADMAGIC");
        assert!(MetadataStore::decode(&data).is_err());
    }

    #[test]
    fn decode_bad_crc() {
        let mut data = MetadataStore::encode(&test_metadata()).unwrap();
        let len = data.len();
        data[len - 1] ^= 0xFF; // flip CRC byte
        assert!(MetadataStore::decode(&data).is_err());
    }

    #[test]
    fn decode_truncated() {
        let data = MetadataStore::encode(&test_metadata()).unwrap();
        assert!(MetadataStore::decode(&data[..20]).is_err());
    }

    #[test]
    fn no_metadata_returns_not_found() {
        let dir = std::env::temp_dir().join(format!("stormblock-meta-empty-{}", Uuid::new_v4()));
        let store = MetadataStore::new(dir.clone()).unwrap();
        assert!(!store.exists());
        let err = store.load().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
