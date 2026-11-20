//! The install done by the running node (#623, step 3): the order of a
//! fresh install, as `/init` runs it today, with each step an action of the
//! node that booted its image (the console, the boot screen and the
//! NodeInstall object up the whole time).
//!
//! [`run`] goes through the plan's steps in order and records each in the
//! install record (`install_progress`, the steps `format`, `lay`, `set`,
//! `system legs`, `flow-over`), which the NodeInstall object, the install page
//! and the boot screen read:
//!
//! 1. **format**: the plan's drives not at its format (#621's jobs);
//! 2. **lay**: every drive at once, a system leg on each of the plan's legs
//!    (`cli::storage_lay`, what `/init` lays with);
//! 3. **set**: the node-data set over the data regions, when the plan has one
//!    (`stormraid create --role node-data`, as `/init` makes it, so the next
//!    boot finds it as it finds `/init`'s);
//! 4. **the system half and its legs**, then the flow-over onto them.
//!
//! A drive that cannot be laid stops the install, or with
//! `storage.on_drive_failure: continue` is left out while the plan still
//! holds (#625), as in `/init`. Each step's effect is behind [`Install`]: the
//! running engine's, or a test's.
//!
//! Not yet started by anything: the switch that sends a fresh install this
//! way (`storage.runtime`) comes with the last step, so no install takes a
//! path that is not all there.

use async_trait::async_trait;

use crate::cli::LayOutcome;
use crate::image::storage::{OnDriveFailure, StoragePlan};

/// What each step does on this node.
#[async_trait]
pub trait Install: Send + Sync {
    /// Format the plan's drives that are not at its format, and wait for
    /// them; what was done, a line each.
    async fn format(&self, plan: &StoragePlan) -> Result<Vec<String>, String>;
    /// Lay `drives`, a system leg on each of `legs`.
    async fn lay(&self, system_bytes: u64, legs: &[String], drives: &[String]) -> Result<LayOutcome, String>;
    /// Make the node-data set over `members` (the last `spares` hot spares)
    /// and serve it; its device.
    async fn make_set(&self, members: &[String], parity: u32, spares: u32) -> Result<String, String>;
    /// Lay the system half on `system_disk` and the data half on
    /// `data_disk`, open the other `legs`, and start the flow-over onto them.
    async fn take(&self, data_disk: &str, system_disk: &str, legs: &[String]) -> Result<String, String>;
    /// One line of the install record.
    fn event(&self, kind: &str, name: &str, state: &str, detail: &str);
}

/// What [`run`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub laid: Vec<String>,
    /// Drives left out (`on_drive_failure: continue`), each with why.
    pub left_out: Vec<(String, String)>,
    pub data_disk: String,
    pub system_disk: String,
    pub legs: Vec<String>,
}

