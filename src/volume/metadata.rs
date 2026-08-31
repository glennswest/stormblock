//! On-disk metadata persistence for volume state.
//!
//! Binary envelope: magic + version + payload length + timestamp + bincode payload + CRC32C.
//! Atomic writes via temp-file + fsync + rename. Keeps `.bak` of previous state.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, Deserialize};

use crate::drive::slab::SlabId;
use crate::raid::RaidArrayId;
use crate::volume::extent::VolumeId;
use crate::volume::gem::{ExtentLocation, ParityGroup};
use crate::volume::redundancy::RedundancyPolicy;
use crate::volume::thin::PhysicalExtent;

/// Magic bytes: "STRMVOL\0"
const MAGIC: [u8; 8] = *b"STRMVOL\0";

/// Current metadata format version.
///
/// V2 persists each volume's slab extent map — the piece slot tables cannot
/// express for COW snapshots (shared slots are recorded under the original
/// writer, so a snapshot's view exists only in this file). See issue #13.
///
/// V3 adds [`Retention`]: whether a volume is meant to be kept or thrown
/// away. Nothing recorded that before, so a container's root and a customer's
/// data looked identical to the engine, and anything acting on one had to be
/// told which it was by whoever happened to mount it.
///
/// V4 makes redundancy a property of the volume: each extent location
/// carries its mirror legs, a parity volume carries its stripes' parity
/// groups, and the record names the policy and the slabs the volume has
/// stopped trusting. A V3 record loads as an unreplicated volume.
///
/// V5 puts lineage, sealing and filesystem identity on the volume (#76):
/// `parent`, `sealed`, `fs`. A template is a volume that has been sealed.
const VERSION: u32 = 5;

/// What is known about the filesystem on a volume — the properties that used
/// to live on a separate template object and are facts about the volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsInfo {
    /// `ext2`, `ext3`, `ext4`, …
    pub kind: String,
    pub journal: bool,
    pub features: Option<String>,
    pub sixty_four_bit: bool,
    pub metadata_csum: bool,
    /// What keeps a UUID stamp to one superblock write.
    pub csum_seed: bool,
    pub label: String,
    /// The filesystem's own UUID. A clone gets a fresh one; two live
    /// filesystems must never claim one identity.
    pub uuid: Option<uuid::Uuid>,
}

impl FsInfo {
    /// What a vfat volume is, for the one case that makes one: a cloud-init
    /// seed. None of the ext-family properties apply, and saying so plainly
    /// beats leaving a record that claims a journal on a FAT filesystem.
    pub fn vfat(label: &str) -> FsInfo {
        FsInfo {
            kind: "vfat".into(),
            journal: false,
            features: None,
            sixty_four_bit: false,
            metadata_csum: false,
            csum_seed: false,
            label: label.to_string(),
            uuid: None,
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "journal": self.journal,
            "features": self.features,
            "64bit": self.sixty_four_bit,
            "metadata_csum": self.metadata_csum,
            "metadata_csum_seed": self.csum_seed,
            "label": self.label,
            "uuid": self.uuid,
        })
    }
}

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
    /// Legacy array binding (V1 records); slab-placed volumes carry None.
    pub array_id: Option<RaidArrayId>,
    /// Virtual extent index → slab location. Authoritative for mappings the
    /// slot tables can't reconstruct (a snapshot's shared slots).
    pub extents: BTreeMap<u64, ExtentLocation>,
    /// Whether this volume is meant to survive. See [`Retention`].
    #[serde(default)]
    pub retention: Retention,
    /// How the volume is protected. `none` for everything written before V4.
    pub redundancy: RedundancyPolicy,
    /// Stripe → parity legs, for a parity volume.
    pub parity: BTreeMap<u64, ParityGroup>,
    /// Slabs a write has failed on: skipped until a resync clears them.
    pub failed_slabs: Vec<SlabId>,
    /// The volume this one was cloned from, if any (#76).
    pub parent: Option<VolumeId>,
    /// Sealed: refuses writes; what clones are taken from.
    pub sealed: bool,
    /// The filesystem on it, when the engine knows.
    pub fs: Option<FsInfo>,
}

