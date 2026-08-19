//! Image assembly — a bootable disk, built out of pallets.
//!
//! From `PALLET-SPEC.md`: *a disk image is a GPT plus a concatenation of
//! pallets*, because every pallet is self-contained and partition-relative.
//! Adding one is appending bytes and adding a GPT entry; removing one is
//! deleting an entry. Nothing inside is rewritten, and nothing is re-signed.
//!
//! ```text
//! ESP (FAT32)                 <- the floor; firmware needs FAT
//! pallet: stormcos-boot v2    <- kernel + initramfs + cmdline
//! pallet: stormcos-boot v1    <- the fallback, still intact
//! pallet: platform-core       <- container images
//! pallet: app-<name>          <- application members
//! stormblock slab             <- everything mutable
//! ```
//!
//! The builder does not reimplement any of that. **An image file is a drive**
//! to this engine — same GPT, same partitions, same code — so assembly opens
//! the file as a `FileDevice` and drives the ordinary `PalletManager`:
//! publishing into an image is the same operation as publishing onto a disk,
//! and every pallet is verified where it lands rather than where it was built.
//!
//! Output formats are conversions of the finished raw image, except the ISO,
//! which is a filesystem in its own right with the same partitions appended
//! behind it.

pub mod build;
pub mod fat;
pub mod formats;
pub mod iso;

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use build::{BuildReport, ImageBuilder};
pub use formats::ImageFormat;

/// Errors from image assembly.
#[derive(Debug)]
pub enum ImageError {
    Io(std::io::Error),
    Drive(crate::drive::DriveError),
    Pallet(crate::pallet::PalletError),
    /// The spec does not describe a buildable image.
    Spec(String),
    /// The declared contents do not fit the declared size.
    TooSmall { need: u64, have: u64 },
    Other(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "I/O error: {e}"),
            ImageError::Drive(e) => write!(f, "drive error: {e}"),
            ImageError::Pallet(e) => write!(f, "pallet error: {e}"),
            ImageError::Spec(m) => write!(f, "bad image spec: {m}"),
            ImageError::TooSmall { need, have } => write!(
                f,
                "image is {have} bytes and its contents need {need}: raise `size`, or drop it \
                 entirely and let the builder size the image"
            ),
            ImageError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        ImageError::Io(e)
    }
}
impl From<crate::drive::DriveError> for ImageError {
    fn from(e: crate::drive::DriveError) -> Self {
        ImageError::Drive(e)
    }
}
impl From<crate::pallet::PalletError> for ImageError {
    fn from(e: crate::pallet::PalletError) -> Self {
        ImageError::Pallet(e)
    }
}

pub type Result<T> = std::result::Result<T, ImageError>;

// ---------------------------------------------------------------- the spec

/// A whole image, as a TOML document.
///
/// Sizes are human strings (`512M`, `8G`). Exactly one partition may say
/// `rest`, and only when the image has an explicit `size` — otherwise there is
/// no "rest" to take.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageSpec {
    #[serde(default)]
    pub name: String,
    /// Total image size. Omit and the builder sizes it from the contents.
    #[serde(default)]
    pub size: Option<String>,
    /// LBA size for the partition table. 512 unless something needs otherwise
    /// — it is what every tool and every firmware assumes of an image.
    #[serde(default)]
    pub block_size: Option<u32>,
    #[serde(default)]
    pub esp: Option<EspSpec>,
    #[serde(default, rename = "pallet")]
    pub pallets: Vec<PalletEntry>,
    #[serde(default, rename = "partition")]
    pub partitions: Vec<RawPartition>,
    #[serde(default)]
    pub slab: Option<SlabPartition>,
}

/// The EFI System Partition — the floor, because firmware needs FAT.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EspSpec {
    /// Defaults to 100M, or the size of `from_image`.
    #[serde(default)]
    pub size: Option<String>,
    /// Build a FAT32 filesystem from this directory tree.
    #[serde(default)]
    pub from_dir: Option<PathBuf>,
    /// Or copy a filesystem image in verbatim.
    #[serde(default)]
    pub from_image: Option<PathBuf>,
    #[serde(default)]
    pub label: Option<String>,
}

/// One pallet in the image: either composed here, or taken from elsewhere.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PalletEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub version_label: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    /// Selection order. Higher wins; 0 never boots.
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub tries: Option<u8>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub sealed: Option<bool>,
    #[serde(default)]
    pub members: Vec<MemberEntry>,
    /// Copy a pallet that already exists, from another image or a drive.
    /// Byte for byte: nothing inside is rewritten and nothing is re-signed.
    #[serde(default)]
    pub from_image: Option<PathBuf>,
    /// Which one, when `from_image` holds several. Omit to take them all.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberEntry {
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// Content from a file…
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// …or written inline, for a kernel command line or a small config.
    #[serde(default)]
    pub text: Option<String>,
}

/// A partition that is not a pallet: firmware blobs, a data image, anything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawPartition {
    pub name: String,
    #[serde(default)]
    pub size: Option<String>,
    /// `esp`, `linux`, `swap`, `basic`, or an explicit GUID.
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub from_file: Option<PathBuf>,
}

/// The mutable end of the image: a formatted slab, usually taking the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlabPartition {
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub slot_size: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
}

impl ImageSpec {
    pub fn from_toml(text: &str) -> Result<ImageSpec> {
        toml::from_str(text).map_err(|e| ImageError::Spec(e.to_string()))
    }

    pub async fn load(path: impl AsRef<std::path::Path>) -> Result<ImageSpec> {
        let text = tokio::fs::read_to_string(path.as_ref()).await?;
        ImageSpec::from_toml(&text)
    }
}

// --------------------------------------------------------- partition types

/// Well-known GPT partition type GUIDs, in the mixed-endian form GPT stores.
pub mod type_guid {
    /// EFI System Partition — `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
    pub const ESP: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];
    /// Linux filesystem data — `0FC63DAF-8483-4772-8E79-3D69D8477DE4`.
    pub const LINUX: [u8; 16] = [
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D,
        0xE4,
    ];
    /// Linux swap — `0657FD6D-A4AB-43C4-84E5-0933C84B4F4F`.
    pub const SWAP: [u8; 16] = [
        0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F,
        0x4F,
    ];
    /// Microsoft basic data — `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`.
    pub const BASIC: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    /// A stormblock slab — `4C9A7B2E-1D63-4F8A-9E51-0B7C2A6D3F14`.
    pub const SLAB: [u8; 16] = [
        0x2E, 0x7B, 0x9A, 0x4C, 0x63, 0x1D, 0x8A, 0x4F, 0x9E, 0x51, 0x0B, 0x7C, 0x2A, 0x6D, 0x3F,
        0x14,
    ];

    /// Resolve a name or an explicit GUID from a spec.
    pub fn parse(s: &str) -> Option<[u8; 16]> {
        match s.to_ascii_lowercase().as_str() {
            "esp" | "efi" => Some(ESP),
            "linux" | "linux-data" => Some(LINUX),
            "swap" => Some(SWAP),
            "basic" | "msdata" | "fat" => Some(BASIC),
            "slab" | "stormblock" => Some(SLAB),
            "pallet" => Some(crate::pallet::PALLET_TYPE_GUID),
            other => uuid::Uuid::parse_str(other).ok().map(|u| u.to_bytes_le()),
        }
    }
}
