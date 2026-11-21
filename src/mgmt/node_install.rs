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
//! [`Live`] is the running engine's: #621's format jobs, `cli::storage_lay`,
//! the stormraid binary, `cli::runtime_take`.
//!
//! **The trigger** (`storage.runtime: true` in the install-config): on a
//! fresh install over drives that hold nothing, `/init` lays nothing. It
//! boots the claimed image with every drive left alone and leaves
//! `/run/stormblock/runtime-install`. [`start`] sees it once the engine is
//! up and runs the install once a boot, from the words `/init` kept
//! (`install-intent.yaml`) and the drives as the engine sees them,
//! [`decide`]d again here: a drive that holds anything stops it (the wipe
//! word, and a keep-data reinstall, are `/init`'s, option 1 on #623). Once
//! the flow-over is done, the disk boots on its own and the install is
//! reported laid, as with `/init`'s install.

use async_trait::async_trait;

use crate::cli::LayOutcome;
use crate::image::storage::{OnDriveFailure, StoragePlan};

/// What each step does on this node.
#[async_trait]
pub trait Install: Send + Sync {
    /// Format the plan's drives that are not at its format, and wait for
    /// them; what was done, a line each.
    async fn format(&self, plan: &StoragePlan) -> Result<Vec<String>, String>;
    /// The plan over the drives as they are after the format: a drive a
    /// card format took off the link comes back, maybe by another name.
    async fn replan(&self, plan: &StoragePlan) -> Result<StoragePlan, String>;
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

    // The same drives, by serial, under the names they have now.
    let plan = match i.replan(plan).await {
        Ok(p) => p,
        Err(e) => return fail("format", e),
    };
    let plan = &plan;

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

/// The running engine's steps (#623).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub struct Live {
    pub state: std::sync::Arc<crate::mgmt::AppState>,
    /// `/sys`, `/dev` (a test points them elsewhere).
    pub sys: std::path::PathBuf,
    pub dev: std::path::PathBuf,
    /// The stormraid binary (`stormraid create`, `serve`).
    pub stormraid: std::path::PathBuf,
    /// The ublk id the set is served at, as /init serves it (#605).
    pub set_id: u32,
    /// How often a format is looked at.
    pub poll: std::time::Duration,
}

/// The stormraid binary: `$STORMBLOCK_STORMRAID`, else the initramfs's, else
/// the node's pallet.
pub fn stormraid_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("STORMBLOCK_STORMRAID") {
        return p.into();
    }
    ["/usr/sbin/stormraid", "/pallets/stormraid/bin/stormraid"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| "/usr/sbin/stormraid".into())
}

/// `/init` leaves this in the run directory when this boot's fresh install
/// is the running node's (#623).
pub const ASKED: &str = "runtime-install";
/// Written when [`start`] begins: an install is run once a boot.
pub const STARTED: &str = "runtime-install.started";

/// The install the words ask of this node over the drives `seen`, checked
/// as `/init` would before it lays anything: `storage.runtime: true`, a plan
/// without the wipe word, and every drive it takes holding nothing. The
/// plan and the system region's size (0: the release's).
pub fn decide(doc: &str, seen: &[crate::image::storage::SeenDrive]) -> Result<(StoragePlan, u64), String> {
    use crate::image::storage::{plan, top_level, StorageSpec};
    let y = top_level(doc, "storage")
        .map_err(|e| format!("storage in the install words cannot be read: {e}"))?
        .ok_or("the install words have no storage: section")?;
    let spec = StorageSpec::from_yaml(&y)?;
    if !spec.runtime {
        return Err("storage.runtime is not true in the install words".into());
    }
    let p = plan(&spec, seen, false)?;
    let held: Vec<String> = p.drives.iter().filter(|d| !d.carries.is_empty()).map(|d| format!("{} carries {}", d.said(), d.carries)).collect();
    if !held.is_empty() {
        return Err(format!(
            "a runtime install lays only drives that hold nothing, and {}: nothing was written \
             (the wipe word installs from /init: storage.wipe: all, without storage.runtime)",
            held.join("; ")
        ));
    }
    let bytes = spec.system_size.unwrap_or(0);
    Ok((p, bytes))
}

