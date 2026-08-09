//! GET /api/v1/sessions — active iSCSI sessions.
//!
//! Exists so a consumer can tell whether an export is still in use before
//! withdrawing it. Without this the only option is a drain timer, which can
//! pull a LUN out from under a live mount (#29).

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    routing::get,
    response::IntoResponse,
    Json,
};
use serde::Serialize;

use crate::mgmt::AppState;
use crate::target::iscsi::session::SessionInfo;

#[derive(Debug, Serialize)]
pub struct SessionsResponse {
    pub items: Vec<SessionInfo>,
    pub count: usize,
    /// Sessions excluding discovery ones — the number that matters when
    /// deciding whether an export is safe to withdraw, since a discovery
    /// session never addresses a LUN.
    pub active: usize,
    /// Target IQN these sessions belong to.
    pub target_name: String,
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "sessions", "method" => "list")
        .increment(1);

    let target = state.iscsi_target.read().await.as_ref().cloned();
    let Some(target) = target else {
        return Json(SessionsResponse {
            items: Vec::new(),
            count: 0,
            active: 0,
            target_name: String::new(),
        });
    };

    let items = target.sessions().await;
    let active = items.iter().filter(|s| !s.discovery).count();
    metrics::gauge!("stormblock_iscsi_sessions_total").set(items.len() as f64);
    metrics::gauge!("stormblock_iscsi_sessions_active").set(active as f64);

    Json(SessionsResponse {
        count: items.len(),
        active,
        target_name: target.target_name().to_string(),
        items,
    })
}

/// GET /api/v1/luns/{id}/sessions is deliberately not offered: the target does
/// not track which session touched which LUN, and inventing that mapping would
/// be a stronger claim than the data supports. With one IQN per export the
/// target-level count is the correct signal.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_sessions))
        .with_state(state)
}