/// What is supposed to happen to a volume's divergence from its golden.
///
/// Every container is a copy-on-write clone of a golden, so what separates a
/// scratch filesystem from a customer's data is not how it is made — it is
/// whether anyone intends to keep it. Nothing recorded that, which left the
/// engine unable to tell a container root from a database, and left every
/// consumer to decide for itself from context it did not have.
///
/// It belongs to the volume rather than to whoever mounted it: the same
/// volume may be mounted by different things over its life, and the answer
/// must not change when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Retention {
    /// Keep it. The default, deliberately: too much kept is a cleanup, and
    /// something thrown away that should not have been is unrecoverable.
    ///
    /// Survives restarts, and an upgrade must carry it forward rather than
    /// removing it with the generation it happened to be created under.
    #[default]
    Keep,
    /// Throw it away. Equivalent to a tmpfs in intent, but backed by the same
    /// copy-on-write machinery as everything else — so it costs nothing until
    /// written, it can be reset to its golden instead of recreated, and the
    /// golden is still there as the fallback.
    ///
    /// Reset when whatever uses it starts.
    Ephemeral,
}

impl Retention {
    pub fn as_str(self) -> &'static str {
        match self {
            Retention::Keep => "keep",
            Retention::Ephemeral => "ephemeral",
        }
    }

    pub fn parse(s: &str) -> Option<Retention> {
        Some(match s.to_ascii_lowercase().as_str() {
            "keep" | "kept" | "persist" | "persistent" => Retention::Keep,
            "ephemeral" | "throwaway" | "throw-away" | "scratch" => Retention::Ephemeral,
            _ => return None,
        })
    }
}

/// The extent location shape every version before V4 wrote: one leg.
#[derive(Debug, Serialize, Deserialize)]
pub struct LegacyLocation {
    pub slab_id: SlabId,
    pub slot_idx: u32,
    pub ref_count: u32,
    pub generation: u64,
}

impl From<LegacyLocation> for ExtentLocation {
    fn from(l: LegacyLocation) -> Self {
        ExtentLocation {
            slab_id: l.slab_id,
            slot_idx: l.slot_idx,
            ref_count: l.ref_count,
            generation: l.generation,
            mirrors: Vec::new(),
        }
    }
}

fn convert_legacy_extents(m: BTreeMap<u64, LegacyLocation>) -> BTreeMap<u64, ExtentLocation> {
    m.into_iter().map(|(k, v)| (k, v.into())).collect()
}

