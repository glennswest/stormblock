//! Drives that come and go on a running node (#593; owner: "You need to
//! watch for hot plug even after boot").
//!
//! A task listens to the kernel's uevents ([`crate::drive::uevent`], no
//! udev on stormcos) for as long as the engine runs. Every whole drive added
//! or removed is logged with its device, serial, WWID, controller and bay,
//! at WARN so that it reaches the node's console and journal (#243), and the
//! last [`KEEP`] events are kept for the API.
//!
//! On a node laid by `storage:` (#573), a drive that is not in the layout
//! and holds nothing (its first and last MiB zeros), or answers with no
//! readable format (NOT READY, #595), is a **candidate** for the layout:
//!
//! - `replacement`: the node-data set misses a member (first, the case to do
//!   first);
//! - `spare`: the set is whole;
//! - `system_leg`: a system mirror leg.
//!
//! Candidates are shown, in `GET /api/v1/storage/layout` and health's
//! `storage_layout`, as `candidates`, with the recent `drive_events`. Nothing
//! is formatted or joined here: a drive that holds anything is never one,
//! and taking a candidate is a person's call (#154 decision 3: no auto
//! format).
//!
//! When uevents cannot be read (no netlink), the drives are read from sysfs
//! every [`RESCAN`] instead; a lost burst (ENOBUFS) is made up the same way.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::drive::uevent::{Action, DriveEvent};

/// How many drive events are kept.
pub const KEEP: usize = 64;
/// How often the drives are read again when nothing is heard.
pub const RESCAN: Duration = Duration::from_secs(30);

/// A drive offered for the node's layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub device: String,
    pub serial: Option<String>,
    pub wwid: Option<String>,
    pub controller: Option<String>,
    pub bay: Option<String>,
    pub bytes: u64,
    /// `blank`, or `not_ready` (it needs a format before anything else).
    pub holds: &'static str,
    /// What it could be, the first the one to do first.
    pub offered: Vec<&'static str>,
}

struct View {
    watching: Option<&'static str>,
    events: VecDeque<DriveEvent>,
    candidates: Vec<Candidate>,
}

static VIEW: Mutex<View> = Mutex::new(View { watching: None, events: VecDeque::new(), candidates: Vec::new() });

fn view() -> std::sync::MutexGuard<'static, View> {
    VIEW.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the layout report carries: `drive_events` (newest last),
/// `candidates`, and how drives are watched (`uevents`, `sysfs` or not yet).
pub fn annotate(v: &mut Value) {
    let w = view();
    if let Some(o) = v.as_object_mut() {
        o.insert("drive_events".into(), serde_json::to_value(&w.events).unwrap_or(Value::Null));
        o.insert("candidates".into(), serde_json::to_value(&w.candidates).unwrap_or(Value::Null));
        o.insert("drives_watched".into(), serde_json::json!(w.watching));
    }
}

/// The drive a slab device or partition is on: `/dev/sda3` → `/dev/sda`,
/// `/dev/nvme0n1p2` → `/dev/nvme0n1`, `/dev/sda@4096` → `/dev/sda`.
pub fn disk_of(dev: &str) -> String {
    let d = dev.split('@').next().unwrap_or(dev);
    let name = d.rsplit('/').next().unwrap_or(d);
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        if let Some(i) = d.rfind('p') {
            if d[i + 1..].chars().all(|c| c.is_ascii_digit()) && !d[i + 1..].is_empty() && d[..i].ends_with(|c: char| c.is_ascii_digit()) {
                return d[..i].to_string();
            }
        }
        return d.to_string();
    }
    d.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
}

/// The candidates, from the drives there now, the layout report (`None`:
/// not laid by `storage:`, no candidates), the devices this engine's slabs
/// are on, and whether a drive is blank (`None`: it could not be read,
/// which is not blank).
pub fn candidates(
    present: &[DriveEvent],
    layout: Option<&Value>,
    slab_devices: &[String],
    blank: &dyn Fn(&DriveEvent) -> Option<bool>,
) -> Vec<Candidate> {
    let Some(layout) = layout else { return Vec::new() };
    let strs = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let mut laid: Vec<String> = layout
        .get("drives")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|d| d.get("device").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    laid.extend(strs(layout.get("system_legs")));
    let set = layout.get("set").filter(|s| s.is_object());
    let mut members = strs(set.and_then(|s| s.get("members")));
    members.extend(strs(layout.get("members")));
    laid.extend(members.iter().map(|m| disk_of(m)));
    let missing = set.and_then(|s| s.get("missing")).and_then(Value::as_array).is_some_and(|m| !m.is_empty())
        || set.and_then(|s| s.get("state")).and_then(Value::as_str).is_some_and(|s| s == "degraded");
    let mut offered: Vec<&'static str> = Vec::new();
    if set.is_some() {
        offered.push(if missing { "replacement" } else { "spare" });
    }
    offered.push("system_leg");
    present
        .iter()
        .filter(|d| !laid.iter().any(|l| l == &d.device))
        .filter(|d| !slab_devices.iter().any(|s| disk_of(s) == d.device))
        .filter_map(|d| {
            let holds = if !d.ready {
                "not_ready"
            } else if blank(d) == Some(true) {
                "blank"
            } else {
                return None;
            };
            Some(Candidate {
                device: d.device.clone(),
                serial: d.serial.clone(),
                wwid: d.wwid.clone(),
                controller: d.controller.clone(),
                bay: d.bay.clone(),
                bytes: d.bytes,
                holds,
                offered: offered.clone(),
            })
        })
        .collect()
}

