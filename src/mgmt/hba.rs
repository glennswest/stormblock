//! The card's rows for the node's life (#611).
//!
//! Owner, #611: "you need to also keep those updated, as it will change".
//! A thread reads every mpt card's pages ([`crate::drive::mpt`]) again every
//! [`RESCAN`], and at once when the drive watch ([`super::hotplug`]) hears a
//! drive come or go ([`poke`]), and matches each row to Linux again. A change
//! is logged at WARN (it reaches the console and journal, #243) and kept: a
//! row appearing, a slot emptied, a state (`no target` -> `not ready` ->
//! `ready`), a link rate or the card's access status changing.
//!
//! `GET /api/v1/storage/hba` gives the rows with when they were read, the
//! recent changes and the errors; `GET /api/v1/storage/hba/{sas}/details`
//! a drive's own low-level answers ([`crate::drive::scsi::details`]), cached
//! with the time they were read (`?fresh=1` reads them again); `POST
//! /api/v1/storage/hba/{sas}/action` an action (admin only, #274; a format
//! needs `confirm` = the SAS address). The details are read again after any
//! action on the row.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// How often the cards are read when nothing is heard.
pub const RESCAN: Duration = Duration::from_secs(30);
/// How many changes are kept.
pub const KEEP: usize = 64;

/// A change to a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub at: u64,
    pub sas_address: String,
    pub slot: Option<u64>,
    pub what: String,
}

#[derive(Default)]
struct State {
    watching: bool,
    read_at: u64,
    rows: Vec<Value>,
    errors: Vec<String>,
    changes: VecDeque<Change>,
    details: HashMap<String, Value>,
    /// The last write-test matrix run on each drive (#627).
    tests: HashMap<String, Value>,
    poked: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(State::default))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Reads the cards again now (a drive came or went, an action ended).
pub fn poke() {
    with(|s| s.poked = true);
    WAKE.notify_all();
}

fn key(r: &Value) -> String {
    r.get("sas_address").and_then(Value::as_str).unwrap_or("?").to_string()
}

fn word(r: &Value, k: &str) -> String {
    match r.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "none".into(),
        Some(v) => v.to_string(),
    }
}

/// What changed between two readings of the rows.
pub fn diff(before: &[Value], after: &[Value], at: u64) -> Vec<Change> {
    let mut out = Vec::new();
    let slot = |r: &Value| r.get("slot").and_then(Value::as_u64);
    let said = |r: &Value| {
        format!(
            "slot {}, {}, {}",
            word(r, "slot"),
            word(r, "state"),
            match (r.get("block").and_then(Value::as_str), r.get("sg").and_then(Value::as_str)) {
                (Some(b), _) => b.to_string(),
                (None, Some(g)) => g.to_string(),
                _ => "no Linux device".into(),
            }
        )
    };
    for a in after {
        let k = key(a);
        match before.iter().find(|b| key(b) == k) {
            None => out.push(Change { at, sas_address: k, slot: slot(a), what: format!("appeared: {}", said(a)) }),
            Some(b) => {
                for f in ["state", "link", "access", "present", "block", "sg", "scsi", "bytes", "slot"] {
                    let (x, y) = (word(b, f), word(a, f));
                    if x != y {
                        out.push(Change { at, sas_address: k.clone(), slot: slot(a), what: format!("{f}: {x} -> {y}") });
                    }
                }
            }
        }
    }
    for b in before {
        let k = key(b);
        if !after.iter().any(|a| key(a) == k) {
            out.push(Change { at, sas_address: k, slot: slot(b), what: format!("gone: the slot is empty (was {})", said(b)) });
        }
    }
    out
}

/// Takes a new reading: rows, errors, and the changes logged.
pub fn take(rows: Vec<Value>, errors: Vec<String>, at: u64) -> Vec<Change> {
    with(|s| {
        let first = s.read_at == 0;
        let ch = if first { Vec::new() } else { diff(&s.rows, &rows, at) };
        for c in &ch {
            tracing::warn!("storage: the card's drive {} ({}): {} (#611)", c.sas_address, c.slot.map(|n| format!("slot {n}")).unwrap_or_default(), c.what);
            s.changes.push_back(c.clone());
            s.details.remove(&c.sas_address);
        }
        while s.changes.len() > KEEP {
            s.changes.pop_front();
        }
        if first && !rows.is_empty() {
            tracing::info!("storage: the card lists {} drive(s) (#611)", rows.len());
        }
        s.rows = rows;
        s.errors = errors;
        s.read_at = at;
        ch
    })
}