/// Run the install the plan describes. `system_bytes`: the system region on
/// each leg (the release's need, or the spec's `system_size`). An error says
/// which step stopped it; that step is `failed` in the record.
pub async fn run(i: &dyn Install, plan: &StoragePlan, system_bytes: u64) -> Result<Outcome, String> {
    let fail = |step: &str, why: String| -> Result<Outcome, String> {
        i.event("step", step, "failed", &why);
        i.event("stage", "install", "failed", &format!("{step}: {why}"));
        Err(format!("{step}: {why}"))
    };
    i.event("stage", "install", "running", "from the running node (#623)");

    // 1. Format.
    i.event("step", "format", "running", "");
    match i.format(plan).await {
        Ok(lines) => i.event("step", "format", "done", &lines.join("; ")),
        Err(e) => return fail("format", e),
    }

    // 2. Lay.
    let drives: Vec<String> = plan.drives.iter().map(|d| d.dev.clone()).collect();
    i.event("step", "lay", "running", &drives.join(" "));
    let laid = match i.lay(system_bytes, &plan.system, &drives).await {
        Ok(o) => o,
        Err(e) => return fail("lay", e),
    };
    if !laid.failed.is_empty() {
        let said: Vec<String> = laid.failed.iter().map(|(d, w)| format!("{d}: {w}")).collect();
        if plan.spec.on_drive_failure == OnDriveFailure::Stop {
            return fail("lay", format!("{} (storage.on_drive_failure: stop)", said.join("; ")));
        }
        for (d, w) in &laid.failed {
            i.event("drive", d, "failed", w);
        }
    }
    let legs: Vec<String> = plan.system.iter().filter(|l| laid.laid.contains(l)).cloned().collect();
    let Some(system_disk) = legs.first().cloned() else {
        return fail("lay", "no system leg was laid".into());
    };
    i.event("step", "lay", "done", &format!("{} laid, a system leg on {}", laid.laid.len(), legs.join(" ")));

    // 3. The set.
    let data_disk = match plan.raid {
        Some(r) if laid.members.len() > 1 => {
            // Fewer drives than the set needs, after one was left out.
            if (laid.members.len() as u32) < r.parity + r.spares + 1 {
                return fail(
                    "set",
                    format!("{} data region(s) for parity {} and {} spare(s)", laid.members.len(), r.parity, r.spares),
                );
            }
            i.event("step", "set", "running", &laid.members.join(" "));
            match i.make_set(&laid.members, r.parity, r.spares).await {
                Ok(dev) => {
                    i.event("step", "set", "done", &format!("node-data {dev}, parity {} over {}", r.parity, laid.members.len()));
                    dev
                }
                Err(e) => return fail("set", e),
            }
        }
        _ => {
            i.event("step", "set", "skipped", "no data_raid: the data half on the first drive's region");
            laid.members.first().cloned().ok_or_else(|| "no data region was laid".to_string())?
        }
    };

    // 4. The system half, its legs, the flow-over.
    let others: Vec<String> = legs[1..].to_vec();
    i.event("step", "system legs", "running", &legs.join(" "));
    match i.take(&data_disk, &system_disk, &others).await {
        Ok(said) => {
            i.event("step", "system legs", "done", &legs.join(" "));
            i.event("step", "flow-over", "running", &said);
        }
        Err(e) => return fail("system legs", e),
    }
    Ok(Outcome { laid: laid.laid, left_out: laid.failed, data_disk, system_disk, legs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::storage::{plan, SeenDrive, StorageSpec};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        events: Mutex<Vec<String>>,
        fail_format: bool,
        fail_lay: Vec<String>,
    }

    #[async_trait]
    impl Install for Fake {
        async fn format(&self, p: &StoragePlan) -> Result<Vec<String>, String> {
            self.calls.lock().unwrap().push(format!("format {}", p.drives.len()));
            if self.fail_format {
                return Err("sdb: FORMAT UNIT refused".into());
            }
            Ok(vec!["4 drives at 4096 + PI type1".into()])
        }
        async fn lay(&self, sys: u64, legs: &[String], drives: &[String]) -> Result<LayOutcome, String> {
            self.calls.lock().unwrap().push(format!("lay {sys} legs={} drives={}", legs.join(","), drives.join(",")));
            let laid: Vec<String> = drives.iter().filter(|d| !self.fail_lay.contains(d)).cloned().collect();
            Ok(LayOutcome {
                members: laid.iter().map(|d| format!("{d}4")).collect(),
                failed: self.fail_lay.iter().map(|d| (d.clone(), "the write at LBA 0 failed".into())).collect(),
                laid,
                regions: crate::image::local::StorageRegions { boot_bytes: 1, system_bytes: sys, member_bytes: 2 },
            })
        }
        async fn make_set(&self, members: &[String], parity: u32, spares: u32) -> Result<String, String> {
            self.calls.lock().unwrap().push(format!("set {} p{parity} s{spares}", members.join(",")));
            Ok("/dev/ublkb65535".into())
        }
        async fn take(&self, data: &str, sys: &str, legs: &[String]) -> Result<String, String> {
            self.calls.lock().unwrap().push(format!("take data={data} system={sys} legs={}", legs.join(",")));
            Ok("flowing over".into())
        }
        fn event(&self, kind: &str, name: &str, state: &str, _detail: &str) {
            self.events.lock().unwrap().push(format!("{kind} {name} {state}"));
        }
    }

    fn the_plan(doc: &str) -> StoragePlan {
        let spec = StorageSpec::from_yaml(&crate::image::storage::top_level(doc, "storage").unwrap().unwrap()).unwrap();
        let seen: Vec<SeenDrive> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, l)| SeenDrive::parse(&format!("dev=/dev/sd{l};serial=S{i};bay={i};position=front;bytes=10000000000000")).unwrap())
            .collect();
        plan(&spec, &seen, false).unwrap()
    }

    /// #623: a fresh install in /init's order: format, lay with a leg on
    /// every drive, the set over the data regions, the system half on the
    /// first leg and the others opened, the flow-over; each step recorded.
    #[tokio::test]
    async fn a_fresh_install_runs_in_inits_order() {
        let f = Fake::default();
        let p = the_plan("storage:\n  drives: all\n  data_raid:\n    parity: 1\n");
        let o = run(&f, &p, 64 << 30).await.unwrap();
        let calls = f.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            [
                "format 4".to_string(),
                format!("lay {} legs=/dev/sda,/dev/sdb,/dev/sdc,/dev/sdd drives=/dev/sda,/dev/sdb,/dev/sdc,/dev/sdd", 64u64 << 30),
                "set /dev/sda4,/dev/sdb4,/dev/sdc4,/dev/sdd4 p1 s0".into(),
                "take data=/dev/ublkb65535 system=/dev/sda legs=/dev/sdb,/dev/sdc,/dev/sdd".into(),
            ]
        );
        assert_eq!(o.data_disk, "/dev/ublkb65535");
        let ev = f.events.lock().unwrap().clone();
        for s in ["step format done", "step lay done", "step set done", "step system legs done", "step flow-over running"] {
            assert!(ev.contains(&s.to_string()), "{s}: {ev:?}");
        }
    }

    /// A drive that cannot be laid stops the install by default; with
    /// `continue` the set is one drive narrower and the install goes on.
    #[tokio::test]
    async fn a_drive_that_will_not_lay_stops_it_or_is_left_out() {
        let f = Fake { fail_lay: vec!["/dev/sdc".into()], ..Default::default() };
        let e = run(&f, &the_plan("storage:\n  drives: all\n  data_raid:\n    parity: 1\n"), 1 << 30).await.unwrap_err();
        assert!(e.starts_with("lay:") && e.contains("/dev/sdc"), "{e}");
        assert!(!f.calls.lock().unwrap().iter().any(|c| c.starts_with("set")), "nothing after a stop");
        assert!(f.events.lock().unwrap().contains(&"step lay failed".to_string()));

        let f = Fake { fail_lay: vec!["/dev/sdc".into()], ..Default::default() };
        let p = the_plan("storage:\n  drives: all\n  on_drive_failure: continue\n  data_raid:\n    parity: 1\n");
        let o = run(&f, &p, 1 << 30).await.unwrap();
        assert_eq!(o.left_out.len(), 1);
        assert!(f.calls.lock().unwrap().contains(&"set /dev/sda4,/dev/sdb4,/dev/sdd4 p1 s0".to_string()));
        assert!(f.events.lock().unwrap().contains(&"drive /dev/sdc failed".to_string()));
    }

    /// No data_raid: the data half on the first drive's region, no set; a
    /// format that fails stops before anything is laid.
    #[tokio::test]
    async fn no_set_and_a_failed_format() {
        let f = Fake::default();
        let o = run(&f, &the_plan("storage:\n  drives: all\n  data_raid: none\n"), 1 << 30).await.unwrap();
        assert_eq!(o.data_disk, "/dev/sda4");
        assert!(!f.calls.lock().unwrap().iter().any(|c| c.starts_with("set")));

        let f = Fake { fail_format: true, ..Default::default() };
        let e = run(&f, &the_plan("storage:\n  drives: all\n"), 1 << 30).await.unwrap_err();
        assert!(e.starts_with("format:"), "{e}");
        assert!(!f.calls.lock().unwrap().iter().any(|c| c.starts_with("lay")), "nothing laid after a failed format");
    }
}
