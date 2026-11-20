//! How full each volume's filesystem is, and the grow its policy asks (#690;
//! stormcos#1049; owner, 2026-11-19: "We need the ability to enlarge space").
//!
//! Every [`PERIOD`], each volume this engine serves as a ublk device and that
//! is mounted somewhere on the node (any mount namespace, #658) is
//! `statfs`ed through `/proc/<pid>/root`. What is read goes on its Volume
//! object:
//!
//! - `status.filesystem` (`mountedAt`, `sizeBytes`, `usedBytes`,
//!   `availBytes`, `usedPercent`, `readAt`) and `status.usedBytes`;
//! - `status.conditions`: `VolumeFilling` (used ≥ [`FILLING_PERCENT`]) and
//!   `VolumeFull` (≥ [`FULL_PERCENT`]), each `True` or `False` with a reason,
//!   a message saying used of size, and `lastTransitionTime`;
//! - metrics `storm_volume_fs_size_bytes`, `storm_volume_fs_used_bytes`,
//!   `storm_volume_fs_used_ratio`, `storm_volume_grows_total`.
//!
//! **The grow** (`spec.grow: {max, step}`, [`GrowPolicy`]): a volume past
//! [`GrowPolicy::AT_PERCENT`] is grown by a step, up to `max`, at most once
//! per [`GROW_EVERY`], within its pool's free bytes less a margin (1 GiB or
//! 5 % of what the pool promises), through the same grow as `spec.sizeBytes`
//! (#655: the volume, its ublk device, its mounted ext4). Each grow is in
//! `status.grown` (the last [`GROWN_KEPT`]); one that could not be made says
//! why in `status.growBlocked`. No policy, no grow.
//!
//! A volume not mounted where the engine sees it (an iSCSI or NVMe/TCP
//! export, a raw device) has no filesystem to read: no conditions.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::metadata::GrowPolicy;
use crate::volume::VolumeId;

/// How often each mounted volume is read.
pub const PERIOD: Duration = Duration::from_secs(30);
/// `VolumeFilling` from here.
pub const FILLING_PERCENT: u64 = 80;
/// `VolumeFull` from here.
pub const FULL_PERCENT: u64 = 95;
/// One grow a volume at most this often: the filesystem's own resize and
/// what it reports after it settle in between.
pub const GROW_EVERY: Duration = Duration::from_secs(300);
/// Grows kept in `status.grown`.
pub const GROWN_KEPT: usize = 10;

/// A filesystem's numbers, as `df` gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsUsage {
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

impl FsUsage {
    /// Used, in percent of what a user can have (`df`'s Use%: the blocks
    /// reserved for root are not counted as room), rounded up.
    pub fn percent(&self) -> u64 {
        let of = self.used + self.avail;
        if of == 0 {
            return 0;
        }
        (self.used * 100).div_ceil(of)
    }
}

/// `statvfs` of `path`.
#[cfg(target_os = "linux")]
pub fn statfs(path: &str) -> std::io::Result<FsUsage> {
    let c = std::ffi::CString::new(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `st` a statvfs to fill.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let f = st.f_frsize as u64;
    let size = st.f_blocks as u64 * f;
    let free = st.f_bfree as u64 * f;
    Ok(FsUsage { size, used: size.saturating_sub(free), avail: st.f_bavail as u64 * f })
}

#[cfg(not(target_os = "linux"))]
pub fn statfs(_path: &str) -> std::io::Result<FsUsage> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "statfs is read on Linux"))
}

/// One condition's state and since when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cond {
    on: bool,
    since: u64,
}

/// What is known of one volume's filesystem.
#[derive(Debug, Clone)]
struct VolFill {
    usage: FsUsage,
    mounted_at: String,
    read_at: u64,
    filling: Cond,
    full: Cond,
    last_grow: Option<Instant>,
    grown: VecDeque<Value>,
    blocked: Option<Value>,
}