/// `GET /api/v1/storage/hba`.
pub fn report() -> Value {
    with(|s| {
        serde_json::json!({
            "watching": s.watching,
            "read_at": s.read_at,
            "every_secs": RESCAN.as_secs(),
            "rows": s.rows,
            "changes": s.changes,
            "errors": s.errors,
        })
    })
}

pub fn cached_details(sas: &str) -> Option<Value> {
    with(|s| s.details.get(sas).cloned())
}

/// The last write-test matrix run on a drive (#627), kept for the engine's life.
pub fn cached_test(sas: &str) -> Option<Value> {
    with(|s| s.tests.get(sas).cloned())
}

/// A card row's link, as the card says it now (#627): rate, state, presence.
fn link_word(rows: &[Value], sas: u64) -> Option<String> {
    let k = format!("0x{sas:016x}");
    rows.iter()
        .find(|r| key(r) == k)
        .map(|r| format!("{} ({}, {})", word(r, "link"), word(r, "state"), if r.get("present") == Some(&Value::Bool(true)) { "present" } else { "not present" }))
        .or(Some("not listed by the card".into()))
}

/// The write-test matrix on a card's drive (#627, blocking), through its sg
/// node or the card: WRPROTECT 0 and 1 at LBA 0 and the last LBA, read back
/// with RDPROTECT 0 and 1, the card's link after each step, the mode and log
/// pages before and after. **Overwrites those two blocks.** Each line goes
/// to the log (the console) and the run is kept for `GET …/write-test`.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn write_test(sas: u64) -> Result<Value, String> {
    use std::path::Path;
    let link = move || {
        let (rows, _) = read();
        link_word(&rows, sas)
    };
    let before = link();
    let m = crate::drive::mpt::with_drive(Path::new("/dev"), Path::new("/sys"), sas, |t| crate::drive::scsi::write_matrix(t, &link))?;
    let lines = m.lines();
    tracing::warn!("storage: 0x{sas:016x}: write test (#627), link before: {}", before.as_deref().unwrap_or("?"));
    for l in &lines {
        tracing::warn!("storage: 0x{sas:016x}: write test: {l} (#627)");
    }
    let mut v = serde_json::to_value(&m).map_err(|e| e.to_string())?;
    if let Some(o) = v.as_object_mut() {
        o.insert("sas_address".into(), serde_json::json!(format!("0x{sas:016x}")));
        o.insert("at".into(), serde_json::json!(now()));
        o.insert("link_before".into(), serde_json::json!(before));
        o.insert("lines".into(), serde_json::json!(lines));
    }
    with(|s| {
        s.tests.insert(format!("0x{sas:016x}"), v.clone());
        s.details.remove(&format!("0x{sas:016x}"));
    });
    poke();
    Ok(v)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn write_test(_sas: u64) -> Result<Value, String> {
    Err("no mpt card here".into())
}