/// The plan again over `seen` after the format: the same drives, by serial.
pub fn replan_over(old: &StoragePlan, seen: &[crate::image::storage::SeenDrive]) -> Result<StoragePlan, String> {
    let p = crate::image::storage::plan(&old.spec, seen, false)?;
    let key = |d: &crate::image::storage::SeenDrive| if d.serial.is_empty() { d.dev.clone() } else { d.serial.clone() };
    let mut a: Vec<String> = old.drives.iter().map(key).collect();
    let mut b: Vec<String> = p.drives.iter().map(key).collect();
    a.sort();
    b.sort();
    if a != b {
        return Err(format!("the drives after the format are not those formatted: {} then {}", a.join(" "), b.join(" ")));
    }
    Ok(p)
}

/// Run this boot's runtime install, when `/init` left it to the node
/// (#623): once a boot, in the background, each step in the install record.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn start(state: &std::sync::Arc<crate::mgmt::AppState>) {
    let rundir = std::path::PathBuf::from(crate::install_progress::RUN);
    if !rundir.join(ASKED).exists() {
        return;
    }
    let live = Live {
        state: state.clone(),
        sys: "/sys".into(),
        dev: "/dev".into(),
        stormraid: stormraid_path(),
        set_id: 65535,
        poll: std::time::Duration::from_secs(10),
    };
    tokio::spawn(async move {
        if rundir.join(STARTED).exists() {
            tracing::warn!(
                "install: this boot's runtime install was started by an earlier run of the engine; not again \
                 (a flow-over it began resumes from the handover record) (#623)"
            );
            return;
        }
        if let Some(why) = crate::boot_mode::refuses("the runtime install") {
            live.event("stage", "install", "failed", &why);
            return;
        }
        let _ = std::fs::write(rundir.join(STARTED), format!("{}\n", crate::install_progress::now()));
        let doc = std::fs::read_to_string(rundir.join(crate::mgmt::install_plan::INTENT)).unwrap_or_default();
        let seen = crate::mgmt::install_plan::seen_now(&live.state, &rundir, &live.sys).await;
        let (plan, bytes) = match decide(&doc, &seen) {
            Ok(d) => d,
            Err(e) => {
                println!("Install: the runtime install does not start: {e}");
                live.event("stage", "install", "failed", &e);
                return;
            }
        };
        println!("Install: from the running node (#623): {}", plan.said.first().cloned().unwrap_or_default());
        match run(&live, &plan, bytes).await {
            Ok(o) => println!("Install: {} laid, flowing over onto {} and {}", o.laid.join(" "), o.system_disk, o.data_disk),
            Err(e) => println!("Install: the runtime install stopped: {e}"),
        }
    });
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
static SET_SERVER: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[async_trait]
impl Install for Live {
    async fn format(&self, plan: &StoragePlan) -> Result<Vec<String>, String> {
        use crate::image::storage::Protection;
        use crate::mgmt::format_job::{self, Opts, Want};
        let want = Want {
            block_size: plan.spec.block_size,
            pi_type: if plan.spec.protection == Protection::Type1 { 1 } else { 0 },
        };
        let mut sas = Vec::new();
        let mut said = Vec::new();
        for d in &plan.drives {
            let name = d.dev.trim_start_matches("/dev/");
            let dev = self.sys.join("block").join(name);
            let addr = std::fs::read_to_string(dev.join("device/sas_address"))
                .ok()
                .and_then(|s| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
            match addr {
                Some(a) => sas.push((a, None, None)),
                None => {
                    // Not on a SAS card: taken at the format it has, which
                    // must be the plan's block size.
                    let lbs: u32 = std::fs::read_to_string(dev.join("queue/logical_block_size"))
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    if lbs != 0 && lbs != want.block_size {
                        return Err(format!("{} is at {lbs}-byte blocks, not {}, and is on no SAS card to be formatted", d.dev, want.block_size));
                    }
                    said.push(format!("{}: at its format, no SAS card", d.dev));
                }
            }
        }
        if sas.is_empty() {
            return Ok(said);
        }
        let fallback = if want.pi_type == 1 { vec![Want { block_size: want.block_size, pi_type: 0 }] } else { vec![] };
        let keys = format_job::start(&format_job::Card, sas, Opts { want, fallback, test: true, canary: false })?;
        // Every job to its end (a skipped drive ends at once).
        loop {
            let states: Vec<(String, String)> = keys
                .iter()
                .map(|k| (k.clone(), format_job::get(k).and_then(|j| j["state"].as_str().map(str::to_string)).unwrap_or_default()))
                .collect();
            if let Some((k, _)) = states.iter().find(|(_, s)| s == "failed") {
                return Err(format!("drive {k}: its format failed (GET /api/v1/storage/formats)"));
            }
            if states.iter().all(|(_, s)| s == "ready") {
                said.extend(states.iter().map(|(k, _)| format!("{k}: ready at {}", want.said())));
                return Ok(said);
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    async fn replan(&self, plan: &StoragePlan) -> Result<StoragePlan, String> {
        let run = std::path::Path::new(crate::install_progress::RUN);
        let mut seen = crate::mgmt::install_plan::seen_now(&self.state, run, &self.sys).await;
        // The plan's drives held nothing before the format ([`decide`]),
        // which left them blank; what any other drive carries stands.
        let ours: Vec<&str> = plan.drives.iter().map(|d| d.serial.as_str()).filter(|s| !s.is_empty()).collect();
        for d in seen.iter_mut().filter(|d| ours.contains(&d.serial.as_str())) {
            d.carries.clear();
        }
        replan_over(plan, &seen)
    }

    async fn lay(&self, system_bytes: u64, legs: &[String], drives: &[String]) -> Result<LayOutcome, String> {
        crate::cli::storage_lay(system_bytes, legs, drives).await.map_err(|e| e.to_string())
    }

    async fn make_set(&self, members: &[String], parity: u32, spares: u32) -> Result<String, String> {
        let keep = members.len().saturating_sub(spares as usize);
        let mut args: Vec<String> = ["create", "--role", "node-data", "--name", "node-data", "--parity"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.push(parity.to_string());
        args.extend(["--pi", "auto", "--force"].iter().map(|s| s.to_string()));
        for (i, m) in members.iter().enumerate() {
            if i >= keep {
                args.push("--spare".into());
            }
            args.push(m.clone());
        }
        let out = tokio::process::Command::new(&self.stormraid)
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("{}: {e}", self.stormraid.display()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("stormraid create: {}", err.lines().last().unwrap_or("failed")));
        }
        // Served as /init serves it, until the node's stormraid takes it over
        // at the next boot.
        let child = std::process::Command::new(&self.stormraid)
            .arg("serve")
            .arg("--id")
            .arg(self.set_id.to_string())
            .args(&members[..keep])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("stormraid serve: {e}"))?;
        *SET_SERVER.lock().unwrap() = Some(child);
        let dev = self.dev.join(format!("ublkb{}", self.set_id));
        for _ in 0..60 {
            if dev.exists() {
                return Ok(dev.display().to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Err(format!("the node-data set did not come up as {}", dev.display()))
    }

    async fn take(&self, data_disk: &str, system_disk: &str, legs: &[String]) -> Result<String, String> {
        crate::cli::runtime_take(&self.state, data_disk, system_disk, legs).await.map_err(|e| e.to_string())
    }

    fn event(&self, kind: &str, name: &str, state: &str, detail: &str) {
        let _ = crate::install_progress::append(
            std::path::Path::new(crate::install_progress::RUN),
            kind,
            name,
            state,
            detail,
            crate::install_progress::now(),
        );
    }
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
        async fn replan(&self, p: &StoragePlan) -> Result<StoragePlan, String> {
            Ok(p.clone())
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

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    async fn live(dir: &std::path::Path) -> Live {
        let vm = crate::volume::VolumeManager::new(crate::volume::DEFAULT_EXTENT_SIZE);
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        let state = std::sync::Arc::new(crate::mgmt::AppState::new(crate::mgmt::config::StormBlockConfig::default(), vm, reg, gem));
        Live {
            state,
            sys: dir.join("sys"),
            dev: dir.join("dev"),
            stormraid: dir.join("stormraid"),
            set_id: 65535,
            poll: std::time::Duration::from_millis(10),
        }
    }

    /// #623: the live set is made as /init makes it (`create --role
    /// node-data`, the last members spares) and served at the set's id, and
    /// its device is waited for.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[tokio::test]
    async fn the_live_set_is_made_and_served_as_init_does() {
        use std::os::unix::fs::PermissionsExt;
        let w = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(w.path().join("dev")).unwrap();
        let log = w.path().join("calls");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {log}\n[ \"$1\" = serve ] && : > {dev}/ublkb$3 && exec sleep 5\nexit 0\n",
            log = log.display(),
            dev = w.path().join("dev").display()
        );
        std::fs::write(w.path().join("stormraid"), script).unwrap();
        std::fs::set_permissions(w.path().join("stormraid"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let l = live(w.path()).await;
        let members: Vec<String> = ["/dev/sda4", "/dev/sdb4", "/dev/sdc4", "/dev/sdd4"].iter().map(|s| s.to_string()).collect();
        let dev = l.make_set(&members, 1, 1).await.unwrap();
        assert_eq!(dev, w.path().join("dev/ublkb65535").display().to_string());
        let calls = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines[0], "create --role node-data --name node-data --parity 1 --pi auto --force /dev/sda4 /dev/sdb4 /dev/sdc4 --spare /dev/sdd4");
        assert_eq!(lines[1], "serve --id 65535 /dev/sda4 /dev/sdb4 /dev/sdc4");
        if let Some(mut c) = SET_SERVER.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// #623: a drive on no SAS card is taken at its own format when it is the
    /// plan's block size, and refused when it is not.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[tokio::test]
    async fn a_drive_on_no_card_is_taken_at_its_format_or_refused() {
        let w = tempfile::tempdir().unwrap();
        let l = live(w.path()).await;
        let lbs = |name: &str, n: u32| {
            let q = w.path().join("sys/block").join(name).join("queue");
            std::fs::create_dir_all(&q).unwrap();
            std::fs::write(q.join("logical_block_size"), format!("{n}\n")).unwrap();
        };
        for d in ["sda", "sdb", "sdc", "sdd"] {
            lbs(d, 4096);
        }
        let p = the_plan("storage:\n  drives: all\n");
        let said = l.format(&p).await.unwrap();
        assert_eq!(said.len(), 4, "{said:?}");
        lbs("sdc", 512);
        let e = l.format(&p).await.unwrap_err();
        assert!(e.contains("/dev/sdc") && e.contains("512"), "{e}");
    }

    /// #623: the engine starts only what the words ask with storage.runtime,
    /// and never over a drive that holds something.
    #[test]
    fn decide_takes_runtime_words_over_blank_drives_only() {
        let seen = |carries: &str| -> Vec<SeenDrive> {
            ["a", "b"]
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let mut d = SeenDrive::parse(&format!("dev=/dev/sd{l};serial=S{i};bay={i};position=front;bytes=10000000000000")).unwrap();
                    if i == 1 {
                        d.carries = carries.into();
                    }
                    d
                })
                .collect()
        };
        let words = "mode: install\nstorage:\n  drives: all\n  runtime: true\n  system_size: 64G\n";
        let (p, bytes) = decide(words, &seen("")).unwrap();
        assert_eq!(p.drives.len(), 2);
        assert_eq!(bytes, 64 << 30);
        assert!(decide("storage:\n  drives: all\n", &seen("")).unwrap_err().contains("storage.runtime"));
        assert!(decide("mode: install\n", &seen("")).unwrap_err().contains("no storage"));
        // Named, a drive that holds data would be taken by /init's plan; the
        // runtime install refuses it.
        let listed = "storage:\n  runtime: true\n  drives:\n    - serial: S0\n    - serial: S1\n";
        let e = decide(listed, &seen("an ext2/3/4 filesystem")).unwrap_err();
        assert!(e.contains("ext2/3/4") && e.contains("nothing was written"), "{e}");
    }

    /// #623: after the format the same drives are laid, under their names now.
    #[test]
    fn the_plan_after_the_format_is_the_same_drives() {
        let p = the_plan("storage:\n  drives: all\n");
        let mut seen: Vec<SeenDrive> = p.drives.clone();
        seen[0].dev = "/dev/sdq".into();
        let again = replan_over(&p, &seen).unwrap();
        assert!(again.drives.iter().any(|d| d.dev == "/dev/sdq"));
        seen.pop();
        assert!(replan_over(&p, &seen).unwrap_err().contains("not those formatted"));
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
