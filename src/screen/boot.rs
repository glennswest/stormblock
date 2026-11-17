//! stormpump's steps after the hand-over, on the same screen (#638,
//! stormpump#237). stormpump writes `/run/stormpump/boot-progress.events`
//! in the install record's line format (`<unix s>\t<kind>\t<name>\t<state>
//! \t<detail>`): `step` lines for PID 1 and each of its phases through
//! `ready`, `service <unit>` lines (`started`, `ready`, `exited`, `failed`),
//! and `stage` lines (`handover`, `re-exec`, `shutdown`). The screen draws its
//! steps after the initramfs's own, and the end: READY with the console's
//! address, or the red panel with stormpump's words.

use serde_json::{json, Value};
use std::path::Path;

/// Where stormpump writes them (relative to the root the screen sees).
pub const EVENTS: &str = "/run/stormpump/boot-progress.events";

/// What stormpump has said so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Boot {
    /// Its steps in the order they first appeared, each at its latest state:
    /// `{step, state, at, detail}` as the record's layout has them.
    pub steps: Vec<Value>,
    pub started: usize,
    pub ready: usize,
    pub exited: usize,
    /// Services that failed, with their words.
    pub failed: Vec<(String, String)>,
    /// `ready`: `done` with the console's address, or `failed` with why.
    pub outcome: Option<(String, String)>,
    /// The last `stage` (`handover`, `re-exec`, `shutdown`).
    pub stage: Option<String>,
}

/// stormpump's steps as the screen names them.
pub fn step_name(step: &str) -> String {
    match step {
        "pid1" => "INIT".into(),
        "install-config" => "CONFIG".into(),
        "coldplug-units" => "UNITS".into(),
        "boot-manifest" => "MANIFEST".into(),
        "all-running" => "SERVICES".into(),
        other => other.to_uppercase(),
    }
}

/// What the events say.
pub fn read(events: &[crate::install_progress::Event]) -> Boot {
    let mut b = Boot::default();
    let mut services: std::collections::BTreeMap<String, (String, String)> = Default::default();
    for e in events {
        match e.kind.as_str() {
            "step" => {
                let v = json!({"step": format!("boot:{}", e.name), "state": e.state, "at": e.at, "detail": e.detail});
                match b.steps.iter_mut().find(|s| s["step"] == v["step"]) {
                    Some(s) => *s = v,
                    None => b.steps.push(v),
                }
                if e.name == "ready" && (e.state == "done" || e.state == "failed") {
                    b.outcome = Some((e.state.clone(), e.detail.clone()));
                }
            }
            "stage" => b.stage = Some(e.name.clone()),
            k if k.starts_with("service") => {
                // `service <unit>` in the kind, or the unit as the name.
                let unit = k.strip_prefix("service").map(str::trim).filter(|u| !u.is_empty()).unwrap_or(&e.name).to_string();
                services.insert(unit, (e.state.clone(), e.detail.clone()));
            }
            _ => {}
        }
    }
    for (unit, (state, detail)) in services {
        match state.as_str() {
            "started" => b.started += 1,
            "ready" => {
                b.started += 1;
                b.ready += 1;
            }
            "exited" => b.exited += 1,
            "failed" => b.failed.push((unit, detail)),
            _ => {}
        }
    }
    b
}

/// Read from the root the screen sees now.
pub fn read_file(path: &Path) -> Boot {
    read(&crate::install_progress::events_in(path))
}

/// The record with stormpump's half laid in: its steps after `before` (the
/// initramfs's steps to show), and `boot` (services, the outcome) for the
/// header and the end panels.
pub fn merge(rec: &mut Value, before: Vec<Value>, b: &Boot) {
    let mut steps = before;
    steps.extend(b.steps.iter().cloned());
    rec["layout"] = Value::Array(steps);
    rec["boot"] = json!({
        "started": b.started, "ready": b.ready, "exited": b.exited,
        "failed": b.failed.iter().map(|(u, d)| json!({"unit": u, "said": d})).collect::<Vec<_>>(),
        "outcome": b.outcome.as_ref().map(|(s, d)| json!({"state": s, "said": d})),
        "stage": b.stage,
    });
    // The big readout follows stormpump's current step.
    if let Some(cur) = b.steps.iter().rev().find(|s| s["state"] == "running").or(b.steps.last()) {
        rec["stage"] = json!({"stage": cur["step"], "state": cur["state"], "detail": cur["detail"]});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_progress::Event;

    fn ev(at: u64, kind: &str, name: &str, state: &str, detail: &str) -> Event {
        Event { at, kind: kind.into(), name: name.into(), state: state.into(), detail: detail.into() }
    }

    #[test]
    fn stormpumps_steps_services_and_the_end() {
        let evs = vec![
            ev(10, "step", "pid1", "running", ""),
            ev(11, "step", "pid1", "done", ""),
            ev(11, "step", "network", "running", "eth0"),
            ev(12, "service stormblock", "stormblock", "started", ""),
            ev(13, "service stormblock", "stormblock", "ready", ""),
            ev(13, "service stormcert", "stormcert", "failed", "exit 1: no CA"),
            ev(14, "step", "network", "done", "192.168.31.203"),
            ev(15, "stage", "handover", "done", ""),
            ev(20, "step", "ready", "done", "console http://192.168.31.203:9094/"),
        ];
        let b = read(&evs);
        let names: Vec<&str> = b.steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
        assert_eq!(names, ["boot:pid1", "boot:network", "boot:ready"]);
        assert_eq!(b.steps[0]["state"], "done", "the latest state of each");
        assert_eq!((b.started, b.ready), (1, 1));
        assert_eq!(b.failed, vec![("stormcert".to_string(), "exit 1: no CA".to_string())]);
        assert_eq!(b.outcome, Some(("done".into(), "console http://192.168.31.203:9094/".into())));
        assert_eq!(b.stage.as_deref(), Some("handover"));
        assert_eq!(step_name("pid1"), "INIT");
        assert_eq!(step_name("all-running"), "SERVICES");

        let mut rec = json!({"layout": [{"step": "inventory", "state": "done"}, {"step": "format", "state": "pending"}]});
        merge(&mut rec, vec![json!({"step": "inventory", "state": "done"})], &b);
        assert_eq!(rec["layout"].as_array().unwrap().len(), 4);
        assert_eq!(rec["boot"]["outcome"]["state"], "done");
        assert_eq!(rec["boot"]["failed"][0]["unit"], "stormcert");
    }
}