pub fn keep_details(sas: &str, v: Value) {
    with(|s| {
        s.details.insert(sas.to_string(), v);
    });
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn read() -> (Vec<Value>, Vec<String>) {
    use std::path::Path;
    let (cards, errs) = crate::drive::mpt::cards(Path::new("/dev"), Path::new("/sys"), true);
    (cards.iter().flat_map(|c| c.rows_json()).collect(), errs)
}

/// Keep the rows for as long as the engine runs, on a thread of its own.
/// Nothing is started on a node with no mpt card.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn spawn() {
    if crate::drive::mpt::mpt_hosts(std::path::Path::new("/sys")).is_empty() {
        return;
    }
    let r = std::thread::Builder::new().name("hba".into()).spawn(|| {
        with(|s| s.watching = true);
        loop {
            let (rows, errs) = read();
            take(rows, errs, now());
            let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let (mut g, _) = WAKE
                .wait_timeout_while(g, RESCAN, |s| !s.as_ref().is_some_and(|s| s.poked))
                .unwrap_or_else(|e| e.into_inner());
            if let Some(s) = g.as_mut() {
                s.poked = false;
            }
        }
    });
    if let Err(e) = r {
        tracing::warn!("storage: no card watch thread ({e}) (#611)");
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn spawn() {}

/// A drive's details, read now through sg or the card (blocking).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn read_details(sas: u64) -> Result<Value, String> {
    use std::path::Path;
    let d = crate::drive::mpt::with_drive(Path::new("/dev"), Path::new("/sys"), sas, crate::drive::scsi::details)?;
    serde_json::to_value(&d).map_err(|e| e.to_string())
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn read_details(_sas: u64) -> Result<Value, String> {
    Err("no mpt card here".into())
}

/// An action on a drive (blocking); the rows are read again after it.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn act(sas: u64, a: crate::drive::scsi::Action) -> Result<String, String> {
    use std::path::Path;
    let r = crate::drive::mpt::with_drive(Path::new("/dev"), Path::new("/sys"), sas, |t| crate::drive::scsi::act(t, a))?;
    with(|s| s.details.remove(&format!("0x{sas:016x}")));
    poke();
    if r.is_ok() && matches!(a, crate::drive::scsi::Action::Format { .. }) {
        follow_format(sas);
    }
    r
}

/// A format started here is followed to its end (#611: "once drive formats
/// complete linux needs to find them without a reboot"): then the kernel is
/// asked to look at the drive again and the rows are read again.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn follow_format(sas: u64) {
    use std::path::Path;
    let _ = std::thread::Builder::new().name("hba-format".into()).spawn(move || {
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(RESCAN);
            match crate::drive::mpt::with_drive(Path::new("/dev"), Path::new("/sys"), sas, crate::drive::scsi::format_progress) {
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => {
                    tracing::warn!("storage: the card's drive 0x{sas:016x}: the format ended after {}m; the kernel asked to look again (#611)", start.elapsed().as_secs() / 60);
                    crate::drive::mpt::rescan_for(Path::new("/sys"), sas);
                }
                Ok(Err(e)) => tracing::warn!("storage: the card's drive 0x{sas:016x}: the format: {e} (#611)"),
                Err(e) => tracing::warn!("storage: the card's drive 0x{sas:016x}: no longer reached: {e} (#611)"),
            }
            with(|s| s.details.remove(&format!("0x{sas:016x}")));
            poke();
            return;
        }
    });
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn act(_sas: u64, _a: crate::drive::scsi::Action) -> Result<String, String> {
    Err("no mpt card here".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(sas: &str, slot: u64, state: &str, block: Option<&str>, sg: Option<&str>) -> Value {
        json!({"sas_address": sas, "slot": slot, "state": state, "link": "12.0 Gbit", "access": null,
               "present": true, "block": block, "sg": sg, "scsi": if state == "no target" { None } else { Some("0:0:1:0") }, "bytes": null})
    }

    #[test]
    fn changes_are_said() {
        let a = "0x5000cca26801b649";
        let before = vec![row(a, 1, "no target", None, None), row("0x5000cca268077301", 3, "ready", Some("sdd"), Some("sg3"))];
        let after = vec![row(a, 1, "not ready", None, Some("sg1")), row("0x5000cca26a819e69", 2, "no target", None, None)];
        let ch = diff(&before, &after, 7);
        let w: Vec<&str> = ch.iter().map(|c| c.what.as_str()).collect();
        assert!(w.contains(&"state: no target -> not ready"), "{w:?}");
        assert!(w.contains(&"sg: none -> sg1"), "{w:?}");
        assert!(w.contains(&"scsi: none -> 0:0:1:0"), "{w:?}");
        assert!(w.contains(&"appeared: slot 2, no target, no Linux device"), "{w:?}");
        assert!(w.contains(&"gone: the slot is empty (was slot 3, ready, sdd)"), "{w:?}");
        assert!(ch.iter().all(|c| c.at == 7));
        assert!(diff(&after, &after, 8).is_empty());
    }

    #[test]
    fn readings_kept_and_reported() {
        let a = "0x5000cca26801b649";
        assert!(take(vec![row(a, 1, "no target", None, None)], vec![], 10).is_empty(), "the first reading is no change");
        keep_details(a, json!({"wwid": "naa.5000cca26801b648"}));
        let ch = take(vec![row(a, 1, "ready", Some("sdb"), Some("sg1"))], vec!["card: x".into()], 20);
        assert!(!ch.is_empty());
        assert!(cached_details(a).is_none(), "a changed row's details are read again");
        let r = report();
        assert_eq!(r["read_at"], 20);
        assert_eq!(r["rows"][0]["state"], "ready");
        assert_eq!(r["errors"][0], "card: x");
        assert!(r["changes"].as_array().unwrap().iter().any(|c| c["what"] == "state: no target -> ready"));
    }
}