/// Every volume's fill, kept between passes.
#[derive(Debug, Default)]
pub struct Fill {
    vols: HashMap<VolumeId, VolFill>,
}

impl Fill {
    /// A reading of `id` at `now` (unix seconds): conditions change state
    /// (and their transition time) only when they cross.
    fn read(&mut self, id: VolumeId, usage: FsUsage, mounted_at: String, now: u64) -> &mut VolFill {
        let p = usage.percent();
        let (filling, full) = (p >= FILLING_PERCENT, p >= FULL_PERCENT);
        let v = self.vols.entry(id).or_insert_with(|| VolFill {
            usage,
            mounted_at: mounted_at.clone(),
            read_at: now,
            filling: Cond { on: filling, since: now },
            full: Cond { on: full, since: now },
            last_grow: None,
            grown: VecDeque::new(),
            blocked: None,
        });
        for (c, on) in [(&mut v.filling, filling), (&mut v.full, full)] {
            if c.on != on {
                *c = Cond { on, since: now };
            }
        }
        v.usage = usage;
        v.mounted_at = mounted_at;
        v.read_at = now;
        v
    }

    /// Volumes no longer read (gone, or no longer mounted) are dropped.
    fn keep_only(&mut self, ids: &std::collections::HashSet<VolumeId>) {
        self.vols.retain(|k, _| ids.contains(k));
    }

    /// What goes on the volume's object status, when its filesystem is read.
    pub fn status(&self, id: &VolumeId) -> Option<Value> {
        let v = self.vols.get(id)?;
        let u = v.usage;
        let p = u.percent();
        let said = format!("{} of {} used ({p} %) on {}", gib(u.used), gib(u.used + u.avail), v.mounted_at);
        let cond = |kind: &str, c: Cond, at: u64, below: &str| {
            json!({
                "type": kind,
                "status": if c.on { "True" } else { "False" },
                "reason": if c.on { format!("UsedAtLeast{at}Percent") } else { below.to_string() },
                "message": if c.on { format!("{said}: at least {at} %") } else { format!("{said}: under {at} %") },
                "lastTransitionTime": rfc3339(c.since),
            })
        };
        let mut s = json!({
            "usedBytes": u.used,
            "filesystem": {
                "mountedAt": v.mounted_at,
                "sizeBytes": u.size,
                "usedBytes": u.used,
                "availBytes": u.avail,
                "usedPercent": p,
                "readAt": rfc3339(v.read_at),
            },
            "conditions": [
                cond("VolumeFilling", v.filling, FILLING_PERCENT, "Room"),
                cond("VolumeFull", v.full, FULL_PERCENT, "Room"),
            ],
        });
        if !v.grown.is_empty() {
            s["grown"] = Value::Array(v.grown.iter().cloned().collect());
        }
        if let Some(b) = &v.blocked {
            s["growBlocked"] = b.clone();
        }
        Some(s)
    }
}

/// Merge what is known of `id`'s filesystem into its object's status.
pub fn annotate(state: &AppState, id: VolumeId, obj: &mut Value) {
    let st = state.fill.lock().unwrap_or_else(|e| e.into_inner()).status(&id);
    if let (Some(Value::Object(add)), Some(have)) = (st, obj["status"].as_object_mut()) {
        have.extend(add);
    }
}

fn gib(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Unix seconds as Kubernetes writes a time: `2026-11-19T19:08:26Z`.
pub fn rfc3339(secs: u64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Start reading every mounted volume, every [`PERIOD`].
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            pass(&state).await;
            tokio::time::sleep(PERIOD).await;
        }
    });
}

/// One volume read this pass.
struct Read {
    id: VolumeId,
    /// Keyed by the engine's own id: a grow can tell its device.
    own_device: bool,
    usage: FsUsage,
    mounted_at: String,
}

/// What the pass needs of a volume it read.
struct Facts {
    name: String,
    capacity: u64,
    policy: Option<GrowPolicy>,
    role: String,
    /// A blank's policy is for its clones: a sealed volume is never grown.
    sealed: bool,
}

