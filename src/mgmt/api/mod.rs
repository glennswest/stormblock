//! REST API routes — /api/v1/{drives,arrays,volumes,exports}.

pub mod drives;
pub mod arrays;
pub mod shelves;
pub mod volumes;
pub mod exports;
pub mod fstemplates;
pub mod images;
pub mod pallets;
pub mod slabs;
pub mod placement;
pub mod legs;
pub mod rebuilds;
pub mod scrub;
pub mod erasures;
pub mod release_erase;
pub mod usage;
#[cfg(feature = "stormfs-data")]
pub mod stormfs;
pub mod discovery;
pub mod v1;
pub mod kube;
pub mod moves;
pub mod synonyms;
pub mod boothost;
pub mod system;
pub mod release_stage;
pub mod releases;
pub mod goldens;
pub mod kernel;
pub mod forge;
#[cfg(feature = "iscsi")]
pub mod luns;
#[cfg(feature = "iscsi")]
pub mod sessions;
#[cfg(feature = "cluster")]
pub mod cluster;

use std::sync::Arc;

use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use super::AppState;

/// `GET /api/v1/health` — is there an engine here, and which one.
///
/// The one endpoint whose answer is the question. Everything else needs a
/// noun: a volume, a slab, a release. A machine that has just come up on a
/// network it was told nothing about has no noun to ask for yet — it is
/// trying to establish whether the thing at this address is an appliance at
/// all, and it has to do that before it can ask for anything.
///
/// Its absence cost five boots. The initramfs probes every candidate address
/// it can find with this path, and it 404'd on the appliance exactly as it
/// did on the machines that were not one, so a node that had been handed the
/// right address by DHCP reported "no engine" about it and dropped to a
/// shell.
///
/// Cheap on purpose: no waiting on a lock, no I/O. The one count, where the
/// slabs are (#322), runs only on a node with a remote slab, at most every
/// 10 s, and is skipped when anything is busy. A discovery probe runs
/// against several addresses on every boot of every node, and one that reads
/// state is one that answers slowly when the appliance is busy — which is
/// when a node most wants an answer.
/// It reports `auth` for the reason #107 exists: a fleet has to be able to ask
/// which of its nodes are open without trying to break into each one. Saying
/// "a token is required here" tells an attacker nothing they do not learn from
/// the first 401.
/// How long a ublk request may go unanswered before health names it (#337).
pub const UBLK_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

