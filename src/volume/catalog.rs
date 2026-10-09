//! What the volume listing reads, without the volume manager's lock (#364).
//!
//! Every create, clone, delete and seal takes the manager's mutex, and on the
//! Dell `GET /api/v1/volumes` waited 10–58 s behind them. A listing only
//! reads: which volumes, each one's handle, and the record the manager keeps
//! beside it (parent, filesystem, owner, origin, template). So the manager
//! publishes that as a [`Catalog`] every time it takes its records (every
//! persist), and readers take the latest one: one `Arc` cloned under a std
//! lock held for no longer than that, never waiting on a writer. What a
//! handle says (size, allocation, health) is read live from the handle.
//!
//! [`VolumeView`] is what the reader code takes, so the same code reads the
//! manager (where it already holds it) or a catalog.

use std::collections::{HashMap, HashSet};

use crate::drive::BlockDevice;
use crate::drive::slab::SlabId;
use crate::raid::RaidArrayId;
use std::sync::Arc;

use super::metadata::{Origin, Owner};
use super::thin::ThinVolumeHandle;
use super::{FsInfo, VolumeId};
use crate::drive::slab_registry::SlabRegistry;
use crate::lockwatch::TrackedRwLock;
use crate::volume::gem::GlobalExtentMap;

/// What a reader may ask of the volumes.
pub trait VolumeView {
    fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>>;
    fn owner(&self, id: &VolumeId) -> Option<&Owner>;
    fn parent(&self, id: &VolumeId) -> Option<VolumeId>;
    fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo>;
    fn origin(&self, id: &VolumeId) -> Origin;
    fn is_template(&self, id: &VolumeId) -> bool;
    fn is_sealed(&self, id: &VolumeId) -> bool {
        self.get_volume_handle(id).map(|h| h.is_sealed()).unwrap_or(false)
    }
    fn volume_ids(&self) -> Vec<VolumeId>;
    fn generation(&self) -> u64;
    fn gem(&self) -> &Arc<TrackedRwLock<GlobalExtentMap>>;
    fn registry(&self) -> &Arc<TrackedRwLock<SlabRegistry>>;
    fn array_of_slab(&self, slab: &SlabId) -> Option<RaidArrayId>;
    fn slot_size(&self) -> u64;
}

/// `(id, name, size, allocated)` for every volume, as
/// `VolumeManager::list_volumes` answers it.
pub async fn list_volumes(v: &impl VolumeView) -> Vec<(VolumeId, String, u64, u64)> {
    let mut out = Vec::new();
    for id in v.volume_ids() {
        if let Some(h) = v.get_volume_handle(&id) {
            out.push((id, h.name().await, h.capacity_bytes(), h.allocated().await));
        }
    }
    out
}

struct Entry {
    /// Weak: a catalog is not a holder. A map is evicted only when nothing
    /// outside the manager holds its volume (#155), and a published catalog
    /// must not keep every volume resident.
    handle: std::sync::Weak<ThinVolumeHandle>,
    parent: Option<VolumeId>,
    fs: Option<FsInfo>,
    owner: Option<Owner>,
    origin: Origin,
}

/// The volumes as the manager last published them.
pub struct Catalog {
    generation: u64,
    entries: HashMap<VolumeId, Entry>,
    array_slabs: Vec<(RaidArrayId, SlabId)>,
    slot_size: u64,
    templates: HashSet<VolumeId>,
    gem: Arc<TrackedRwLock<GlobalExtentMap>>,
    registry: Arc<TrackedRwLock<SlabRegistry>>,
}

impl Catalog {
    pub(super) fn of(m: &super::VolumeManager) -> Catalog {
        let entries = m
            .volume_ids()
            .into_iter()
            .filter_map(|id| {
                let handle = m.get_volume_handle(&id)?;
                Some((
                    id,
                    Entry {
                        handle: Arc::downgrade(&handle),
                        parent: m.parent(&id),
                        fs: m.fs_info(&id).cloned(),
                        owner: m.owner(&id).cloned(),
                        origin: m.origin(&id),
                    },
                ))
            })
            .collect();
        Catalog {
            generation: m.generation(),
            entries,
            templates: m.template_ids(),
            array_slabs: m.array_slab_pairs(),
            slot_size: m.slot_size(),
            gem: m.gem().clone(),
            registry: m.registry().clone(),
        }
    }
}