/// V4 payload shapes — decode-only, converted on load.
mod v4 {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeMetadata {
        pub extent_size: u64,
        pub arrays: Vec<super::ArrayRecord>,
        pub volumes: Vec<VolumeRecord>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeRecord {
        pub id: VolumeId,
        pub name: String,
        pub virtual_size: u64,
        pub array_id: Option<RaidArrayId>,
        pub extents: BTreeMap<u64, ExtentLocation>,
        pub retention: Retention,
        pub redundancy: RedundancyPolicy,
        pub parity: BTreeMap<u64, ParityGroup>,
        pub failed_slabs: Vec<SlabId>,
    }
}

impl From<v4::VolumeMetadata> for VolumeMetadata {
    fn from(old: v4::VolumeMetadata) -> Self {
        VolumeMetadata {
            extent_size: old.extent_size,
            arrays: old.arrays,
            volumes: old
                .volumes
                .into_iter()
                .map(|v| VolumeRecord {
                    id: v.id,
                    name: v.name,
                    virtual_size: v.virtual_size,
                    array_id: v.array_id,
                    extents: v.extents,
                    retention: v.retention,
                    redundancy: v.redundancy,
                    parity: v.parity,
                    failed_slabs: v.failed_slabs,
                    // Nothing before V5 recorded lineage; the template
                    // store's adoption fills in what it knows at startup.
                    parent: None,
                    sealed: false,
                    fs: None,
                })
                .collect(),
        }
    }
}

/// V3 payload shapes — decode-only, converted on load.
///
/// **bincode is not self-describing**, so a field added to a struct is not
/// something an older payload can be read *around*: the decoder would run off
/// the end of the record or, worse, read the next record's bytes as this
/// one's. `#[serde(default)]` does nothing here. Every version that ever
/// existed therefore keeps its own shape, and conversion happens on load.
mod v3 {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeMetadata {
        pub extent_size: u64,
        pub arrays: Vec<super::ArrayRecord>,
        pub volumes: Vec<VolumeRecord>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeRecord {
        pub id: VolumeId,
        pub name: String,
        pub virtual_size: u64,
        pub array_id: Option<RaidArrayId>,
        pub extents: BTreeMap<u64, LegacyLocation>,
        pub retention: Retention,
    }
}

impl From<v3::VolumeMetadata> for VolumeMetadata {
    fn from(old: v3::VolumeMetadata) -> Self {
        VolumeMetadata {
            extent_size: old.extent_size,
            arrays: old.arrays,
            volumes: old
                .volumes
                .into_iter()
                .map(|v| VolumeRecord {
                    id: v.id,
                    name: v.name,
                    virtual_size: v.virtual_size,
                    array_id: v.array_id,
                    extents: convert_legacy_extents(v.extents),
                    retention: v.retention,
                    // Nothing before V4 was replicated: one leg per extent.
                    redundancy: RedundancyPolicy::none(),
                    parity: BTreeMap::new(),
                    failed_slabs: Vec::new(),
                    parent: None,
                    sealed: false,
                    fs: None,
                })
                .collect(),
        }
    }
}

/// V2 payload shapes — decode-only, converted on load.
mod v2 {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeMetadata {
        pub extent_size: u64,
        pub arrays: Vec<super::ArrayRecord>,
        pub volumes: Vec<VolumeRecord>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeRecord {
        pub id: VolumeId,
        pub name: String,
        pub virtual_size: u64,
        pub array_id: Option<RaidArrayId>,
        pub extents: BTreeMap<u64, LegacyLocation>,
    }
}

impl From<v2::VolumeMetadata> for VolumeMetadata {
    fn from(old: v2::VolumeMetadata) -> Self {
        VolumeMetadata {
            extent_size: old.extent_size,
            arrays: old.arrays,
            volumes: old
                .volumes
                .into_iter()
                .map(|v| VolumeRecord {
                    id: v.id,
                    name: v.name,
                    virtual_size: v.virtual_size,
                    array_id: v.array_id,
                    extents: convert_legacy_extents(v.extents),
                    // V2 did not ask. Silence means keep — a volume discarded
                    // because its record predates the question is gone.
                    retention: Retention::Keep,
                    redundancy: RedundancyPolicy::none(),
                    parity: BTreeMap::new(),
                    failed_slabs: Vec::new(),
                    parent: None,
                    sealed: false,
                    fs: None,
                })
                .collect(),
        }
    }
}

/// V1 payload shapes — decode-only, converted on load.
mod v1 {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeMetadata {
        pub extent_size: u64,
        pub arrays: Vec<super::ArrayRecord>,
        pub volumes: Vec<VolumeRecord>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct VolumeRecord {
        pub id: VolumeId,
        pub name: String,
        pub virtual_size: u64,
        pub array_id: RaidArrayId,
        pub extent_map: BTreeMap<u64, PhysicalExtent>,
    }
}

impl From<v1::VolumeMetadata> for VolumeMetadata {
    fn from(old: v1::VolumeMetadata) -> Self {
        VolumeMetadata {
            extent_size: old.extent_size,
            arrays: old.arrays,
            volumes: old
                .volumes
                .into_iter()
                .map(|v| VolumeRecord {
                    id: v.id,
                    name: v.name,
                    virtual_size: v.virtual_size,
                    array_id: Some(v.array_id),
                    // V1 never persisted slab extents; the GEM rebuild from
                    // slot tables covers everything V1 could express.
                    extents: BTreeMap::new(),
                    // Nothing older than V3 said, and the safe reading of
                    // silence is "keep": a volume thrown away because a format
                    // predates the question is not recoverable.
                    retention: Retention::Keep,
                    redundancy: RedundancyPolicy::none(),
                    parity: BTreeMap::new(),
                    failed_slabs: Vec::new(),
                    parent: None,
                    sealed: false,
                    fs: None,
                })
                .collect(),
        }
    }
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

    pub fn dir(&self) -> &Path {
        &self.data_dir
    }

