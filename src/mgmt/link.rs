//! The SAS link of every drive on a card, both sides, every minute (#642).
//!
//! Owner, on forge1 bay 3: "We need to monitor the sas metrics on the sas
//! buss", "alarms when something is not normal, instead of when writes
//! fail". Bay 3 was "failing writes" for a week while its own phy counters
//! (log page 0x18: loss of DWORD sync 73 million and climbing about 500 a
//! minute idle at 12 Gbit) said why, read by nobody.
//!
//! Each minute the card's thread samples, for every drive the card lists,
//! the drive's side (log page 0x18, through its sg node or the card) and
//! the host's side (the HBA phy facing it, `/sys/class/sas_phy`). It keeps
//! the last sample and the **rate per minute**: lifetime totals are history,
//! saturated at 2^32 - 1 on an old shelf drive, and the rate is what says
//! "now". The level is:
//! - `settling`: one sample so far, no rate yet;
//! - `ok`: nothing moving;
//! - `degraded`: any counter moving;
//! - `failing`: loss of sync or phy resets moving in two samples running.
//!
//! A change of level is logged at WARN (the alarm before a write fails). The
//! sample is in `GET /api/v1/storage/hba` (each row's `link`) and on
//! `/metrics` (`storm_drive_link_*`). stormdrive carries it in the Drive
//! object; ironprom keeps the trend and the alert rules.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::drive::scsi::PhyLink;

/// How often a drive's link is sampled.
pub const EVERY_SECS: u64 = 60;
/// Samples in a row with loss of sync or phy resets moving before `failing`.
pub const FAILING_AFTER: u32 = 2;

/// The level of a drive's link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Settling,
    Ok,
    Degraded,
    Failing,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Settling => "settling",
            Level::Ok => "ok",
            Level::Degraded => "degraded",
            Level::Failing => "failing",
        }
    }
    /// For `/metrics`: -1 settling, 0 ok, 1 degraded, 2 failing.
    pub fn gauge(self) -> f64 {
        match self {
            Level::Settling => -1.0,
            Level::Ok => 0.0,
            Level::Degraded => 1.0,
            Level::Failing => 2.0,
        }
    }
}

/// One side of a drive's link at one moment, and its rates since the last.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Side {
    pub phys: Vec<PhyLink>,
    /// Per minute since the last sample, per phy, the same order; `None` on
    /// the first sample or a saturated counter.
    pub rates: Vec<Rates>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Rates {
    pub phy: u8,
    pub invalid_dwords: Option<f64>,
    pub disparity_errors: Option<f64>,
    pub loss_of_sync: Option<f64>,
    pub phy_resets: Option<f64>,
}

/// A drive's link.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Link {
    pub sas_address: String,
    pub at: u64,
    pub drive: Side,
    pub host: Side,
    /// The HBA phy facing the drive (`phy-1:2`) and its maximum rate.
    pub host_phy: Option<String>,
    pub host_maximum: Option<String>,
    /// The drive's own maximum (#646, mode page 0x19/1, the lowest of its
    /// phys), Mbit/s: what its firmware programs and what its hardware can.
    /// forge1 bay 3's EMC HITACHI: 6000 and 12000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_programmed_max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_hardware_max: Option<u32>,
    pub level: Level,
    /// Since when it has been at this level.
    pub since: u64,
    /// Samples in a row with loss of sync or phy resets moving.
    pub streak: u32,
    /// What did not answer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

fn rate(before: u32, after: u32, minutes: f64) -> Option<f64> {
    // Saturated, or reset (a re-plug): no rate from this pair.
    if before == u32::MAX || after < before || minutes <= 0.0 {
        return None;
    }
    Some((after - before) as f64 / minutes)
}

/// The rates of one side between two samples.
pub fn rates(before: &[PhyLink], after: &[PhyLink], minutes: f64) -> Vec<Rates> {
    after
        .iter()
        .map(|a| match before.iter().find(|b| b.phy == a.phy) {
            Some(b) => Rates {
                phy: a.phy,
                invalid_dwords: rate(b.invalid_dwords, a.invalid_dwords, minutes),
                disparity_errors: rate(b.disparity_errors, a.disparity_errors, minutes),
                loss_of_sync: rate(b.loss_of_sync, a.loss_of_sync, minutes),
                phy_resets: rate(b.phy_resets, a.phy_resets, minutes),
            },
            None => Rates { phy: a.phy, ..Default::default() },
        })
        .collect()
}

