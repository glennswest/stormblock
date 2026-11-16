//! The node's drive layout, reported (#573; stormcos STORAGE-LAYOUT.md step 8:
//! "report the layout: drives, format used, mirror legs, set state").
//!
//! The initramfs writes `/run/stormblock/storage-layout.json` on the boot that
//! laid the drives (`from: install`, with the format each drive got and what
//! the install-config asked) and on every boot that serves the node-data set
//! (`from: boot`). This module serves it at `GET /api/v1/storage/layout` and
//! in health as `storage_layout`, with what the engine sees now laid over it:
//!
//! - each drive's `logical_block` and `integrity` (T10 PI) as the kernel has
//!   them, so a format that did not take shows here whatever was asked;
//! - each drive's identity as sysfs has it now: `serial`, `wwid`,
//!   `controller` and `bay` (its enclosure slot; else the mpt card's slot,
//!   `bay_from: card`, #611), so a check can name drives by serial and bay
//!   (stormcentral#1318);
//! - each drive's `pi_type`: the P_TYPE it is formatted at (0 none, 1 type
//!   1), from sd's `protection_type`. `integrity` is the kernel's DIX profile,
//!   `none` on an HBA that registers none however the drive is formatted
//!   (forge1's HBA330);
//! - what the install did to each drive (`format`, `protection`, `method`)
//!   and what it `asked`, on every later boot too: the install's report is
//!   kept in system-data and put beside the boot's as
//!   `storage-layout.install.json`, matched by WWID (`format_from: install`);
//! - `legs_present` / `legs_missing`: the system legs that have a local
//!   system slab open in this engine, from the cached slab report (never a
//!   volume-manager lock, #571);
//! - `state`: `ok`, or `degraded` when the set is missing a member, is not
//!   served, or a system leg is missing.
//!
//! A node laid without `storage:` has no file, and no report.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::slab_report::SlabReport;

/// Where the initramfs writes the report (`$STORM_RUN/storage-layout.json`).
pub const LAYOUT_PATH: &str = "/run/stormblock/storage-layout.json";

