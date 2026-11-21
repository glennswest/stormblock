//! Pruning rollback copies (#565): what an install set aside, kept to the
//! newest `[rollback] keep` of each name once the release that set them
//! aside has booted and been confirmed (a boot pallet marked successful).
//! The recognition and the order are [`crate::volume::rollback`]'s.
//!
//! Only a copy its install marked is pruned unasked: one recognised by its
//! name alone (set aside before #565) is *inferred*, and goes only when a
//! person asks for inferred copies (`include_inferred`, #720).
//!
//! Never pruned: a volume of a #122 stage (staged, current or previous
//! generation, all in `release-generations.json`), a migration's source (the
//! node's data a release's hook has still to read), and a copy anything
//! serves. Nothing at all while a stage waits to be activated: an upgrade not
//! yet confirmed.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::volume::rollback::{self, Copy};
use crate::volume::VolumeId;

/// A copy as the API reports it.
#[derive(Debug, Clone, Serialize)]
pub struct CopyView {
    pub id: uuid::Uuid,
    pub name: String,
    pub namespace: String,
    pub of: String,
    pub from: String,
    pub generation: u32,
    /// Unix seconds; 0 for a copy set aside before #565, recognised by its name.
    pub set_aside_at: u64,
    /// Recognised by its name, not marked by its install: pruned only when
    /// asked with `include_inferred` (#720).
    pub inferred: bool,
}

impl From<&Copy> for CopyView {
    fn from(c: &Copy) -> Self {
        CopyView {
            id: c.id.0,
            name: c.name.clone(),
            namespace: c.namespace.clone(),
            of: c.mark.of.clone(),
            from: c.mark.from.clone(),
            generation: c.mark.generation,
            set_aside_at: c.mark.set_aside_at,
            inferred: c.by_name,
        }
    }
}

/// What a prune did, or would do.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub keep: usize,
    pub dry_run: bool,
    /// Copies recognised by name only were in the plan (#720).
    pub include_inferred: bool,
    pub kept: Vec<CopyView>,
    pub pruned: Vec<CopyView>,
    /// Copies the prune would have deleted and did not, with why.
    pub skipped: Vec<Skipped>,
    /// Set when nothing was pruned because an upgrade is not yet confirmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub name: String,
    pub why: String,
}

/// The volumes a #122 stage keeps track of, and why a prune must wait (a
/// stage not yet activated).
fn staging(state: &AppState) -> (HashSet<VolumeId>, HashSet<String>, Option<String>) {
    let Some(dir) = state.data_dir.as_deref() else {
        return Default::default();
    };
    let g = crate::image::stage::Generations::load(dir);
    let mut ids = HashSet::new();
    let mut migration_sources = HashSet::new();
    for gen in [&g.current, &g.staged, &g.previous].into_iter().flatten() {
        ids.extend(gen.volumes.iter().map(|v| v.id));
        for m in &gen.migrations {
            migration_sources.insert(m.node.clone().unwrap_or_else(|| m.volume.clone()));
        }
    }
    let wait = g.staged.as_ref().map(|s| {
        format!("release {} is staged and not yet activated: an upgrade not yet confirmed", s.version)
    });
    (ids, migration_sources, wait)
}

/// The rollback copies on the node, with what a stage keeps left out.
async fn copies(state: &AppState) -> (Vec<Copy>, HashSet<String>, Option<String>) {
    let (found, _, migration_sources, wait) = copies_seen(state).await;
    (found, migration_sources, wait)
}

async fn copies_seen(state: &AppState) -> (Vec<Copy>, Vec<rollback::Seen>, HashSet<String>, Option<String>) {
    let (exclude, migration_sources, wait) = staging(state);
    let vm = state.volume_manager.lock().await;
    let seen = rollback::seen(&vm).await;
    drop(vm);
    (rollback::recognise(&seen, &exclude), seen, migration_sources, wait)
}

/// Mark the copies set aside before #565 (recognised by name), so the
/// volume API says what they are, and clear such a mark from a volume the
/// name rule no longer takes for a copy (#720). Persists when it changed
/// any; answers how many it marked.
pub async fn mark_by_name(state: &AppState) -> usize {
    let (found, seen, _, _) = copies_seen(state).await;
    let (n, cleared) = {
        let mut vm = state.volume_manager.lock().await;
        rollback::mark_by_name(&mut vm, &seen, &found)
    };
    if n > 0 {
        tracing::info!("rollback: {n} copy(ies) set aside before #565 recognised by name and marked (inferred: pruned only when asked)");
    }
    if cleared > 0 {
        tracing::info!("rollback: {cleared} volume(s) marked by name before #720 are not rollback copies: marks cleared");
    }
    if n + cleared > 0 {
        crate::volume::VolumeManager::persist_detached(&state.volume_manager).await;
    }
    n
}

