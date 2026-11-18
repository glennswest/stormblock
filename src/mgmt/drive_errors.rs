//! A drive's own error and PI-check counters, every minute (#648).
//!
//! Owner, 2026-11-18: "we also need raid counters and keep track of pi
//! rewrites? Ie bit rot". stormraid#80 counts what the set finds and
//! repairs; this is the drive's side, sampled beside its link (#642) by the
//! card's thread:
//! - the drive's error counter log pages, 0x02 write, 0x03 read, 0x05
//!   verify (corrected fast and delayed, rewrites/rereads, uncorrected) and
//!   0x06 non-medium, and the last informational exception (0x2F);
//! - the grown defect list's length, and a SATA drive's SMART reallocated,
//!   pending and offline-uncorrectable counts;
//! - the kernel's sense lines for the drive (`/dev/kmsg`, by its H:C:T:L):
//!   PI guard / application tag / reference tag checks the drive failed,
//!   the same raised by the card (mpt3sas makes an IOC EEDP error ILLEGAL
//!   REQUEST 0x10/0x0N, so the card's PI is told from the drive's), medium,
//!   recovered and hardware errors. The drive keeps no count of these
//!   itself: 0x2F holds only the last one.
//!
//! It keeps the rates: per minute for errors, per day (a 24 h window) for
//! defects. The level is:
//! - `settling`: one sample so far;
//! - `ok`: nothing that matters moving (corrected-on-the-fly ECC is a
//!   disk's ordinary life and never counts);
//! - `degraded`: a retry-corrected error, a non-medium, recovered or
//!   hardware error, a medium error or a PI check failure once, a defect
//!   grown in the last day;
//! - `failing`: an uncorrected error, a PI check failure or medium error in
//!   [`FAILING_AFTER`] samples running, or [`DEFECTS_FAILING_PER_DAY`]
//!   defects grown in a day.
//!
//! A change of level is logged at WARN. The sample is in each card row's
//! `errors`, health's `drive_errors` and `/metrics`
//! (`storm_drive_errors_total{kind}`, `storm_drive_defects{kind}`,
//! `storm_drive_errors_level`).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::drive::scsi::{ErrorCounters, SenseCounts};

/// Samples in a row with a PI check failure or medium error before `failing`.
pub const FAILING_AFTER: u32 = 2;
/// Defects grown in a day that make a drive `failing`.
pub const DEFECTS_FAILING_PER_DAY: u64 = 10;
/// The defect window.
pub const DAY_SECS: u64 = 86_400;

pub use super::link::{Labels, Level};

/// Every counter a drive has, by its `/metrics` kind: lifetime totals from
/// the drive, the kernel's sense lines since the engine started.
pub fn kinds(c: &ErrorCounters, s: &SenseCounts) -> Vec<(&'static str, Option<u64>)> {
    let page = |p: &Option<crate::drive::scsi::ErrorPage>, f: fn(&crate::drive::scsi::ErrorPage) -> Option<u64>| p.as_ref().and_then(f);
    vec![
        ("pi_guard", Some(s.pi_guard)),
        ("pi_app_tag", Some(s.pi_app_tag)),
        ("pi_ref_tag", Some(s.pi_ref_tag)),
        ("eedp_guard", Some(s.eedp_guard)),
        ("eedp_app_tag", Some(s.eedp_app_tag)),
        ("eedp_ref_tag", Some(s.eedp_ref_tag)),
        ("medium_error", Some(s.medium_error)),
        ("recovered_error", Some(s.recovered_error)),
        ("hardware_error", Some(s.hardware_error)),
        ("read_corrected_fast", page(&c.read, |p| p.corrected_fast)),
        ("read_corrected_delayed", page(&c.read, |p| p.corrected_delayed)),
        ("read_rereads", page(&c.read, |p| p.retries)),
        ("read_corrected", page(&c.read, |p| p.corrected)),
        ("read_uncorrected", page(&c.read, |p| p.uncorrected)),
        ("write_corrected_fast", page(&c.write, |p| p.corrected_fast)),
        ("write_corrected_delayed", page(&c.write, |p| p.corrected_delayed)),
        ("write_rewrites", page(&c.write, |p| p.retries)),
        ("write_corrected", page(&c.write, |p| p.corrected)),
        ("write_uncorrected", page(&c.write, |p| p.uncorrected)),
        ("verify_corrected_fast", page(&c.verify, |p| p.corrected_fast)),
        ("verify_corrected_delayed", page(&c.verify, |p| p.corrected_delayed)),
        ("verify_rereads", page(&c.verify, |p| p.retries)),
        ("verify_corrected", page(&c.verify, |p| p.corrected)),
        ("verify_uncorrected", page(&c.verify, |p| p.uncorrected)),
        ("non_medium", c.non_medium),
    ]
}