fn moving(r: &[Rates], critical: bool) -> bool {
    r.iter().any(|x| {
        let m = |v: Option<f64>| v.is_some_and(|v| v > 0.0);
        m(x.loss_of_sync) || m(x.phy_resets) || (!critical && (m(x.invalid_dwords) || m(x.disparity_errors)))
    })
}

/// The next state of a drive's link from its last and a new sample.
pub fn next(prev: Option<&Link>, sas: &str, drive: Vec<PhyLink>, host: Vec<PhyLink>, at: u64) -> Link {
    let Some(p) = prev else {
        return Link {
            sas_address: sas.to_string(),
            at,
            drive: Side { phys: drive, rates: vec![] },
            host: Side { phys: host, rates: vec![] },
            host_phy: None,
            host_maximum: None,
            drive_programmed_max: None,
            drive_hardware_max: None,
            level: Level::Settling,
            since: at,
            streak: 0,
            errors: vec![],
        };
    };
    let minutes = at.saturating_sub(p.at) as f64 / 60.0;
    let dr = rates(&p.drive.phys, &drive, minutes);
    let hr = rates(&p.host.phys, &host, minutes);
    let critical = moving(&dr, true) || moving(&hr, true);
    let any = moving(&dr, false) || moving(&hr, false);
    let streak = if critical { p.streak + 1 } else { 0 };
    let level = if streak >= FAILING_AFTER {
        Level::Failing
    } else if any {
        Level::Degraded
    } else {
        Level::Ok
    };
    Link {
        sas_address: sas.to_string(),
        at,
        drive: Side { phys: drive, rates: dr },
        host: Side { phys: host, rates: hr },
        host_phy: p.host_phy.clone(),
        host_maximum: p.host_maximum.clone(),
        drive_programmed_max: p.drive_programmed_max,
        drive_hardware_max: p.drive_hardware_max,
        level,
        since: if level == p.level { p.since } else { at },
        streak,
        errors: vec![],
    }
}

/// What a link says in a line (the level change's log line, the console).
pub fn said(l: &Link) -> String {
    let mut parts = Vec::new();
    for (side, s) in [("drive", &l.drive), ("host", &l.host)] {
        for r in &s.rates {
            for (name, v) in [("loss of sync", r.loss_of_sync), ("phy resets", r.phy_resets), ("invalid dwords", r.invalid_dwords), ("disparity errors", r.disparity_errors)] {
                if let Some(v) = v.filter(|v| *v > 0.0) {
                    parts.push(format!("{side} phy {} {name} {v:.0}/min", r.phy));
                }
            }
        }
    }
    let rate = l.drive.phys.iter().find(|p| p.rate_mbps > 0).map(|p| format!(" at {} Gbit", p.rate_mbps as f64 / 1000.0)).unwrap_or_default();
    let programs = match (l.drive_programmed_max, l.drive_hardware_max) {
        (Some(p), Some(h)) if p < h => format!(" ({})", programmed_said(p, h)),
        _ => String::new(),
    };
    format!("{}{rate}{programs}: {}", l.level.as_str(), if parts.is_empty() { "nothing moving".into() } else { parts.join(", ") })
}

/// `the drive programs 6 Gbit (hardware 12)` (#646).
pub fn programmed_said(programmed: u32, hardware: u32) -> String {
    use crate::drive::scsi::gbit;
    format!("the drive programs {} Gbit (hardware {})", gbit(programmed), gbit(hardware))
}

/// The drive's maxima from its phys' rates: the lowest programmed and
/// hardware maximum of the phys that report one.
pub fn maxima(r: &[crate::drive::scsi::PhyRates]) -> (Option<u32>, Option<u32>) {
    let low = |f: fn(&crate::drive::scsi::PhyRates) -> u32| r.iter().map(f).filter(|v| *v > 0).min();
    (low(|p| p.programmed_max_mbps), low(|p| p.hardware_max_mbps))
}

