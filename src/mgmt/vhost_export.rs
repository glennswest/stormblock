//! vhost-user-blk exports (#82): a volume served to a VMM on this node over
//! a Unix socket, the guest's virtqueues handed to the engine
//! ([`crate::drive::vhost_user_blk`]).
//!
//! One socket per volume, at `<data_dir>/vhost/<volume id>.sock` (or
//! `/run/stormblock/vhost/` with no data dir). Like a ublk device, the
//! socket holds its volume (#267): the volume manager refuses to delete a
//! volume a guest may be using. Asked for by name only
//! (`transport: vhost-user-blk` on `POST /api/v1/volumes/{id}/attach`):
//! never the engine's choice, since only a VMM can use it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use crate::drive::vhost_user_blk::{BlkSpec, VhostUserBlk};
use crate::drive::BlockDevice;

/// What the volume's hold is called, in a refusal and in `/debug`.
const HOLD: &str = "a vhost-user-blk socket";

struct Export {
    blk: Arc<VhostUserBlk>,
    read_only: bool,
}

/// The live vhost-user-blk sockets on this node.
pub struct VhostExports {
    dir: PathBuf,
    exports: HashMap<Uuid, Export>,
    holds: crate::volume::holds::ServeHolds,
}

/// A socket's state, for the attach answer and `GET …/attach`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VhostInfo {
    pub socket: String,
    pub read_only: bool,
    /// A VMM is connected right now.
    pub connected: bool,
    pub requests: u64,
}

impl VhostExports {
    pub fn new(data_dir: Option<&std::path::Path>, holds: crate::volume::holds::ServeHolds) -> Self {
        let dir = match data_dir {
            Some(d) => d.join("vhost"),
            None => PathBuf::from("/run/stormblock/vhost"),
        };
        VhostExports { dir, exports: HashMap::new(), holds }
    }

    pub fn is_exported(&self, volume: Uuid) -> bool {
        self.exports.contains_key(&volume)
    }

    pub fn info(&self, volume: Uuid) -> Option<VhostInfo> {
        self.exports.get(&volume).map(|e| VhostInfo {
            socket: e.blk.path().display().to_string(),
            read_only: e.read_only,
            connected: e.blk.counters.connected.load(std::sync::atomic::Ordering::SeqCst),
            requests: e.blk.counters.requests.load(std::sync::atomic::Ordering::Relaxed),
        })
    }

    /// Serve `dev` on its socket; the socket's path. Attaching again in the
    /// same mode is the same socket; in the other mode is refused, since the
    /// guest that may be connected negotiated the first.
    pub fn attach(&mut self, volume: Uuid, dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<VhostInfo, String> {
        if let Some(e) = self.exports.get(&volume) {
            if e.read_only != read_only {
                return Err(format!(
                    "volume {volume} is already served over vhost-user-blk {}: detach it first",
                    if e.read_only { "read-only" } else { "read-write" }
                ));
            }
            return Ok(self.info(volume).expect("present"));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let path = self.dir.join(format!("{volume}.sock"));
        // sun_path is 108 bytes with its NUL.
        if path.as_os_str().len() > 107 {
            return Err(format!("{}: too long for a Unix socket path", path.display()));
        }
        let spec = BlkSpec { read_only, serial: volume.simple().to_string() };
        let blk = VhostUserBlk::serve(&path, dev, spec, tokio::runtime::Handle::current())
            .map_err(|e| format!("serving {}: {e}", path.display()))?;
        self.holds.hold(volume, HOLD);
        self.exports.insert(volume, Export { blk: Arc::new(blk), read_only });
        Ok(self.info(volume).expect("just inserted"))
    }

    /// Stop serving a volume: the VMM is disconnected and the socket goes.
    /// What to wait on is handed back, so the caller can do it off the
    /// runtime's threads.
    pub fn detach(&mut self, volume: Uuid) -> Option<Arc<VhostUserBlk>> {
        let e = self.exports.remove(&volume)?;
        self.holds.release(volume, HOLD);
        Some(e.blk)
    }

    /// Every socket, for a daemon stop.
    pub fn detach_all(&mut self) -> Vec<Arc<VhostUserBlk>> {
        let ids: Vec<Uuid> = self.exports.keys().copied().collect();
        ids.into_iter().filter_map(|v| self.detach(v)).collect()
    }
}

/// Stop the sockets [`VhostExports::detach`] handed back, off the runtime.
pub async fn stop(blks: Vec<Arc<VhostUserBlk>>) {
    if blks.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for b in blks {
            b.stop(std::time::Duration::from_secs(5));
        }
    })
    .await;
}
