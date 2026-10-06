//! Staging the next release on this node, activating it, rolling back (#122).
//!
//! ```text
//! POST   /api/v1/releases/{v}/stage     {source, current?, root?, disk?}  202: a job
//! GET    /api/v1/releases/{v}/stage     the job: running|complete|failed, progress, plan
//! DELETE /api/v1/releases/{v}/stage     discard what is staged
//! POST   /api/v1/releases/{v}/activate  {current?}: N aside, N+1 onto the plain names
//! POST   /api/v1/releases/rollback      back to the previous generation
//! GET    /api/v1/releases/generations   current, staged, previous
//! ```
//!
//! What it does and why: [`crate::image::stage`]. Every verb but the GETs is
//! destructive in the auth sense (#274): it rewrites what the node boots.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::ApiError;
use crate::drive::BlockDevice;
use crate::image::stage::{self, Generations, Progress, StageOptions};
use crate::mgmt::AppState;

/// A stage in progress, or the last one.
#[derive(Clone)]
pub struct StageJob {
    pub version: String,
    pub state: String,
    pub error: Option<String>,
    pub plan: Vec<stage::PlanItem>,
    pub pallet: Option<String>,
    pub progress: Arc<Progress>,
    pub started: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn data_dir(state: &AppState) -> Result<PathBuf, Response> {
    state
        .config
        .management
        .data_dir
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| ApiError::conflict("no management.data_dir: a node's release generations have nowhere to be kept"))
}

fn conflict(e: impl std::fmt::Display) -> Response {
    ApiError::conflict(e.to_string())
}

/// The node's disk: `asked`, else the first slab path that carries a node
/// layout. `None` when there is none (a node without a laid disk).
async fn node_disk(state: &AppState, asked: Option<&str>) -> Option<(String, Arc<dyn BlockDevice>)> {
    let paths: Vec<String> = match asked {
        Some(p) => vec![p.to_string()],
        None => state.slab_paths.read().await.clone(),
    };
    for p in paths {
        let Ok(dev) = crate::drive::open_path(&p, false).await else { continue };
        if matches!(crate::image::local::node_layout(&dev).await, Ok(Some(_))) {
            return Some((p, dev));
        }
    }
    None
}

async fn open_source(source: &str) -> anyhow::Result<Arc<dyn BlockDevice>> {
    if source.starts_with("http://") {
        return Ok(Arc::new(crate::drive::httpdev::HttpDevice::open(source).await?));
    }
    Ok(crate::drive::open_path(source, true).await?)
}

#[derive(Debug, Deserialize)]
pub struct StageRequest {
    /// The release's image: `http://…/api/v1/releases/{v}/image.img` on the
    /// appliance, or a path.
    pub source: String,
    /// The release the node runs now, when the engine has not been told yet:
    /// the name N's volumes take at activate (`<name>@<current>`).
    #[serde(default)]
    pub current: Option<String>,
    /// The release's root volume, where its policy file is (default
    /// `stormpump`).
    #[serde(default)]
    pub root: Option<String>,
    /// The node's disk, when the engine cannot tell (default: the slab path
    /// with a node layout).
    #[serde(default)]
    pub disk: Option<String>,
}

fn job_json(job: &StageJob) -> serde_json::Value {
    use std::sync::atomic::Ordering::Relaxed;
    json!({
        "version": job.version,
        "state": job.state,
        "error": job.error,
        "started": job.started,
        "volumes_done": job.progress.volumes_done.load(Relaxed),
        "volumes_total": job.progress.volumes_total.load(Relaxed),
        "bytes_copied": job.progress.bytes_copied.load(Relaxed),
        "pallet": job.pallet,
        "plan": job.plan,
    })
}

