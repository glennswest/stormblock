//! Storage topology — device classification by tier and locality.
//!
//! The placement engine uses topology to decide where data should live.
//! Devices closer to compute (lower latency, local) are preferred for hot data.
//! Remote and slower devices serve as cold/backup targets.

use std::fmt;
use std::sync::Arc;

use serde::{Serialize, Deserialize};
use uuid::Uuid;

use crate::drive::BlockDevice;

/// Storage tier — classifies a device by its performance/cost position.
///
/// Lower tier number = hotter (faster, closer to compute, more expensive).
/// Data flows from cold to hot (attraction toward compute) and from
/// hot to cold (backup/archive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum StorageTier {
    /// Tier 0: Directly attached NVMe, local PCIe SSD — closest to CPU.
    Hot = 0,
    /// Tier 1: Local SAS SSD, SATA SSD — fast but not fastest.
    Warm = 1,
    /// Tier 2: Remote fast storage (iSCSI SSD, NVMe-oF) or local HDD.
    Cool = 2,
    /// Tier 3: Archive/backup — remote HDD, object storage, tape.
    Cold = 3,
    /// Opt-in (#672): a drive set aside for what asks for it by name — the
    /// small SSD beside a blade's system HDD, the build VMs' private drive.
    /// Never in a default placement or any fallback chain, so nothing lands
    /// on it unasked. Byte 4 on disk: an engine before #672 reads a `fast`
    /// slab as `cold` and a `fast` volume as `hot`.
    Fast = 4,
}

impl StorageTier {
    /// Every tier, in the order a fallback chain walks them. `Fast` is not
    /// in it: it is reached only by asking for it (#672).
    pub const SHARED: [StorageTier; 4] = [StorageTier::Hot, StorageTier::Warm, StorageTier::Cool, StorageTier::Cold];

    /// The tier a slab header or a volume record's byte names; `None` for a
    /// byte no tier has.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(StorageTier::Hot),
            1 => Some(StorageTier::Warm),
            2 => Some(StorageTier::Cool),
            3 => Some(StorageTier::Cold),
            4 => Some(StorageTier::Fast),
            _ => None,
        }
    }

    /// A tier by name (`hot`, `warm`, `cool`, `cold`, `fast`), any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hot" => Some(StorageTier::Hot),
            "warm" => Some(StorageTier::Warm),
            "cool" => Some(StorageTier::Cool),
            "cold" => Some(StorageTier::Cold),
            "fast" => Some(StorageTier::Fast),
            _ => None,
        }
    }

    /// The names [`Self::parse`] takes, for a refusal's words.
    pub const NAMES: &'static str = "hot, warm, cool, cold or fast";

    /// Whether only a volume that names this tier may allocate on it (#672).
    pub fn opt_in(self) -> bool {
        self == StorageTier::Fast
    }
}

impl fmt::Display for StorageTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageTier::Hot => write!(f, "hot"),
            StorageTier::Warm => write!(f, "warm"),
            StorageTier::Cool => write!(f, "cool"),
            StorageTier::Cold => write!(f, "cold"),
            StorageTier::Fast => write!(f, "fast"),
        }
    }
}

/// How close a device is to the compute node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Locality {
    /// Directly attached (PCIe, SATA, USB, local file).
    Local,
    /// Reachable over network.
    Remote {
        /// Network address (e.g., "192.168.1.50:3260").
        addr: String,
        /// Estimated round-trip latency in microseconds.
        latency_us: u32,
    },
}

impl fmt::Display for Locality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Locality::Local => write!(f, "local"),
            Locality::Remote { addr, latency_us } => {
                write!(f, "remote({}, ~{}us)", addr, latency_us)
            }
        }
    }
}

impl Locality {
    /// Is this device locally attached?
    pub fn is_local(&self) -> bool {
        matches!(self, Locality::Local)
    }

    /// Estimated latency for sorting/selection (local = 1us).
    pub fn latency_us(&self) -> u32 {
        match self {
            Locality::Local => 1,
            Locality::Remote { latency_us, .. } => *latency_us,
        }
    }
}

/// A storage device known to the placement engine.
///
/// Wraps a `BlockDevice` with topology metadata (tier, locality, priority).
/// The placement engine uses this to decide where to put data.
pub struct StorageDevice {
    pub id: Uuid,
    pub name: String,
    pub device: Arc<dyn BlockDevice>,
    pub tier: StorageTier,
    pub locality: Locality,
    /// Priority: higher = more attractive for data placement.
    /// Local devices get high priority. Remote archive gets low priority.
    pub priority: i32,
}

impl StorageDevice {
    pub fn new(
        name: impl Into<String>,
        device: Arc<dyn BlockDevice>,
        tier: StorageTier,
        locality: Locality,
    ) -> Self {
        let priority = match (&tier, &locality) {
            (StorageTier::Hot, Locality::Local) => 100,
            (StorageTier::Warm, Locality::Local) => 75,
            (StorageTier::Cool, Locality::Local) => 50,
            (StorageTier::Hot, Locality::Remote { .. }) => 40,
            (StorageTier::Warm, Locality::Remote { .. }) => 30,
            (StorageTier::Cool, Locality::Remote { .. }) => 20,
            (StorageTier::Cold, _) => 10,
            // Opt-in (#672): never attractive to anything that did not ask.
            (StorageTier::Fast, _) => 0,
        };
        StorageDevice {
            id: Uuid::new_v4(),
            name: name.into(),
            device,
            tier,
            locality,
            priority,
        }
    }

    pub fn capacity(&self) -> u64 {
        self.device.capacity_bytes()
    }
}

impl fmt::Display for StorageDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f, "{} [{}] {} priority={} cap={}",
            self.name, self.tier, self.locality, self.priority,
            self.device.capacity_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ordering() {
        assert!(StorageTier::Hot < StorageTier::Warm);
        assert!(StorageTier::Warm < StorageTier::Cool);
        assert!(StorageTier::Cool < StorageTier::Cold);
    }

    #[test]
    fn locality_latency() {
        assert_eq!(Locality::Local.latency_us(), 1);
        let remote = Locality::Remote { addr: "1.2.3.4:3260".into(), latency_us: 500 };
        assert_eq!(remote.latency_us(), 500);
        assert!(Locality::Local.is_local());
        assert!(!remote.is_local());
    }

    #[test]
    fn tier_display() {
        assert_eq!(StorageTier::Hot.to_string(), "hot");
        assert_eq!(StorageTier::Cold.to_string(), "cold");
        assert_eq!(StorageTier::Fast.to_string(), "fast");
    }

    /// #672: `fast` by name and by byte, and never in the shared chain.
    #[test]
    fn fast_is_named_read_and_opt_in() {
        for t in StorageTier::SHARED.into_iter().chain([StorageTier::Fast]) {
            assert_eq!(StorageTier::parse(&t.to_string()), Some(t));
            assert_eq!(StorageTier::from_byte(t as u8), Some(t));
        }
        assert_eq!(StorageTier::parse(" FAST "), Some(StorageTier::Fast));
        assert_eq!(StorageTier::parse("ssd"), None);
        assert_eq!(StorageTier::from_byte(5), None);
        assert!(!StorageTier::SHARED.contains(&StorageTier::Fast));
        assert!(StorageTier::Fast.opt_in() && !StorageTier::Hot.opt_in());
    }
}
