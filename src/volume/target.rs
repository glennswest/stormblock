//! A volume's placement target (#426): where its extents may live, narrower
//! than a tier.
//!
//! "Onto the shelf" or "off sdc" names slabs, not a tier: `to` is the set of
//! slabs every new extent of the volume goes to, `except` the slabs none
//! does. A move (`POST /api/v1/volumes/{id}/move`) sets the target and then
//! moves what is outside it; the target stays afterwards, so the guest's
//! later writes and copy-on-writes keep to it and the move does not undo
//! itself. It is a hard boundary, as a pin is (#150): a volume whose target
//! is full is refused space, never spilled back where it was moved from.
//!
//! The target is kept process-wide by volume id, as ANA states are
//! ([`crate::mgmt::ana`]), and every allocation of a volume asks it here, so
//! it applies whichever path restored the volume. It is written to
//! `<data_dir>/placement_targets.json` by the API and read back at start.
//! The volume records do not carry it: an engine of before #426 reads the
//! same slabs and simply places the volume as it always did.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabId;
use crate::volume::VolumeId;

/// What was asked, by name: a slab, a drive or an array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array: Option<String>,
}

impl Place {
    /// Exactly one of slab, drive, array.
    pub fn check(&self, field: &str) -> Result<(), String> {
        let n = [self.slab.is_some(), self.drive.is_some(), self.array.is_some()].iter().filter(|b| **b).count();
        if n == 1 {
            Ok(())
        } else {
            Err(format!("{field}: one of slab, drive or array"))
        }
    }

    pub fn describe(&self) -> String {
        match (&self.slab, &self.drive, &self.array) {
            (Some(s), _, _) => format!("slab {s}"),
            (_, Some(d), _) => format!("drive {d}"),
            (_, _, Some(a)) => format!("array {a}"),
            _ => "nothing".into(),
        }
    }
}

/// A volume's target: the slabs its extents go to (`to`, every slab when
/// absent) less the slabs they never go to (`except`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<BTreeSet<SlabId>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub except: BTreeSet<SlabId>,
    /// The names it was made from, for whoever reads it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_asked: Option<Place>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except_asked: Option<Place>,
}

impl Target {
    /// Whether an extent of the volume may be on `slab`.
    pub fn allows(&self, slab: &SlabId) -> bool {
        self.to.as_ref().is_none_or(|t| t.contains(slab)) && !self.except.contains(slab)
    }

    /// Whether this target constrains nothing.
    pub fn is_empty(&self) -> bool {
        self.to.is_none() && self.except.is_empty()
    }

    pub fn describe(&self) -> String {
        match (&self.to_asked, &self.except_asked) {
            (Some(t), Some(e)) => format!("onto {} except {}", t.describe(), e.describe()),
            (Some(t), None) => format!("onto {}", t.describe()),
            (None, Some(e)) => format!("off {}", e.describe()),
            (None, None) => "anywhere".into(),
        }
    }
}

static TARGETS: RwLock<Option<HashMap<VolumeId, Target>>> = RwLock::new(None);

/// The target of a volume, if one is set.
pub fn of(id: &VolumeId) -> Option<Target> {
    TARGETS.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(id).cloned())
}

/// Set (or with an empty target, clear) a volume's target, in this process.
pub fn set(id: VolumeId, target: Option<Target>) {
    let mut g = TARGETS.write().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match target.filter(|t| !t.is_empty()) {
        Some(t) => {
            m.insert(id, t);
        }
        None => {
            m.remove(&id);
        }
    }
}

/// Every target this process holds.
pub fn all() -> Vec<(VolumeId, Target)> {
    TARGETS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect())
        .unwrap_or_default()
}

const FILE: &str = "placement_targets.json";

pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// The process-wide lock on the file's read-modify-write.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_file(p: &Path) -> BTreeMap<String, Target> {
    match std::fs::read(p) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("{}: unreadable ({e}); no placement target restored from it", p.display());
                BTreeMap::new()
            }
        },
        Err(_) => BTreeMap::new(),
    }
}

/// Read the targets kept in `data_dir` into this process. Answers how many.
pub fn load(data_dir: &Path) -> usize {
    let all = read_file(&path_in(data_dir));
    let mut n = 0;
    for (k, t) in all {
        if let Ok(u) = k.parse::<uuid::Uuid>() {
            set(VolumeId(u), Some(t));
            n += 1;
        }
    }
    if n > 0 {
        tracing::info!("placement targets: {n} restored from {}", path_in(data_dir).display());
    }
    n
}

/// Set a volume's target and keep it in `data_dir` (none: in this process
/// only, as a node without a data directory keeps everything).
pub fn keep(data_dir: Option<&Path>, id: VolumeId, target: Option<Target>) -> anyhow::Result<()> {
    let target = target.filter(|t| !t.is_empty());
    if let Some(dir) = data_dir {
        let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = path_in(dir);
        let mut all = read_file(&p);
        match &target {
            Some(t) => {
                all.insert(id.0.to_string(), t.clone());
            }
            None => {
                all.remove(&id.0.to_string());
            }
        }
        let body = serde_json::to_vec_pretty(&all)?;
        crate::serve::wiring::write_atomic(&p, &body)?;
    }
    set(id, target);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u128) -> SlabId {
        SlabId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn a_target_allows_its_slabs_less_the_exceptions() {
        let t = Target { to: Some([s(1), s(2)].into()), except: [s(2)].into(), ..Default::default() };
        assert!(t.allows(&s(1)));
        assert!(!t.allows(&s(2)));
        assert!(!t.allows(&s(3)));
        let off = Target { except: [s(3)].into(), ..Default::default() };
        assert!(off.allows(&s(1)) && !off.allows(&s(3)));
        assert!(Target::default().is_empty());
    }

    #[test]
    fn a_kept_target_comes_back_and_a_cleared_one_does_not() {
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b) = (VolumeId(uuid::Uuid::new_v4()), VolumeId(uuid::Uuid::new_v4()));
        let t = Target { to: Some([s(7)].into()), to_asked: Some(Place { slab: Some("x".into()), ..Default::default() }), ..Default::default() };
        keep(Some(dir.path()), a, Some(t.clone())).unwrap();
        keep(Some(dir.path()), b, Some(t.clone())).unwrap();
        keep(Some(dir.path()), b, None).unwrap();
        set(a, None);
        assert_eq!(of(&a), None);
        assert_eq!(load(dir.path()), 1);
        assert_eq!(of(&a), Some(t));
        assert_eq!(of(&b), None);
    }
}