/// Whether a block device's first and last MiB are zeros; `None` when it
/// cannot be read.
pub fn blank_on_disk(device: &str, bytes: u64) -> Option<bool> {
    use std::os::unix::fs::FileExt;
    const MIB: u64 = 1 << 20;
    let f = std::fs::File::open(device).ok()?;
    let len = bytes.min(MIB) as usize;
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    for off in [0, bytes.saturating_sub(len as u64)] {
        f.read_exact_at(&mut buf, off).ok()?;
        if buf.iter().any(|b| *b != 0) {
            return Some(false);
        }
    }
    Some(true)
}

fn said(c: &Candidate) -> String {
    format!(
        "{} ({}{}{}) is {} and not in this node's layout: offered as {} (#593)",
        c.device,
        c.serial.as_deref().map(|s| format!("serial {s}")).unwrap_or_else(|| "no serial".into()),
        c.controller.as_deref().map(|s| format!(", on {s}")).unwrap_or_default(),
        c.bay.as_deref().map(|s| format!(", bay {s}")).unwrap_or_default(),
        if c.holds == "blank" { "blank" } else { "NOT READY (it needs a format)" },
        c.offered.join(", ")
    )
}

/// The candidates again, from what is there now; each new one said.
fn refresh(state: &crate::mgmt::AppState, present: &[DriveEvent]) {
    let slabs = crate::mgmt::slab_report::for_health(state);
    let devices: Vec<String> =
        slabs.as_ref().map(|r| r.items.iter().filter_map(|i| i.device.clone()).collect()).unwrap_or_default();
    let layout = std::fs::read_to_string(crate::mgmt::storage_layout::path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let now = candidates(present, layout.as_ref(), &devices, &|d| blank_on_disk(&d.device, d.bytes));
    let mut w = view();
    for c in &now {
        if !w.candidates.iter().any(|o| o.device == c.device) {
            tracing::warn!("{}", said(c));
        }
    }
    w.candidates = now;
}

fn record(ev: &DriveEvent) {
    tracing::warn!("{} (#593)", ev.said());
    let mut w = view();
    w.events.push_back(ev.clone());
    while w.events.len() > KEEP {
        w.events.pop_front();
    }
}

/// Watch the drives for as long as the engine runs, on a thread of its own
/// (a blocking netlink socket).
#[cfg(target_os = "linux")]
pub fn spawn(state: std::sync::Arc<crate::mgmt::AppState>) {
    use crate::drive::uevent::{Listener, Sys, Tracker};
    let r = std::thread::Builder::new().name("hotplug".into()).spawn(move || {
        let listener = match Listener::open() {
            Ok(l) => {
                view().watching = Some("uevents");
                Some(l)
            }
            Err(e) => {
                tracing::warn!("hotplug: the kernel's uevents cannot be read ({e}): drives are read again every {}s (#593)", RESCAN.as_secs());
                view().watching = Some("sysfs");
                None
            }
        };
        let mut t = Tracker::new(Sys::default());
        let mut present = t.prime();
        refresh(&state, &present);
        loop {
            let evs = match listener.as_ref().map(|l| l.next(RESCAN)) {
                Some(Ok(Some(ev))) => t.on(&ev).into_iter().collect(),
                Some(Ok(None)) | None => {
                    if listener.is_none() {
                        std::thread::sleep(RESCAN);
                    }
                    t.rescan()
                }
                Some(Err(e)) => {
                    if e.raw_os_error() != Some(libc::ENOBUFS) {
                        tracing::warn!("hotplug: reading uevents: {e}; reading the drives again (#593)");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    t.rescan()
                }
            };
            if evs.is_empty() {
                continue;
            }
            for ev in &evs {
                record(ev);
            }
            present = t.sys.present();
            refresh(&state, &present);
        }
    });
    if let Err(e) = r {
        tracing::warn!("hotplug: no watch thread ({e}) (#593)");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn spawn(_state: std::sync::Arc<crate::mgmt::AppState>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drive(dev: &str, ready: bool) -> DriveEvent {
        DriveEvent {
            action: Action::Add,
            device: dev.into(),
            serial: Some(format!("S-{}", &dev[5..])),
            wwid: None,
            controller: Some("0000:03:00.0".into()),
            bay: Some("3".into()),
            bytes: if ready { 10 << 30 } else { 0 },
            ready,
            at: 1,
        }
    }

    fn laid(missing: bool) -> Value {
        json!({
            "from": "boot",
            "drives": [{"device": "/dev/sda"}, {"device": "/dev/sdb"}],
            "system_legs": ["/dev/sda", "/dev/sdb"],
            "set": {"device": "/dev/ublkb64", "state": if missing { "degraded" } else { "optimal" },
                    "members": ["/dev/sda3", "/dev/sdb3"],
                    "missing": if missing { json!(["slot 2 (abcd) is missing"]) } else { json!([]) }}
        })
    }

    #[test]
    fn partitions_name_their_drive() {
        assert_eq!(disk_of("/dev/sda3"), "/dev/sda");
        assert_eq!(disk_of("/dev/sda@4296015872"), "/dev/sda");
        assert_eq!(disk_of("/dev/nvme0n1p2"), "/dev/nvme0n1");
        assert_eq!(disk_of("/dev/nvme0n1"), "/dev/nvme0n1");
        assert_eq!(disk_of("/dev/vdb"), "/dev/vdb");
    }

    #[test]
    fn a_blank_drive_outside_a_degraded_set_is_a_replacement_first() {
        let present = [drive("/dev/sda", true), drive("/dev/sdb", true), drive("/dev/sdc", true)];
        let c = candidates(&present, Some(&laid(true)), &[], &|_| Some(true));
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].device, "/dev/sdc");
        assert_eq!(c[0].holds, "blank");
        assert_eq!(c[0].offered, ["replacement", "system_leg"]);
        assert_eq!(c[0].bay.as_deref(), Some("3"));
    }

    #[test]
    fn beside_a_whole_set_it_is_a_spare() {
        let present = [drive("/dev/sdc", true)];
        let c = candidates(&present, Some(&laid(false)), &[], &|_| Some(true));
        assert_eq!(c[0].offered, ["spare", "system_leg"]);
    }

    #[test]
    fn a_drive_that_holds_anything_or_cannot_be_read_is_never_one() {
        let present = [drive("/dev/sdc", true), drive("/dev/sdd", true)];
        let c = candidates(&present, Some(&laid(false)), &[], &|d| if d.device == "/dev/sdc" { Some(false) } else { None });
        assert!(c.is_empty(), "{c:?}");
    }

    #[test]
    fn a_not_ready_drive_is_one_that_needs_a_format() {
        let c = candidates(&[drive("/dev/sg4", false)], Some(&laid(true)), &[], &|_| None);
        assert_eq!(c[0].holds, "not_ready");
        assert!(said(&c[0]).contains("NOT READY"), "{}", said(&c[0]));
    }

    #[test]
    fn a_drive_with_a_slab_open_or_a_node_not_laid_by_storage_has_none() {
        let present = [drive("/dev/sdc", true)];
        assert!(candidates(&present, Some(&laid(false)), &["/dev/sdc@1048576".into()], &|_| Some(true)).is_empty());
        assert!(candidates(&present, None, &[], &|_| Some(true)).is_empty());
    }

    #[test]
    fn a_layout_with_no_set_offers_a_system_leg() {
        let v = json!({"from": "boot", "drives": [{"device": "/dev/sda"}], "system_legs": ["/dev/sda"],
                       "set": null, "members": ["/dev/sda3"]});
        let c = candidates(&[drive("/dev/sda", true), drive("/dev/sdb", true)], Some(&v), &[], &|_| Some(true));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].offered, ["system_leg"]);
        assert!(said(&c[0]).contains("/dev/sdb (serial S-sdb, on 0000:03:00.0, bay 3) is blank"), "{}", said(&c[0]));
    }

    #[test]
    fn the_report_carries_events_and_candidates() {
        record(&drive("/dev/sdz", true));
        let mut v = json!({"from": "boot"});
        annotate(&mut v);
        assert!(v["drive_events"].as_array().unwrap().iter().any(|e| e["device"] == "/dev/sdz" && e["action"] == "add"), "{v}");
        assert!(v["candidates"].is_array());
    }
}
