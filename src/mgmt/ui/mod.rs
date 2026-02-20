//! Web UI — HTMX + Askama server-rendered management interface.

pub mod shared;
pub mod dashboard;
pub mod drives;
pub mod arrays;
pub mod volumes;
pub mod exports;
#[cfg(feature = "cluster")]
pub mod cluster;

use std::sync::Arc;

use axum::{
    Router,
    extract::Path,
    response::IntoResponse,
    routing::get,
};
use rust_embed::Embed;

use crate::mgmt::AppState;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Serve embedded static files (htmx.min.js, style.css).
async fn static_handler(Path(path): Path<String>) -> impl IntoResponse {
    match StaticAssets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Build the UI router — mounted at /ui.
pub fn ui_router(state: Arc<AppState>) -> Router {
    let r = Router::new()
        .route("/", get(dashboard::index))
        .route("/drives", get(drives::list_page))
        .route("/drives/table", get(drives::table_partial))
        .route("/arrays", get(arrays::list_page).post(arrays::create))
        .route("/arrays/table", get(arrays::table_partial))
        .route("/arrays/{id}", axum::routing::delete(arrays::delete))
        .route("/volumes", get(volumes::list_page).post(volumes::create))
        .route("/volumes/table", get(volumes::table_partial))
        .route("/volumes/{id}", axum::routing::delete(volumes::delete))
        .route(
            "/volumes/{id}/snapshot",
            axum::routing::post(volumes::snapshot),
        )
        .route("/volumes/snapshot", axum::routing::post(volumes::snapshot_form))
        .route("/exports", get(exports::list_page).post(exports::create))
        .route("/exports/table", get(exports::table_partial))
        .route("/exports/{id}", axum::routing::delete(exports::delete))
        .route("/static/{*path}", get(static_handler));

    #[cfg(feature = "cluster")]
    let r = r
        .route("/cluster", get(cluster::status_page))
        .route("/cluster/nodes/table", get(cluster::nodes_table_partial));

    #[cfg(not(feature = "cluster"))]
    let r = r.route("/cluster", get(cluster_disabled));

    r.with_state(state)
}

/// Placeholder when cluster feature is disabled.
#[cfg(not(feature = "cluster"))]
async fn cluster_disabled() -> impl IntoResponse {
    use askama::Template;
    #[derive(Template)]
    #[template(path = "cluster.html")]
    struct ClusterDisabled {
        active: &'static str,
        enabled: bool,
        local_node_id: u64,
        is_leader: bool,
        node_count: usize,
        online_count: usize,
        leader_display: String,
        nodes: Vec<()>,
    }
    let tmpl = ClusterDisabled {
        active: "cluster",
        enabled: false,
        local_node_id: 0,
        is_leader: false,
        node_count: 0,
        online_count: 0,
        leader_display: "None".to_string(),
        nodes: vec![],
    };
    shared::render(&tmpl)
}