    /// Serialize metadata into the binary envelope format.
    ///
    /// Public because the envelope — not the file — is the format. A slab
    /// that carries its own metadata stores exactly these bytes, so there is
    /// one encoder however the record is kept.
    pub fn encode(metadata: &VolumeMetadata) -> io::Result<Vec<u8>> {
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
    pub fn decode(data: &[u8]) -> io::Result<VolumeMetadata> {
        if data.len() < 32 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "metadata too short"));
        }

        // Check magic
        if data[0..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }

        // Every version that ever existed is decoded through its own shapes
        // and converted; see the note on `mod v2`.
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if !(1..=VERSION).contains(&version) {
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
        if version == 1 {
            let (old, _): (v1::VolumeMetadata, _) =
                bincode::serde::decode_from_slice(payload, bincode::config::standard())
                    .map_err(|e| io::Error::other(format!("bincode decode (v1): {e}")))?;
            return Ok(old.into());
        }
        if version == 2 {
            let (old, _): (v2::VolumeMetadata, _) =
                bincode::serde::decode_from_slice(payload, bincode::config::standard())
                    .map_err(|e| io::Error::other(format!("bincode decode (v2): {e}")))?;
            return Ok(old.into());
        }
        if version == 3 {
            let (old, _): (v3::VolumeMetadata, _) =
                bincode::serde::decode_from_slice(payload, bincode::config::standard())
                    .map_err(|e| io::Error::other(format!("bincode decode (v3): {e}")))?;
            return Ok(old.into());
        }
        if version == 4 {
            let (old, _): (v4::VolumeMetadata, _) =
                bincode::serde::decode_from_slice(payload, bincode::config::standard())
                    .map_err(|e| io::Error::other(format!("bincode decode (v4): {e}")))?;
            return Ok(old.into());
        }
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
        let slab_id = crate::drive::slab::SlabId(Uuid::new_v4());
        let mut extents = BTreeMap::new();
        extents.insert(0, ExtentLocation { slab_id, slot_idx: 3, ref_count: 2, generation: 1, mirrors: vec![] });
        extents.insert(1, ExtentLocation::new(slab_id, 9));

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
                array_id: Some(array_id),
                extents,
                retention: Retention::Keep,
                redundancy: RedundancyPolicy::none(),
                parity: BTreeMap::new(),
                failed_slabs: Vec::new(),
                parent: None,
                sealed: false,
                fs: None,
            }],
        }
    }