/// One pass: read, publish, grow.
pub async fn pass(state: &Arc<AppState>) {
    let devices = crate::mgmt::api::usage::ublk_devices(state).await;
    let mut reads: Vec<Read> = Vec::new();
    for (id, dev, own_device) in devices {
        let r = tokio::task::spawn_blocking(move || {
            let (at, path) = crate::mgmt::ublk_export::reach_of(&dev)?;
            statfs(&path).ok().map(|u| (at, u))
        })
        .await
        .ok()
        .flatten();
        if let Some((mounted_at, usage)) = r {
            reads.push(Read { id: VolumeId(id), own_device, usage, mounted_at });
        }
    }
    // Handles and policies, the manager held only to take them; names after.
    let held: Vec<(VolumeId, Arc<crate::volume::thin::ThinVolumeHandle>, Option<GrowPolicy>)> = {
        let vm = state.volume_manager.lock().await;
        reads.iter().filter_map(|r| Some((r.id, vm.get_volume_handle(&r.id)?, vm.grow_policy(&r.id)))).collect()
    };
    let mut facts: HashMap<VolumeId, Facts> = HashMap::new();
    for (id, h, policy) in held {
        facts.insert(
            id,
            Facts { name: h.name().await, capacity: h.capacity_bytes(), policy, role: h.placement_role().to_string(), sealed: h.is_sealed() },
        );
    }
    let now = now_secs();
    let ids: std::collections::HashSet<VolumeId> = reads.iter().map(|r| r.id).collect();
    let mut to_grow: Vec<(VolumeId, GrowPolicy, u64, String)> = Vec::new();
    {
        let mut fill = state.fill.lock().unwrap_or_else(|e| e.into_inner());
        fill.keep_only(&ids);
        for r in &reads {
            let Some(f) = facts.get(&r.id) else { continue };
            let name = f.name.clone();
            let before = fill.vols.get(&r.id).map(|v| (v.filling.on, v.full.on));
            let v = fill.read(r.id, r.usage, r.mounted_at.clone(), now);
            let p = r.usage.percent();
            if before != Some((v.filling.on, v.full.on)) && (v.filling.on || before.is_some()) {
                let what = if v.full.on { "VolumeFull" } else if v.filling.on { "VolumeFilling" } else { "room again" };
                tracing::warn!(
                    "volume {name} ({}): {what}: {} of {} used ({p} %) on {} (#690)",
                    r.id,
                    gib(r.usage.used),
                    gib(r.usage.used + r.usage.avail),
                    r.mounted_at
                );
            }
            let key = r.id.0.to_string();
            metrics::gauge!("storm_volume_fs_size_bytes", "volume" => key.clone(), "name" => name.clone()).set(r.usage.size as f64);
            metrics::gauge!("storm_volume_fs_used_bytes", "volume" => key.clone(), "name" => name.clone()).set(r.usage.used as f64);
            metrics::gauge!("storm_volume_fs_used_ratio", "volume" => key, "name" => name.clone()).set(p as f64 / 100.0);
            let (Some(policy), false) = (f.policy, f.sealed) else { continue };
            if p < GrowPolicy::AT_PERCENT || v.last_grow.is_some_and(|t| t.elapsed() < GROW_EVERY) {
                continue;
            }
            if f.capacity >= policy.max {
                v.blocked = Some(json!({ "at": rfc3339(now), "reason": "AtMax", "message": format!("{p} % used, and the volume is at its spec.grow.max ({} bytes)", policy.max) }));
                continue;
            }
            if !r.own_device {
                v.blocked = Some(json!({ "at": rfc3339(now), "reason": "NotTheEngines", "message": "its device is a /v1 attach's: grown by its claim (CSI expansion), not here" }));
                continue;
            }
            // Tried now, whatever comes of it: never more than once per GROW_EVERY.
            v.last_grow = Some(Instant::now());
            to_grow.push((r.id, policy, f.capacity, f.role.clone()));
        }
    }
    for (id, policy, capacity, role) in to_grow {
        let name = facts.get(&id).map(|f| f.name.clone()).unwrap_or_default();
        grow_one(state, id, policy, capacity, &role, name, now).await;
    }
}