impl VolumeView for Catalog {
    fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>> {
        self.entries.get(id).and_then(|e| e.handle.upgrade())
    }
    fn owner(&self, id: &VolumeId) -> Option<&Owner> {
        self.entries.get(id).and_then(|e| e.owner.as_ref())
    }
    fn parent(&self, id: &VolumeId) -> Option<VolumeId> {
        self.entries.get(id).and_then(|e| e.parent)
    }
    fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo> {
        self.entries.get(id).and_then(|e| e.fs.as_ref())
    }
    fn origin(&self, id: &VolumeId) -> Origin {
        self.entries.get(id).map(|e| e.origin).unwrap_or_default()
    }
    fn is_template(&self, id: &VolumeId) -> bool {
        self.templates.contains(id)
    }
    fn volume_ids(&self) -> Vec<VolumeId> {
        self.entries.keys().copied().collect()
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn gem(&self) -> &Arc<TrackedRwLock<GlobalExtentMap>> {
        &self.gem
    }
    fn registry(&self) -> &Arc<TrackedRwLock<SlabRegistry>> {
        &self.registry
    }
    fn array_of_slab(&self, slab: &SlabId) -> Option<RaidArrayId> {
        self.array_slabs.iter().find(|(_, s)| s == slab).map(|(a, _)| *a)
    }
    fn slot_size(&self) -> u64 {
        self.slot_size
    }
}

impl VolumeView for super::VolumeManager {
    fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>> {
        super::VolumeManager::get_volume_handle(self, id)
    }
    fn owner(&self, id: &VolumeId) -> Option<&Owner> {
        super::VolumeManager::owner(self, id)
    }
    fn parent(&self, id: &VolumeId) -> Option<VolumeId> {
        super::VolumeManager::parent(self, id)
    }
    fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo> {
        super::VolumeManager::fs_info(self, id)
    }
    fn origin(&self, id: &VolumeId) -> Origin {
        super::VolumeManager::origin(self, id)
    }
    fn is_template(&self, id: &VolumeId) -> bool {
        super::VolumeManager::is_template(self, id)
    }
    fn is_sealed(&self, id: &VolumeId) -> bool {
        super::VolumeManager::is_sealed(self, id)
    }
    fn volume_ids(&self) -> Vec<VolumeId> {
        super::VolumeManager::volume_ids(self)
    }
    fn generation(&self) -> u64 {
        super::VolumeManager::generation(self)
    }
    fn gem(&self) -> &Arc<TrackedRwLock<GlobalExtentMap>> {
        super::VolumeManager::gem(self)
    }
    fn registry(&self) -> &Arc<TrackedRwLock<SlabRegistry>> {
        super::VolumeManager::registry(self)
    }
    fn array_of_slab(&self, slab: &SlabId) -> Option<RaidArrayId> {
        super::VolumeManager::array_of_slab(self, slab)
    }
    fn slot_size(&self) -> u64 {
        super::VolumeManager::slot_size(self)
    }
}

/// Where the manager publishes its catalog, and readers take it from.
#[derive(Clone, Default)]
pub struct CatalogCell(Arc<std::sync::RwLock<Option<Arc<Catalog>>>>);

impl CatalogCell {
    pub(super) fn publish(&self, c: Catalog) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
    }
    /// The latest catalog, `None` before the manager first published one.
    pub fn latest(&self) -> Option<Arc<Catalog>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The manager behind a lock guard reads as the manager, so code that
/// already holds it passes `&guard` as before.
macro_rules! view_through {
    ($t:ty) => {
        impl VolumeView for $t {
            fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>> {
                VolumeView::get_volume_handle(&**self, id)
            }
            fn owner(&self, id: &VolumeId) -> Option<&Owner> {
                VolumeView::owner(&**self, id)
            }
            fn parent(&self, id: &VolumeId) -> Option<VolumeId> {
                VolumeView::parent(&**self, id)
            }
            fn fs_info(&self, id: &VolumeId) -> Option<&FsInfo> {
                VolumeView::fs_info(&**self, id)
            }
            fn origin(&self, id: &VolumeId) -> Origin {
                VolumeView::origin(&**self, id)
            }
            fn is_template(&self, id: &VolumeId) -> bool {
                VolumeView::is_template(&**self, id)
            }
            fn is_sealed(&self, id: &VolumeId) -> bool {
                VolumeView::is_sealed(&**self, id)
            }
            fn volume_ids(&self) -> Vec<VolumeId> {
                VolumeView::volume_ids(&**self)
            }
            fn generation(&self) -> u64 {
                VolumeView::generation(&**self)
            }
            fn gem(&self) -> &Arc<TrackedRwLock<GlobalExtentMap>> {
                VolumeView::gem(&**self)
            }
            fn registry(&self) -> &Arc<TrackedRwLock<SlabRegistry>> {
                VolumeView::registry(&**self)
            }
            fn array_of_slab(&self, slab: &SlabId) -> Option<RaidArrayId> {
                VolumeView::array_of_slab(&**self, slab)
            }
            fn slot_size(&self) -> u64 {
                VolumeView::slot_size(&**self)
            }
        }
    };
}
view_through!(crate::lockwatch::TrackedMutexGuard<'_, super::VolumeManager>);
view_through!(crate::lockwatch::OwnedTrackedMutexGuard<super::VolumeManager>);
view_through!(Arc<Catalog>);

/// Whether `c` still names exactly the volumes that exist (#364): a volume
/// added or removed without a persist since is not in it yet, and then the
/// listing reads the manager instead.
pub fn is_current(c: &Catalog, present: &super::VolumePresence) -> bool {
    c.entries.len() == present.len() && c.entries.keys().all(|id| present.contains(id))
}
