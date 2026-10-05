//! `/api/v1/erasures` — secure delete (#286): the node's erase level, what is
//! waiting to be overwritten, and the record of every volume whose freed data
//! was (`<data_dir>/erasures.json`, the last 1000).

use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, routing::get, Json, Router};

use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/", get(list)).with_state(state)
}

async fn list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.eraser.status().await)
}