    /// A V4 record — redundancy but no lineage — loads with nothing sealed.
    #[test]
    fn a_v4_record_loads_with_no_lineage() {
        let slab_id = crate::drive::slab::SlabId(Uuid::new_v4());
        let mut extents = BTreeMap::new();
        extents.insert(1u64, ExtentLocation::new(slab_id, 5));
        let old = v4::VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![v4::VolumeRecord {
                id: VolumeId(Uuid::from_u128(44)),
                name: "four".into(),
                virtual_size: 1 << 30,
                array_id: None,
                extents,
                retention: Retention::Keep,
                redundancy: RedundancyPolicy::mirror(2),
                parity: BTreeMap::new(),
                failed_slabs: vec![slab_id],
            }],
        };
        let payload = bincode::serde::encode_to_vec(&old, bincode::config::standard()).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        let back = MetadataStore::decode(&buf).expect("a V4 record must still load");
        let v = &back.volumes[0];
        assert_eq!(v.redundancy, RedundancyPolicy::mirror(2));
        assert_eq!(v.failed_slabs, vec![slab_id]);
        assert!(v.parent.is_none() && !v.sealed && v.fs.is_none());
    }

    #[test]
    fn v5_lineage_round_trips() {
        let parent = VolumeId(Uuid::from_u128(1));
        let meta = VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![VolumeRecord {
                id: VolumeId(Uuid::from_u128(2)),
                name: "child".into(),
                virtual_size: 1 << 30,
                array_id: None,
                extents: BTreeMap::new(),
                retention: Retention::Keep,
                redundancy: RedundancyPolicy::none(),
                parity: BTreeMap::new(),
                failed_slabs: Vec::new(),
                parent: Some(parent),
                sealed: true,
                fs: Some(FsInfo {
                    kind: "ext4".into(),
                    journal: true,
                    features: Some("^64bit".into()),
                    sixty_four_bit: false,
                    metadata_csum: true,
                    csum_seed: true,
                    label: "root".into(),
                    uuid: Some(Uuid::from_u128(9)),
                }),
            }],
        };
        let back = MetadataStore::decode(&MetadataStore::encode(&meta).unwrap()).unwrap();
        let v = &back.volumes[0];
        assert_eq!(v.parent, Some(parent));
        assert!(v.sealed);
        assert_eq!(v.fs.as_ref().unwrap().uuid, Some(Uuid::from_u128(9)));
    }

    /// A V3 record — one leg per extent, no policy — must load as an
    /// unreplicated volume with its extents intact. Same reasoning as the V2
    /// test below: bincode has no defaults, so the old shape is decoded as
    /// itself.
    #[test]
    fn a_v3_record_loads_as_unreplicated() {
        let slab_id = crate::drive::slab::SlabId(Uuid::new_v4());
        let mut extents = BTreeMap::new();
        extents.insert(4u64, LegacyLocation { slab_id, slot_idx: 11, ref_count: 2, generation: 3 });
        let old = v3::VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![v3::VolumeRecord {
                id: VolumeId(Uuid::from_u128(3)),
                name: "three".into(),
                virtual_size: 1 << 30,
                array_id: None,
                extents,
                retention: Retention::Ephemeral,
            }],
        };
        let payload = bincode::serde::encode_to_vec(&old, bincode::config::standard()).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let back = MetadataStore::decode(&buf).expect("a V3 record must still load");
        let v = &back.volumes[0];
        assert_eq!(v.name, "three");
        assert_eq!(v.retention, Retention::Ephemeral);
        assert!(v.redundancy.is_none());
        let loc = &v.extents[&4];
        assert_eq!((loc.slab_id, loc.slot_idx, loc.ref_count, loc.generation), (slab_id, 11, 2, 3));
        assert!(loc.mirrors.is_empty());
        assert!(v.parity.is_empty() && v.failed_slabs.is_empty());
    }

    /// Mirrors, parity groups, the policy and the failed set all survive.
    #[test]
    fn v4_redundancy_round_trips() {
        let (a, b, c) = (
            crate::drive::slab::SlabId(Uuid::new_v4()),
            crate::drive::slab::SlabId(Uuid::new_v4()),
            crate::drive::slab::SlabId(Uuid::new_v4()),
        );
        let mut extents = BTreeMap::new();
        extents.insert(0u64, ExtentLocation::with_legs(
            crate::volume::gem::Leg::new(a, 1), vec![crate::volume::gem::Leg::new(b, 2)],
        ));
        let mut parity = BTreeMap::new();
        parity.insert(0u64, ParityGroup::new(vec![crate::volume::gem::Leg::new(c, 5)], 4));
        let meta = VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![VolumeRecord {
                id: VolumeId(Uuid::from_u128(4)),
                name: "four".into(),
                virtual_size: 1 << 30,
                array_id: None,
                extents,
                retention: Retention::Keep,
                redundancy: RedundancyPolicy::parse("raid5:4+1@shelf").unwrap(),
                parity,
                failed_slabs: vec![b],
                parent: None,
                sealed: false,
                fs: None,
            }],
        };
        let back = MetadataStore::decode(&MetadataStore::encode(&meta).unwrap()).unwrap();
        let v = &back.volumes[0];
        assert_eq!(v.redundancy.spelling(), "raid5:4+1@shelf");
        assert_eq!(v.extents[&0].mirrors, vec![crate::volume::gem::Leg::new(b, 2)]);
        assert_eq!(v.parity[&0].legs, vec![crate::volume::gem::Leg::new(c, 5)]);
        assert_eq!(v.failed_slabs, vec![b]);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let meta = test_metadata();
        let encoded = MetadataStore::encode(&meta).unwrap();

        // Verify header
        assert_eq!(&encoded[0..8], b"STRMVOL\0");
        let version = u32::from_le_bytes(encoded[8..12].try_into().unwrap());
        assert_eq!(version, VERSION);

        let decoded = MetadataStore::decode(&encoded).unwrap();
        assert_eq!(decoded.extent_size, meta.extent_size);
        assert_eq!(decoded.volumes.len(), 1);
        assert_eq!(decoded.volumes[0].name, "test-vol");
        assert_eq!(decoded.volumes[0].extents.len(), 2);
    }

    /// A V1 file (pre-#13) must still load: legacy shapes decode and convert,
    /// with empty slab extents (the GEM rebuild covers what V1 could express).
    #[test]
    fn v1_metadata_still_loads() {
        let array_id = RaidArrayId(Uuid::new_v4());
        let vol_id = VolumeId(Uuid::new_v4());
        let mut extent_map = BTreeMap::new();
        extent_map.insert(0, PhysicalExtent {
            array_id,
            offset: 0,
            length: 4 * 1024 * 1024,
            ref_count: 1,
        });
        let old = v1::VolumeMetadata {
            extent_size: 4 * 1024 * 1024,
            arrays: vec![ArrayRecord { array_id, total_capacity: 64 * 1024 * 1024 }],
            volumes: vec![v1::VolumeRecord {
                id: vol_id,
                name: "legacy".to_string(),
                virtual_size: 100 * 1024 * 1024,
                array_id,
                extent_map,
            }],
        };

        // Hand-build a version-1 envelope around the V1 payload.
        let payload =
            bincode::serde::encode_to_vec(&old, bincode::config::standard()).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let loaded = MetadataStore::decode(&buf).unwrap();
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.volumes[0].name, "legacy");
        assert_eq!(loaded.volumes[0].array_id, Some(array_id));
        assert!(loaded.volumes[0].extents.is_empty());
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
        assert_eq!(loaded.volumes[0].extents.len(), 2);
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