static STATE: Mutex<Option<HashMap<String, Link>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Link>) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// Take a new sample of a drive (from the card's thread); a change of level
/// is logged, and the metrics are set.
#[allow(clippy::too_many_arguments)]
pub fn record(
    sas: &str,
    labels: &Labels,
    drive: Result<Vec<PhyLink>, String>,
    host: Option<(String, PhyLink, String)>,
    rates: Option<Vec<crate::drive::scsi::PhyRates>>,
    at: u64,
) {
    let (dphys, derr) = match drive {
        Ok(v) => (v, None),
        Err(e) => (vec![], Some(format!("drive side: {e}"))),
    };
    let (hname, hphys, hmax) = match host {
        Some((n, p, m)) => (Some(n), vec![p], Some(m)),
        None => (None, vec![], None),
    };
    let l = with(|m| {
        let mut l = next(m.get(sas), sas, dphys, hphys, at);
        l.host_phy = hname;
        l.host_maximum = hmax;
        if let Some(r) = &rates {
            (l.drive_programmed_max, l.drive_hardware_max) = maxima(r);
            let was = m.get(sas).and_then(|p| p.drive_programmed_max);
            if let (Some(p), Some(h)) = (l.drive_programmed_max, l.drive_hardware_max) {
                if p < h && was != Some(p) {
                    tracing::warn!("storage: drive {sas} (bay {}): {} (#646: storage.drives[].linkRate raises it)", labels.bay, programmed_said(p, h));
                }
            }
        }
        l.errors = derr.into_iter().collect();
        if let Some(p) = m.get(sas) {
            if p.level != l.level && l.level != Level::Settling {
                let line = format!("storage: drive {sas} (bay {}): link {} -> {} (#642)", labels.bay, p.level.as_str(), said(&l));
                if l.level == Level::Ok {
                    tracing::info!("{line}");
                } else {
                    tracing::warn!("{line}");
                }
            }
        }
        m.insert(sas.to_string(), l.clone());
        l
    });
    publish(&l, labels);
}

/// What `/metrics` labels a drive's series with.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    pub node: String,
    pub drive: String,
    pub bay: String,
    pub shelf: String,
}

/// The drive's series on `/metrics` (#642, stormcos docs/METRICS.md).
fn publish(l: &Link, lb: &Labels) {
    for (side, s) in [("drive", &l.drive), ("host", &l.host)] {
        for p in &s.phys {
            macro_rules! labelled {
                ($kind:ident, $name:expr) => {
                    metrics::$kind!(
                        $name,
                        "node" => lb.node.clone(),
                        "drive" => lb.drive.clone(),
                        "bay" => lb.bay.clone(),
                        "shelf" => lb.shelf.clone(),
                        "side" => side.to_string(),
                        "phy" => p.phy.to_string()
                    )
                };
            }
            labelled!(counter, "storm_drive_link_invalid_dwords_total").absolute(p.invalid_dwords as u64);
            labelled!(counter, "storm_drive_link_disparity_errors_total").absolute(p.disparity_errors as u64);
            labelled!(counter, "storm_drive_link_loss_of_sync_total").absolute(p.loss_of_sync as u64);
            labelled!(counter, "storm_drive_link_phy_resets_total").absolute(p.phy_resets as u64);
            labelled!(gauge, "storm_drive_link_rate_mbps").set(p.rate_mbps as f64);
        }
    }
    metrics::gauge!(
        "storm_drive_link_level",
        "node" => lb.node.clone(),
        "drive" => lb.drive.clone(),
        "bay" => lb.bay.clone(),
        "shelf" => lb.shelf.clone()
    )
    .set(l.level.gauge());
}

/// A drive's SAS address from sysfs: `/dev/sdb` (its block device) or
/// `/dev/sg3` (a drive the card lists that sd made no disk of).
pub fn sas_of_dev(sys: &std::path::Path, dev: &str) -> Option<u64> {
    let name = dev.rsplit('/').next()?;
    let p = if name.starts_with("sg") {
        sys.join("class/scsi_generic").join(name).join("device/sas_address")
    } else {
        sys.join("block").join(name).join("device/sas_address")
    };
    crate::drive::mpt::parse_addr(std::fs::read_to_string(p).ok()?.trim())
}

