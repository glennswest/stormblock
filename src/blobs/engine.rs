//! The blob store on the engine's own volume (#732).

use std::sync::Arc;

use async_trait::async_trait;

use crate::drive::slab::SlabRole;
use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::thin::ThinVolumeHandle;
use crate::volume::{CreateOptions, VolumeId};

use super::{Backing, BlobError, BlobStore, StoreConfig};

/// The volume the store lives in, in the default namespace.
pub const VOLUME_NAME: &str = "stormblock-blobs";

/// Its first size: it grows with what the store uses (the pool counts a
/// volume's size as promised, #152).
const INITIAL_SIZE: u64 = 1 << 30;

pub struct EngineBacking {
    state: Arc<AppState>,
    id: VolumeId,
    handle: Arc<ThinVolumeHandle>,
    dev: Arc<dyn BlockDevice>,
}

#[async_trait]
impl Backing for EngineBacking {
    fn device(&self) -> Arc<dyn BlockDevice> {
        self.dev.clone()
    }

    fn slot_size(&self) -> u64 {
        self.handle.extent_size()
    }

    fn volume(&self) -> Option<uuid::Uuid> {
        Some(self.id.0)
    }

    async fn mapped_slots(&self) -> Result<Vec<u64>, String> {
        self.handle.resident().await.map_err(|e| e.to_string())?;
        let n = self.dev.capacity_bytes().div_ceil(self.handle.extent_size());
        Ok(self.state.gem.read().await.mapped_in(self.id, 0..n))
    }

    async fn allocated_bytes(&self) -> u64 {
        self.handle.allocated().await
    }

    async fn pool_free_bytes(&self) -> u64 {
        let role = self.handle.placement_role();
        let reg = self.state.slab_registry.read().await;
        let free: u64 = match self.handle.pinned_slab() {
            Some(p) => reg.get(&p).map(|s| s.free_slots() * s.slot_size()).unwrap_or(0),
            None => reg
                .iter()
                .filter(|(id, s)| s.role() == role && !reg.is_quarantined(id))
                .map(|(_, s)| s.free_slots() * s.slot_size())
                .sum(),
        };
        free / self.handle.redundancy().scheme.copies().max(1) as u64
    }

    async fn grow(&self, new_size: u64) -> Result<(), String> {
        let mut vm = self.state.volume_manager.lock().await;
        vm.resize_volume(self.id, new_size).await.map_err(|e| e.to_string())
    }
}

/// The node's blob store: opened on first use, its volume made when `create`
/// (a put or a session) and there is none. `None`: no store, and nothing
/// asked to make one.
pub async fn store(state: &Arc<AppState>, create: bool) -> Result<Option<Arc<BlobStore>>, BlobError> {
    let mut cell = state.blobs.lock().await;
    if let Some(s) = cell.as_ref() {
        if s.is_broken() {
            // Opened again only when nothing holds it: a put of the old one
            // still writing could land in a slot the new one hands out.
            if Arc::strong_count(s) > 1 {
                return Err(BlobError::Unavailable(
                    "the blob store's journal failed; it is opened again once the requests on it end".into(),
                ));
            }
            tracing::warn!("blob store: opened again after its journal failed");
            *cell = None;
        }
    }
    if let Some(s) = cell.as_ref() {
        match s.backing.volume() {
            Some(v) if state.volume_presence.contains(&VolumeId(v)) => return Ok(Some(s.clone())),
            // Its volume was deleted: what it held is gone.
            _ => {
                tracing::warn!("blob store: its volume is gone; a new store is made on the next put");
                *cell = None;
            }
        }
    }
    let found = {
        let vm = state.volume_manager.lock().await;
        vm.named_in(crate::volume::metadata::DEFAULT_NAMESPACE, VOLUME_NAME, None).await
    };
    let id = match found.as_slice() {
        [id] => *id,
        [] if !create => return Ok(None),
        [] => {
            let role = {
                let reg = state.slab_registry.read().await;
                let data = reg.iter().any(|(_, s)| s.is_data());
                data.then_some(SlabRole::Data)
            };
            let mut vm = state.volume_manager.lock().await;
            let id = vm
                .create_volume_with(VOLUME_NAME, INITIAL_SIZE, CreateOptions::default().in_role_opt(role))
                .await
                .map_err(|e| match e {
                    crate::volume::VolumeError::NoSpace => BlobError::NoSpace(e.to_string()),
                    e => BlobError::Io(format!("making the blob store's volume: {e}")),
                })?;
            tracing::info!("blob store: volume {VOLUME_NAME} ({}) made", id.0);
            id
        }
        many => {
            return Err(BlobError::Invalid(format!(
                "{} volumes are named {VOLUME_NAME}: rename all but the store's",
                many.len()
            )))
        }
    };
    let (handle, dev) = {
        let vm = state.volume_manager.lock().await;
        match (vm.get_volume_handle(&id), vm.get_volume(&id)) {
            (Some(h), Some(d)) => (h, d),
            _ => return Err(BlobError::Io(format!("volume {} went away", id.0))),
        }
    };
    handle.resident().await.map_err(|e| BlobError::Io(format!("loading the store's volume: {e}")))?;
    let backing = Arc::new(EngineBacking { state: state.clone(), id, handle, dev });
    let s = BlobStore::open(backing, StoreConfig::default()).await?;
    *cell = Some(s.clone());
    Ok(Some(s))
}