pub async fn post_stage(
    State(state): State<Arc<AppState>>,
    Path(version): Path<String>,
    Json(req): Json<StageRequest>,
) -> Response {
    let dir = match data_dir(&state) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if version.contains('@') || version.contains('/') || version.trim().is_empty() {
        return ApiError::bad_request(format!("{version:?} is not a release version"));
    }
    {
        let job = state.stage_job.lock().await;
        if let Some(j) = job.as_ref().filter(|j| j.state == "running") {
            return conflict(format!("release {} is being staged", j.version));
        }
    }
    let mut gens = Generations::load(&dir);
    if gens.current.as_ref().map(|c| c.version.as_str()) == Some(version.as_str()) {
        return conflict(format!("release {version} is what this node runs"));
    }
    if let Some(c) = req.current.as_deref() {
        match &gens.current {
            Some(cur) if cur.version != c => {
                return conflict(format!("this node runs {} by its own record, not {c}", cur.version))
            }
            Some(_) => {}
            None => {
                gens.current = Some(stage::Generation {
                    version: c.to_string(),
                    volumes: Vec::new(),
                    complete: true,
                    pallet: None,
                    migrations: Vec::new(),
                    kept: Vec::new(),
                    source: None,
                    at: now(),
                })
            }
        }
    }
    let image = match open_source(&req.source).await {
        Ok(d) => d,
        Err(e) => return ApiError::bad_request(format!("{}: {e}", req.source)),
    };
    let progress = Arc::new(Progress::default());
    let job = StageJob {
        version: version.clone(),
        state: "running".into(),
        error: None,
        plan: Vec::new(),
        pallet: None,
        progress: progress.clone(),
        started: now(),
    };
    *state.stage_job.lock().await = Some(job.clone());
    let body = job_json(&job);
    let st = state.clone();
    tokio::spawn(async move {
        let result = run_stage(&st, &dir, gens, &version, req, image, &progress).await;
        let mut job = st.stage_job.lock().await;
        if let Some(j) = job.as_mut() {
            match result {
                Ok((plan, pallet)) => {
                    j.state = "complete".into();
                    j.plan = plan;
                    j.pallet = pallet;
                    tracing::info!(version = %j.version, "release staged");
                }
                Err(e) => {
                    j.state = "failed".into();
                    j.error = Some(e.to_string());
                    tracing::error!(version = %j.version, "staging failed: {e}");
                }
            }
        }
    });
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

async fn run_stage(
    state: &Arc<AppState>,
    dir: &std::path::Path,
    mut gens: Generations,
    version: &str,
    req: StageRequest,
    image: Arc<dyn BlockDevice>,
    progress: &Progress,
) -> anyhow::Result<(Vec<stage::PlanItem>, Option<String>)> {
    // Room for the new generation: the one before the current goes (the
    // owner's "one previous generation"), and so does anything staged
    // before, whole or not.
    if let Some(prev) = gens.previous.take() {
        let n = stage::delete_generation(&state.volume_manager, &prev).await?;
        tracing::info!("stage {version}: release {}'s {n} volume(s) removed", prev.version);
        gens.save(dir)?;
    }
    if let Some(old) = gens.staged.take() {
        let n = stage::delete_generation(&state.volume_manager, &old).await?;
        tracing::info!("stage {version}: the earlier stage of {} discarded ({n} volume(s))", old.version);
        gens.save(dir)?;
    }
    let opts = StageOptions {
        version: version.to_string(),
        root: req.root.clone().unwrap_or_else(|| "stormpump".into()),
        source: req.source.clone(),
    };
    // Every volume is recorded as it is made, so a stage cut short is
    // discarded by the next (its goldens are unsealed: never "held").
    let save_dir = dir.to_path_buf();
    let mut record = gens.clone();
    let (mut gen, plan) = stage::stage(&state.volume_manager, image.clone(), &opts, progress, |g| {
        record.staged = Some(g.clone());
        if let Err(e) = record.save(&save_dir) {
            tracing::warn!("stage: recording progress: {e}");
        }
    })
    .await?;

    // The boot pallet, below the one the disk boots now.
    let mut pallet = None;
    match node_disk(state, req.disk.as_deref()).await {
        Some((path, disk)) => {
            let report = crate::image::local_boot::lay_local_boot_ranked(
                &path,
                disk,
                vec![(req.source.clone(), image.clone())],
                crate::image::local_boot::BootRank::BelowActive,
            )
            .await?;
            if !report.failed.is_empty() {
                anyhow::bail!("the boot pallet: {}", report.failed.join("; "));
            }
            let ladder = crate::image::local_boot::local_boot_ladder(&req.source, image.clone()).await?;
            pallet = ladder.first().map(|p| stage::hex(&p.0));
        }
        None => tracing::warn!("stage {version}: no node disk known; the boot pallet is not staged"),
    }
    gen.pallet = pallet.clone();
    gens.staged = Some(gen);
    gens.save(dir)?;
    Ok((plan, pallet))
}

pub async fn get_stage(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let job = state.stage_job.lock().await.clone();
    let gens = match data_dir(&state) {
        Ok(d) => Generations::load(&d),
        Err(r) => return r,
    };
    let staged = gens.staged.filter(|g| g.version == version);
    match job.filter(|j| j.version == version) {
        Some(j) => {
            let mut v = job_json(&j);
            v["generation"] = json!(staged);
            Json(v).into_response()
        }
        None => match staged {
            Some(g) => Json(json!({"version": version, "state": if g.complete { "complete" } else { "incomplete" }, "generation": g})).into_response(),
            None => ApiError::not_found(format!("release {version} is not staged here")),
        },
    }
}

pub async fn delete_stage(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let dir = match data_dir(&state) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if state.stage_job.lock().await.as_ref().is_some_and(|j| j.state == "running" && j.version == version) {
        return conflict(format!("release {version} is being staged"));
    }
    let mut gens = Generations::load(&dir);
    let Some(g) = gens.staged.take().filter(|g| g.version == version) else {
        return ApiError::not_found(format!("release {version} is not staged here"));
    };
    match stage::delete_generation(&state.volume_manager, &g).await {
        Ok(n) => {
            if let Err(e) = gens.save(&dir) {
                return ApiError::internal(e.to_string());
            }
            Json(json!({"version": version, "deleted": n})).into_response()
        }
        Err(e) => conflict(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ActivateRequest {
    /// The release the node runs now, when the engine has not been told.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub disk: Option<String>,
}

#[derive(Debug, Serialize)]
struct Renamed {
    id: crate::volume::VolumeId,
    from: String,
    to: String,
}

pub async fn post_activate(
    State(state): State<Arc<AppState>>,
    Path(version): Path<String>,
    body: Option<Json<ActivateRequest>>,
) -> Response {
    let req = body.map(|b| b.0).unwrap_or_default();
    let dir = match data_dir(&state) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut gens = Generations::load(&dir);
    let Some(staged) = gens.staged.clone().filter(|g| g.version == version) else {
        return ApiError::not_found(format!("release {version} is not staged here"));
    };
    if !staged.complete {
        return conflict(format!("release {version} is not completely staged"));
    }
    let current = match (gens.current.as_ref().map(|c| c.version.clone()), req.current) {
        (Some(c), _) => c,
        (None, Some(c)) => c,
        (None, None) => {
            return conflict(
                "the release this node runs is not known: say it (`current`) — its volumes are kept as `<name>@<current>`",
            )
        }
    };
    let renames = {
        let mut vm = state.volume_manager.lock().await;
        let (renames, moved) = match stage::activation_renames(&vm, &staged, &current).await {
            Ok(r) => r,
            Err(e) => return conflict(e),
        };
        if let Err(e) = stage::apply_renames(&mut vm, &renames).await {
            return conflict(e);
        }
        let prev_pallet = gens.current.as_ref().and_then(|c| c.pallet.clone());
        gens.previous = Some(stage::Generation {
            version: current.clone(),
            volumes: moved,
            complete: true,
            pallet: prev_pallet,
            migrations: Vec::new(),
            kept: Vec::new(),
            source: None,
            at: now(),
        });
        renames
    };
    // The pallet, after the names: a boot that finds N+1's kernel finds N+1's
    // volumes. The active pallet's digest is what a rollback raises again.
    let mut ladder = None;
    if let Some(p) = staged.pallet.as_deref().and_then(stage::unhex) {
        match node_disk(&state, req.disk.as_deref()).await {
            Some((path, disk)) => {
                let before = crate::image::local_boot::local_boot_ladder(&path, disk.clone()).await.ok();
                if let (Some(prev), Some(top)) = (gens.previous.as_mut(), before.as_ref().and_then(|l| l.first())) {
                    if prev.pallet.is_none() {
                        prev.pallet = Some(stage::hex(&top.0));
                    }
                }
                match crate::image::local_boot::raise_local_boot(&path, disk, p).await {
                    Ok(l) => ladder = Some(l),
                    Err(e) => tracing::error!("activate {version}: raising its boot pallet: {e}"),
                }
            }
            None => tracing::warn!("activate {version}: no node disk known; the boot pallet is not raised"),
        }
    }
    gens.current = Some(staged.clone());
    gens.staged = None;
    if let Err(e) = gens.save(&dir) {
        return ApiError::internal(format!("recording the generations: {e}"));
    }
    tracing::info!(version, previous = %current, "release activated: {} rename(s)", renames.len());
    Json(json!({
        "version": version,
        "previous": current,
        "renamed": renames.iter().map(|(id, from, to)| Renamed { id: *id, from: from.clone(), to: to.clone() }).collect::<Vec<_>>(),
        "boot_ladder": ladder,
        "migrations": staged.migrations,
    }))
    .into_response()
}

pub async fn post_rollback(State(state): State<Arc<AppState>>, body: Option<Json<ActivateRequest>>) -> Response {
    let req = body.map(|b| b.0).unwrap_or_default();
    let dir = match data_dir(&state) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut gens = Generations::load(&dir);
    let (Some(current), Some(previous)) = (gens.current.clone(), gens.previous.clone()) else {
        return conflict("no previous release on this node to roll back to");
    };
    let renames = stage::rollback_renames(&current, &previous);
    {
        let mut vm = state.volume_manager.lock().await;
        if let Err(e) = stage::apply_renames(&mut vm, &renames).await {
            return conflict(e);
        }
    }
    let mut ladder = None;
    if let Some(p) = previous.pallet.as_deref().and_then(stage::unhex) {
        if let Some((path, disk)) = node_disk(&state, req.disk.as_deref()).await {
            match crate::image::local_boot::raise_local_boot(&path, disk, p).await {
                Ok(l) => ladder = Some(l),
                Err(e) => tracing::error!("rollback to {}: raising its boot pallet: {e}", previous.version),
            }
        }
    }
    // What was current is staged again, whole: it can be activated again.
    gens.staged = Some(current.clone());
    gens.current = Some(previous.clone());
    gens.previous = None;
    if let Err(e) = gens.save(&dir) {
        return ApiError::internal(format!("recording the generations: {e}"));
    }
    tracing::warn!(from = %current.version, to = %previous.version, "release rolled back");
    Json(json!({
        "version": previous.version,
        "from": current.version,
        "renamed": renames.iter().map(|(id, from, to)| Renamed { id: *id, from: from.clone(), to: to.clone() }).collect::<Vec<_>>(),
        "boot_ladder": ladder,
    }))
    .into_response()
}

pub async fn get_generations(State(state): State<Arc<AppState>>) -> Response {
    match data_dir(&state) {
        Ok(d) => Json(Generations::load(&d)).into_response(),
        Err(r) => r,
    }
}
