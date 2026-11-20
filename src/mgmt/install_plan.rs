//! The plan an install with this boot's install words would make over the
//! drives the running node sees now (#623): what the NodeInstall object
//! shows beside the install record.
//!
//! `/init` keeps the install-config's words in
//! `/run/stormblock/install-intent.yaml` (`stormblock image install-intent`:
//! `mode`, `system_drive`, `storage:` without its wipe word) before it deletes
//! the file. Here they are read back and `image::storage::plan`, the code
//! `/init` lays drives by, runs over the drives sysfs lists, described as
//! `/init` describes them (`image::storage::seen_in`):
//!
//! - **this node's own drives** (a slab open in this engine is on it, or it
//!   is a member of the node-data set the layout names) carry nothing, as
//!   `/init` counts them on a keep-data reinstall;
//! - **any other** carries what is on it: our slabs (`stormblock slabs (an
//!   install)`), else the signatures in its first and last MiB
//!   (`drive_offer::survey`); blank is nothing;
//! - **a drive being formatted** (#621) is not read: it carries `a format
//!   running`.
//!
//! Nothing is laid: this is what the plan says, not an action. The drives
//! are read again only when the words or the drives sysfs lists change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::image::storage::{plan, seen_in, top_level, SeenDrive, StorageSpec, Yaml};
use crate::mgmt::AppState;

/// The words' file under `/run/stormblock`.
pub const INTENT: &str = "install-intent.yaml";

/// The last plan and what it was made from.
#[derive(Default)]
pub struct Cache {
    key: Option<String>,
    value: Option<Value>,
}

