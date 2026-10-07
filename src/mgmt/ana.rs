//! A volume's NVMe ANA state on this node, kept across restarts (#83).
//!
//! The state itself lives in the target ([`crate::target::nvmeof::ana`]),
//! where every subsystem serving the volume reads it. This keeps it in
//! `<data_dir>/ana.json`: a node told that a volume is `inaccessible` here
//! (the node it moved away from) must not come back from a restart saying
//! `optimized`, or a host with both paths writes to the side it left.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use uuid::Uuid;

use crate::mgmt::config::StormBlockConfig;
use crate::target::nvmeof::ana::{self, AnaState};

fn path(config: &StormBlockConfig) -> Option<PathBuf> {
    config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join("ana.json"))
}

/// Put the kept states back, before anything is served.
pub fn load(config: &StormBlockConfig) {
    let Some(p) = path(config) else { return };
    let Ok(bytes) = std::fs::read(&p) else { return };
    match serde_json::from_slice::<HashMap<Uuid, AnaState>>(&bytes) {
        Ok(m) => {
            if !m.is_empty() {
                tracing::info!("NVMe ANA: {} volume(s) not optimized on this node", m.len());
            }
            ana::load(m);
        }
        Err(e) => tracing::error!(
            "NVMe ANA: {} does not parse ({e}); every volume reads optimized",
            p.display()
        ),
    }
}

/// Set a volume's state and keep it, written down before any host is told:
/// a state that could not be kept is not applied. True when it changed.
pub fn set(config: &StormBlockConfig, volume: Uuid, state: AnaState) -> std::io::Result<bool> {
    if let Some(p) = path(config) {
        let mut all: BTreeMap<Uuid, AnaState> = ana::all().into_iter().collect();
        if state == AnaState::default() {
            all.remove(&volume);
        } else {
            all.insert(volume, state);
        }
        let bytes = serde_json::to_vec_pretty(&all).map_err(std::io::Error::other)?;
        let tmp = p.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &p)?;
    }
    let changed = ana::set(volume, state);
    if changed {
        tracing::info!(%volume, "NVMe ANA state on this node: {}", state.as_str());
    }
    Ok(changed)
}