/// The link-rate policy file (#642): `<sas address> <Gbit/s>` a line,
/// written by /init from the install-config's `storage.drives[].linkRate`.
pub const POLICY: &str = "link-policy";

/// The policy's lines, from a file's text.
pub fn parse_policy(text: &str) -> Vec<(u64, f32)> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some((crate::drive::mpt::parse_addr(w.next()?)?, w.next()?.parse().ok()?))
        })
        .collect()
}

/// At the engine's start (#642): an install's policy (`/run/stormblock/
/// link-policy`, from /init) is kept in the data directory, which the state
/// volume carries across reboots; the kept policy is applied on every boot,
/// since a sysfs write does not persist. Said on the log, a line a drive.
pub fn apply_policy(run: &std::path::Path, data_dir: &std::path::Path, install_boot: bool) {
    let live = run.join(POLICY);
    let kept = data_dir.join(POLICY);
    if install_boot {
        // An install's policy replaces what was kept; none clears it.
        match std::fs::read(&live) {
            Ok(b) => {
                let _ = std::fs::create_dir_all(data_dir);
                let _ = std::fs::write(&kept, b);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&kept);
            }
        }
    }
    let Ok(text) = std::fs::read_to_string(&kept) else { return };
    for (sas, g) in parse_policy(&text) {
        apply_one(sas, g);
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn apply_one(sas: u64, g: f32) {
    use std::path::Path;
    let (drive, host) = crate::drive::mpt::set_link_max(Path::new("/dev"), Path::new("/sys"), sas, Some(g));
    match drive {
        Ok(s) => tracing::info!("storage: drive 0x{sas:016x}: {s} (the link-rate policy, #646)"),
        Err(e) => tracing::warn!("storage: drive 0x{sas:016x}: its own maximum not programmed to {g} Gbit: {e} (#646)"),
    }
    match host {
        Ok(s) => tracing::warn!("storage: drive 0x{sas:016x}: {s} (the link-rate policy, #642)"),
        Err(e) => tracing::warn!("storage: drive 0x{sas:016x}: the link-rate policy ({g} Gbit) not applied: {e} (#642)"),
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn apply_one(_sas: u64, _g: f32) {}

/// Every drive's link in brief (#642, for health): its SAS address, level,
/// since when, and the drive side's loss-of-sync rate per minute.
pub fn summary() -> Vec<Value> {
    let mut v: Vec<Value> = with(|m| {
        m.values()
            .map(|l| {
                let sync: Option<f64> = l.drive.rates.iter().filter_map(|r| r.loss_of_sync).reduce(|a, b| a + b);
                serde_json::json!({
                    "sas_address": l.sas_address, "level": l.level.as_str(), "since": l.since,
                    "loss_of_sync_per_min": sync,
                    "rate_mbps": l.drive.phys.iter().map(|p| p.rate_mbps).max(),
                    "drive_programmed_max": l.drive_programmed_max,
                    "drive_hardware_max": l.drive_hardware_max,
                })
            })
            .collect()
    });
    v.sort_by(|a, b| a["sas_address"].as_str().cmp(&b["sas_address"].as_str()));
    v
}

/// The last sample of a drive's link, for the card's rows.
pub fn get(sas: &str) -> Option<Value> {
    with(|m| m.get(sas).and_then(|l| serde_json::to_value(l).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phy(sync: u32, resets: u32, inv: u32) -> PhyLink {
        PhyLink { phy: 0, rate_mbps: 12000, attached_sas: None, invalid_dwords: inv, disparity_errors: 0, loss_of_sync: sync, phy_resets: resets }
    }

    /// forge1 bay 3: loss of sync climbing about 500 a minute idle at 12
    /// Gbit, the invalid DWORD and disparity counters saturated, the host
    /// side 0: settling, then degraded, then failing; and ok again at 6.
    #[test]
    fn the_policy_kept_and_read() {
        assert_eq!(parse_policy("0x5000cca26a819e69 6\nbad\n5000cca2680ab8d9 12\n"), vec![(0x5000cca26a819e69, 6.0), (0x5000cca2680ab8d9, 12.0)]);
        let w = tempfile::tempdir().unwrap();
        let (run, dir) = (w.path().join("run"), w.path().join("engine"));
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(POLICY), "0x5000cca26a819e69 6\n").unwrap();
        apply_policy(&run, &dir, true);
        assert_eq!(std::fs::read_to_string(dir.join(POLICY)).unwrap(), "0x5000cca26a819e69 6\n", "an install's policy kept");
        std::fs::remove_file(run.join(POLICY)).unwrap();
        apply_policy(&run, &dir, false);
        assert!(dir.join(POLICY).exists(), "an ordinary boot keeps it");
        apply_policy(&run, &dir, true);
        assert!(!dir.join(POLICY).exists(), "an install with none clears it");
        let sys = w.path().join("sys");
        std::fs::create_dir_all(sys.join("block/sdb/device")).unwrap();
        std::fs::write(sys.join("block/sdb/device/sas_address"), "0x5000cca26a819e69\n").unwrap();
        assert_eq!(sas_of_dev(&sys, "/dev/sdb"), Some(0x5000cca26a819e69));
    }

    #[test]
    fn bay_3s_link_by_its_levels() {
        let sas = "0x5000cca26a819e69";
        let host = |n: u32| vec![PhyLink { phy: 2, rate_mbps: 12000, attached_sas: None, invalid_dwords: n, disparity_errors: 0, loss_of_sync: 0, phy_resets: 0 }];
        let l0 = next(None, sas, vec![phy(73_506_628, 0, u32::MAX)], host(0), 1000);
        assert_eq!(l0.level, Level::Settling);
        let l1 = next(Some(&l0), sas, vec![phy(73_507_128, 0, u32::MAX)], host(0), 1060);
        assert_eq!(l1.level, Level::Degraded, "loss of sync moving once");
        assert_eq!(l1.drive.rates[0].loss_of_sync, Some(500.0));
        assert_eq!(l1.drive.rates[0].invalid_dwords, None, "a saturated counter has no rate");
        let l2 = next(Some(&l1), sas, vec![phy(73_507_628, 0, u32::MAX)], host(0), 1120);
        assert_eq!(l2.level, Level::Failing, "moving two samples running");
        assert_eq!(l2.since, 1120);
        assert!(said(&l2).starts_with("failing at 12 Gbit: drive phy 0 loss of sync 500/min"), "{}", said(&l2));
        // Pinned to 6 Gbit: the counter froze.
        let l3 = next(Some(&l2), sas, vec![phy(73_507_628, 0, u32::MAX)], host(0), 1180);
        assert_eq!(l3.level, Level::Ok);
        // Only the host's invalid DWORDs moving: degraded, never failing.
        let l4 = next(Some(&l3), sas, vec![phy(73_507_628, 0, u32::MAX)], host(5), 1240);
        let l5 = next(Some(&l4), sas, vec![phy(73_507_628, 0, u32::MAX)], host(9), 1300);
        assert_eq!((l4.level, l5.level), (Level::Degraded, Level::Degraded));
        // A re-plug resets the drive's counters: no rate from that pair.
        let l6 = next(Some(&l5), sas, vec![phy(46_622, 0, 10)], host(9), 1360);
        // #646: the drive's own maxima carried from sample to sample, said.
        let mut l7 = l6.clone();
        (l7.drive_programmed_max, l7.drive_hardware_max) = maxima(&[
            crate::drive::scsi::PhyRates { phy: 0, programmed_max_mbps: 6000, hardware_max_mbps: 12000, ..Default::default() },
            crate::drive::scsi::PhyRates { phy: 1, ..Default::default() },
        ]);
        assert_eq!((l7.drive_programmed_max, l7.drive_hardware_max), (Some(6000), Some(12000)));
        let l8 = next(Some(&l7), sas, vec![phy(46_622, 0, 10)], host(9), 1420);
        assert_eq!(l8.drive_programmed_max, Some(6000));
        assert!(said(&l8).starts_with("ok at 12 Gbit (the drive programs 6 Gbit (hardware 12)): nothing moving"), "{}", said(&l8));
        assert_eq!(l6.drive.rates[0].loss_of_sync, None);
        assert_eq!(l6.level, Level::Ok);
    }
}
