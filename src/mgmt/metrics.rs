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
    metrics::describe_gauge!(
        "stormblock_drive_healthy",
        "Drive health status (1 = healthy, 0 = unhealthy)"
    );
    metrics::describe_gauge!(
        "stormblock_drive_temperature_celsius",
        "Drive temperature in degrees Celsius"
    );
    metrics::describe_gauge!(
        "stormblock_drive_media_errors",
        "Drive media error count"
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

    // Slab capacity — sampled at scrape time so thin-allocation growth and
    // reclaim are both visible (#25).
    metrics::describe_gauge!("stormblock_slabs_total", "Number of formatted slabs");
    metrics::describe_gauge!(
        "stormblock_slab_capacity_bytes",
        "Slab capacity in bytes"
    );
    metrics::describe_gauge!(
        "stormblock_slab_allocated_bytes",
        "Slab storage currently allocated to extents, in bytes"
    );
    metrics::describe_gauge!(
        "stormblock_slab_free_bytes",
        "Slab storage available for allocation, in bytes (rises as discards are reclaimed)"
    );
    metrics::describe_gauge!(
        "stormblock_slab_capacity_bytes_total",
        "Total slab capacity across all slabs in bytes"
    );
    metrics::describe_gauge!(
        "stormblock_slab_allocated_bytes_total",
        "Total allocated slab storage across all slabs in bytes"
    );
    metrics::describe_gauge!(
        "stormblock_slab_free_bytes_total",
        "Total free slab storage across all slabs in bytes"
    );
    metrics::describe_gauge!("stormblock_luns_total", "Number of exported iSCSI LUNs");
}

/// Refresh slab capacity gauges from live state.
///
/// Sampled at scrape time rather than tracked incrementally, so the numbers
/// cannot drift away from the slabs no matter which path allocated or freed.
/// This is what makes thin-allocation growth (and whether trims actually come
/// back) observable over time (#25).
async fn refresh_capacity_gauges(state: &crate::mgmt::AppState) {
    let registry = state.slab_registry.lock().await;

    let (mut cap_total, mut alloc_total, mut free_total) = (0u64, 0u64, 0u64);

    for (id, slab) in registry.iter() {
        let slot_size = slab.slot_size();
        let total = slab.total_slots();
        let free = slab.free_slots();
        let allocated = total.saturating_sub(free);

        let capacity_bytes = total * slot_size;
        let allocated_bytes = allocated * slot_size;
        let free_bytes = free * slot_size;

        let slab_id = id.0.to_string();
        let tier = slab.tier().to_string();

        metrics::gauge!("stormblock_slab_capacity_bytes", "slab" => slab_id.clone(), "tier" => tier.clone())
            .set(capacity_bytes as f64);
        metrics::gauge!("stormblock_slab_allocated_bytes", "slab" => slab_id.clone(), "tier" => tier.clone())
            .set(allocated_bytes as f64);
        metrics::gauge!("stormblock_slab_free_bytes", "slab" => slab_id, "tier" => tier)
            .set(free_bytes as f64);

        cap_total += capacity_bytes;
        alloc_total += allocated_bytes;
        free_total += free_bytes;
    }

    metrics::gauge!("stormblock_slabs_total").set(registry.len() as f64);
    metrics::gauge!("stormblock_slab_capacity_bytes_total").set(cap_total as f64);
    metrics::gauge!("stormblock_slab_allocated_bytes_total").set(alloc_total as f64);
    metrics::gauge!("stormblock_slab_free_bytes_total").set(free_total as f64);
}

async fn handle_metrics(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::mgmt::AppState>>,
) -> impl IntoResponse {
    refresh_capacity_gauges(&state).await;
    match PROMETHEUS_HANDLE.get() {
        Some(handle) => (StatusCode::OK, handle.render()),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "metrics not initialized".to_string()),
    }
}

/// Router for the /metrics endpoint.
pub fn metrics_router(state: std::sync::Arc<crate::mgmt::AppState>) -> Router {
    Router::new()
        .route("/metrics", get(handle_metrics))
        .with_state(state)
}