/// The room a role's pool can still give, less a margin: 1 GiB or 5 % of
/// what it promises, whichever is more.
async fn room_of(state: &AppState, role: &str) -> Option<(u64, u64)> {
    let acc = crate::mgmt::capacity::account_now(state).await;
    let pool = acc.pools.iter().find(|p| p.pool == role)?;
    let margin = GrowPolicy::GIB.max(pool.promisable_bytes / 20);
    Some((pool.free_bytes.saturating_sub(margin), margin))
}

async fn grow_one(state: &Arc<AppState>, id: VolumeId, policy: GrowPolicy, capacity: u64, role: &str, name: String, now: u64) {
    let blocked = |reason: &str, message: String| json!({ "at": rfc3339(now), "reason": reason, "message": message });
    let (room, margin) = room_of(state, role).await.unwrap_or((0, 0));
    let Some(to) = policy.next(capacity, room) else {
        let b = blocked(
            "NoRoom",
            format!("the {role} pool has {} free past its margin of {}: not a GiB to grow by", gib(room), gib(margin)),
        );
        tracing::warn!("volume {name} ({id}): not grown: {} (#690)", b["message"].as_str().unwrap_or_default());
        set_blocked(state, id, Some(b));
        return;
    };
    match crate::mgmt::api::kube::grow_volume(state, id, to).await {
        Ok(r) => {
            tracing::warn!("volume {name} ({id}): grown by its spec.grow from {capacity} to {to} bytes: {r} (#690)");
            metrics::counter!("storm_volume_grows_total", "volume" => id.0.to_string(), "name" => name).increment(1);
            let mut fill = state.fill.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = fill.vols.get_mut(&id) {
                let mut g = r;
                g["at"] = json!(rfc3339(now));
                g["by"] = json!("spec.grow");
                v.grown.push_back(g);
                while v.grown.len() > GROWN_KEPT {
                    v.grown.pop_front();
                }
                v.blocked = None;
            }
        }
        Err(e) => {
            tracing::warn!("volume {name} ({id}): spec.grow refused: {} (#690)", e.message);
            set_blocked(state, id, Some(blocked("Refused", e.message)));
        }
    }
}

