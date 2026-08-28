//! Prometheus metrics — /metrics endpoint.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::{Router, routing::get, response::IntoResponse, http::StatusCode};
use metrics::{Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// The recorder behind `/metrics`.
///
/// `metrics-exporter-prometheus` was a second HTTP server's worth of crates
/// (#79) to format text this router already serves. What `/metrics` needs
/// is a registry of counters, gauges and histograms keyed by name + labels,
/// and ~100 lines to print them in the exposition format — which is this.
/// The `metrics` facade every call site uses is unchanged.
static REGISTRY: OnceLock<Arc<PrometheusRegistry>> = OnceLock::new();

/// Histogram buckets, in the unit the histogram is recorded in (seconds
/// for every histogram this engine keeps). Prometheus' defaults.
const BUCKETS: [f64; 11] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

#[derive(Default)]
struct HistogramCell {
    counts: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    /// Sum as f64 bits, updated with a CAS loop.
    sum_bits: AtomicU64,
}

impl HistogramFn for HistogramCell {
    fn record(&self, value: f64) {
        for (i, b) in BUCKETS.iter().enumerate() {
            if value <= *b {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut cur = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + value).to_bits();
            match self.sum_bits.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}

#[derive(Default)]
struct GaugeCell(AtomicU64);

impl GaugeFn for GaugeCell {
    fn increment(&self, value: f64) {
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + value).to_bits();
            match self.0.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
    fn decrement(&self, value: f64) {
        self.increment(-value);
    }
    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct CounterCell(AtomicU64);

impl CounterFn for CounterCell {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    fn absolute(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Inner {
    descriptions: HashMap<String, (String, Option<Unit>)>,
    counters: HashMap<Key, Arc<CounterCell>>,
    gauges: HashMap<Key, Arc<GaugeCell>>,
    histograms: HashMap<Key, Arc<HistogramCell>>,
}

#[derive(Default)]
pub struct PrometheusRegistry {
    inner: Mutex<Inner>,
}

impl Recorder for PrometheusRegistry {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.lock().unwrap().descriptions.insert(key.as_str().to_string(), (description.to_string(), unit));
    }
    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.lock().unwrap().descriptions.insert(key.as_str().to_string(), (description.to_string(), unit));
    }
    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.lock().unwrap().descriptions.insert(key.as_str().to_string(), (description.to_string(), unit));
    }
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let cell = self.inner.lock().unwrap().counters.entry(key.clone()).or_default().clone();
        Counter::from_arc(cell)
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let cell = self.inner.lock().unwrap().gauges.entry(key.clone()).or_default().clone();
        Gauge::from_arc(cell)
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let cell = self.inner.lock().unwrap().histograms.entry(key.clone()).or_default().clone();
        Histogram::from_arc(cell)
    }
}

fn labels_of(key: &Key) -> String {
    let mut labels: Vec<(String, String)> =
        key.labels().map(|l| (l.key().to_string(), l.value().to_string())).collect();
    labels.sort();
    if labels.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")))
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn labels_with(key: &Key, extra: (&str, &str)) -> String {
    let mut labels: Vec<(String, String)> =
        key.labels().map(|l| (l.key().to_string(), l.value().to_string())).collect();
    labels.push((extra.0.to_string(), extra.1.to_string()));
    labels.sort();
    let parts: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
    format!("{{{}}}", parts.join(","))
}

impl PrometheusRegistry {
    /// The exposition-format text of everything recorded so far.
    pub fn render(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut out = String::new();
        // Group by metric name so HELP/TYPE appear once per family.
        let mut families: BTreeMap<String, (&str, Vec<String>)> = BTreeMap::new();
        for (key, cell) in &inner.counters {
            families
                .entry(key.name().to_string())
                .or_insert_with(|| ("counter", Vec::new()))
                .1
                .push(format!("{}{} {}", key.name(), labels_of(key), cell.0.load(Ordering::Relaxed)));
        }
        for (key, cell) in &inner.gauges {
            let v = f64::from_bits(cell.0.load(Ordering::Relaxed));
            families
                .entry(key.name().to_string())
                .or_insert_with(|| ("gauge", Vec::new()))
                .1
                .push(format!("{}{} {}", key.name(), labels_of(key), fmt_f64(v)));
        }
        for (key, cell) in &inner.histograms {
            let name = key.name();
            let entry = families.entry(name.to_string()).or_insert_with(|| ("histogram", Vec::new()));
            for (i, b) in BUCKETS.iter().enumerate() {
                entry.1.push(format!(
                    "{name}_bucket{} {}",
                    labels_with(key, ("le", &fmt_f64(*b))),
                    cell.counts[i].load(Ordering::Relaxed)
                ));
            }
            let count = cell.count.load(Ordering::Relaxed);
            entry.1.push(format!("{name}_bucket{} {count}", labels_with(key, ("le", "+Inf"))));
            entry.1.push(format!("{name}_sum{} {}", labels_of(key), fmt_f64(f64::from_bits(cell.sum_bits.load(Ordering::Relaxed)))));
            entry.1.push(format!("{name}_count{} {count}", labels_of(key)));
        }
        for (name, (kind, mut lines)) in families {
            if let Some((desc, _)) = inner.descriptions.get(&name) {
                let _ = writeln!(out, "# HELP {name} {desc}");
            }
            let _ = writeln!(out, "# TYPE {name} {kind}");
            lines.sort();
            for l in lines {
                out.push_str(&l);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

fn fmt_f64(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Initialize the metrics recorder.
/// Must be called once at startup before any metrics are recorded.
pub fn init_metrics() {
    let reg = Arc::new(PrometheusRegistry::default());
    if REGISTRY.set(reg.clone()).is_ok() {
        if let Err(e) = metrics::set_global_recorder(RegistryHandle(reg)) {
            tracing::warn!("metrics recorder already installed: {e}");
        }
    }
}

/// `metrics` wants an owned recorder; the registry itself lives behind the
/// `OnceLock` so `/metrics` can read it.
struct RegistryHandle(Arc<PrometheusRegistry>);

impl Recorder for RegistryHandle {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_counter(key, unit, description)
    }
    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_gauge(key, unit, description)
    }
    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_histogram(key, unit, description)
    }
    fn register_counter(&self, key: &Key, m: &Metadata<'_>) -> Counter {
        self.0.register_counter(key, m)
    }
    fn register_gauge(&self, key: &Key, m: &Metadata<'_>) -> Gauge {
        self.0.register_gauge(key, m)
    }
    fn register_histogram(&self, key: &Key, m: &Metadata<'_>) -> Histogram {
        self.0.register_histogram(key, m)
    }
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
    let registry = state.slab_registry.read().await;

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
    match REGISTRY.get() {
        Some(reg) => (StatusCode::OK, reg.render()),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "metrics not initialized".to_string()),
    }
}

/// Router for the /metrics endpoint.
pub fn metrics_router(state: std::sync::Arc<crate::mgmt::AppState>) -> Router {
    Router::new()
        .route("/metrics", get(handle_metrics))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry renders the exposition format Prometheus scrapes:
    /// HELP/TYPE per family, labels sorted, histograms as cumulative buckets.
    #[test]
    fn renders_exposition_format() {
        let reg = PrometheusRegistry::default();
        reg.describe_gauge("t_gauge".into(), None, "a gauge".into());
        let md = Metadata::new("t", metrics::Level::INFO, None);
        let g = reg.register_gauge(&Key::from_parts("t_gauge", vec![metrics::Label::new("tier", "hot"), metrics::Label::new("slab", "a")]), &md);
        g.set(1.5);
        let c = reg.register_counter(&Key::from_static_name("t_total"), &md);
        c.increment(3);
        let h = reg.register_histogram(&Key::from_static_name("t_seconds"), &md);
        h.record(0.03);
        h.record(2.0);
        let text = reg.render();
        assert!(text.contains("# HELP t_gauge a gauge\n# TYPE t_gauge gauge\nt_gauge{slab=\"a\",tier=\"hot\"} 1.5\n"), "{text}");
        assert!(text.contains("# TYPE t_total counter\nt_total 3\n"), "{text}");
        assert!(text.contains("t_seconds_bucket{le=\"0.05\"} 1\n"), "{text}");
        assert!(text.contains("t_seconds_bucket{le=\"2.5\"} 2\n"), "{text}");
        assert!(text.contains("t_seconds_bucket{le=\"+Inf\"} 2\n"), "{text}");
        assert!(text.contains("t_seconds_sum 2.03\n"), "{text}");
        assert!(text.contains("t_seconds_count 2\n"), "{text}");
    }
}