#[cfg(test)]
mod retention_tests {
    use super::*;

    fn v3_record(retention: Retention) -> VolumeMetadata {
        VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![VolumeRecord {
                id: VolumeId(uuid::Uuid::from_u128(7)),
                name: "data".into(),
                virtual_size: 1 << 30,
                array_id: None,
                extents: BTreeMap::new(),
                retention,
                redundancy: RedundancyPolicy::none(),
                parity: BTreeMap::new(),
                failed_slabs: Vec::new(),
                parent: None,
                sealed: false,
                fs: None,
            }],
        }
    }

    #[test]
    fn retention_survives_the_round_trip() {
        for r in [Retention::Keep, Retention::Ephemeral] {
            let bytes = MetadataStore::encode(&v3_record(r)).unwrap();
            let back = MetadataStore::decode(&bytes).unwrap();
            assert_eq!(back.volumes[0].retention, r);
        }
    }

    /// A record written before the question existed must still load, and must
    /// load as **keep**. bincode is not self-describing, so this is not a
    /// matter of a defaulted field — the older shape has to be decoded as
    /// itself and converted, or the decoder reads past the end of the record.
    #[test]
    fn a_v2_record_still_loads_and_is_kept() {
        // Encode a genuine V2 payload: the old shape, with the old version.
        let old = v2::VolumeMetadata {
            extent_size: 1 << 20,
            arrays: vec![],
            volumes: vec![v2::VolumeRecord {
                id: VolumeId(uuid::Uuid::from_u128(9)),
                name: "from-before".into(),
                virtual_size: 4096,
                array_id: None,
                extents: BTreeMap::new(),
            }],
        };
        let payload =
            bincode::serde::encode_to_vec(&old, bincode::config::standard()).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        let back = MetadataStore::decode(&buf).expect("a V2 record must still load");
        assert_eq!(back.volumes[0].name, "from-before");
        assert_eq!(
            back.volumes[0].retention,
            Retention::Keep,
            "silence must not throw data away"
        );
    }

    #[test]
    fn silence_means_keep() {
        assert_eq!(Retention::default(), Retention::Keep);
    }

    #[test]
    fn every_spelling_an_operator_might_write() {
        assert_eq!(Retention::parse("keep"), Some(Retention::Keep));
        assert_eq!(Retention::parse("persistent"), Some(Retention::Keep));
        assert_eq!(Retention::parse("Ephemeral"), Some(Retention::Ephemeral));
        assert_eq!(Retention::parse("throw-away"), Some(Retention::Ephemeral));
        assert_eq!(Retention::parse("scratch"), Some(Retention::Ephemeral));
        assert_eq!(Retention::parse("maybe"), None);
        for r in [Retention::Keep, Retention::Ephemeral] {
            assert_eq!(Retention::parse(r.as_str()), Some(r));
        }
    }
}