/// Keep the newest `keep` copies of each name; delete the rest unless
/// `dry_run`. Copies recognised by name only are left alone, said in
/// `skipped`, unless `include_inferred` (#720, finding 6f750e62ad35).
pub async fn prune(state: &AppState, keep: usize, dry_run: bool, include_inferred: bool) -> Report {
    let (found, migration_sources, wait) = copies(state).await;
    let (planned, inferred): (Vec<Copy>, Vec<Copy>) = found.into_iter().partition(|c| include_inferred || !c.by_name);
    let (kept, gone) = rollback::plan(&planned, keep);
    let mut report =
        Report { keep, dry_run, include_inferred, kept: kept.iter().map(CopyView::from).collect(), ..Default::default() };
    for c in &inferred {
        report.skipped.push(Skipped {
            name: c.name.clone(),
            why: "recognised by its name only, not marked by an install: pruned only when asked with include_inferred".into(),
        });
    }
    if let Some(why) = wait {
        report.refused = Some(why);
        report.kept.extend(gone.iter().map(CopyView::from));
        return report;
    }
    for c in &gone {
        if migration_sources.contains(&c.name) {
            report.skipped.push(Skipped { name: c.name.clone(), why: "a migration's source, its hook not yet run".into() });
            continue;
        }
        let busy = crate::mgmt::api::what_is_serving(state, c.id.0).await;
        if !busy.is_empty() {
            report.skipped.push(Skipped { name: c.name.clone(), why: format!("in use: {}", busy.join(", ")) });
            continue;
        }
        if dry_run {
            report.pruned.push(CopyView::from(c));
            continue;
        }
        let pre = crate::volume::VolumeManager::preread_unlocked(&state.volume_manager, c.id).await.ok();
        let r = state.volume_manager.lock().await.delete_volume_preread(c.id, None, pre).await;
        match r {
            Ok(_) => report.pruned.push(CopyView::from(c)),
            Err(e) => report.skipped.push(Skipped { name: c.name.clone(), why: e.to_string() }),
        }
    }
    if !dry_run && !report.pruned.is_empty() {
        crate::volume::VolumeManager::persist_detached(&state.volume_manager).await;
        tracing::info!(
            "rollback: {} older copy(ies) pruned, {} kept ({keep} of each name){}",
            report.pruned.len(),
            report.kept.len(),
            if report.skipped.is_empty() { String::new() } else { format!(", {} skipped", report.skipped.len()) },
        );
    }
    // The inferred ones in one line: forge1 has a hundred of them.
    for s in report.skipped.iter().skip(inferred.len()) {
        tracing::warn!("rollback: {} not pruned: {}", s.name, s.why);
    }
    if !inferred.is_empty() {
        tracing::info!(
            "rollback: {} copy(ies) recognised by name only left alone; `POST /api/v1/rollback/prune {{\"include_inferred\": true}}` prunes them",
            inferred.len()
        );
    }
    report
}

/// A boot pallet was marked successful: the release running has booted and
/// been confirmed, so the copies older than the newest `keep` go.
pub fn after_boot_ok(state: &Arc<AppState>) {
    let cfg = state.config.rollback.clone();
    if !cfg.on_boot_ok() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        mark_by_name(&state).await;
        // What an install marked, only: a name is no proof (#720).
        let r = prune(&state, cfg.keep, false, false).await;
        if let Some(why) = r.refused {
            tracing::info!("rollback: boot-ok, nothing pruned: {why}");
        }
    });
}

/// At start: mark the copies set aside before #565, nothing deleted.
pub fn start(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        mark_by_name(&state).await;
    });
}

#[derive(Debug, Default, Deserialize)]
pub struct PruneRequest {
    /// Copies kept of each name; the node's `[rollback] keep` when absent.
    #[serde(default)]
    pub keep: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
    /// Copies recognised by their name only (set aside before #565) are in
    /// the plan too (#720): the person asking has seen them listed.
    #[serde(default)]
    pub include_inferred: bool,
}

async fn list(State(state): State<Arc<AppState>>) -> Response {
    let (found, _, wait) = copies(&state).await;
    let cfg = &state.config.rollback;
    Json(serde_json::json!({
        "keep": cfg.keep,
        "prune": cfg.prune,
        "waiting": wait,
        "copies": found.iter().map(CopyView::from).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn prune_now(State(state): State<Arc<AppState>>, body: Option<Json<PruneRequest>>) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let keep = req.keep.unwrap_or(state.config.rollback.keep);
    mark_by_name(&state).await;
    let r = prune(&state, keep, req.dry_run, req.include_inferred).await;
    let status = if r.refused.is_some() { axum::http::StatusCode::CONFLICT } else { axum::http::StatusCode::OK };
    (status, Json(r)).into_response()
}

/// `/api/v1/rollback`: `GET` lists the copies, `POST /prune` (admin) prunes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/", get(list)).route("/prune", post(prune_now)).with_state(state)
}