fn set_blocked(state: &AppState, id: VolumeId, b: Option<Value>) {
    if let Some(v) = state.fill.lock().unwrap_or_else(|e| e.into_inner()).vols.get_mut(&id) {
        v.blocked = b;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(used: u64, avail: u64) -> FsUsage {
        FsUsage { size: used + avail + 3, used, avail }
    }

    #[test]
    fn percent_is_dfs_use_rounded_up() {
        assert_eq!(u(0, 100).percent(), 0);
        assert_eq!(u(80, 20).percent(), 80);
        assert_eq!(u(801, 199).percent(), 81);
        assert_eq!(FsUsage::default().percent(), 0);
    }

    #[test]
    fn conditions_turn_at_80_and_95_and_keep_their_transition_time() {
        let id = VolumeId(uuid::Uuid::new_v4());
        let mut f = Fill::default();
        f.read(id, u(50, 50), "/d/registry".into(), 1_000);
        let s = f.status(&id).unwrap();
        assert_eq!(s["conditions"][0]["type"], "VolumeFilling");
        assert_eq!(s["conditions"][0]["status"], "False");
        assert_eq!(s["conditions"][1]["status"], "False");
        assert_eq!(s["filesystem"]["usedPercent"], 50);
        // Filling: the time it crossed.
        f.read(id, u(90, 10), "/d/registry".into(), 2_000);
        f.read(id, u(91, 9), "/d/registry".into(), 3_000);
        let s = f.status(&id).unwrap();
        assert_eq!(s["conditions"][0]["status"], "True");
        assert_eq!(s["conditions"][0]["reason"], "UsedAtLeast80Percent");
        assert_eq!(s["conditions"][0]["lastTransitionTime"], rfc3339(2_000));
        assert_eq!(s["conditions"][1]["status"], "False");
        assert_eq!(s["usedBytes"], 91);
        assert!(s["conditions"][0]["message"].as_str().unwrap().contains("(91 %) on /d/registry"), "{s}");
        // Full.
        f.read(id, u(96, 4), "/d/registry".into(), 4_000);
        let s = f.status(&id).unwrap();
        assert_eq!((s["conditions"][0]["status"].as_str(), s["conditions"][1]["status"].as_str()), (Some("True"), Some("True")));
        assert_eq!(s["conditions"][1]["lastTransitionTime"], rfc3339(4_000));
        // Room again: both turn, at that time.
        f.read(id, u(10, 90), "/d/registry".into(), 5_000);
        let s = f.status(&id).unwrap();
        assert_eq!(s["conditions"][0]["status"], "False");
        assert_eq!(s["conditions"][0]["lastTransitionTime"], rfc3339(5_000));
        // Gone from the reads: gone from the status.
        f.keep_only(&Default::default());
        assert!(f.status(&id).is_none());
    }

    #[test]
    fn rfc3339_is_kubernetes_time() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_115_306), "2026-11-19T19:08:26Z");
    }

    #[test]
    fn a_policy_grows_by_its_step_to_its_max_within_the_room() {
        const G: u64 = GrowPolicy::GIB;
        let p = GrowPolicy { max: 8 * G, step: 2 * G };
        assert_eq!(p.next(4 * G, 100 * G), Some(6 * G));
        assert_eq!(p.next(7 * G, 100 * G), Some(8 * G), "the last step stops at max");
        assert_eq!(p.next(8 * G, 100 * G), None, "at max");
        assert_eq!(p.next(4 * G, G + G / 2), Some(5 * G), "the room, in whole GiB");
        assert_eq!(p.next(4 * G, G / 2), None, "no GiB of room");
        let small = GrowPolicy { max: 64 << 20, step: G };
        assert_eq!(small.next(32 << 20, 10 * G), Some(64 << 20), "a step past max stops at max");
        assert_eq!(small.next(64 << 20, 10 * G), None);
    }

    #[test]
    fn spec_grow_is_parsed_as_asked() {
        use serde_json::json;
        const G: u64 = GrowPolicy::GIB;
        assert_eq!(GrowPolicy::parse(&json!("64G"), Some(&json!("1G"))).unwrap(), GrowPolicy { max: 64 * G, step: G });
        assert_eq!(GrowPolicy::parse(&json!("64Gi"), None).unwrap(), GrowPolicy { max: 64 * G, step: G }, "a step is 1 GiB unsaid");
        assert_eq!(GrowPolicy::parse(&json!(10 * G), Some(&json!(G + 1))).unwrap().step, 2 * G, "whole GiB, rounded up");
        assert!(GrowPolicy::parse(&json!("512M"), None).is_err(), "max under a step");
        assert!(GrowPolicy::parse(&json!("8G"), Some(&json!(0))).is_err());
        assert!(GrowPolicy::parse(&json!(true), None).is_err());
        assert_eq!(crate::mgmt::api::kube::grow_policy_of(&json!(null)).unwrap(), None);
        assert!(crate::mgmt::api::kube::grow_policy_of(&json!({ "step": "1G" })).is_err(), "max is required");
        assert!(crate::mgmt::api::kube::grow_policy_of(&json!({ "max": "8G", "every": 3 })).is_err(), "an unknown field");
        assert_eq!(
            crate::mgmt::api::kube::grow_policy_of(&json!({ "max": "8G" })).unwrap(),
            Some(GrowPolicy { max: 8 * G, step: G })
        );
    }
}