fn word(doc: &str, key: &str) -> Option<String> {
    match top_level(doc, key) {
        Ok(Some(Yaml::Str(s))) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// The NodeInstall's `install` from the words and the drives, `carries`
/// already filled in.
pub fn render(doc: &str, seen: &[SeenDrive]) -> Value {
    let mut v = json!({
        "mode": word(doc, "mode").unwrap_or_else(|| "install".into()),
        "systemDrive": word(doc, "system_drive"),
        "drivesSeen": seen.len(),
    });
    match top_level(doc, "storage") {
        Ok(None) => {
            v["storage"] = json!(false);
        }
        Ok(Some(y)) => {
            v["storage"] = json!(true);
            match StorageSpec::from_yaml(&y).and_then(|spec| plan(&spec, seen, false)) {
                Ok(p) => v["plan"] = p.to_json(),
                Err(e) => v["planError"] = json!(e),
            }
        }
        Err(e) => v["planError"] = json!(format!("storage in the install words cannot be read: {e}")),
    }
    v
}

/// The disk names this node's own storage is on: every slab open in the
/// engine (its drive), and the node-data set's members the layout names.
async fn own_disks(state: &AppState, run: &Path) -> HashSet<String> {
    let mut own = HashSet::new();
    let registry = state.volume_manager.lock().await.registry().clone();
    {
        let reg = registry.read().await;
        for (_, slab) in reg.iter() {
            let path = slab.device().drive_id().path;
            let disk = crate::drive::identity::of(&path).map(|i| i.disk).filter(|d| !d.is_empty());
            own.insert(disk.unwrap_or_else(|| path.rsplit('/').next().unwrap_or_default().to_string()));
        }
    }
    let layout: Value = std::fs::read(run.join("storage-layout.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    for d in layout["drives"].as_array().into_iter().flatten() {
        if let Some(dev) = d["device"].as_str() {
            own.insert(dev.rsplit('/').next().unwrap_or_default().to_string());
        }
    }
    own
}

fn sas_of(sys: &Path, name: &str) -> Option<u64> {
    let s = std::fs::read_to_string(sys.join("block").join(name).join("device/sas_address")).ok()?;
    u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// What a drive that is not this node's holds, as `/init` says it.
async fn carries_of(dev: &str) -> String {
    let d = match crate::drive::open_path(dev, true).await {
        Ok(d) => d,
        Err(e) => return format!("content that could not be read ({e})"),
    };
    if !crate::drive::discover::slabs_in_partitions(&d).await.is_empty() {
        return "stormblock slabs (an install)".into();
    }
    match crate::mgmt::drive_offer::survey(&d).await {
        crate::mgmt::drive_offer::Offer::Blank => String::new(),
        o => o.destroys(),
    }
}

/// The NodeInstall's `install`: `None` when this boot kept no install words.
pub async fn status(state: &AppState, cache: &mut Cache) -> Option<Value> {
    status_in(state, &PathBuf::from(crate::install_progress::RUN), Path::new("/sys"), cache).await
}

async fn status_in(state: &AppState, run: &Path, sys: &Path, cache: &mut Cache) -> Option<Value> {
    let doc = std::fs::read_to_string(run.join(INTENT)).ok()?;
    let own = own_disks(state, run).await;
    let mut seen = {
        let sys = sys.to_path_buf();
        tokio::task::spawn_blocking(move || seen_in(&sys, |_| String::new())).await.ok()?
    };
    // What the plan depends on, but for what is on the drives: the same,
    // and the last plan stands.
    let key = format!(
        "{doc}\n{}\n{:?}",
        seen.iter().map(|d| format!("{} {} {} {}", d.dev, d.serial, d.wwn, d.bytes)).collect::<Vec<_>>().join("\n"),
        {
            let mut o: Vec<&String> = own.iter().collect();
            o.sort();
            o
        }
    );
    if cache.key.as_deref() == Some(key.as_str()) {
        return cache.value.clone();
    }
    for d in seen.iter_mut() {
        let name = d.dev.trim_start_matches("/dev/").to_string();
        if own.contains(&name) {
            continue;
        }
        if sas_of(sys, &name).is_some_and(crate::mgmt::format_job::running) {
            d.carries = "a format running (#621)".into();
            continue;
        }
        d.carries = carries_of(&d.dev).await;
    }
    let v = render(&doc, &seen);
    cache.key = Some(key);
    cache.value = Some(v.clone());
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drives(carries: &[&str]) -> Vec<SeenDrive> {
        carries
            .iter()
            .enumerate()
            .map(|(i, c)| SeenDrive {
                dev: format!("/dev/sd{}", (b'a' + i as u8) as char),
                serial: format!("S{i}"),
                bay: i.to_string(),
                position: "front".into(),
                bytes: 10_000_000_000_000,
                carries: c.to_string(),
                ..Default::default()
            })
            .collect()
    }

    /// #623: the words and the drives give the plan /init would lay by.
    #[test]
    fn the_install_words_plan_over_the_drives_seen() {
        let doc = "mode: install\nstorage:\n  drives: all\n  data_raid:\n    parity: 1\n";
        let v = render(doc, &drives(&["", "", "", ""]));
        assert_eq!(v["mode"], json!("install"));
        assert_eq!(v["storage"], json!(true));
        assert_eq!(v["drivesSeen"], json!(4));
        assert_eq!(v["plan"]["drives"].as_array().unwrap().len(), 4);
        assert_eq!(v["plan"]["set"]["parity"], json!(1));
        assert!(v.get("planError").is_none(), "{v}");
    }

    /// #623: a drive that holds someone's data is not taken without the
    /// wipe word, which the words never carry: the plan says why.
    #[test]
    fn a_drive_that_carries_data_is_said_not_taken() {
        let doc = "storage:\n  drives: all\n";
        let v = render(doc, &drives(&["", "an ext2/3/4 filesystem"]));
        let taken = v["plan"]["drives"].as_array().map(Vec::len).unwrap_or(0);
        let refused = v["planError"].as_str().unwrap_or_default().to_string();
        let said = v["plan"]["said"].to_string();
        assert!(
            taken == 1 && said.contains("ext2/3/4") || refused.contains("ext2/3/4"),
            "the filesystem is named as why: {v}"
        );
    }

    /// #623: no storage words: the mode alone, no plan.
    #[test]
    fn no_storage_words_no_plan() {
        let v = render("mode: diagnostic\n", &drives(&[""]));
        assert_eq!(v["mode"], json!("diagnostic"));
        assert_eq!(v["storage"], json!(false));
        assert!(v.get("plan").is_none() && v.get("planError").is_none());
    }
}