/// The report's path: `STORMBLOCK_STORAGE_LAYOUT`, else [`LAYOUT_PATH`].
pub fn path() -> PathBuf {
    std::env::var_os("STORMBLOCK_STORAGE_LAYOUT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(LAYOUT_PATH))
}

/// The install's report, kept by system-data and put back on every boot
/// beside [`path`]'s (`storage-layout.install.json`).
pub fn install_path() -> PathBuf {
    path().with_file_name("storage-layout.install.json")
}

/// The report with the engine's live view laid over it, or `None` when this
/// node was not laid by `storage:` (or the file does not parse).
pub fn report(slabs: Option<&SlabReport>) -> Option<Value> {
    let text = std::fs::read_to_string(path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let install = std::fs::read_to_string(install_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let v = with_install(v, install.as_ref());
    let mut v = annotate(v, slabs, &live_drive);
    // Drives that came and went, and those offered for the layout (#593).
    super::hotplug::annotate(&mut v);
    Some(v)
}

/// What the kernel says of a drive now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    pub logical_block: Option<u64>,
    /// The kernel's integrity (DIX) profile.
    pub integrity: Option<String>,
    /// sd's `protection_type`: the P_TYPE the drive is formatted at.
    pub pi_type: Option<u64>,
    pub serial: Option<String>,
    pub wwid: Option<String>,
    pub controller: Option<String>,
    pub bay: Option<String>,
    /// `enclosure` (sysfs) or `card` (the mpt card's slot, #611).
    pub bay_from: Option<&'static str>,
}

fn live_drive(device: &str) -> Live {
    let name = std::path::Path::new(device).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let read = |p: String| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut l = Live {
        logical_block: read(format!("/sys/block/{name}/queue/logical_block_size")).and_then(|s| s.parse().ok()),
        integrity: super::slab_report::integrity_of(device),
        ..Default::default()
    };
    if let Ok(rd) = std::fs::read_dir(format!("/sys/block/{name}/device/scsi_disk")) {
        l.pi_type = rd.flatten().find_map(|e| read(e.path().join("protection_type").to_string_lossy().into_owned())?.parse().ok());
    }
    if let Some(d) = crate::drive::uevent::Sys::default().block_disk(name, crate::drive::uevent::Action::Add) {
        l.serial = d.serial;
        l.wwid = d.wwid;
        l.controller = d.controller;
        l.bay_from = d.bay.as_ref().map(|_| "enclosure");
        l.bay = d.bay;
    }
    if l.bay.is_none() {
        let rows = super::hba::report();
        if let Some(slot) = card_slot(&rows["rows"], name) {
            l.bay = Some(slot);
            l.bay_from = Some("card");
        }
    }
    l
}

/// The mpt card's slot for the Linux disk `name` (`sdb`), from its rows.
fn card_slot(rows: &Value, name: &str) -> Option<String> {
    rows.as_array()?
        .iter()
        .find(|r| r.get("block").and_then(Value::as_str) == Some(name))
        .and_then(|r| r.get("slot").and_then(Value::as_u64))
        .map(|n| n.to_string())
}

/// The install's facts on a report that lacks them (a boot's): each drive's
/// `format`, `protection` and `method` by WWID (else by device), and what
/// the install `asked` and its `regions`.
fn with_install(mut v: Value, install: Option<&Value>) -> Value {
    let Some(inst) = install else { return v };
    if v.get("from").and_then(Value::as_str) == Some("install") {
        return v;
    }
    let idrives: Vec<&Value> = inst.get("drives").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    if let Some(drives) = v.get_mut("drives").and_then(Value::as_array_mut) {
        for d in drives {
            if !d.get("format").is_none_or(Value::is_null) {
                continue;
            }
            let wwid = d.get("wwid").and_then(Value::as_str).filter(|w| !w.is_empty()).map(str::to_string);
            let dev = d.get("device").and_then(Value::as_str).map(str::to_string);
            let found = idrives.iter().find(|i| match &wwid {
                Some(w) => i.get("wwid").and_then(Value::as_str) == Some(w.as_str()),
                None => i.get("device").and_then(Value::as_str) == dev.as_deref(),
            });
            let (Some(i), Some(o)) = (found, d.as_object_mut()) else { continue };
            for k in ["format", "protection", "method"] {
                o.insert(k.into(), i.get(k).cloned().unwrap_or(Value::Null));
            }
            o.insert("format_from".into(), json!("install"));
        }
    }
    if let Some(o) = v.as_object_mut() {
        for k in ["asked", "regions"] {
            if !o.contains_key(k) {
                if let Some(x) = inst.get(k) {
                    o.insert(k.into(), x.clone());
                }
            }
        }
    }
    v
}

/// Whether a slab's device (`/dev/sda@<offset>`, `/dev/sda3`, `/dev/nvme0n1p2`)
/// is on `drive`.
pub fn on_drive(device: &str, drive: &str) -> bool {
    let dev = device.split('@').next().unwrap_or(device);
    let Some(rest) = dev.strip_prefix(drive) else { return false };
    let rest = rest.strip_prefix('p').unwrap_or(rest);
    rest.chars().all(|c| c.is_ascii_digit())
}

fn annotate(mut v: Value, slabs: Option<&SlabReport>, live: &dyn Fn(&str) -> Live) -> Value {
    let mut degraded = false;
    if let Some(drives) = v.get_mut("drives").and_then(Value::as_array_mut) {
        for d in drives {
            let Some(dev) = d.get("device").and_then(Value::as_str).map(str::to_string) else { continue };
            let l = live(&dev);
            if let Some(o) = d.as_object_mut() {
                o.insert("logical_block".into(), json!(l.logical_block));
                o.insert("integrity".into(), json!(l.integrity));
                o.insert("pi_type".into(), json!(l.pi_type));
                o.insert("serial".into(), json!(l.serial));
                // The initramfs's WWID stands when sysfs gives none now.
                if l.wwid.is_some() || !o.contains_key("wwid") {
                    o.insert("wwid".into(), json!(l.wwid));
                }
                o.insert("controller".into(), json!(l.controller));
                o.insert("bay".into(), json!(l.bay));
                o.insert("bay_from".into(), json!(l.bay_from));
            }
        }
    }
    if let Some(set) = v.get("set").filter(|s| s.is_object()) {
        let missing = set.get("missing").and_then(Value::as_array).is_some_and(|m| !m.is_empty());
        let state = set.get("state").and_then(Value::as_str).unwrap_or("unknown");
        if missing || state != "optimal" {
            degraded = true;
        }
    }
    // The legs this engine has a system slab open on (unknown until the
    // first slab report: then neither list is given).
    if let Some(r) = slabs {
        let legs: Vec<String> = v
            .get("system_legs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let (present, missing): (Vec<String>, Vec<String>) = legs.into_iter().partition(|leg| {
            r.items.iter().any(|i| {
                i.role == "system" && i.source == "local" && i.device.as_deref().is_some_and(|d| on_drive(d, leg))
            })
        });
        if !missing.is_empty() {
            degraded = true;
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("legs_present".into(), json!(present));
            o.insert("legs_missing".into(), json!(missing));
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("state".into(), json!(if degraded { "degraded" } else { "ok" }));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mgmt::slab_report::SlabItem;

    fn item(role: &'static str, device: &str) -> SlabItem {
        SlabItem {
            role,
            source: "local",
            device: Some(device.into()),
            transport: None,
            volumes: None,
            logical_block: None,
            integrity: None,
        }
    }

    fn slabs(items: Vec<SlabItem>) -> SlabReport {
        SlabReport {
            diskless: false,
            system: "local",
            data: "local",
            items,
            local_disk: None,
            pair: None,
            boot_order: None,
        }
    }

    fn laid() -> Value {
        json!({
            "from": "install",
            "drives": [
                {"device": "/dev/sda", "format": "formatted", "protection": "none", "method": "auto"},
                {"device": "/dev/sdb", "format": "already", "protection": "none", "method": null}
            ],
            "system_legs": ["/dev/sda", "/dev/sdb"],
            "set": {"device": "/dev/ublkb65535", "state": "optimal", "parity": 1, "slots": 2, "spares": 0,
                    "members": ["/dev/sda3", "/dev/sdb3"], "missing": []}
        })
    }

    fn live(dev: &str) -> Live {
        Live {
            logical_block: Some(4096),
            integrity: Some("none".into()),
            pi_type: Some(1),
            serial: Some(format!("S-{}", &dev[5..])),
            wwid: Some(format!("naa.{}", &dev[5..])),
            controller: Some("0000:c1:00.0".into()),
            bay: Some("3".into()),
            bay_from: Some("card"),
        }
    }

    /// forge1 (#573 step 8, stormcentral#1318): after the reboot the report
    /// is the boot's, and the install's format is the kept record's.
    #[test]
    fn a_boot_report_carries_identity_pi_and_the_installs_format() {
        let boot = json!({
            "from": "boot",
            "drives": [{"device": "/dev/sda", "wwid": "naa.sda", "format": null, "protection": null, "method": null},
                       {"device": "/dev/sdc", "wwid": "naa.sdc", "format": null, "protection": null, "method": null}],
            "system_legs": ["/dev/sda", "/dev/sdc"],
        });
        let install = json!({
            "from": "install",
            "drives": [{"device": "/dev/sdb", "wwid": "naa.sda", "format": "formatted", "protection": "type1", "method": "full"},
                       {"device": "/dev/sdc", "wwid": "naa.sdc", "format": "already", "protection": "type1", "method": null}],
            "asked": {"block_size": 4096, "protection": "type1", "method": "auto"},
        });
        let v = annotate(with_install(boot.clone(), Some(&install)), None, &live);
        // By WWID, not by name: sda was sdb at install.
        assert_eq!(v["drives"][0]["format"], "formatted");
        assert_eq!(v["drives"][0]["method"], "full");
        assert_eq!(v["drives"][0]["format_from"], "install");
        assert_eq!(v["drives"][1]["format"], "already");
        assert_eq!(v["asked"]["block_size"], 4096);
        assert_eq!(v["drives"][0]["pi_type"], 1);
        assert_eq!(v["drives"][0]["serial"], "S-sda");
        assert_eq!(v["drives"][0]["bay"], "3");
        assert_eq!(v["drives"][0]["bay_from"], "card");
        // No record: nothing is made up.
        let v = with_install(boot.clone(), None);
        assert!(v["drives"][0]["format"].is_null());
        assert!(v.get("asked").is_none());
        // An install's own report is never overwritten.
        let mut inst2 = install.clone();
        inst2["drives"][0]["format"] = json!("other");
        let v = with_install(install.clone(), Some(&inst2));
        assert_eq!(v["drives"][0]["format"], "formatted");
    }

    #[test]
    fn the_cards_slot_for_a_disk() {
        let rows = json!([{"block": "sda", "slot": 0}, {"block": null, "slot": 2}, {"block": "sdb", "slot": 3}]);
        assert_eq!(card_slot(&rows, "sdb").as_deref(), Some("3"));
        assert_eq!(card_slot(&rows, "sdc"), None);
        assert_eq!(card_slot(&Value::Null, "sda"), None);
    }

    #[test]
    fn a_slab_device_is_on_its_drive() {
        assert!(on_drive("/dev/sda@1048576", "/dev/sda"));
        assert!(on_drive("/dev/sda2", "/dev/sda"));
        assert!(on_drive("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(on_drive("/dev/sda", "/dev/sda"));
        assert!(!on_drive("/dev/sdab2", "/dev/sda"));
        assert!(!on_drive("/dev/sdb2", "/dev/sda"));
    }

    #[test]
    fn every_leg_open_and_the_set_whole_is_ok() {
        let r = slabs(vec![item("system", "/dev/sda2"), item("system", "/dev/sdb@1048576"), item("data", "/dev/ublkb65535")]);
        let v = annotate(laid(), Some(&r), &live);
        assert_eq!(v["state"], "ok");
        assert_eq!(v["legs_present"], json!(["/dev/sda", "/dev/sdb"]));
        assert_eq!(v["legs_missing"], json!([]));
        assert_eq!(v["drives"][0]["logical_block"], 4096);
        assert_eq!(v["drives"][1]["integrity"], "none");
        // What the initramfs said is kept.
        assert_eq!(v["drives"][0]["format"], "formatted");
        assert_eq!(v["set"]["parity"], 1);
    }

    #[test]
    fn a_leg_with_no_system_slab_is_degraded() {
        let r = slabs(vec![item("system", "/dev/sda2"), item("data", "/dev/sdb3")]);
        let v = annotate(laid(), Some(&r), &live);
        assert_eq!(v["state"], "degraded");
        assert_eq!(v["legs_missing"], json!(["/dev/sdb"]));
    }

    #[test]
    fn a_set_missing_a_member_or_not_served_is_degraded() {
        let r = slabs(vec![item("system", "/dev/sda2"), item("system", "/dev/sdb2")]);
        let mut v = laid();
        v["set"]["missing"] = json!(["slot 1 (abcd) is missing"]);
        assert_eq!(annotate(v, Some(&r), &live)["state"], "degraded");
        let mut v = laid();
        v["set"]["state"] = json!("not-served");
        assert_eq!(annotate(v, Some(&r), &live)["state"], "degraded");
    }

    #[test]
    fn no_slab_report_yet_says_nothing_of_the_legs() {
        let v = annotate(laid(), None, &live);
        assert!(v.get("legs_missing").is_none());
        assert_eq!(v["state"], "ok");
    }

    #[test]
    fn no_raid_set_is_not_degraded() {
        let mut v = laid();
        v["set"] = Value::Null;
        let r = slabs(vec![item("system", "/dev/sda2"), item("system", "/dev/sdb2")]);
        assert_eq!(annotate(v, Some(&r), &live)["state"], "ok");
    }
}