/// The defect counts by `/metrics` kind.
pub fn defect_kinds(c: &ErrorCounters) -> Vec<(&'static str, Option<u64>)> {
    vec![
        ("grown", c.grown_defects.map(u64::from)),
        ("reallocated", c.reallocated),
        ("pending", c.pending),
        ("offline_uncorrectable", c.offline_uncorrectable),
    ]
}

/// A drive's errors.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Errors {
    pub sas_address: String,
    pub at: u64,
    /// The drive's own counters, as read.
    pub drive: ErrorCounters,
    /// The kernel's sense lines for it, counted since the engine started.
    pub sense: SenseCounts,
    /// Per minute since the last sample, by kind; a kind left out had no
    /// rate (the first sample, a counter gone backwards, none kept).
    pub per_min: Vec<(String, f64)>,
    /// Defects grown in the last day (or since the first sample, if less).
    pub defects_per_day: Option<u64>,
    /// `pending` (SATA) moved in the last day.
    pub pending_per_day: Option<u64>,
    pub level: Level,
    pub since: u64,
    /// Samples in a row with a PI check failure or medium error.
    pub streak: u32,
    /// Why it is at its level, in words.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<String>,
    /// The grown list's (and pending's) history for the day: (at, grown, pending).
    #[serde(skip)]
    pub window: VecDeque<Mark>,
}

/// A day-window mark: when, the grown list's length, SATA's pending count.
type Mark = (u64, Option<u64>, Option<u64>);

fn grew(w: &VecDeque<Mark>, f: fn(&Mark) -> Option<u64>) -> Option<u64> {
    let first = w.iter().find_map(f)?;
    let last = w.iter().rev().find_map(f)?;
    Some(last.saturating_sub(first))
}

const PI: [&str; 6] = ["pi_guard", "pi_app_tag", "pi_ref_tag", "eedp_guard", "eedp_app_tag", "eedp_ref_tag"];
const UNCORRECTED: [&str; 3] = ["read_uncorrected", "write_uncorrected", "verify_uncorrected"];
const DEGRADING: [&str; 7] = ["read_corrected_delayed", "write_corrected_delayed", "verify_corrected_delayed", "non_medium", "recovered_error", "hardware_error", "medium_error"];

/// The next state of a drive's errors from its last and a new sample.
pub fn next(prev: Option<&Errors>, sas: &str, drive: ErrorCounters, sense: SenseCounts, at: u64) -> Errors {
    let mut window = prev.map(|p| p.window.clone()).unwrap_or_default();
    window.push_back((at, drive.grown_defects.map(u64::from), drive.pending));
    while window.front().is_some_and(|(t, _, _)| at.saturating_sub(*t) > DAY_SECS) {
        window.pop_front();
    }
    let defects_per_day = grew(&window, |x| x.1);
    let pending_per_day = grew(&window, |x| x.2);
    let Some(p) = prev else {
        return Errors {
            sas_address: sas.to_string(),
            at,
            drive,
            sense,
            per_min: vec![],
            defects_per_day,
            pending_per_day,
            level: Level::Settling,
            since: at,
            streak: 0,
            why: vec![],
            window,
        };
    };
    let minutes = at.saturating_sub(p.at) as f64 / 60.0;
    let before: HashMap<&str, Option<u64>> = kinds(&p.drive, &p.sense).into_iter().collect();
    let per_min: Vec<(String, f64)> = kinds(&drive, &sense)
        .into_iter()
        .filter_map(|(k, v)| {
            let (b, a) = (before.get(k).copied().flatten()?, v?);
            (a >= b && minutes > 0.0).then(|| (k.to_string(), (a - b) as f64 / minutes))
        })
        .collect();
    let moving = |k: &str| per_min.iter().any(|(n, r)| n == k && *r > 0.0);
    // What is said: what decides the level, never the corrected counts.
    let mut why: Vec<String> = per_min
        .iter()
        .filter(|(k, r)| *r > 0.0 && (PI.contains(&k.as_str()) || UNCORRECTED.contains(&k.as_str()) || DEGRADING.contains(&k.as_str())))
        .map(|(k, r)| format!("{k} {r:.1}/min"))
        .collect();
    let checks = PI.iter().any(|k| moving(k)) || moving("medium_error");
    let streak = if checks { p.streak + 1 } else { 0 };
    let defects = defects_per_day.unwrap_or(0) + pending_per_day.unwrap_or(0);
    if defects > 0 {
        why.push(format!("{defects} defect(s) grown in the last day"));
    }
    let failing = UNCORRECTED.iter().any(|k| moving(k)) || streak >= FAILING_AFTER || defects >= DEFECTS_FAILING_PER_DAY;
    let degraded = checks || DEGRADING.iter().any(|k| moving(k)) || defects > 0;
    let level = if failing {
        Level::Failing
    } else if degraded {
        Level::Degraded
    } else {
        Level::Ok
    };
    Errors {
        sas_address: sas.to_string(),
        at,
        drive,
        sense,
        per_min,
        defects_per_day,
        pending_per_day,
        level,
        since: if level == p.level { p.since } else { at },
        streak,
        why,
        window,
    }
}

