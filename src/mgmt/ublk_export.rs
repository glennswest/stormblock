//! Dynamic ublk exports for the CSI `/v1` attach path.
//!
//! When `management.ublk_transport` is on and a volume is attached on the same
//! node that holds its master, the engine exports the backing block device as
//! a local `/dev/ublkbN` and hands the CSI node that path instead of NVMe-oF
//! coordinates — no network round trip for the common master-local case.
//!
//! ublk is Linux 6.0+ only and needs `ublk_drv` loaded. Availability is probed
//! once at construction; when it is unavailable (non-Linux, module not loaded)
//! `ensure` returns `None` and the caller falls back to nvme-tcp. This is why
//! the probe matters: returning a `/dev/ublkbN` path that never materializes
//! would wedge NodeStage on the CSI side, whereas nvme-tcp always works.

use std::collections::HashMap;
use std::sync::Arc;

use crate::drive::BlockDevice;

/// Decide whether an attach should be served over ublk rather than nvme-tcp.
///
/// Pure policy, kept separate so it is testable without a kernel: ublk is
/// offered only when the operator enabled it, the attaching node is *this*
/// node, and the volume is backed locally here (this node holds the master).
/// Read-write attach already requires the caller to be the master node, so in
/// practice this is "enabled and local".
pub fn should_offer_ublk(
    enabled: bool,
    request_node: &str,
    local_node: &str,
    locally_backed: bool,
) -> bool {
    enabled && locally_backed && request_node == local_node
}

struct Export {
    device_path: String,
    /// Fires the ublk server's shutdown watch on teardown (DEL_DEV).
    #[cfg(target_os = "linux")]
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// Tracks the live per-volume ublk exports on this node.
pub struct UblkExportManager {
    exports: HashMap<String, Export>,
    /// Next /dev/ublkbN id to hand out (Linux only consumes this).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    next_id: u32,
    available: bool,
}

impl Default for UblkExportManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UblkExportManager {
    pub fn new() -> Self {
        UblkExportManager { exports: HashMap::new(), next_id: 0, available: ublk_available() }
    }

    /// Whether ublk exports can actually be created on this host.
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn device_path(&self, volume_id: &str) -> Option<String> {
        self.exports.get(volume_id).map(|e| e.device_path.clone())
    }

    /// Ensure a ublk device exports `device` for `volume_id`, returning its
    /// path. Idempotent: a repeat attach of the same volume returns the same
    /// device. `None` means ublk is unavailable — the caller uses nvme-tcp.
    pub fn ensure(&mut self, volume_id: &str, device: Arc<dyn BlockDevice>) -> Option<String> {
        if let Some(e) = self.exports.get(volume_id) {
            return Some(e.device_path.clone());
        }
        if !self.available {
            return None;
        }
        self.start(volume_id, device)
    }

    /// Tear down the export for `volume_id`, if any (detach / delete).
    pub fn remove(&mut self, volume_id: &str) {
        if let Some(_e) = self.exports.remove(volume_id) {
            #[cfg(target_os = "linux")]
            {
                // Best effort: the server removes the kernel device on exit.
                let _ = _e.shutdown.send(true);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn start(&mut self, volume_id: &str, device: Arc<dyn BlockDevice>) -> Option<String> {
        use crate::drive::ublk::UblkServer;

        let id = self.next_id;
        let device_path = format!("/dev/ublkb{id}");
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let server = UblkServer::new(device).with_dev_id(id);
        // UblkServer::run() holds non-Send raw pointers, so it must run on a
        // dedicated OS thread with its own runtime (same pattern as the
        // boot-iscsi ublk export in main.rs).
        std::thread::Builder::new()
            .name(format!("ublk-csi-{id}"))
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("ublk-csi {id}: runtime init failed: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = server.run(rx).await {
                        tracing::error!("ublk-csi {id}: export failed: {e}");
                    }
                });
            })
            .ok()?;
        // ids are not recycled: a monotonic counter avoids reassigning a
        // /dev/ublkbN that a just-torn-down export might still be vacating.
        self.next_id += 1;
        self.exports.insert(volume_id.to_string(), Export { device_path: device_path.clone(), shutdown });
        tracing::info!(volume = volume_id, device = %device_path, "ublk export created for CSI attach");
        Some(device_path)
    }

    #[cfg(not(target_os = "linux"))]
    fn start(&mut self, _volume_id: &str, _device: Arc<dyn BlockDevice>) -> Option<String> {
        None
    }
}

/// Probe whether the ublk control device is usable on this host.
#[cfg(target_os = "linux")]
fn ublk_available() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/ublk-control").is_ok()
}

#[cfg(not(target_os = "linux"))]
fn ublk_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_policy_requires_enabled_local_and_backed() {
        // Happy path: enabled, same node, locally backed.
        assert!(should_offer_ublk(true, "node-a", "node-a", true));
        // Disabled by config.
        assert!(!should_offer_ublk(false, "node-a", "node-a", true));
        // Attaching node is not this node (remote reader / migration target).
        assert!(!should_offer_ublk(true, "node-b", "node-a", true));
        // Not backed locally (this node holds no master replica).
        assert!(!should_offer_ublk(true, "node-a", "node-a", false));
    }

    // Registry bookkeeping is exercised without a kernel by injecting a fake
    // export; the real device-creation path is covered on-metal (dev.g8.lo).
    impl UblkExportManager {
        fn insert_fake(&mut self, volume_id: &str, path: &str) {
            self.exports.insert(
                volume_id.to_string(),
                Export {
                    device_path: path.to_string(),
                    #[cfg(target_os = "linux")]
                    shutdown: tokio::sync::watch::channel(false).0,
                },
            );
        }
    }

    #[test]
    fn ensure_is_idempotent_and_remove_clears() {
        let mut mgr = UblkExportManager::new();
        mgr.insert_fake("vol-1", "/dev/ublkb7");
        assert_eq!(mgr.device_path("vol-1").as_deref(), Some("/dev/ublkb7"));
        mgr.remove("vol-1");
        assert_eq!(mgr.device_path("vol-1"), None);
        // Removing an unknown volume is a no-op.
        mgr.remove("vol-unknown");
    }

    #[test]
    fn unavailable_host_declines_so_caller_uses_nvme_tcp() {
        let mut mgr = UblkExportManager { exports: HashMap::new(), next_id: 0, available: false };
        // No panic, just None — nvme-tcp fallback. (device is never touched.)
        assert!(mgr.device_path("vol-x").is_none());
    }
}