async fn health(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    #[derive(Serialize)]
    struct Health {
        status: &'static str,
        service: &'static str,
        version: &'static str,
        /// The commit this engine was built from (#466): `version` moves
        /// only at a tag, so this is what changes on every new golden.
        commit: &'static str,
        auth: &'static str,
        /// Why the volume records are not being written (#428): `status` is
        /// then `degraded`, and nothing new is made until they are. forge ran
        /// four days on records in memory only, health ok, and a restart lost
        /// 483 volumes.
        #[serde(skip_serializing_if = "Option::is_none")]
        durability: Option<String>,
        /// Sealed volumes' digests (#384): how many copies read intact, how
        /// many do not (`mismatched`, by id), how many were never read, and
        /// how many seals wait for a digest. Read without waiting.
        #[serde(skip_serializing_if = "Option::is_none")]
        digests: Option<serde_json::Value>,
        /// Volumes still here that lost mappings when they were restored or
        /// adopted, their slab not attached (#500): `extents` in all and
        /// `volumes` (id, name, extents). Such an extent reads as zeros. An
        /// install's old-release volumes, deleted right after, are not
        /// counted. Left out when every volume came back whole, or when the
        /// manager is busy (read without waiting).
        #[serde(skip_serializing_if = "Option::is_none")]
        lost_mappings: Option<serde_json::Value>,
        /// The mount list's volumes that did not mount at this boot, or took a
        /// repair (#532).
        #[serde(skip_serializing_if = "Option::is_none")]
        boot_mounts: Option<crate::mgmt::boot_mounts::BootMounts>,
        /// The worst of this node's RAID sets — `clean`, `rebuilding`,
        /// `degraded`, `failed` — when it has any (#252). Read without
        /// waiting: when the array table is busy, the last reading (#364).
        #[serde(skip_serializing_if = "Option::is_none")]
        raid: Option<&'static str>,
        /// Extents of this node's volumes still on a remote slab while the
        /// flow-over moves them onto the local disk; 0 once it has finished
        /// (#260). Left out when this engine runs no flow-over. Kept by the
        /// flow-over itself, so it is never left out for being busy: absent
        /// must not read as settled mid-move.
        #[serde(skip_serializing_if = "Option::is_none")]
        flow_over_remaining: Option<u64>,
        /// `pending` while a release erase waits for the next boot (#291);
        /// left out otherwise. The plan and the last report:
        /// `/api/v1/release`.
        #[serde(skip_serializing_if = "Option::is_none")]
        release: Option<&'static str>,
        /// How the flow-over is going (#401), when this engine runs one:
        /// extents moved, extents found all zeros and unmapped instead of
        /// copied, bytes copied, seconds since it began, MB/s, extents per
        /// second, and the ETA for `flow_over_remaining` at that rate.
        #[serde(skip_serializing_if = "Option::is_none")]
        flow_over: Option<FlowOver>,
        /// Why the flow-over stopped with extents still remote (#172): its
        /// destination full, or extents that would not move. Left out while
        /// it runs or once it has finished.
        #[serde(skip_serializing_if = "Option::is_none")]
        flow_over_stalled: Option<String>,
        /// Where the node's slabs are (#322): whether it runs from its own
        /// disk or from a remote (forge) clone, per half. A remote slab is
        /// named by its transport only. Never waited for: left out until a
        /// first answer could be read without waiting (unknown, not settled).
        #[serde(skip_serializing_if = "Option::is_none")]
        slabs: Option<super::slab_report::SlabReport>,
        /// The drives' layout (#573, STORAGE-LAYOUT.md step 8) on a node laid
        /// by `storage:`: drives and the format each got, system legs, the
        /// node-data set's state; as `GET /api/v1/storage/layout`. Left out
        /// on a node laid otherwise. `status` does not follow it: its own
        /// `state` says `degraded`.
        #[serde(skip_serializing_if = "Option::is_none")]
        storage_layout: Option<serde_json::Value>,
        /// The SAS link of each drive on a card (#642), in brief: its level
        /// and since when, and the drive side's loss-of-sync rate. Open, so
        /// the alarm reads without a token; the whole sample is in
        /// `/api/v1/storage/hba` and on `/metrics`. Left out with no card.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        drive_links: Vec<serde_json::Value>,
        /// Each drive's error and PI-check counters (#648), in brief: its
        /// level and since when, its PI checks (the drive's and the card's),
        /// uncorrected and medium errors, grown defects and their day's rate,
        /// why. Open like `drive_links`; left out with no card.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        drive_errors: Vec<serde_json::Value>,
        /// The boot's mode (#629): `diagnostic` when the install-config said
        /// so; this node runs from its image and lays and flows over nothing.
        /// Left out on an ordinary boot.
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<&'static str>,
        /// Runtime formats running or failed (#621), in brief: each drive's
        /// state, percent, ETA and format. Left out when none.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        formats: Vec<serde_json::Value>,
        /// Whether the volume record is kept (#587), as
        /// `/api/v1/slabs/durability` says it: `ok`, the slabs whose own
        /// region is `stale` (the data directory holds the record) and those
        /// `short` of room. Read without waiting on the volume manager; while
        /// it is busy, the last reading.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<crate::mgmt::api::slabs::DurabilitySummary>,
        /// Requests a ublk device was handed and has not answered for 30 s
        /// or more, oldest first: device (`volume:<id>`), `/dev/ublkbN`,
        /// queue, tag, op, seconds (#337). A FLUSH here is a consumer's
        /// fsync that has not returned. Left out when there is none. `status`
        /// stays `ok`: a probe asks whether the engine answers, not whether
        /// every device does.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        ublk_stuck: Vec<crate::drive::ublk::Stuck>,
        /// ublk devices this engine served that another process stopped or
        /// deleted (#299): `ublk`, `device`, `mount`, `how`, `since`, `boot`.
        /// A boot device lost makes `status` `failed`; another, `degraded`.
        #[cfg(target_os = "linux")]
        #[serde(skip_serializing_if = "Vec::is_empty")]
        ublk_lost: Vec<crate::drive::ublk::Lost>,
        /// The ublk features this kernel offers and the devices use, and how
        /// the engine's io_uring rings were set up (#505).
        #[cfg(target_os = "linux")]
        ublk_features: crate::drive::ublk::FeatureReport,
        /// Local drives whose I/O fails with transport errors (a controller
        /// reset, a dropped link) (#391): `path`, `since` (unix s),
        /// `last_error`, `retries`, and `gave_up` once the ride-through
        /// window ran out and errors go up. While `gave_up` is false the
        /// node is riding it through and callers only wait. Left out when
        /// every drive answers.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        drives_unreachable: Vec<crate::drive::ridethrough::Stall>,
        /// `data_path` and `control` (#85): the storage network's interface,
        /// `state` (`up`, `down`, `no_address`, `missing`), the address the
        /// data targets serve on, its MTU against `[storage] mtu`; and the
        /// control interface's, when named. Read from sysfs and the
        /// interface's addresses, never a lock. `status` stays `ok` with the
        /// data path down: this answer is the control channel, and it is up —
        /// the node's volumes are what cannot be reached.
        #[serde(flatten)]
        networks: crate::mgmt::config::NetworkReport,
        /// This process's open file descriptors and its soft limit (#510): a
        /// leak shows long before EMFILE; a warning goes to the log past 80 %.
        #[cfg(target_os = "linux")]
        #[serde(skip_serializing_if = "Option::is_none")]
        fds: Option<crate::fdlimit::Fds>,
        /// With TLS configured (#483): what the listener `serving` (`https`,
        /// or `http` while the pair is not there) and the keys of the
        /// configured files `missing`, none of which stops the engine. Key
        /// names only, never a path. Left out with no TLS configured.
        #[serde(skip_serializing_if = "Option::is_none")]
        tls: Option<crate::mgmt::tls::TlsStatus>,
        /// `[management] boot_listen`, when set (#524): whether it is bound.
        #[serde(skip_serializing_if = "Option::is_none")]
        boot_listen: Option<BootListen>,
    }
    #[derive(Serialize)]
    struct BootListen {
        addr: String,
        listening: bool,
    }
    #[derive(Serialize)]
    struct FlowOver {
        #[serde(flatten)]
        progress: crate::flowprogress::Snapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        eta_seconds: Option<f64>,
    }
    let flow_over_remaining =
        u64::try_from(state.flow_over_remaining.load(std::sync::atomic::Ordering::Relaxed)).ok();
    let flow_over = flow_over_remaining.and_then(|left| {
        let progress = crate::flowprogress::FLOW.snapshot()?;
        Some(FlowOver { progress, eta_seconds: crate::flowprogress::FLOW.eta_seconds(left) })
    });
    // From the last reading (#364): read when the table is free, kept, and
    // answered from when it is busy, so a busy node does not drop the field.
    static RAID_SEEN: std::sync::Mutex<Option<Option<&'static str>>> = std::sync::Mutex::new(None);
    let raid = match state.arrays.try_read() {
        Ok(a) => {
            let rank = |s: &str| match s {
                "failed" => 3,
                "degraded" => 2,
                "rebuilding" => 1,
                _ => 0,
            };
            let now = a.values().map(|i| i.array.status().state).max_by_key(|s| rank(s));
            *RAID_SEEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
            now
        }
        Err(_) => RAID_SEEN.lock().unwrap_or_else(|e| e.into_inner()).flatten(),
    };
    let durability = state.durability.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let digests = crate::volume::digest::summary(&state.volume_manager).await;
    let lost_mappings = state.volume_manager.try_lock().ok().and_then(|m| {
        let lost = m.lost_mappings();
        (!lost.is_empty()).then(|| {
            serde_json::json!({
                "extents": lost.iter().map(|(_, _, n)| n).sum::<usize>(),
                "volumes": lost.iter().map(|(id, name, n)| serde_json::json!({"id": id.0, "name": name, "extents": n})).collect::<Vec<_>>(),
            })
        })
    });
    #[cfg(target_os = "linux")]
    let lost_devices = crate::drive::ublk::lost();
    #[cfg(target_os = "linux")]
    let lost_status = if lost_devices.iter().any(|l| l.boot) {
        Some("failed")
    } else if !lost_devices.is_empty() {
        Some("degraded")
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let lost_status: Option<&'static str> = None;
    let slabs = super::slab_report::for_health(&state);
    let storage_layout = super::storage_layout::report(slabs.as_ref());
    let drive_links = super::link::summary();
    let drive_errors = super::drive_errors::summary();
    let metadata = crate::mgmt::api::slabs::summary_for_health(&state).await;
    let boot_mounts = crate::mgmt::boot_mounts::read();
    let mounts_failed = boot_mounts.as_ref().is_some_and(|m| !m.failed.is_empty());
    Json(Health {
        status: if mounts_failed {
            "failed"
        } else {
            lost_status.unwrap_or(if durability.is_some() { "degraded" } else { "ok" })
        },
        durability,
        digests,
        lost_mappings,
        boot_mounts,
        service: "stormblock",
        version: crate::version::VERSION,
        commit: crate::version::COMMIT,
        auth: if state.auth_enforced() { "required" } else { "none" },
        raid,
        flow_over_remaining,
        release: release_erase::pending_here(&state).then_some("pending"),
        flow_over,
        flow_over_stalled: state.flow_over_stalled.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        slabs,
        storage_layout,
        drive_links,
        drive_errors,
        mode: crate::boot_mode::mode(),
        formats: super::format_job::summary(),
        metadata,
        ublk_stuck: crate::drive::ublk::stuck(UBLK_STUCK_AFTER),
        #[cfg(target_os = "linux")]
        ublk_lost: lost_devices.clone(),
        #[cfg(target_os = "linux")]
        ublk_features: crate::drive::ublk::feature_report(),
        drives_unreachable: crate::drive::ridethrough::stalls(),
        networks: state.config.network_report(),
        #[cfg(target_os = "linux")]
        fds: crate::fdlimit::report(),
        tls: state.mgmt_tls.get().map(|r| r.status()),
        boot_listen: state.config.management.boot_listen.clone().map(|addr| BootListen {
            addr,
            listening: state.boot_listening.load(std::sync::atomic::Ordering::Relaxed),
        }),
    })
    .into_response()
}

/// Which build this engine is (#466): `{service, version, commit}`. Open,
/// like health, and cheaper: nothing read but two constants, for a caller
/// (stormcentral's performance gate) that only wants to see a change.
/// `GET /api/v1/storage/layout` (#573): the drives' layout, or 404 on a node
/// not laid by `storage:`.
async fn storage_layout(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    let slabs = super::slab_report::for_health(&state);
    match super::storage_layout::report(slabs.as_ref()) {
        Some(v) => Json(v).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "this node was not laid by a storage: section (no storage-layout.json)"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/storage/hba` (#611): the card's rows, kept current.
async fn hba_rows() -> Response {
    Json(super::hba::report()).into_response()
}

#[derive(serde::Deserialize, Default)]
struct HbaDetailsQuery {
    fresh: Option<String>,
}

fn hba_addr(sas: &str) -> Result<u64, Response> {
    crate::drive::mpt::parse_addr(sas).ok_or_else(|| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("{sas} is no SAS address")}))).into_response()
    })
}

/// `GET /api/v1/storage/hba/{sas}/details` (#611): the drive's own answers,
/// cached with when they were read; `?fresh=1` reads them again.
async fn hba_details(
    axum::extract::Path(sas): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HbaDetailsQuery>,
) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let k = format!("0x{a:016x}");
    let fresh = matches!(q.fresh.as_deref(), Some("1" | "true" | "yes"));
    if !fresh {
        if let Some(v) = super::hba::cached_details(&k) {
            return Json(v).into_response();
        }
    }
    match tokio::task::spawn_blocking(move || super::hba::read_details(a)).await {
        Ok(Ok(v)) => {
            super::hba::keep_details(&k, v.clone());
            Json(v).into_response()
        }
        Ok(Err(e)) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct HbaActionRequest {
    action: String,
    block_size: Option<u32>,
    pi: Option<String>,
    /// A format's confirmation: the drive's SAS address.
    confirm: Option<String>,
}

/// `POST /api/v1/storage/hba/{sas}/action` (#611; admin only, #274):
/// spin_up, spin_down, self_test, self_test_extended, format. A format
/// starts (IMMED) and runs on: its progress is in the details.
async fn hba_action(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sas): axum::extract::Path<String>,
    Json(req): Json<HbaActionRequest>,
) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let act = match crate::drive::scsi::Action::parse(&req.action, req.block_size, req.pi.as_deref()) {
        Ok(x) => x,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    };
    let want = format!("0x{a:016x}");
    if matches!(act, crate::drive::scsi::Action::Format { .. })
        && req.confirm.as_deref().and_then(crate::drive::mpt::parse_addr) != Some(a)
    {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({"error": format!("a format loses every byte on {want}: confirm with its SAS address"), "confirm": want})),
        )
            .into_response();
    }
    if matches!(act, crate::drive::scsi::Action::Format { .. }) {
        if let Some(r) = in_use(&state, &[a]).await.into_iter().next() {
            return (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{r}: not formatted")}))).into_response();
        }
    }
    tracing::warn!(drive = %want, action = %req.action, "an action on the card's drive (#611)");
    match tokio::task::spawn_blocking(move || super::hba::act(a, act)).await {
        Ok(Ok(said)) => Json(serde_json::json!({"sas_address": want, "said": said})).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"sas_address": want, "error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// The drives among `drives` this engine has a slab open on, each said
/// (`0x… is /dev/sdb, which holds a slab this engine has open`).
async fn in_use(state: &AppState, drives: &[u64]) -> Vec<String> {
    let rows = super::hba::report()["rows"].as_array().cloned().unwrap_or_default();
    let reg = state.slab_registry.read().await;
    let mut out = Vec::new();
    for a in drives {
        let want = format!("0x{a:016x}");
        let Some(b) = rows.iter().find(|r| r["sas_address"] == want.as_str()).and_then(|r| r["block"].as_str()) else { continue };
        let dev = format!("/dev/{b}");
        if reg.iter().any(|(_, s)| crate::mgmt::storage_layout::on_drive(&s.device().id().path, &dev)) {
            out.push(format!("{want} is {dev}, which holds a slab this engine has open"));
        }
    }
    out
}

#[derive(serde::Deserialize, Default)]
struct FormatRequest {
    /// `4096+pi1` (the default, the storage policy's), `4096` or `512`.
    format: Option<String>,
    /// Formats to fall back to when the drive's firmware refuses writes at
    /// the one before; default `["4096"]` (4096 without PI).
    fallback: Option<Vec<String>>,
    /// The write test after each format (default true).
    test: Option<bool>,
    /// A shelf: one drive first (default: when any drive is NetApp's).
    canary: Option<bool>,
    /// The drive's SAS address, or the enclosure's id for a shelf.
    confirm: Option<String>,
}

fn format_opts(req: &FormatRequest, canary_default: bool) -> Result<super::format_job::Opts, String> {
    let want = match &req.format {
        Some(f) => super::format_job::Want::parse(f)?,
        None => super::format_job::POLICY,
    };
    let fallback = match &req.fallback {
        Some(v) => v.iter().map(|f| super::format_job::Want::parse(f)).collect::<Result<Vec<_>, _>>()?,
        None if want == super::format_job::POLICY => vec![super::format_job::Want { block_size: 4096, pi_type: 0 }],
        None => vec![],
    };
    Ok(super::format_job::Opts { want, fallback, test: req.test.unwrap_or(true), canary: req.canary.unwrap_or(canary_default) })
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn start_format(drives: Vec<(u64, Option<String>, Option<u64>)>, opts: super::format_job::Opts) -> Result<Vec<String>, String> {
    super::format_job::start(&super::format_job::Card, drives, opts)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn start_format(_drives: Vec<(u64, Option<String>, Option<u64>)>, _opts: super::format_job::Opts) -> Result<Vec<String>, String> {
    Err("no mpt card here".into())
}

fn row_of(sas: &str) -> Option<serde_json::Value> {
    super::hba::report()["rows"].as_array().and_then(|rows| rows.iter().find(|r| r["sas_address"] == sas).cloned())
}

/// `POST /api/v1/storage/hba/{sas}/format` (#621; admin only, #274): the
/// runtime format of one drive as a job, followed to its end: rescanned,
/// write-tested, falling back as asked; its progress in the row's
/// `format_job` and `GET /api/v1/storage/formats`. `confirm`: its SAS
/// address. A drive this engine has a slab open on is refused.
async fn hba_format(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sas): axum::extract::Path<String>,
    Json(req): Json<FormatRequest>,
) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let want = format!("0x{a:016x}");
    if req.confirm.as_deref().and_then(crate::drive::mpt::parse_addr) != Some(a) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({"error": format!("a format loses every byte on {want}: confirm with its SAS address"), "confirm": want})),
        )
            .into_response();
    }
    let opts = match format_opts(&req, false) {
        Ok(o) => o,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    };
    let Some(row) = row_of(&want) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("no card lists {want}")}))).into_response();
    };
    if let Some(r) = in_use(&state, &[a]).await.into_iter().next() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{r}: not formatted")}))).into_response();
    }
    tracing::warn!(drive = %want, format = %opts.want.said(), "a runtime format (#621)");
    let at = (row["enclosure"].as_str().map(str::to_string), row["slot"].as_u64());
    match tokio::task::spawn_blocking(move || start_format(vec![(a, at.0, at.1)], opts)).await {
        Ok(Ok(v)) => (StatusCode::ACCEPTED, Json(serde_json::json!({"started": v}))).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// `POST /api/v1/storage/enclosures/{id}/format` (#621; admin only): every
/// present drive the card lists in that enclosure (a shelf), as jobs.
/// `confirm`: the enclosure's id. Drives this engine has a slab open on are
/// left out, said; a drive already at the format is not formatted again.
/// A shelf with a NetApp-firmware drive has one drive first (`canary`).
async fn enclosure_format(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<FormatRequest>,
) -> Response {
    let Some(e) = crate::drive::mpt::parse_addr(&id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("{id} is no enclosure id")}))).into_response();
    };
    let enc = format!("0x{e:016x}");
    if req.confirm.as_deref().and_then(crate::drive::mpt::parse_addr) != Some(e) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({"error": format!("a shelf format loses every byte on every drive of enclosure {enc}: confirm with its id"), "confirm": enc})),
        )
            .into_response();
    }
    let rows = super::hba::report()["rows"].as_array().cloned().unwrap_or_default();
    let mut shelf: Vec<&serde_json::Value> = rows.iter().filter(|r| r["enclosure"] == enc.as_str() && r["present"] == true).collect();
    shelf.sort_by_key(|r| r["slot"].as_u64().unwrap_or(u64::MAX));
    if shelf.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("no card lists a drive in enclosure {enc}")}))).into_response();
    }
    let netapp = shelf.iter().any(|r| r["vendor"].as_str().is_some_and(|v| v.trim().eq_ignore_ascii_case("NETAPP")));
    let opts = match format_opts(&req, netapp) {
        Ok(o) => o,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    };
    let addrs: Vec<u64> = shelf.iter().filter_map(|r| r["sas_address"].as_str().and_then(crate::drive::mpt::parse_addr)).collect();
    let busy = in_use(&state, &addrs).await;
    let drives: Vec<(u64, Option<String>, Option<u64>)> = shelf
        .iter()
        .filter_map(|r| r["sas_address"].as_str().and_then(crate::drive::mpt::parse_addr).map(|a| (a, Some(enc.clone()), r["slot"].as_u64())))
        .filter(|(a, _, _)| !busy.iter().any(|b| b.starts_with(&format!("0x{a:016x}"))))
        .collect();
    tracing::warn!(enclosure = %enc, drives = drives.len(), format = %opts.want.said(), canary = opts.canary, "a shelf format (#621)");
    match tokio::task::spawn_blocking(move || start_format(drives, opts)).await {
        Ok(Ok(v)) => (StatusCode::ACCEPTED, Json(serde_json::json!({"started": v, "left_out": busy, "canary": netapp || req.canary.unwrap_or(false)}))).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e, "left_out": busy}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// `GET /api/v1/storage/formats` (#621): every runtime format job, newest
/// first: state, format, percent, ETA, elapsed, its lines.
async fn storage_formats() -> Response {
    Json(serde_json::json!({"jobs": super::format_job::all()})).into_response()
}

#[derive(serde::Deserialize)]
struct FirmwareQuery {
    confirm: Option<String>,
    /// WRITE BUFFER mode: `0x0e` (deferred, then activated; the default) or `0x07`.
    mode: Option<String>,
}

/// `POST /api/v1/storage/hba/{sas}/firmware?confirm=<sas>` (#621; admin
/// only): the body is the vendor's microcode image, downloaded with WRITE
/// BUFFER through sg or the card; the revision read before and after. A
/// drive in use or being formatted is refused. Never another vendor's image:
/// the drive refuses what it does not take, and that is said.
async fn hba_firmware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sas): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<FirmwareQuery>,
    body: axum::body::Bytes,
) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let want = format!("0x{a:016x}");
    if q.confirm.as_deref().and_then(crate::drive::mpt::parse_addr) != Some(a) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({"error": format!("a firmware download replaces {want}'s microcode: confirm with its SAS address"), "confirm": want})),
        )
            .into_response();
    }
    let mode = match q.mode.as_deref().unwrap_or("0x0e").trim_start_matches("0x") {
        "0e" | "e" => 0x0E,
        "07" | "7" => 0x07,
        m => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("mode {m}: 0x0e or 0x07")}))).into_response(),
    };
    if super::format_job::running(a) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{want} is being formatted")}))).into_response();
    }
    if let Some(r) = in_use(&state, &[a]).await.into_iter().next() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{r}: no firmware download")}))).into_response();
    }
    tracing::warn!(drive = %want, bytes = body.len(), "a drive firmware download (#621)");
    match tokio::task::spawn_blocking(move || super::hba::firmware(a, &body, mode)).await {
        Ok(Ok(lines)) => Json(serde_json::json!({"sas_address": want, "lines": lines})).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"sas_address": want, "error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct HbaWriteTestRequest {
    /// The drive's SAS address: two of its blocks are overwritten.
    confirm: Option<String>,
}

/// `POST /api/v1/storage/hba/{sas}/write-test` (#627; admin only, #274):
/// the write-test matrix on the drive at the format it has. Overwrites LBA 0
/// and the last LBA: `confirm` is its SAS address, and a drive this engine
/// has a slab open on is refused. `GET` answers the last run.
async fn hba_write_test(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sas): axum::extract::Path<String>,
    Json(req): Json<HbaWriteTestRequest>,
) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let want = format!("0x{a:016x}");
    if req.confirm.as_deref().and_then(crate::drive::mpt::parse_addr) != Some(a) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({"error": format!("the write test overwrites LBA 0 and the last LBA of {want}: confirm with its SAS address"), "confirm": want})),
        )
            .into_response();
    }
    // Never a drive in use: its block device, by the card's row, against
    // every slab this engine has open.
    let block = super::hba::report()["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["sas_address"] == want.as_str()).and_then(|r| r["block"].as_str().map(str::to_string)));
    if let Some(b) = block {
        let dev = format!("/dev/{b}");
        let reg = state.slab_registry.read().await;
        if reg.iter().any(|(_, s)| crate::mgmt::storage_layout::on_drive(&s.device().id().path, &dev)) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": format!("{want} is {dev}, which holds a slab this engine has open: not tested")})),
            )
                .into_response();
        }
    }
    tracing::warn!(drive = %want, "the write-test matrix on the card's drive (#627)");
    match tokio::task::spawn_blocking(move || super::hba::write_test(a)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"sas_address": want, "error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// `GET /api/v1/storage/hba/{sas}/write-test` (#627): the last run.
async fn hba_write_test_last(axum::extract::Path(sas): axum::extract::Path<String>) -> Response {
    let a = match hba_addr(&sas) {
        Ok(a) => a,
        Err(r) => return r,
    };
    match super::hba::cached_test(&format!("0x{a:016x}")) {
        Some(v) => Json(v).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no write test run on this drive since the engine started"}))).into_response(),
    }
}

async fn version() -> Response {
    Json(serde_json::json!({
        "service": "stormblock",
        "version": crate::version::VERSION,
        "commit": crate::version::COMMIT,
    }))
    .into_response()
}

/// Build the complete API router.
pub fn router(state: Arc<AppState>) -> Router {
    let r = Router::new()
        .route("/api/v1/health", axum::routing::get(health).with_state(state.clone()))
        .route("/api/v1/version", axum::routing::get(version))
        .route("/api/v1/storage/layout", axum::routing::get(storage_layout).with_state(state.clone()))
        .route("/api/v1/storage/hba", axum::routing::get(hba_rows))
        .route("/api/v1/storage/hba/{sas}/details", axum::routing::get(hba_details))
        .route("/api/v1/storage/hba/{sas}/action", axum::routing::post(hba_action).with_state(state.clone()))
        .route("/api/v1/storage/hba/{sas}/format", axum::routing::post(hba_format).with_state(state.clone()))
        .route(
            "/api/v1/storage/hba/{sas}/firmware",
            axum::routing::post(hba_firmware).with_state(state.clone()).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/v1/storage/enclosures/{id}/format", axum::routing::post(enclosure_format).with_state(state.clone()))
        .route("/api/v1/storage/formats", axum::routing::get(storage_formats))
        .route(
            "/api/v1/storage/hba/{sas}/write-test",
            axum::routing::post(hba_write_test).get(hba_write_test_last).with_state(state.clone()),
        )
        .nest("/api/v1/drives", drives::router(state.clone()))
        .nest("/api/v1/arrays", arrays::router(state.clone()))
        .nest("/api/v1/shelves", shelves::shelves_router(state.clone()))
        .nest("/api/v1/spares", shelves::spares_router(state.clone()))
        .nest("/api/v1/volumes", volumes::router(state.clone()))
        .nest("/api/v1/exports", exports::router(state.clone()))
        .nest("/api/v1/slabs", slabs::router(state.clone()))
        .nest("/api/v1/pallets", pallets::router(state.clone()))
        .nest("/api/v1/images", images::router(state.clone()))
        .nest("/api/v1/fstemplates", fstemplates::router(state.clone()))
        .nest("/api/v1/moves", moves::router(state.clone()))
        .nest("/api/v1/rebuilds", rebuilds::router(state.clone()))
        .nest("/api/v1/scrub", scrub::router(state.clone()))
        .nest("/api/v1/namespaces", crate::mgmt::namespaces::router(state.clone()))
        .nest("/api/v1/erasures", erasures::router(state.clone()))
        .nest("/api/v1/rollback", crate::mgmt::rollback::router(state.clone()))
        .nest("/api/v1/release", release_erase::router(state.clone()))
        .nest("/api/v1/synonyms", synonyms::router(state.clone()))
        .nest("/api/v1/boothost", boothost::router(state.clone()))
        .nest("/api/v1/system", system::router(state.clone()))
        .nest("/api/v1/releases", releases::router(state.clone()))
        .nest("/api/v1/goldens", goldens::router(state.clone()))
        .nest("/api/v1/kernel", kernel::router(state.clone()))
        .nest("/api/v1/discovery", discovery::router(state.clone()))
        // CSI/wander-operator contract surface (stormblock-csi docs/stormblock-api.md)
        .nest("/v1", v1::router(state.clone()))
        // Kubernetes-shaped resources, served by the engine itself (#80).
        .merge(kube::router(state.clone()));

    // This node as its site's forge (#272).
    let r = r.nest("/api/v1/forge", forge::router(state.clone()));

    // The StormFS data path (#49, #50). Out of the `mikrotik` profile: a
    // RouterOS node with 256 MB serves container volumes over NVMe-TCP; it is
    // not a StormFS data node, and a surface that is there invites being
    // called.
    #[cfg(feature = "stormfs-data")]
    let r = r.nest("/api/v1/stormfs", stormfs::router(state.clone()));

    // The serving surface (#60). Layer 2 in `docs/layering.md` — "what it
    // takes to serve volumes to something", which is the job rather than a
    // choice a deployment makes differently. Mounted here rather than by each
    // profile so that a consumer calling it can rely on it being there:
    // a surface only some profiles serve is a convention, not a guarantee.
    let r = match state.serve.get() {
        Some(serve) => r.merge(crate::serve::api::router(serve.clone())),
        None => r,
    };

    #[cfg(feature = "iscsi")]
    let r = r.nest("/api/v1/luns", luns::router(state.clone()));

    #[cfg(feature = "iscsi")]
    let r = r.nest("/api/v1/sessions", sessions::router(state.clone()));
    let r = r.nest("/api/v1/connections", exports::connections_router());

    #[cfg(feature = "cluster")]
    let r = r.merge(cluster::router(state.clone()));

    #[cfg(feature = "cluster")]
    let r = if let Some(ref cluster_mgr) = state.cluster {
        r.merge(cluster_mgr.rpc_router())
    } else {
        r
    };

    // `/metrics` is served here rather than beside this router so that the
    // credential check below covers it too: telemetry names this node's
    // volumes and how full it is, and `serve::api::is_public` has always said
    // a scrape is not a probe. A scraper presents the token like anything
    // else.
    let r = r.merge(crate::mgmt::metrics::metrics_router(state.clone()));

    // The web UI (feature `ui`), here and not beside this router (#166): a
    // `Router::layer` wraps only the routes that exist when it is called, so
    // `/ui` nested after the check below was served to anyone, its create and
    // delete forms included.
    #[cfg(feature = "ui")]
    let r = r
        .nest("/ui", crate::mgmt::ui::ui_router(state.clone()))
        .route("/", axum::routing::get(|| async { axum::response::Redirect::permanent("/ui/") }));

    // One credential check, over every surface this router serves (#107).
    //
    // It goes on last so it wraps `/api/v1`, `/v1`, `/serve/v1` and the kube
    // resources alike — and unmatched paths too, which is right: a 404 is
    // still an answer about what this node is. `/v1` used to check a token by
    // itself and everything beside it was open, which is the shape of hole
    // that survives review: the setting existed, so the surface read as
    // guarded.
    //
    // What it enforces is whatever the node resolved at startup
    // (`mgmt::auth::resolve`). A router built in-process with no resolution
    // enforces the config's own token, or nothing.
    let r = r.merge(crate::mgmt::debug::router(state.clone()));
    // The install's page and record (#616), served by the node after the
    // initramfs's `install-status` stops at switch_root: one view on the
    // host for install and runtime. Open, GET only (`serve::api::is_public`).
    // With the old web UI (feature `ui`) `/` stays its redirect.
    let r = r.merge(crate::install_progress::router(
        std::path::PathBuf::from(crate::install_progress::RUN),
        !cfg!(feature = "ui"),
    ));
    r.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        crate::mgmt::auth::require_token,
    ))
    // Outermost: every request is registered for the stall watchdog (#269),
    // from the moment it arrives, the credential check included.
    .layer(axum::middleware::from_fn_with_state(state.clone(), crate::mgmt::debug::track))
}

/// Standard error response.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 404,
        };
        (StatusCode::NOT_FOUND, Json(body)).into_response()
    }

    pub fn bad_request(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 400,
        };
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    }

    pub fn unprocessable(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 422,
        };
        (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
    }

    pub fn conflict(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 409,
        };
        (StatusCode::CONFLICT, Json(body)).into_response()
    }

    pub fn forbidden(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 403,
        };
        (StatusCode::FORBIDDEN, Json(body)).into_response()
    }

    pub fn internal(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 500,
        };
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

/// Everything on this node that is currently serving `volume_id`.
///
/// One answer, shared: a volume-level move must not copy a filesystem
/// something is still writing to (#20), and the template orphan sweep must not
/// delete a volume something has attached (#48). Both questions are the same
/// question, and having two implementations of it means one of them is
/// eventually wrong.
/// Lock order note: the StormFS state is taken **first** here, because two of
/// its own handlers hold it while reaching for the volume manager (#49, #50).
/// One order, stated once.
pub async fn what_is_serving(state: &AppState, volume_id: uuid::Uuid) -> Vec<String> {
    let mut busy = Vec::new();

    // A pin's snapshot is being read by a StormFS reader that has no other
    // way of saying so — deleting it out from under them is the exact thing
    // the pin exists to prevent.
    #[cfg(feature = "stormfs-data")]
    {
        let st = state.stormfs.lock().await;
        if st.pins.is_pinned_snapshot(crate::volume::VolumeId(volume_id)) {
            busy.push("a StormFS version pin".to_string());
        }
    }

    for e in state.exports.read().await.iter() {
        if e.volume_id == volume_id {
            busy.push(format!("export {} ({})", e.id, e.protocol));
        }
    }
    #[cfg(feature = "iscsi")]
    for (lun_id, entry) in state.lun_entries.read().await.iter() {
        if matches!(entry.backing, crate::mgmt::LunBacking::Volume { volume_id: v } if v == volume_id)
        {
            busy.push(format!("iSCSI LUN {lun_id}"));
        }
    }
    if state.ublk_exports.lock().await.is_exported(&volume_id.to_string()) {
        busy.push("a ublk device".to_string());
    }
    #[cfg(target_os = "linux")]
    if state.vhost_exports.lock().await.is_exported(volume_id) {
        busy.push("a vhost-user-blk socket".to_string());
    }
    // A namespace on the shared NVMe subsystem — what the volume attach hot-
    // adds — was not checked here, so a volume attached that way could be
    // deleted from under its host (#138).
    if let Some(nsid) = state.v1.lock().await.nvme_nsids.get(&volume_id.to_string()) {
        busy.push(format!("NVMe namespace {nsid}"));
    }
    // Served to a host from its own subsystem (#210).
    for (nqn, nsid) in state.nvme_hosts.lock().await.serving(volume_id) {
        busy.push(format!("NVMe namespace {nsid} of {nqn}"));
    }
    // A published release is a reference too, and the only one that survives
    // this process. Nothing was checking it, so eight releases ended up naming
    // volumes that had been reclaimed — manifests promising a download that
    // cannot happen (#106). Withdrawing the release is the deliberate act;
    // deleting the volume under it is not.
    for version in releases::naming_volume(state, volume_id).await {
        busy.push(format!("published release {version} (unpublish it first)"));
    }
    busy
}

/// Every volume this node is currently serving.
///
/// The set form, for callers deciding what is safe to delete in bulk.
pub async fn volumes_in_use(state: &AppState) -> std::collections::HashSet<uuid::Uuid> {
    let mut in_use = std::collections::HashSet::new();
    // StormFS lock first, as above — this one goes on to take the volume
    // manager, so the order is not optional.
    #[cfg(feature = "stormfs-data")]
    for pin in state.stormfs.lock().await.pins.list() {
        in_use.insert(pin.snapshot.0);
    }
    for e in state.exports.read().await.iter() {
        in_use.insert(e.volume_id);
    }
    #[cfg(feature = "iscsi")]
    for entry in state.lun_entries.read().await.values() {
        if let crate::mgmt::LunBacking::Volume { volume_id } = entry.backing {
            in_use.insert(volume_id);
        }
    }
    // ublk exports are keyed by the /v1 volume id rather than the engine's, so
    // they are matched by string against the engine ids we already have.
    // The ids first, the manager let go, then the exports (#571): holding
    // `ublk_exports` while waiting on the manager deadlocked with a listing
    // that held the manager and asked for the exports.
    let volumes: Vec<uuid::Uuid> = state.volume_manager.lock().await.volume_ids().into_iter().map(|id| id.0).collect();
    let ublk = state.ublk_exports.lock().await;
    for id in volumes {
        if ublk.is_exported(&id.to_string()) {
            in_use.insert(id);
        }
    }
    drop(ublk);
    // And everything the Volumes view calls attached — shared-subsystem
    // namespaces, per-volume subsystems, the serve wiring, adopted boot
    // devices — so the listing and the guards cannot disagree (#138).
    in_use.extend(usage::Context::gather(state).await.attached());
    // Anything a published release names, for the same reason (#106).
    in_use.extend(releases::published_volumes(state).await);
    in_use
}

/// Standard list response wrapper.
#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub count: usize,
}
