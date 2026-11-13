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

/// The report with the engine's live view laid over it, or `None` when this
/// node was not laid by `storage:` (or the file does not parse).
pub fn report(slabs: Option<&SlabReport>) -> Option<Value> {
    let text = std::fs::read_to_string(path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(annotate(v, slabs, &live_drive))
}

/// A drive's logical block and PI profile as the kernel has them now.
fn live_drive(device: &str) -> (Option<u64>, Option<String>) {
    let name = std::path::Path::new(device).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lb = std::fs::read_to_string(format!("/sys/block/{name}/queue/logical_block_size"))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    (lb, super::slab_report::integrity_of(device))
}

/// Whether a slab's device (`/dev/sda@<offset>`, `/dev/sda3`, `/dev/nvme0n1p2`)
/// is on `drive`.
pub fn on_drive(device: &str, drive: &str) -> bool {
    let dev = device.split('@').next().unwrap_or(device);
    let Some(rest) = dev.strip_prefix(drive) else { return false };
    let rest = rest.strip_prefix('p').unwrap_or(rest);
    rest.chars().all(|c| c.is_ascii_digit())
}

fn annotate(mut v: Value, slabs: Option<&SlabReport>, live: &dyn Fn(&str) -> (Option<u64>, Option<String>)) -> Value {
    let mut degraded = false;
    if let Some(drives) = v.get_mut("drives").and_then(Value::as_array_mut) {
        for d in drives {
            let Some(dev) = d.get("device").and_then(Value::as_str).map(str::to_string) else { continue };
            let (lb, pi) = live(&dev);
            if let Some(o) = d.as_object_mut() {
                o.insert("logical_block".into(), json!(lb));
                o.insert("integrity".into(), json!(pi));
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
            "set": {"device": "/dev/ublkb64", "state": "optimal", "parity": 1, "slots": 2, "spares": 0,
                    "members": ["/dev/sda3", "/dev/sdb3"], "missing": []}
        })
    }

    fn live(_: &str) -> (Option<u64>, Option<String>) {
        (Some(4096), Some("none".into()))
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
        let r = slabs(vec![item("system", "/dev/sda2"), item("system", "/dev/sdb@1048576"), item("data", "/dev/ublkb64")]);
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