/// What a drive's errors say in a line.
pub fn said(e: &Errors) -> String {
    format!("{}: {}", e.level.as_str(), if e.why.is_empty() { "nothing moving".into() } else { e.why.join(", ") })
}

static STATE: Mutex<Option<HashMap<String, Errors>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Errors>) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// Take a new sample of a drive (from the card's thread); a change of level
/// is logged, and the metrics are set.
pub fn record(sas: &str, labels: &Labels, drive: ErrorCounters, sense: SenseCounts, at: u64) {
    let e = with(|m| {
        let e = next(m.get(sas), sas, drive, sense, at);
        if let Some(p) = m.get(sas) {
            if p.level != e.level && e.level != Level::Settling {
                let line = format!("storage: drive {sas} (bay {}): errors {} -> {} (#648)", labels.bay, p.level.as_str(), said(&e));
                if e.level == Level::Ok {
                    tracing::info!("{line}");
                } else {
                    tracing::warn!("{line}");
                }
            }
        }
        m.insert(sas.to_string(), e.clone());
        e
    });
    publish(&e, labels);
}

/// The drive's series on `/metrics` (#648, stormcos docs/METRICS.md).
fn publish(e: &Errors, lb: &Labels) {
    for (kind, v) in kinds(&e.drive, &e.sense) {
        let Some(v) = v else { continue };
        metrics::counter!(
            "storm_drive_errors_total",
            "node" => lb.node.clone(),
            "drive" => lb.drive.clone(),
            "bay" => lb.bay.clone(),
            "shelf" => lb.shelf.clone(),
            "kind" => kind
        )
        .absolute(v);
    }
    for (kind, v) in defect_kinds(&e.drive) {
        let Some(v) = v else { continue };
        metrics::gauge!(
            "storm_drive_defects",
            "node" => lb.node.clone(),
            "drive" => lb.drive.clone(),
            "bay" => lb.bay.clone(),
            "shelf" => lb.shelf.clone(),
            "kind" => kind
        )
        .set(v as f64);
    }
    metrics::gauge!(
        "storm_drive_errors_level",
        "node" => lb.node.clone(),
        "drive" => lb.drive.clone(),
        "bay" => lb.bay.clone(),
        "shelf" => lb.shelf.clone()
    )
    .set(e.level.gauge());
}

/// Every drive's errors in brief (#648, for health): level, since when,
/// the PI checks and uncorrected errors it has, the defects.
pub fn summary() -> Vec<Value> {
    let mut v: Vec<Value> = with(|m| {
        m.values()
            .map(|e| {
                let all: HashMap<&str, Option<u64>> = kinds(&e.drive, &e.sense).into_iter().collect();
                let sum = |ks: &[&str]| ks.iter().filter_map(|k| all.get(k).copied().flatten()).sum::<u64>();
                serde_json::json!({
                    "sas_address": e.sas_address, "level": e.level.as_str(), "since": e.since,
                    "pi_checks": sum(&PI[..3]), "eedp_checks": sum(&PI[3..]),
                    "uncorrected": sum(&UNCORRECTED), "medium_errors": e.sense.medium_error,
                    "grown_defects": e.drive.grown_defects, "defects_per_day": e.defects_per_day,
                    "why": e.why,
                })
            })
            .collect()
    });
    v.sort_by(|a, b| a["sas_address"].as_str().cmp(&b["sas_address"].as_str()));
    v
}

