//! Target protocol layer — NVMe-oF/TCP and iSCSI.
//!
//! Each protocol is served by a dedicated module that accepts connections
//! and dispatches I/O to `BlockDevice` volumes via the reactor pool.

pub mod reactor;

#[cfg(feature = "iscsi")]
pub mod iscsi;

#[cfg(feature = "nvmeof")]
pub mod nvmeof;

use std::collections::HashMap;
use std::sync::Arc;

use crate::drive::BlockDevice;

/// Configuration for all target protocols.
#[derive(Debug, Clone)]
pub struct TargetConfig {
    pub reactor: reactor::ReactorConfig,
    #[cfg(feature = "iscsi")]
    pub iscsi: Option<iscsi::IscsiConfig>,
    #[cfg(feature = "nvmeof")]
    pub nvmeof: Option<nvmeof::NvmeofConfig>,
}

/// A volume exported via target protocols, mapped by LUN or namespace ID.
pub struct ExportedVolume {
    pub name: String,
    pub device: Arc<dyn BlockDevice>,
}

/// Registry of exported volumes shared between target protocols.
pub type VolumeMap = Arc<HashMap<u64, ExportedVolume>>;
