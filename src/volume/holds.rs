//! What is serving a volume right now, as the volume manager sees it (#267).
//!
//! A volume served as a block device must not be deleted under whatever has
//! it mounted. The API's `DELETE` asked `what_is_serving` first, but some
//! twenty internal paths call [`VolumeManager::delete_volume`] directly, and
//! one of them, the serving layer's GC of an ephemeral export, deleted an
//! image clone that a node had attached over ublk and mounted under its
//! running containers. The containers' executables then read zeros and other
//! volumes' data (#267).
//!
//! So the refusal lives at the bottom. Whatever serves a volume takes a hold
//! on it here, and `delete_volume` refuses a held volume
//! ([`VolumeError::InUse`](super::thin::VolumeError::InUse)), whichever path
//! asked.
//!
//! [`VolumeManager::delete_volume`]: super::VolumeManager::delete_volume

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// Shared between the volume manager and whatever serves its volumes.
/// Cloning shares the same set.
#[derive(Clone, Default)]
pub struct ServeHolds(Arc<Mutex<HashMap<Uuid, Vec<String>>>>);

impl ServeHolds {
    /// `what` holds `volume`, until [`release`](Self::release) with the same
    /// words. Several holders may hold one volume.
    pub fn hold(&self, volume: Uuid, what: impl Into<String>) {
        self.0.lock().unwrap().entry(volume).or_default().push(what.into());
    }

    /// Release one hold `what` took. A release nobody took is a no-op.
    pub fn release(&self, volume: Uuid, what: &str) {
        let mut m = self.0.lock().unwrap();
        if let Some(v) = m.get_mut(&volume) {
            if let Some(i) = v.iter().position(|w| w == what) {
                v.remove(i);
            }
            if v.is_empty() {
                m.remove(&volume);
            }
        }
    }

    /// Who holds `volume`; empty when nothing does.
    pub fn held_by(&self, volume: Uuid) -> Vec<String> {
        self.0.lock().unwrap().get(&volume).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_count_by_holder() {
        let h = ServeHolds::default();
        let v = Uuid::new_v4();
        assert!(h.held_by(v).is_empty());
        h.hold(v, "/dev/ublkb3");
        h.clone().hold(v, "/dev/ublkb7");
        assert_eq!(h.held_by(v).len(), 2, "a clone shares the set");
        h.release(v, "/dev/ublkb3");
        assert_eq!(h.held_by(v), vec!["/dev/ublkb7".to_string()]);
        h.release(v, "nobody");
        h.release(v, "/dev/ublkb7");
        assert!(h.held_by(v).is_empty());
    }
}
