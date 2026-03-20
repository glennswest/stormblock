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
}

impl fmt::Display for StorageTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageTier::Hot => write!(f, "hot"),
            StorageTier::Warm => write!(f, "warm"),
            StorageTier::Cool => write!(f, "cool"),
            StorageTier::Cold => write!(f, "cold"),
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
    }
}
