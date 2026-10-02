//! Hot spares: drives held back, each in a pool — a shelf's name, or the
//! global pool (`""`). A set whose member fails takes a spare from its own
//! pool first, then from the global one (#252).
//!
//! A spare carries a spare superblock (level 0, no array) naming its pool, so
//! a restart finds it again and nothing formats over it by accident.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use super::superblock::{Superblock, SUPERBLOCK_BYTES};
use super::RaidError;
use crate::drive::BlockDevice;

#[derive(Clone)]
pub struct Spare {
    /// The spare's own uuid (its superblock's member uuid).
    pub uuid: Uuid,
    /// "" = global.
    pub pool: String,
    pub device: Arc<dyn BlockDevice>,
}

#[derive(Default)]
pub struct SparePool {
    spares: Mutex<Vec<Spare>>,
    /// Signalled when a spare is added, for the arrays' supervisors.
    pub arrived: Arc<tokio::sync::Notify>,
}

impl SparePool {
    pub fn new() -> Arc<Self> {
        Arc::new(SparePool::default())
    }

    /// Make a drive a spare in `pool`: write its spare superblock.
    pub async fn add(&self, device: Arc<dyn BlockDevice>, pool: &str) -> Result<Uuid, RaidError> {
        super::superblock::check_name("pool", pool)?;
        if self.holds(&device) {
            return Err(RaidError::InvalidStripe(format!("{} is already a spare", device.id().path)));
        }
        let uuid = Uuid::new_v4();
        let sb = Superblock::spare(uuid, pool);
        device.write(0, &sb.to_bytes()).await?;
        device.flush().await?;
        self.spares.lock().unwrap().push(Spare { uuid, pool: pool.to_string(), device });
        self.arrived.notify_waiters();
        Ok(uuid)
    }

    /// Take back a spare found on a drive at startup.
    pub fn adopt(&self, device: Arc<dyn BlockDevice>, sb: &Superblock) {
        if self.holds(&device) {
            return;
        }
        self.spares.lock().unwrap().push(Spare { uuid: sb.member_uuid, pool: sb.pool.clone(), device });
        self.arrived.notify_waiters();
    }

    /// Stop holding a spare, wiping its superblock.
    pub async fn remove(&self, uuid: Uuid) -> Result<Spare, RaidError> {
        let spare = {
            let mut s = self.spares.lock().unwrap();
            let i = s
                .iter()
                .position(|x| x.uuid == uuid)
                .ok_or_else(|| RaidError::InvalidStripe(format!("no spare {uuid}")))?;
            s.remove(i)
        };
        spare.device.write(0, &vec![0u8; SUPERBLOCK_BYTES]).await?;
        spare.device.flush().await?;
        Ok(spare)
    }

    pub fn list(&self) -> Vec<Spare> {
        self.spares.lock().unwrap().clone()
    }

    /// Whether a drive is one of the spares.
    pub fn holds(&self, device: &Arc<dyn BlockDevice>) -> bool {
        let id = device.id();
        self.spares.lock().unwrap().iter().any(|s| s.device.id().uuid == id.uuid && s.device.id().path == id.path)
    }

    /// Hand out a spare of at least `min_bytes`: the smallest that fits, from
    /// `pool` if it has one, else from the global pool. A pool's spares are
    /// never given to another pool's array.
    pub fn take(&self, pool: &str, min_bytes: u64) -> Option<Spare> {
        let mut s = self.spares.lock().unwrap();
        for want in [pool, ""] {
            let best = s
                .iter()
                .enumerate()
                .filter(|(_, x)| x.pool == want && x.device.capacity_bytes() >= min_bytes)
                .min_by_key(|(_, x)| x.device.capacity_bytes())
                .map(|(i, _)| i);
            if let Some(i) = best {
                return Some(s.remove(i));
            }
            if pool.is_empty() {
                break;
            }
        }
        None
    }
}
