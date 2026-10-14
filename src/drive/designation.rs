//! The drive stormdrive designates `system` (#402, stormdrive#66).
//!
//! A storage-admin marks the drive the system half belongs on (the X9
//! blades' SSD beside the HDD that keeps the data). stormdrive writes the set
//! to `<system-data>/stormdrive/system-drives.json`, in the `system-data`
//! volume every install keeps, so the next install can read it before
//! stormdrive runs. The owner's rule (#402, A): that designation outranks the
//! drive the system half is on now.
//!
//! Matched to a drive on this machine by its WWN when both sides have one,
//! else by model and serial together (a serial alone is not unique); never by
//! path, which moves.

use std::path::Path;

use serde::Deserialize;

/// Where the file is, inside the `system-data` volume.
pub const FILE: &str = "/stormdrive/system-drives.json";
/// The volume it is in.
pub const VOLUME: &str = "system-data";

/// One designated drive, as stormdrive writes it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Designated {
    #[serde(default)]
    pub wwn: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub model: String,
    /// Where stormdrive last saw it: for a message only.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    drives: Vec<Designated>,
}

/// The designated drives in the file's bytes.
pub fn parse(bytes: &[u8]) -> Result<Vec<Designated>, String> {
    serde_json::from_slice::<File>(bytes).map(|f| f.drives).map_err(|e| format!("system-drives.json: {e}"))
}

/// Whether drive `id` is the designated one `d`.
pub fn matches(d: &Designated, id: &super::identity::Identity) -> bool {
    let norm = |s: &str| s.trim().to_ascii_lowercase();
    if !d.wwn.trim().is_empty() && !id.wwn.trim().is_empty() {
        return norm(&d.wwn) == norm(&id.wwn);
    }
    !d.serial.trim().is_empty()
        && d.serial.trim() == id.serial.trim()
        && !d.model.trim().is_empty()
        && d.model.trim() == id.model.trim()
}

/// The machine's disks under a sysfs root: `sd*`, `nvme*n*` (no
/// partitions), `vd*`, by kernel name, sorted.
pub fn disks_in(sys: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(sys.join("class/block"))
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    out.retain(|n| {
        let whole = !sys.join("class/block").join(n).join("partition").exists();
        whole && (n.starts_with("sd") || n.starts_with("vd") || (n.starts_with("nvme") && n.contains('n')))
    });
    out.sort();
    out
}

/// The present drive (kernel name) for each designated one, in the file's
/// order; `None` for one not on this machine.
pub fn find(designated: &[Designated], sys: &Path) -> Vec<(Designated, Option<String>)> {
    let ids: Vec<(String, super::identity::Identity)> = disks_in(sys)
        .into_iter()
        .filter_map(|n| super::identity::of_in(sys, &n).map(|i| (n, i)))
        .collect();
    designated
        .iter()
        .map(|d| (d.clone(), ids.iter().find(|(_, id)| matches(d, id)).map(|(n, _)| n.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(sys: &Path, name: &str, wwid: &str, model: &str, serial: &str) {
        let d = sys.join("class/block").join(name);
        std::fs::create_dir_all(d.join("device")).unwrap();
        if !wwid.is_empty() {
            std::fs::write(d.join("device/wwid"), format!("{wwid}\n")).unwrap();
        }
        std::fs::write(d.join("device/model"), format!("{model}\n")).unwrap();
        std::fs::write(d.join("device/serial"), format!("{serial}\n")).unwrap();
    }

    const FILE_BYTES: &str = r#"{"version": 1, "node": "server8", "updated": "2026-10-13T20:00:00Z", "stormdrive": "0.32.0",
        "drives": [{"id": "x", "wwn": "NAA.5002538E40A1B2C3", "serial": "TF512A", "model": "TEAM T253X2512G",
                    "kind": "sata_ssd", "capacity_bytes": 512110190592, "shelf": "int", "bay": 1, "path": "/dev/sdb", "present": true}]}"#;

    /// #402: the SSD found by its WWN in any case, wherever it enumerates;
    /// the HDD beside it not taken for it.
    #[test]
    fn the_designated_drive_is_found_by_wwn_wherever_it_enumerates() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path();
        disk(sys, "sda", "naa.5002538e40a1b2c3", "TEAM T253X2512G", "TF512A");
        disk(sys, "sdb", "naa.5000c500d00dfeed", "ST2000DM008-2FR102", "ZFL1ABCD");
        std::fs::create_dir_all(sys.join("class/block/sda1")).unwrap();
        std::fs::write(sys.join("class/block/sda1/partition"), "1\n").unwrap();
        let d = parse(FILE_BYTES.as_bytes()).unwrap();
        assert_eq!(disks_in(sys), vec!["sda".to_string(), "sdb".to_string()], "partitions are not drives");
        let found = find(&d, sys);
        assert_eq!(found[0].1.as_deref(), Some("sda"), "the SSD, though stormdrive last saw it as sdb");
    }

    /// No WWN on one side: model and serial together, never a serial alone.
    #[test]
    fn without_a_wwn_model_and_serial_both_match() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path();
        disk(sys, "sda", "", "OTHER MODEL", "TF512A");
        disk(sys, "sdc", "", "TEAM T253X2512G", "TF512A");
        let d = vec![Designated { serial: "TF512A".into(), model: "TEAM T253X2512G".into(), ..Default::default() }];
        assert_eq!(find(&d, sys)[0].1.as_deref(), Some("sdc"));
        let gone = vec![Designated { wwn: "naa.ffff".into(), ..Default::default() }];
        assert_eq!(find(&gone, sys)[0].1, None, "a designated drive not here");
        assert!(parse(b"not json").is_err());
        assert!(parse(br#"{"version": 1}"#).unwrap().is_empty(), "nobody designated");
    }
}