/// The last sample of a drive's errors, for the card's rows.
pub fn get(sas: &str) -> Option<Value> {
    with(|m| m.get(sas).and_then(|e| serde_json::to_value(e).ok()))
}

/// The kernel's sense lines, counted across samples.
static SENSE: Mutex<Option<crate::drive::scsi::SenseTally>> = Mutex::new(None);

/// Read `/dev/kmsg`'s new records into the tally (once a sample), and give
/// each device's counts (`0:0:1:0` -> counts).
pub fn tally(records: &[String]) -> HashMap<String, SenseCounts> {
    let mut g = SENSE.lock().unwrap_or_else(|e| e.into_inner());
    let t = g.get_or_insert_with(Default::default);
    t.take(records);
    t.by_device.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::scsi::ErrorPage;

    fn counters(read_unc: u64, delayed: u64, grown: u32) -> ErrorCounters {
        ErrorCounters {
            read: Some(ErrorPage { corrected_fast: Some(1_000_000 + read_unc * 7), corrected_delayed: Some(delayed), retries: Some(0), corrected: Some(1_000_000), uncorrected: Some(read_unc) }),
            grown_defects: Some(grown),
            ..Default::default()
        }
    }

    #[test]
    fn levels_by_what_moves() {
        let sas = "0x5000cca26a819e69";
        let s0 = SenseCounts::default();
        let e0 = next(None, sas, counters(0, 0, 4), s0, 1000);
        assert_eq!(e0.level, Level::Settling);
        // ECC on the fly moving is a disk's ordinary life.
        let mut c = counters(0, 0, 4);
        c.read.as_mut().unwrap().corrected_fast = Some(2_000_000);
        let e1 = next(Some(&e0), sas, c, s0, 1060);
        assert_eq!(e1.level, Level::Ok, "{:?}", e1.why);
        assert_eq!(said(&e1), "ok: nothing moving");
        // One PI guard failure the drive raised: degraded.
        let s1 = SenseCounts { pi_guard: 1, ..Default::default() };
        let e2 = next(Some(&e1), sas, counters(0, 0, 4), s1, 1120);
        assert_eq!(e2.level, Level::Degraded);
        assert_eq!(e2.why, vec!["pi_guard 1.0/min".to_string()]);
        // Again the next minute: failing.
        let s2 = SenseCounts { pi_guard: 2, ..Default::default() };
        let e3 = next(Some(&e2), sas, counters(0, 0, 4), s2, 1180);
        assert_eq!((e3.level, e3.streak), (Level::Failing, 2));
        // Quiet: ok; then a retry-corrected read: degraded; uncorrected: failing.
        let e4 = next(Some(&e3), sas, counters(0, 0, 4), s2, 1240);
        assert_eq!(e4.level, Level::Ok);
        let e5 = next(Some(&e4), sas, counters(0, 3, 4), s2, 1300);
        assert_eq!(e5.level, Level::Degraded);
        let e6 = next(Some(&e5), sas, counters(1, 3, 4), s2, 1360);
        assert_eq!(e6.level, Level::Failing);
        assert!(e6.why.contains(&"read_uncorrected 1.0/min".to_string()), "{:?}", e6.why);
        // A grown defect: degraded for a day; ten in a day: failing.
        let e7 = next(Some(&e6), sas, counters(1, 3, 5), s2, 1420);
        assert_eq!((e7.level, e7.defects_per_day), (Level::Degraded, Some(1)));
        let e8 = next(Some(&e7), sas, counters(1, 3, 14), s2, 1480);
        assert_eq!(e8.level, Level::Failing);
        let e9 = next(Some(&e8), sas, counters(1, 3, 14), s2, 1480 + DAY_SECS + 60);
        assert_eq!((e9.level, e9.defects_per_day), (Level::Ok, Some(0)), "a day later the window has moved on");
        // The card's EEDP error is its own kind, a drive's PI stays at 2.
        let s3 = SenseCounts { pi_guard: 2, eedp_guard: 1, ..Default::default() };
        let e10 = next(Some(&e9), sas, counters(1, 3, 14), s3, 1480 + DAY_SECS + 120);
        assert_eq!(e10.why, vec!["eedp_guard 1.0/min".to_string()]);
        let k: HashMap<_, _> = kinds(&e10.drive, &e10.sense).into_iter().collect();
        assert_eq!((k["pi_guard"], k["eedp_guard"], k["verify_uncorrected"]), (Some(2), Some(1), None));
    }
}
