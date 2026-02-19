//! Prometheus metrics — /metrics endpoint.

use axum::{Router, routing::get, response::IntoResponse, http::StatusCode};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static PROMETHEUS_HANDLE: std::sync::OnceLock<PrometheusHandle> = std::sync::OnceLock::new();

/// Initialize the Prometheus metrics recorder.
/// Must be called once at startup before any metrics are recorded.
pub fn init_metrics() {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
    PROMETHEUS_HANDLE.set(handle).ok();
}

/// Register metric descriptions for drives, arrays, volumes, API.
pub fn register_metrics() {
    metrics::describe_gauge!("stormblock_drives_total", "Number of opened drives");
    metrics::describe_gauge!("stormblock_arrays_total", "Number of RAID arrays");
    metrics::describe_gauge!("stormblock_volumes_total", "Number of volumes");
    metrics::describe_gauge!("stormblock_exports_total", "Number of active exports");
    metrics::describe_gauge!(
        "stormblock_capacity_bytes",
        "Total raw capacity across all drives in bytes"
    );
    metrics::describe_gauge!(
        "stormblock_allocated_bytes",
        "Total allocated volume storage in bytes"
    );
    metrics::describe_counter!(
        "stormblock_api_requests_total",
        "Total REST API requests"
    );

    // Cluster metrics (registered unconditionally, only emitted when cluster is enabled)
    metrics::describe_gauge!(
        "stormblock_cluster_nodes_total",
        "Total known cluster nodes"
    );
    metrics::describe_gauge!(
        "stormblock_cluster_nodes_online",
        "Number of online cluster nodes"
    );
    metrics::describe_counter!(
        "stormblock_cluster_heartbeat_success_total",
        "Total successful heartbeats sent"
    );
    metrics::describe_counter!(
        "stormblock_cluster_heartbeat_failures_total",
        "Total failed heartbeats"
    );
}

async fn handle_metrics() -> impl IntoResponse {
    match PROMETHEUS_HANDLE.get() {
        Some(handle) => (StatusCode::OK, handle.render()),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "metrics not initialized".to_string()),
    }
}

/// Router for the /metrics endpoint.
pub fn metrics_router() -> Router {
    Router::new().route("/metrics", get(handle_metrics))
}
