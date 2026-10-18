//! `/api/v1/release` (#291): erase everything this node holds for its
//! cluster, once, at the next boot (`crate::release_erase`).
//!
//! - `POST {erase?: "once", force?: false, dry_run?: false}` (destructive):
//!   what a release would keep, lay again and erase. Unless `dry_run`, the
//!   ticket is written and captured to `stormblock-state` before the answer;
//!   it runs when the node next boots from its own disk. Refused (409) where
//!   it cannot run: no state volume (not a stormcos node's engine), a
//!   flow-over still moving, or a volume it would erase attached by a remote
//!   host (#292) unless `force`.
//! - `GET`: the pending ticket and the last run's report.
//! - `DELETE` (destructive): cancel a pending ticket.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::drive::erase::EraseLevel;
use crate::mgmt::auth::Caller;
use crate::mgmt::AppState;
use crate::release_erase::{self as rel, Action, Ticket};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/", get(status).post(ask).delete(cancel)).with_state(state)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    #[serde(default)]
    pub erase: Option<EraseLevel>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub dry_run: bool,
}

fn conflict(code: &str, message: String, extra: serde_json::Value) -> Response {
    let mut body = json!({"code": code, "message": message});
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    (StatusCode::CONFLICT, Json(body)).into_response()
}

fn read_json<T: serde::de::DeserializeOwned>(state: &AppState, name: &str) -> Option<T> {
    let dir = state.config.management.data_dir.as_deref()?;
    serde_json::from_slice(&std::fs::read(std::path::Path::new(dir).join(name)).ok()?).ok()
}

/// The ticket written into the data directory, then the directory captured
/// to `stormblock-state` at once.
async fn write_ticket(state: &AppState, t: &Ticket) -> Result<(), String> {
    let dir = state.config.management.data_dir.as_deref().ok_or("no data directory")?;
    let store = state.state_store.get().ok_or("no state volume")?;
    let dir = std::path::Path::new(dir);
    let tmp = dir.join(format!(".{}.tmp", rel::TICKET));
    std::fs::write(&tmp, serde_json::to_vec_pretty(t).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(rel::TICKET)).map_err(|e| e.to_string())?;
    store.capture_from(dir).await.map_err(|e| format!("capturing it to {}: {e}", rel::STATE_VOLUME))?;
    Ok(())
}

/// A release waiting for the next boot (health).
pub fn pending_here(state: &AppState) -> bool {
    read_json::<Ticket>(state, rel::TICKET).is_some_and(|t| t.pending())
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let ticket: Option<Ticket> = read_json(&state, rel::TICKET);
    let report: Option<rel::Report> = read_json(&state, rel::REPORT);
    Json(json!({
        "pending": ticket.as_ref().is_some_and(Ticket::pending),
        "ticket": ticket.map(|mut t| { t.declaration = None; t }),
        "report": report,
        "runs_here": state.state_store.get().is_some(),
    }))
    .into_response()
}

async fn ask(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    body: Option<Json<AskRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let erase = req.erase.unwrap_or(EraseLevel::Once).max(EraseLevel::Once);
    let items = rel::plan(&*state.volume_manager.lock().await).await;

    // A volume it would erase that a remote host has attached (#292).
    let ctx = crate::mgmt::api::usage::Context::gather(&state).await;
    let mut attached = Vec::new();
    for i in items.iter().filter(|i| !matches!(i.action, Action::Keep { .. })) {
        let Ok(u) = i.id.parse::<uuid::Uuid>() else { continue };
        let hosts: Vec<String> = ctx
            .attachments_of(&u)
            .iter()
            .flat_map(|a| a.remote.iter().map(|r| format!("{} ({})", r.host, r.addr)))
            .collect();
        if !hosts.is_empty() {
            attached.push(json!({"volume": i.name, "id": i.id, "hosts": hosts}));
        }
    }
    let plan = json!({
        "erase": erase,
        "keep": items.iter().filter(|i| matches!(i.action, Action::Keep { .. })).collect::<Vec<_>>(),
        "relay": items.iter().filter(|i| matches!(i.action, Action::Relay { .. })).collect::<Vec<_>>(),
        "erased": items.iter().filter(|i| matches!(i.action, Action::Erase { .. })).collect::<Vec<_>>(),
        "attached": attached,
    });
    if req.dry_run {
        return Json(json!({"dry_run": true, "plan": plan})).into_response();
    }
    if state.state_store.get().is_none() {
        return conflict(
            "not_a_node",
            format!("a release runs at the next boot of a stormcos node from its own disk; this engine has no {} volume", rel::STATE_VOLUME),
            json!({}),
        );
    }
    let left = state.flow_over_remaining.load(std::sync::atomic::Ordering::Relaxed);
    if left > 0 {
        return conflict("flow_over_running", format!("a flow-over is still moving {left} extent(s) onto this node's disk"), json!({}));
    }
    if !attached.is_empty() && !req.force {
        return conflict(
            "attached",
            format!("{} volume(s) it would erase are attached by a remote host; detach them, or ask with force", attached.len()),
            json!({"attached": attached}),
        );
    }
    let t = Ticket {
        id: uuid::Uuid::new_v4().to_string(),
        erase,
        asked_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        asked_by: caller.map(|Extension(c)| c.0).unwrap_or_else(|| "admin".into()),
        force: req.force,
        state: "pending".into(),
        declaration: None,
    };
    if let Err(e) = write_ticket(&state, &t).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"code": "ticket", "message": e}))).into_response();
    }
    println!("RELEASE: ticket {} recorded ({erase}): runs at the next boot", t.id);
    (StatusCode::ACCEPTED, Json(json!({"ticket": t, "plan": plan, "runs": "at the next boot from this node's disk"}))).into_response()
}

async fn cancel(State(state): State<Arc<AppState>>) -> Response {
    let Some(mut t) = read_json::<Ticket>(&state, rel::TICKET).filter(Ticket::pending) else {
        return (StatusCode::NOT_FOUND, Json(json!({"code": "no_ticket", "message": "no release is pending"}))).into_response();
    };
    t.state = "cancelled".into();
    match write_ticket(&state, &t).await {
        Ok(()) => Json(json!({"ticket": t})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"code": "ticket", "message": e}))).into_response(),
    }
}
